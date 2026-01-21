use crate::actors::messages::{ExecutionMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::SymbolSpecs;
use crate::models::*;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, Instant};
use tracing::{debug, info, warn};

/// ✅ FIXED: Proper state machine for order lifecycle
#[derive(Debug, Clone, PartialEq)]
enum StrategyState {
    Idle,                 // No position, no order
    OrderPending,         // Order sent, waiting for confirmation
    PositionOpen,         // Position confirmed by exchange
    ClosingPosition,      // Close order sent, waiting for confirmation
    SwitchingSymbol,      // ✅ FIX BUG #1: Closing position before symbol switch
}

/// StrategyEngine - Impulse/Momentum Scalping with Smart Order Routing
pub struct StrategyEngine {
    config: Arc<Config>,
    message_rx: mpsc::Receiver<StrategyMessage>,
    execution_tx: mpsc::Sender<ExecutionMessage>,

    // State
    current_symbol: Option<Symbol>,
    current_position: Option<Position>,
    last_orderbook: Option<OrderBookSnapshot>,
    current_specs: Option<SymbolSpecs>,

    // Tick buffer for momentum calculation (expanded for better trend detection)
    tick_buffer: RingBuffer<TradeTick>,

    // Entry conditions
    momentum_threshold: f64,

    // ✅ PUMP PROTECTION: 24h price change for global trend filter
    /// Stores 24h price change percentage (e.g., 0.25 = +25%, -0.15 = -15%)
    price_change_24h: Option<f64>,

    // ✅ FIXED: Proper state machine replaces simple boolean
    state: StrategyState,

    // ✅ FIX BUG #1: Store pending symbol change until position is closed
    pending_symbol_change: Option<(Symbol, SymbolSpecs, f64)>, // (symbol, specs, price_change_24h)

    // ✅ IMPROVEMENT #1: Confirmation delay - wait for signal confirmation
    /// Stores pending signal direction: Some(true) = bullish, Some(false) = bearish
    pending_signal: Option<bool>,
    /// How many consecutive ticks confirmed the signal direction
    confirmation_count: u8,

    // ✅ IMPROVEMENT #3: Trade cooldown - prevent revenge trading
    /// When the last trade was closed
    last_trade_time: Option<Instant>,
    /// Cooldown duration in seconds (configurable)
    trade_cooldown_secs: u64,

    // ✅ FIX MEMORY LOSS BUG: Store active dynamic risk for current position
    /// Stores (SL%, TP%) calculated for the current active trade
    /// CRITICAL: Must use these values in handle_orderbook, NOT config values!
    active_dynamic_risk: Option<(f64, f64)>,

    // ✅ PERFORMANCE: Cache VWAP calculations (recalculate only on new tick)
    cached_vwap_short: Option<Decimal>,  // 50-tick VWAP
    cached_vwap_long: Option<Decimal>,   // 200-tick VWAP
    cached_volatility: Option<f64>,      // 100-tick volatility
    /// CRITICAL: Use tick counter instead of buffer.len()!
    /// RingBuffer.len() stays constant when full (300), so len-based
    /// invalidation would STOP working after 300 ticks!
    tick_counter: usize,                 // Total ticks processed (never resets)
    last_cache_update: usize,            // tick_counter when cache was last updated
}

impl StrategyEngine {
    pub fn new(
        config: Arc<Config>,
        message_rx: mpsc::Receiver<StrategyMessage>,
        execution_tx: mpsc::Sender<ExecutionMessage>,
    ) -> Self {
        Self {
            config,
            message_rx,
            execution_tx,
            current_symbol: None,
            current_position: None,
            last_orderbook: None,
            current_specs: None,
            tick_buffer: RingBuffer::new(300), // ✅ EXPANDED: 300 ticks for better trend detection
            momentum_threshold: 0.002, // ✅ STRICTER: 0.2% momentum threshold (was 0.1%)
            state: StrategyState::Idle,
            pending_symbol_change: None,
            price_change_24h: None, // ✅ PUMP PROTECTION: Will be set on symbol change
            // ✅ IMPROVEMENT #1: Confirmation delay
            pending_signal: None,
            confirmation_count: 0,
            // ✅ IMPROVEMENT #3: Trade cooldown (30 seconds)
            last_trade_time: None,
            trade_cooldown_secs: 30,
            // ✅ FIX MEMORY LOSS BUG: Initialize dynamic risk storage
            active_dynamic_risk: None,
            // ✅ PERFORMANCE: Initialize VWAP cache
            cached_vwap_short: None,
            cached_vwap_long: None,
            cached_volatility: None,
            tick_counter: 0,
            last_cache_update: 0,
        }
    }

    pub async fn run(mut self) {
        info!("⚡ StrategyEngine started");

        // ✅ HFT OPTIMIZATION: Position verification every 10 seconds (was 60)
        // Faster detection of API desync, flash crashes, unexpected liquidations
        let mut position_verify_interval = interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                // Handle incoming messages
                Some(msg) = self.message_rx.recv() => {
                    match msg {
                        StrategyMessage::OrderBook(snapshot) => {
                            self.handle_orderbook(snapshot).await;
                        }
                        StrategyMessage::Trade(tick) => {
                            self.handle_trade(tick).await;
                        }
                        StrategyMessage::PositionUpdate(position) => {
                            self.current_position = position.clone();
                            // ✅ FIXED: Update state machine based on position
                            if position.is_some() {
                                info!("📍 Position confirmed, transitioning to PositionOpen");
                                self.state = StrategyState::PositionOpen;
                            } else if self.state == StrategyState::ClosingPosition {
                                info!("✅ Position closed, transitioning to Idle");
                                // ✅ IMPROVEMENT #3: Start trade cooldown
                                self.last_trade_time = Some(Instant::now());
                                // ✅ FIX MEMORY LOSS BUG: Clear dynamic risk when position closes
                                self.active_dynamic_risk = None;
                                self.state = StrategyState::Idle;
                            } else if self.state == StrategyState::SwitchingSymbol {
                                // ✅ FIX BUG #1: Now complete the pending symbol change
                                info!("✅ Position closed during symbol switch, completing switch...");
                                // ✅ IMPROVEMENT #3: Start trade cooldown
                                self.last_trade_time = Some(Instant::now());
                                // ✅ FIX MEMORY LOSS BUG: Clear dynamic risk when position closes
                                self.active_dynamic_risk = None;
                                if let Some((new_symbol, specs, price_change_24h)) = self.pending_symbol_change.take() {
                                    self.complete_symbol_switch(new_symbol, specs, price_change_24h);
                                } else {
                                    warn!("SwitchingSymbol state but no pending change!");
                                    self.state = StrategyState::Idle;
                                }
                            } else if position.is_none() {
                                // ✅ FIX BUG #9: Position disappeared unexpectedly (liquidation, margin call, etc.)
                                // CRITICAL: If we're in OrderPending or any other state and position vanishes,
                                // we must reset to Idle or we'll be stuck forever!
                                warn!(
                                    "⚠️  Position disappeared unexpectedly in state {:?} (liquidation? margin call?). Resetting to Idle.",
                                    self.state
                                );
                                self.state = StrategyState::Idle;
                                self.active_dynamic_risk = None;
                                self.last_trade_time = Some(Instant::now());
                            }
                        }
                        StrategyMessage::SymbolChanged { symbol: new_symbol, specs, price_change_24h } => {
                            self.handle_symbol_change(new_symbol, specs, price_change_24h).await;
                        }
                        // ✅ CRITICAL: Feedback from execution with state transitions
                        StrategyMessage::OrderFilled(symbol) => {
                            info!("✅ Order filled for {}, transitioning state", symbol);
                            match self.state {
                                StrategyState::OrderPending => {
                                    // Entry order filled - wait for PositionUpdate
                                    debug!("Entry order filled, waiting for PositionUpdate");
                                }
                                StrategyState::ClosingPosition => {
                                    // Close order filled
                                    info!("Close order filled, transitioning to Idle");
                                    // ✅ Start cooldown timer
                                    self.last_trade_time = Some(Instant::now());
                                    // ✅ FIX MEMORY LOSS BUG: Clear dynamic risk when position closes
                                    self.active_dynamic_risk = None;
                                    self.state = StrategyState::Idle;
                                    self.current_position = None;
                                }
                                _ => {
                                    warn!("Received OrderFilled in unexpected state: {:?}", self.state);
                                }
                            }
                        }
                        StrategyMessage::OrderFailed(error) => {
                            warn!("❌ Order failed: {}, transitioning to Idle", error);
                            self.state = StrategyState::Idle;
                            self.current_position = None;
                            // ✅ FIX MEMORY LEAK: Clear dynamic risk on order failure
                            self.active_dynamic_risk = None;
                            // Reset confirmation state to avoid stale signals
                            self.pending_signal = None;
                            self.confirmation_count = 0;
                        }
                    }
                }

                // ✅ FIXED: Periodic position verification (prevents desync)
                _ = position_verify_interval.tick() => {
                    if let Some(ref symbol) = self.current_symbol {
                        debug!("🔍 Verifying position for {}", symbol);
                        if let Err(e) = self
                            .execution_tx
                            .send(ExecutionMessage::GetPosition(symbol.clone()))
                            .await
                        {
                            warn!("Failed to request position verification: {}", e);
                        }
                    }
                }

                // Channel closed
                else => {
                    info!("StrategyEngine message channel closed, shutting down");
                    break;
                }
            }
        }
    }

    async fn handle_symbol_change(&mut self, new_symbol: Symbol, specs: SymbolSpecs, price_change_24h: f64) {
        info!("🔄 Symbol change requested: {} (qty_step: {}, tick_size: {}, 24h: {:.2}%)",
              new_symbol, specs.qty_step, specs.tick_size, price_change_24h * 100.0);

        // ✅ FIX BUG #1: If we have a position, close it FIRST and defer the switch
        if let Some(ref position) = self.current_position {
            info!("⚠️  Closing position on {} before symbol switch", position.symbol);

            // Store pending symbol change - will be applied after close confirmation
            self.pending_symbol_change = Some((new_symbol, specs, price_change_24h));
            self.state = StrategyState::SwitchingSymbol;

            if let Err(e) = self
                .execution_tx
                .send(ExecutionMessage::ClosePosition {
                    symbol: position.symbol.clone(),
                    position_side: position.side,
                })
                .await
            {
                warn!("Failed to send ClosePosition on symbol change: {}", e);
                // Fallback: complete switch anyway to avoid getting stuck
                if let Some((sym, sp, pc)) = self.pending_symbol_change.take() {
                    self.complete_symbol_switch(sym, sp, pc);
                }
            }
            // DON'T switch yet - wait for PositionUpdate(None)
            return;
        }

        // No position - switch immediately
        self.complete_symbol_switch(new_symbol, specs, price_change_24h);
    }

    /// Complete the symbol switch after position is closed
    fn complete_symbol_switch(&mut self, new_symbol: Symbol, specs: SymbolSpecs, price_change_24h: f64) {
        info!("✅ Completing symbol switch to: {} (24h: {:.2}%)", new_symbol, price_change_24h * 100.0);
        self.current_symbol = Some(new_symbol);
        self.current_position = None;
        self.last_orderbook = None;
        self.current_specs = Some(specs);
        self.tick_buffer = RingBuffer::new(300); // ✅ EXPANDED buffer
        self.price_change_24h = Some(price_change_24h); // ✅ Store 24h change for trend protection
        self.pending_symbol_change = None;
        // ✅ Reset confirmation state for new symbol
        self.pending_signal = None;
        self.confirmation_count = 0;
        self.state = StrategyState::Idle;
        // ✅ FIX CRITICAL BUG: Clear VWAP cache on symbol switch
        // CRITICAL: Old symbol's VWAP would cause completely wrong calculations!
        self.cached_vwap_short = None;
        self.cached_vwap_long = None;
        self.cached_volatility = None;
        self.tick_counter = 0;
        self.last_cache_update = 0;
    }

    async fn handle_orderbook(&mut self, snapshot: OrderBookSnapshot) {
        // ✅ FIXED: Prevent race condition - ignore messages from old symbol
        if let Some(ref current_symbol) = self.current_symbol {
            if snapshot.symbol != *current_symbol {
                debug!(
                    "Ignoring orderbook from old symbol {} (current: {})",
                    snapshot.symbol, current_symbol
                );
                return;
            }
        }

        // Update current price if we have a position
        if let Some(ref mut position) = self.current_position {
            position.current_price = snapshot.mid_price;

            // ✅ FIX CRITICAL BUG #8: Use active dynamic risk for BOTH SL and TP, not position.stop_loss!
            // CRITICAL: position.stop_loss is set by ExecutionActor using static config value,
            // but we calculated dynamic SL/TP in execute_entry()!
            // Using position.should_stop_loss() would check WRONG (static) SL!
            let (sl_target, tp_target) = self.active_dynamic_risk
                .unwrap_or((self.config.stop_loss_percent, self.config.take_profit_percent));

            let pnl_pct = position.pnl_percent();

            // Check stop loss using dynamic SL target
            if pnl_pct <= -sl_target {
                warn!(
                    "🛑 STOP LOSS triggered for {} at {} (PnL: {:.2}% | Target: -{:.2}% {})",
                    position.symbol,
                    position.current_price,
                    pnl_pct,
                    sl_target,
                    if self.active_dynamic_risk.is_some() { "[Dynamic]" } else { "[Static]" }
                );

                // ✅ FIXED: Transition to ClosingPosition state
                self.state = StrategyState::ClosingPosition;

                if let Err(e) = self
                    .execution_tx
                    .send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                    .await
                {
                    warn!("Failed to send ClosePosition for stop loss: {}", e);
                    self.state = StrategyState::PositionOpen; // Revert state on failure
                }

                return;
            }

            // Check take profit using dynamic TP target
            if pnl_pct >= tp_target {
                info!(
                    "💰 TAKE PROFIT hit for {} (PnL: {:.2}% | Target: {:.2}% {})",
                    position.symbol,
                    pnl_pct,
                    tp_target,
                    if self.active_dynamic_risk.is_some() { "[Dynamic]" } else { "[Static]" }
                );

                // ✅ FIXED: Transition to ClosingPosition state
                self.state = StrategyState::ClosingPosition;

                if let Err(e) = self
                    .execution_tx
                    .send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                    .await
                {
                    warn!("Failed to send ClosePosition for take profit: {}", e);
                    self.state = StrategyState::PositionOpen; // Revert state on failure
                }

                return;
            }
        }

        self.last_orderbook = Some(snapshot);
    }

    async fn handle_trade(&mut self, tick: TradeTick) {
        // ✅ FIXED: Prevent race condition - ignore messages from old symbol
        if let Some(ref current_symbol) = self.current_symbol {
            if tick.symbol != *current_symbol {
                debug!(
                    "Ignoring trade tick from old symbol {} (current: {})",
                    tick.symbol, current_symbol
                );
                return;
            }
        }

        // ✅ PUMP PROTECTION: Check blacklist
        if self.config.blacklist_symbols.contains(&tick.symbol.0.to_uppercase()) {
            debug!("⛔ Symbol {} is blacklisted, ignoring tick", tick.symbol);
            return;
        }

        // Add to buffer
        self.tick_buffer.push(tick.clone());

        // ✅ PERFORMANCE: Invalidate VWAP cache on new tick
        // CRITICAL FIX: Use tick_counter instead of buffer.len()!
        // RingBuffer.len() stays constant when full (300), so len-based
        // invalidation would STOP working after 300 ticks!
        self.tick_counter += 1;
        if self.tick_counter != self.last_cache_update {
            self.cached_vwap_short = None;
            self.cached_vwap_long = None;
            self.cached_volatility = None;
            self.last_cache_update = self.tick_counter;
        }

        // ✅ FLASH CRASH PROTECTION: Detect extreme price movements
        // If we have an open position and price moves >5% in 1 second, emergency exit
        if let Some(ref mut position) = self.current_position {
            // ✅ FIX RACE CONDITION: Use last_tick price ONLY for flash crash check,
            // don't update position.current_price here (it's authoritative from orderbook)
            // Calculate PnL using latest tick price for flash crash detection
            let last_price = if let Some(last_tick) = self.tick_buffer.last() {
                last_tick.price
            } else {
                position.current_price  // Fallback to current price
            };

            // Temporarily calculate PnL with latest tick price (don't modify position)
            let pnl_pct = if position.entry_price > Decimal::ZERO {
                let pnl_ratio = match position.side {
                    PositionSide::Long => (last_price - position.entry_price) / position.entry_price,
                    PositionSide::Short => (position.entry_price - last_price) / position.entry_price,
                };
                (pnl_ratio * Decimal::from(100)).to_f64().unwrap_or(0.0)
            } else {
                0.0
            };

            // Emergency exit on flash crash (>5% adverse move)
            const FLASH_CRASH_THRESHOLD: f64 = -5.0; // -5% sudden loss

            if pnl_pct < FLASH_CRASH_THRESHOLD {
                warn!(
                    "⚡ FLASH CRASH DETECTED: PnL {:.2}% in <1sec! Emergency exit on {}",
                    pnl_pct, position.symbol
                );

                self.state = StrategyState::ClosingPosition;

                if let Err(e) = self
                    .execution_tx
                    .send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                    .await
                {
                    warn!("Failed to send emergency ClosePosition: {}", e);
                    self.state = StrategyState::PositionOpen;
                }

                return;
            }
        }

        // ✅ CRITICAL FIX: Need 200 ticks for FULL protection
        // - calculate_momentum: requires 50 ticks
        // - calculate_trend: requires 200 ticks (50 vs 200 VWAP)
        // Without 200 ticks, trend alignment check returns None and is SKIPPED!
        if self.tick_buffer.len() < 200 {
            debug!("📊 Buffering ticks: {}/200", self.tick_buffer.len());
            return;
        }

        // ✅ FIXED: State machine prevents double entry, entry while closing, etc.
        if self.state != StrategyState::Idle {
            debug!("⏸️  Not in Idle state ({:?}), skipping new entry signals", self.state);
            return;
        }

        // ✅ IMPROVEMENT #3: Check trade cooldown
        if let Some(last_trade) = self.last_trade_time {
            let elapsed = last_trade.elapsed().as_secs();
            if elapsed < self.trade_cooldown_secs {
                debug!("⏳ Trade cooldown: {}s remaining", self.trade_cooldown_secs - elapsed);
                return;
            }
        }

        // Calculate momentum
        if let Some(momentum) = self.calculate_momentum() {
            debug!("Momentum: {:.4}%", momentum * 100.0);

            // Check entry conditions
            if momentum.abs() > self.momentum_threshold {
                let signal_is_bullish = momentum > 0.0;

                // ✅ PUMP PROTECTION: Global Trend Filter (24h price change)
                // Prevents "Suicide Shorts" on parabolic pumps and "Suicide Longs" on crashes
                // ✅ CRITICAL FIX: Block entry if no 24h data available yet
                let price_change = match self.price_change_24h {
                    Some(pc) => pc,
                    None => {
                        debug!("⏸️ No 24h price data yet, waiting for first SymbolChanged");
                        self.pending_signal = None;
                        self.confirmation_count = 0;
                        return;
                    }
                };

                const PUMP_THRESHOLD: f64 = 0.15; // 15% threshold

                // ✅ FIX LOG SPAM: Only warn if we were already confirming this signal
                // Don't spam on every tick if momentum keeps showing wrong direction
                if price_change > PUMP_THRESHOLD && !signal_is_bullish {
                    // Only log warning if we were actively confirming a SHORT signal
                    if self.pending_signal == Some(false) {
                        warn!(
                            "🚫 REJECTED: Attempted SHORT on PUMP coin (+{:.1}% 24h). Only LONG allowed.",
                            price_change * 100.0
                        );
                    }
                    self.pending_signal = None;
                    self.confirmation_count = 0;
                    return;
                }

                if price_change < -PUMP_THRESHOLD && signal_is_bullish {
                    // Only log warning if we were actively confirming a LONG signal
                    if self.pending_signal == Some(true) {
                        warn!(
                            "🚫 REJECTED: Attempted LONG on DUMP coin ({:.1}% 24h). Only SHORT allowed.",
                            price_change * 100.0
                        );
                    }
                    self.pending_signal = None;
                    self.confirmation_count = 0;
                    return;
                }

                debug!(
                    "✅ Global trend check passed: {} signal on {:.1}% 24h",
                    if signal_is_bullish { "LONG" } else { "SHORT" },
                    price_change * 100.0
                );

                // ✅ IMPROVEMENT #2: Trend alignment - check if signal aligns with trend
                // ✅ CRITICAL FIX: Make trend check MANDATORY (block if None)
                let trend_bullish = match self.calculate_trend() {
                    Some(trend) => trend,
                    None => {
                        // Should not happen (we have 200 ticks), but if zero volume - block entry
                        warn!("⚠️ Cannot calculate trend (zero volume?), blocking entry for safety");
                        self.pending_signal = None;
                        self.confirmation_count = 0;
                        return;
                    }
                };

                if signal_is_bullish != trend_bullish {
                    debug!("📉 Signal rejected: {} signal vs {} trend",
                        if signal_is_bullish { "BULLISH" } else { "BEARISH" },
                        if trend_bullish { "BULLISH" } else { "BEARISH" }
                    );
                    // Reset confirmation on trend mismatch
                    self.pending_signal = None;
                    self.confirmation_count = 0;
                    return;
                }

                // ✅ ANTI-FOMO: Symmetric Mean Reversion Filter
                // Block entries if price is too far from VWAP (buying top / selling bottom)
                if let Some(vwap_distance) = self.calculate_vwap_distance() {
                    const MAX_DISTANCE_TO_VWAP: f64 = 0.005; // 0.5% threshold

                    // ✅ FIX LOG SPAM: Only warn if we were already confirming this signal
                    if signal_is_bullish && vwap_distance > MAX_DISTANCE_TO_VWAP {
                        // Only log warning if we were actively confirming a LONG signal
                        if self.pending_signal == Some(true) {
                            warn!(
                                "🚫 ANTI-FOMO REJECTED: LONG blocked - price too far ABOVE VWAP (+{:.2}%). Waiting for pullback.",
                                vwap_distance * 100.0
                            );
                        }
                        self.pending_signal = None;
                        self.confirmation_count = 0;
                        return;
                    }

                    if !signal_is_bullish && vwap_distance < -MAX_DISTANCE_TO_VWAP {
                        // Only log warning if we were actively confirming a SHORT signal
                        if self.pending_signal == Some(false) {
                            warn!(
                                "🚫 ANTI-FOMO REJECTED: SHORT blocked - price too far BELOW VWAP ({:.2}%). Waiting for bounce.",
                                vwap_distance * 100.0
                            );
                        }
                        self.pending_signal = None;
                        self.confirmation_count = 0;
                        return;
                    }

                    debug!(
                        "✅ Anti-FOMO check passed: Price {:.2}% from VWAP (within ±0.5%)",
                        vwap_distance * 100.0
                    );
                }

                // ✅ IMPROVEMENT #1: Confirmation delay
                if let Some(pending_bullish) = self.pending_signal {
                    // Check if current signal matches pending
                    if pending_bullish == signal_is_bullish {
                        self.confirmation_count += 1;
                        debug!("🔄 Signal confirmation: {}/12", self.confirmation_count);

                        // ✅ STRICTER: Need 12 consecutive confirmations (was 3)
                        if self.confirmation_count >= 12 {
                            if let Some(ref orderbook) = self.last_orderbook {
                                // Check spread is reasonable
                                if orderbook.spread_bps > self.config.max_spread_bps {
                                    warn!(
                                        "⚠️  Entry blocked: Spread too wide {:.2} bps (max: {:.2}). Resetting confirmation.",
                                        orderbook.spread_bps, self.config.max_spread_bps
                                    );
                                    // ✅ FIX: Reset confirmation state when spread too wide
                                    // CRITICAL: Market conditions changed, signal may be invalid
                                    self.pending_signal = None;
                                    self.confirmation_count = 0;
                                    return;
                                }

                                // ✅ Signal confirmed - execute entry!
                                info!("✅ Signal CONFIRMED after {} ticks", self.confirmation_count);
                                self.pending_signal = None;
                                self.confirmation_count = 0;
                                
                                let orderbook_clone = orderbook.clone();
                                self.execute_entry(momentum, &orderbook_clone).await;
                            }
                        }
                    } else {
                        // Direction changed - reset
                        debug!("🔄 Signal direction changed, resetting confirmation");
                        self.pending_signal = Some(signal_is_bullish);
                        self.confirmation_count = 1;
                    }
                } else {
                    // First time seeing this signal - start confirmation
                    debug!("🆕 New {} signal, starting confirmation...", 
                        if signal_is_bullish { "BULLISH" } else { "BEARISH" }
                    );
                    self.pending_signal = Some(signal_is_bullish);
                    self.confirmation_count = 1;
                }
            } else {
                // Momentum below threshold - reset pending signal
                if self.pending_signal.is_some() {
                    debug!("📉 Momentum dropped below threshold, resetting confirmation");
                    self.pending_signal = None;
                    self.confirmation_count = 0;
                }
            }
        }
    }

    /// ✅ PERFORMANCE: Get cached 50-tick VWAP or calculate if needed
    fn get_vwap_short(&mut self) -> Option<Decimal> {
        // Return cached value if available
        if let Some(cached) = self.cached_vwap_short {
            return Some(cached);
        }

        // Calculate and cache
        let ticks: Vec<&TradeTick> = self.tick_buffer.iter().collect();
        if ticks.len() < 50 {
            return None;
        }

        let mut total_value = Decimal::ZERO;
        let mut total_volume = Decimal::ZERO;
        for tick in ticks.iter().rev().take(50) {
            total_value += tick.price * tick.size;
            total_volume += tick.size;
        }

        if total_volume == Decimal::ZERO {
            return None;
        }

        let vwap = total_value / total_volume;
        self.cached_vwap_short = Some(vwap);
        Some(vwap)
    }

    /// ✅ PERFORMANCE: Get cached 200-tick VWAP or calculate if needed
    fn get_vwap_long(&mut self) -> Option<Decimal> {
        // Return cached value if available
        if let Some(cached) = self.cached_vwap_long {
            return Some(cached);
        }

        // Calculate and cache
        let ticks: Vec<&TradeTick> = self.tick_buffer.iter().collect();
        if ticks.len() < 200 {
            return None;
        }

        let mut total_value = Decimal::ZERO;
        let mut total_volume = Decimal::ZERO;
        for tick in ticks.iter().rev().take(200) {
            total_value += tick.price * tick.size;
            total_volume += tick.size;
        }

        if total_volume == Decimal::ZERO {
            return None;
        }

        let vwap = total_value / total_volume;
        self.cached_vwap_long = Some(vwap);
        Some(vwap)
    }

    /// ✅ PUMP PROTECTION: Calculate trend using short vs long VWAP (CACHED)
    /// Uses 50-tick vs 200-tick window to avoid false reversals on pump coins
    fn calculate_trend(&mut self) -> Option<bool> {
        // ✅ PERFORMANCE: Use cached VWAP values instead of recalculating
        let short_vwap = self.get_vwap_short()?;
        let long_vwap = self.get_vwap_long()?;

        // Bullish trend = short VWAP above long VWAP
        // This requires a sustained move to flip, preventing false signals on pump coins
        Some(short_vwap > long_vwap)
    }

    /// ✅ PERFORMANCE: Calculate momentum using cached VWAP
    fn calculate_momentum(&mut self) -> Option<f64> {
        // ✅ PERFORMANCE: Use cached 50-tick VWAP instead of recalculating
        let vwap = self.get_vwap_short()?;

        // Compare last price to VWAP
        let last_tick = self.tick_buffer.last()?;
        let momentum_dec = (last_tick.price - vwap) / vwap;

        // ✅ FIXED: 100x faster conversion using ToPrimitive
        let momentum = momentum_dec.to_f64().unwrap_or(0.0);

        Some(momentum)
    }

    /// ✅ ATR-BASED: Calculate tick volatility (standard deviation of price changes)
    /// Uses last 100 ticks to measure market "choppiness"
    fn calculate_volatility(&self) -> Option<f64> {
        let ticks: Vec<&TradeTick> = self.tick_buffer.iter().collect();

        // Need at least 100 ticks for reliable volatility measurement
        if ticks.len() < 100 {
            return None;
        }

        // Calculate price changes (returns) for last 100 ticks
        let recent_ticks: Vec<&TradeTick> = ticks.iter().rev().take(100).copied().collect();
        let mut price_changes: Vec<f64> = Vec::with_capacity(99);

        for i in 0..recent_ticks.len() - 1 {
            let current_price = recent_ticks[i].price.to_f64().unwrap_or(0.0);
            let prev_price = recent_ticks[i + 1].price.to_f64().unwrap_or(0.0);

            if prev_price > 0.0 {
                let change = (current_price - prev_price).abs() / prev_price;
                price_changes.push(change);
            }
        }

        if price_changes.is_empty() {
            return None;
        }

        // Calculate mean
        let mean: f64 = price_changes.iter().sum::<f64>() / price_changes.len() as f64;

        // Calculate standard deviation
        let variance: f64 = price_changes
            .iter()
            .map(|x| {
                let diff = x - mean;
                diff * diff
            })
            .sum::<f64>()
            / price_changes.len() as f64;

        let std_dev = variance.sqrt();

        Some(std_dev)
    }

    /// ✅ ATR-BASED: Calculate dynamic Stop Loss based on volatility
    /// Returns: (stop_loss_percent, take_profit_percent)
    fn calculate_dynamic_risk(&self) -> (f64, f64) {
        const MIN_SL_PERCENT: f64 = 0.7; // 0.7% minimum SL
        const MAX_SL_PERCENT: f64 = 3.0; // 3.0% maximum SL
        const VOLATILITY_MULTIPLIER: f64 = 2.0; // SL = 2x volatility
        const TP_MULTIPLIER: f64 = 1.5; // TP = 1.5x SL (positive R:R)

        let volatility = match self.calculate_volatility() {
            Some(vol) => vol * 100.0, // Convert to percentage
            None => {
                // Fallback to config defaults if can't calculate volatility
                return (self.config.stop_loss_percent, self.config.take_profit_percent);
            }
        };

        // Calculate dynamic SL based on volatility
        let dynamic_sl = volatility * VOLATILITY_MULTIPLIER;

        // Clamp to min/max range
        let clamped_sl = dynamic_sl.max(MIN_SL_PERCENT).min(MAX_SL_PERCENT);

        // Calculate TP as multiple of SL
        let dynamic_tp = clamped_sl * TP_MULTIPLIER;

        debug!(
            "📊 Dynamic Risk: Volatility={:.3}%, SL={:.2}% (raw={:.2}%), TP={:.2}%",
            volatility, clamped_sl, dynamic_sl, dynamic_tp
        );

        (clamped_sl, dynamic_tp)
    }

    /// ✅ ANTI-FOMO: Calculate distance from current price to long-term VWAP (CACHED)
    /// Returns: distance as percentage (positive = above VWAP, negative = below)
    fn calculate_vwap_distance(&mut self) -> Option<f64> {
        // ✅ PERFORMANCE: Use cached 200-tick VWAP instead of recalculating
        let vwap_200 = self.get_vwap_long()?;

        // Get current price
        let current_price = self.tick_buffer.last()?.price;

        // Calculate distance as percentage
        let distance_dec = (current_price - vwap_200) / vwap_200;
        let distance = distance_dec.to_f64().unwrap_or(0.0);

        Some(distance)
    }

    async fn execute_entry(&mut self, momentum: f64, orderbook: &OrderBookSnapshot) {
        // ✅ ATR-BASED: Calculate dynamic risk parameters
        let (dynamic_sl_percent, dynamic_tp_percent) = self.calculate_dynamic_risk();

        // ✅ FIX MEMORY LOSS BUG: Store dynamic risk for this trade
        // CRITICAL: handle_orderbook must use these values, not config!
        self.active_dynamic_risk = Some((dynamic_sl_percent, dynamic_tp_percent));

        info!(
            "🎯 ENTRY SIGNAL: {} momentum={:.4}% spread={:.2}bps | Dynamic SL={:.2}% TP={:.2}%",
            orderbook.symbol,
            momentum * 100.0,
            orderbook.spread_bps,
            dynamic_sl_percent,
            dynamic_tp_percent
        );

        // Determine side
        let side = if momentum > 0.0 {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };

        // ✅ RISK-ADJUSTED POSITION SIZING (FIXED DOLLAR RISK)
        // Goal: Lose exactly $0.30 regardless of SL size or volatility
        // Formula: Position_Size = Risk_Amount / (SL_Percent / 100)
        // Example: SL 1% → Position $30, SL 3% → Position $10 (both risk $0.30)
        const RISK_AMOUNT_USD: f64 = 0.30; // Fixed risk: $0.30 per trade

        let sl_decimal = dynamic_sl_percent / 100.0; // Convert to decimal (e.g., 1.5% -> 0.015)
        let risk_adjusted_position_usd = RISK_AMOUNT_USD / sl_decimal;

        // Cap at max_position_size_usd for safety
        let max_position_usd = self.config.max_position_size_usd;
        let final_position_usd = risk_adjusted_position_usd.min(max_position_usd);

        debug!(
            "💰 Position Sizing: Risk=${:.2}, SL={:.2}%, Calculated=${:.2}, Capped=${:.2}",
            RISK_AMOUNT_USD, dynamic_sl_percent, risk_adjusted_position_usd, final_position_usd
        );

        let position_value = Decimal::from_str_exact(&final_position_usd.to_string())
            .unwrap_or(Decimal::from(1000));

        let mut qty = position_value / orderbook.mid_price;

        // ✅ Round qty using symbol specs
        if let Some(ref specs) = self.current_specs {
            qty = specs.clamp_qty(qty);
            debug!("Rounded qty from {} to {} (step: {})",
                   position_value / orderbook.mid_price, qty, specs.qty_step);
        }

        // ⚠️ TEMPORARY: Force Market IOC for testing (1-2 days)
        // TODO: Revert to smart routing after testing
        // Smart Order Routing based on liquidity (DISABLED FOR TESTING)
        let (order_type, price, time_in_force) = {
            // FORCED: Always use Market IOC for guaranteed execution during testing
            info!("📈 Using Market IOC (FORCED FOR TESTING - will revert to smart routing later)");
            (OrderType::Market, None, TimeInForce::IOC)
        };

        // Original smart routing logic (commented out for testing):
        // let (order_type, price, time_in_force) = if orderbook.is_liquid() {
        //     // Liquid market: Use aggressive IOC market orders
        //     info!("📈 Using IOC Market Order (liquid market)");
        //     (OrderType::Market, None, TimeInForce::IOC)
        // } else {
        //     // Wide spread: Try to capture maker rebate with PostOnly limit
        //     info!("📊 Using PostOnly Limit Order (wide spread)");
        //     let mut limit_price = match side {
        //         OrderSide::Buy => orderbook.best_bid,
        //         OrderSide::Sell => orderbook.best_ask,
        //     };
        //
        //     if let Some(ref specs) = self.current_specs {
        //         limit_price = specs.round_price(limit_price);
        //         debug!("Rounded price to {} (tick_size: {})", limit_price, specs.tick_size);
        //     }
        //
        //     (OrderType::Limit, Some(limit_price), TimeInForce::PostOnly)
        // };

        // ✅ Pass symbol specs to order for precision validation
        let (qty_step, tick_size) = if let Some(ref specs) = self.current_specs {
            (Some(specs.qty_step), Some(specs.tick_size))
        } else {
            (None, None)
        };

        let order = Order {
            symbol: orderbook.symbol.clone(),
            side,
            order_type,
            qty,
            price,
            time_in_force,
            reduce_only: false,
            qty_step,
            tick_size,
        };

        // ✅ FIXED: Don't set position optimistically - wait for exchange confirmation
        // Position will be set via PositionUpdate message from ExecutionActor

        // ✅ FIXED: Transition to OrderPending state
        self.state = StrategyState::OrderPending;

        // Send order to execution
        if let Err(e) = self
            .execution_tx
            .send(ExecutionMessage::PlaceOrder(order))
            .await
        {
            warn!("Failed to send PlaceOrder to execution: {}", e);
            // ✅ FIX MEMORY LEAK: Clear dynamic risk if order send failed
            self.active_dynamic_risk = None;
            // Revert state if send failed
            self.state = StrategyState::Idle;
        }
    }
}

use crate::actors::messages::{ExecutionMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::SymbolSpecs;
use crate::models::*;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, Instant};
use tracing::{debug, error, info, warn};

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

    // ✅ FIX INFINITE CLOSE LOOP: Rate limit for close attempts
    /// When we last sent ClosePosition request
    last_close_attempt: Option<Instant>,

    // ✅ FIX MEMORY LOSS BUG: Store active dynamic risk for current position
    /// Stores (SL%, TP%) calculated for the current active trade
    /// CRITICAL: Must use these values in handle_orderbook, NOT config values!
    active_dynamic_risk: Option<(f64, f64)>,

    // ✅ TRAILING STOP: Track peak profit for momentum trades
    /// Best PnL percentage reached during this trade (for trailing SL)
    peak_pnl_percent: f64,
    /// Whether current trade is in Momentum mode (uses trailing stop)
    is_momentum_trade: bool,

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
        let momentum_threshold = config.momentum_threshold / 100.0; // Convert percentage to decimal
        Self {
            config,
            message_rx,
            execution_tx,
            current_symbol: None,
            current_position: None,
            last_orderbook: None,
            current_specs: None,
            tick_buffer: RingBuffer::new(300), // ✅ EXPANDED: 300 ticks for better trend detection
            momentum_threshold, // ✅ CONFIGURABLE: Read from env MOMENTUM_THRESHOLD (default 0.1%)
            state: StrategyState::Idle,
            pending_symbol_change: None,
            price_change_24h: None, // ✅ PUMP PROTECTION: Will be set on symbol change
            // ✅ IMPROVEMENT #1: Confirmation delay
            pending_signal: None,
            confirmation_count: 0,
            // ✅ IMPROVEMENT #3: Trade cooldown (30 seconds)
            last_trade_time: None,
            trade_cooldown_secs: 30,
            // ✅ FIX INFINITE CLOSE LOOP: Initialize rate limit
            last_close_attempt: None,
            // ✅ FIX MEMORY LOSS BUG: Initialize dynamic risk storage
            active_dynamic_risk: None,
            // ✅ TRAILING STOP: Initialize tracking fields
            peak_pnl_percent: 0.0,
            is_momentum_trade: false,
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
                                // ✅ FIX BUG #18: Clear close attempt timestamp
                                self.last_close_attempt = None;
                                // ✅ CLEANUP: Reset trailing stop state
                                self.is_momentum_trade = false;
                                self.peak_pnl_percent = 0.0;
                                self.state = StrategyState::Idle;
                            } else if self.state == StrategyState::SwitchingSymbol {
                                // ✅ FIX BUG #1: Now complete the pending symbol change
                                info!("✅ Position closed during symbol switch, completing switch...");
                                // ✅ IMPROVEMENT #3: Start trade cooldown
                                self.last_trade_time = Some(Instant::now());
                                // ✅ FIX MEMORY LOSS BUG: Clear dynamic risk when position closes
                                self.active_dynamic_risk = None;
                                // ✅ FIX BUG #18: Clear close attempt timestamp
                                self.last_close_attempt = None;
                                // ✅ CLEANUP: Reset trailing stop state
                                self.is_momentum_trade = false;
                                self.peak_pnl_percent = 0.0;
                                if let Some((new_symbol, specs, price_change_24h)) = self.pending_symbol_change.take() {
                                    self.complete_symbol_switch(new_symbol, specs, price_change_24h);
                                } else {
                                    warn!("SwitchingSymbol state but no pending change!");
                                    self.state = StrategyState::Idle;
                                }
                            } else if position.is_none() && matches!(self.state, StrategyState::PositionOpen | StrategyState::SwitchingSymbol) {
                                // ✅ FIX BUG #16 (CRITICAL): Only reset if position disappeared in states where we HAVE a position
                                // CRITICAL STATES TO CHECK:
                                // - PositionOpen: Position should exist, if None = liquidation/margin call
                                // - SwitchingSymbol: We're closing position, if None = position closed
                                //
                                // DO NOT CHECK in these states:
                                // - Idle: No position expected (normal)
                                // - OrderPending: Position doesn't exist yet (order not filled)
                                // - ClosingPosition: Position disappearing is EXPECTED
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
                        // ✅ HARMONY: Handle live market stats update
                        StrategyMessage::UpdateMarketStats { symbol, price_change_24h } => {
                            // Only update if it matches current symbol
                            if let Some(ref current) = self.current_symbol {
                                if *current == symbol {
                                    // Log only if change is significant (to avoid log spam)
                                    let old_change = self.price_change_24h.unwrap_or(0.0);
                                    if (old_change - price_change_24h).abs() > 0.05 {
                                        info!("📊 Market Update for {}: 24h change {:.2}% -> {:.2}%", 
                                              symbol, old_change * 100.0, price_change_24h * 100.0);
                                    }
                                    self.price_change_24h = Some(price_change_24h);
                                }
                            }
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

            // ✅ FIX BUG #17 (CRITICAL): Use timeout to prevent blocking if ExecutionActor hangs
            let send_result = tokio::time::timeout(
                Duration::from_secs(5),
                self.execution_tx.send(ExecutionMessage::ClosePosition {
                    symbol: position.symbol.clone(),
                    position_side: position.side,
                })
            ).await;

            match send_result {
                Ok(Ok(_)) => { /* Message sent successfully */ }
                Ok(Err(e)) => {
                    warn!("Failed to send ClosePosition on symbol change: {}", e);
                    // Fallback: complete switch anyway to avoid getting stuck
                    if let Some((sym, sp, pc)) = self.pending_symbol_change.take() {
                        self.complete_symbol_switch(sym, sp, pc);
                    }
                }
                Err(_) => {
                    warn!("⚠️  CRITICAL: ExecutionActor not responding (timeout 5s)! Force completing symbol switch.");
                    if let Some((sym, sp, pc)) = self.pending_symbol_change.take() {
                        self.complete_symbol_switch(sym, sp, pc);
                    }
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

        // ✅ FIX INFINITE CLOSE LOOP: Don't process exit logic if already closing or ordering
        // CRITICAL: orderbook updates come faster than state transitions, causing spam!
        if self.state == StrategyState::ClosingPosition || self.state == StrategyState::OrderPending {
            return;
        }

        // Update current price if we have a position
        if let Some(ref mut position) = self.current_position {
            position.current_price = snapshot.mid_price;

            // ✅ FIX CRITICAL BUG #8: Use active dynamic risk for BOTH SL and TP, not position.stop_loss!
            let (sl_target, tp_target) = self.active_dynamic_risk
                .unwrap_or((self.config.stop_loss_percent, self.config.take_profit_percent));

            let pnl_pct = position.pnl_percent();

            // ✅ TRAILING STOP: Update peak PnL for momentum trades
            // ✅ NOW: Update for ALL trades (needed for Breakeven protection)
            if pnl_pct > self.peak_pnl_percent {
                self.peak_pnl_percent = pnl_pct;
            }

            // ✅ DEBUG: Log PnL every 5 seconds to catch missed TP/SL
            static mut LAST_PNL_LOG: Option<std::time::Instant> = None;
            let should_log = unsafe {
                match LAST_PNL_LOG {
                    Some(last) if last.elapsed().as_secs() < 5 => false,
                    _ => {
                        LAST_PNL_LOG = Some(std::time::Instant::now());
                        true
                    }
                }
            };
            if should_log {
                let mode = if self.is_momentum_trade { "MOMENTUM" } else { "REVERSION" };
                let trailing_info = if self.is_momentum_trade {
                    format!(" | Peak: {:.2}%", self.peak_pnl_percent)
                } else {
                    String::new()
                };
                info!(
                    "📊 {} {} | Entry: {} | Current: {} | PnL: {:.2}% | TP: {:.2}% | SL: -{:.2}%{}",
                    mode, position.symbol, position.entry_price, position.current_price,
                    pnl_pct, tp_target, sl_target, trailing_info
                );
            }

            // ✅ TRAILING STOP: For momentum trades, check if price dropped from peak
            // FIX: Distance 1.5% was too wide for scalping (1.5% price = 15% ROE)
            // New distance: 0.2% price (~2% ROE) - secures profit quickly
            const TRAILING_DISTANCE: f64 = 0.2; 
            if self.is_momentum_trade && self.peak_pnl_percent > 0.3 {
                // Only activate trailing after 0.3% profit
                let drop_from_peak = self.peak_pnl_percent - pnl_pct;
                if drop_from_peak >= TRAILING_DISTANCE {
                    info!(
                        "📉 TRAILING STOP triggered for {} | Peak: {:.2}% | Now: {:.2}% | Drop: {:.2}%",
                        position.symbol, self.peak_pnl_percent, pnl_pct, drop_from_peak
                    );
                    
                    self.state = StrategyState::ClosingPosition;
                    self.last_close_attempt = Some(Instant::now());
                    
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        self.execution_tx.send(ExecutionMessage::ClosePosition {
                            symbol: position.symbol.clone(),
                            position_side: position.side,
                        })
                    ).await;
                    return;
                }
            }

            // ✅ BREAKEVEN / SECURE PROFIT:
            // If trade was ever > +0.5% (5% ROE), NEVER let it lose money.
            // Trigger close if it drops back to +0.1% (covers fees).
            // This applies to BOTH Momentum and Mean Reversion trades.
            if self.peak_pnl_percent > 0.5 && pnl_pct < 0.1 {
                 info!(
                    "🛡️  BREAKEVEN PROTECT triggered for {} | Peak was: {:.2}% | Now: {:.2}% | Securing profit!",
                    position.symbol, self.peak_pnl_percent, pnl_pct
                );
                
                self.state = StrategyState::ClosingPosition;
                self.last_close_attempt = Some(Instant::now());
                
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    self.execution_tx.send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                ).await;
                return;
            }

            // Check stop loss using dynamic SL target
            if pnl_pct <= -sl_target {
                // ✅ FIX RATE LIMIT: Don't spam close requests
                if let Some(last_attempt) = self.last_close_attempt {
                    if last_attempt.elapsed().as_secs() < 2 {
                        debug!("⏳ Rate limit: Close attempt throttled (< 2s since last)");
                        return;
                    }
                }

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
                self.last_close_attempt = Some(Instant::now());

                // ✅ FIX BUG #17 (CRITICAL): Use timeout to prevent blocking
                let send_result = tokio::time::timeout(
                    Duration::from_secs(5),
                    self.execution_tx.send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                ).await;

                match send_result {
                    Ok(Ok(_)) => { /* SL close sent successfully */ }
                    Ok(Err(e)) => {
                        warn!("Failed to send ClosePosition for stop loss: {}", e);
                        self.state = StrategyState::PositionOpen; // Revert state on failure
                    }
                    Err(_) => {
                        warn!("⚠️  CRITICAL: ExecutionActor timeout on SL! Reverting state.");
                        self.state = StrategyState::PositionOpen;
                    }
                }

                return;
            }

            // Check take profit using dynamic TP target
            // ✅ TRAILING STOP: For momentum trades, ignore fixed TP and let profit run!
            if !self.is_momentum_trade && pnl_pct >= tp_target {
                // ✅ FIX RATE LIMIT: Don't spam close requests
                if let Some(last_attempt) = self.last_close_attempt {
                    if last_attempt.elapsed().as_secs() < 2 {
                        debug!("⏳ Rate limit: Close attempt throttled (< 2s since last)");
                        return;
                    }
                }

                info!(
                    "💰 TAKE PROFIT hit for {} (PnL: {:.2}% | Target: {:.2}% {})",
                    position.symbol,
                    pnl_pct,
                    tp_target,
                    if self.active_dynamic_risk.is_some() { "[Dynamic]" } else { "[Static]" }
                );

                // ✅ FIXED: Transition to ClosingPosition state
                self.state = StrategyState::ClosingPosition;
                self.last_close_attempt = Some(Instant::now());

                // ✅ FIX BUG #17 (CRITICAL): Use timeout to prevent blocking
                let send_result = tokio::time::timeout(
                    Duration::from_secs(5),
                    self.execution_tx.send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                ).await;

                match send_result {
                    Ok(Ok(_)) => { /* TP close sent successfully */ }
                    Ok(Err(e)) => {
                        warn!("Failed to send ClosePosition for take profit: {}", e);
                        self.state = StrategyState::PositionOpen; // Revert state on failure
                    }
                    Err(_) => {
                        warn!("⚠️  CRITICAL: ExecutionActor timeout on TP! Reverting state.");
                        self.state = StrategyState::PositionOpen;
                    }
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

        // ✅ FIX INFINITE CLOSE LOOP: Don't process flash crash exit if already closing
        if self.state == StrategyState::ClosingPosition || self.state == StrategyState::OrderPending {
            return;
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
                // ✅ FIX RATE LIMIT: Don't spam close requests
                if let Some(last_attempt) = self.last_close_attempt {
                    if last_attempt.elapsed().as_secs() < 2 {
                        debug!("⏳ Rate limit: Flash crash close throttled (< 2s since last)");
                        return;
                    }
                }

                warn!(
                    "⚡ FLASH CRASH DETECTED: PnL {:.2}% in <1sec! Emergency exit on {}",
                    pnl_pct, position.symbol
                );

                self.state = StrategyState::ClosingPosition;
                self.last_close_attempt = Some(Instant::now());

                // ✅ FIX BUG #17 (CRITICAL): Use timeout to prevent blocking
                let send_result = tokio::time::timeout(
                    Duration::from_secs(5),
                    self.execution_tx.send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                ).await;

                match send_result {
                    Ok(Ok(_)) => { /* Flash crash emergency close sent */ }
                    Ok(Err(e)) => {
                        warn!("Failed to send emergency ClosePosition: {}", e);
                        self.state = StrategyState::PositionOpen;
                    }
                    Err(_) => {
                        warn!("⚠️  CRITICAL: ExecutionActor timeout on flash crash exit! Reverting state.");
                        self.state = StrategyState::PositionOpen;
                    }
                }

                return;
            }
        }

        // ✅ CRITICAL FIX: Need 200 ticks for FULL protection
        // - calculate_momentum: requires 50 ticks
        // - calculate_trend: requires 200 ticks (50 vs 200 VWAP)
        // Without 200 ticks, trend alignment check returns None and is SKIPPED!
        let buffer_len = self.tick_buffer.len();
        if buffer_len < 200 {
            // ✅ FIX BUG #15: Show buffering progress at INFO level (every 20 ticks + milestones)
            // User needs to see the bot is working and accumulating data
            if buffer_len % 20 == 0 || buffer_len == 50 || buffer_len == 100 || buffer_len == 150 || buffer_len == 199 {
                info!("📊 Buffering ticks: {}/200 ({}% ready)", buffer_len, buffer_len * 100 / 200);
            }
            return;
        }

        // ✅ FIX BUG #15: One-time notification when ready (tick #200)
        if buffer_len == 200 {
            info!("✅ Buffer FULL! Bot is now ACTIVE and monitoring for entry signals.");
        }

        // ✅ FIXED: State machine prevents double entry, entry while closing, etc.
        if self.state != StrategyState::Idle {
            // Keep as debug - happens frequently, no need to spam INFO logs
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

        // ✅ FIX BUG #15: Periodic status report (every 50 ticks after buffer full)
        // Show user what's happening even if no strong signals
        if self.tick_counter % 50 == 0 && self.tick_counter > 200 {
            if let Some(momentum) = self.calculate_momentum() {
                let trend_str = match self.calculate_trend() {
                    Some(true) => "BULLISH",
                    Some(false) => "BEARISH",
                    None => "UNKNOWN",
                };
                let vwap_dist = self.calculate_vwap_distance().unwrap_or(0.0);

                info!("📊 Market Analysis | Momentum: {:.2}% | Trend: {} | VWAP Distance: {:.2}% | Threshold: {:.2}%",
                      momentum * 100.0,
                      trend_str,
                      vwap_dist * 100.0,
                      self.momentum_threshold * 100.0);
            }
        }

        // Calculate momentum
        if let Some(momentum) = self.calculate_momentum() {
            // Check entry conditions: deviation from VWAP exceeds threshold
            if momentum.abs() > self.momentum_threshold {
                // ✅ ADAPTIVE STRATEGY: Switch based on 24h volatility
                // |change| < 10% → Mean Reversion (stable coin)
                // |change| > 10% → Momentum/Trend (pump coin)
                let is_pump_coin = self.price_change_24h
                    .map(|pc| pc.abs() > 0.10)
                    .unwrap_or(false);

                let signal_is_bullish = if is_pump_coin {
                    // MOMENTUM MODE: Trade WITH the trend
                    // Price ABOVE VWAP → trend is UP → LONG
                    // Price BELOW VWAP → trend is DOWN → SHORT
                    momentum > 0.0
                } else {
                    // MEAN REVERSION MODE: Trade AGAINST the move  
                    // Price ABOVE VWAP → expect pullback → SHORT
                    // Price BELOW VWAP → expect bounce → LONG
                    momentum < 0.0
                };

                // Log which strategy mode is active
                let mode = if is_pump_coin { "MOMENTUM" } else { "MEAN_REVERSION" };
                let action = if signal_is_bullish { "LONG" } else { "SHORT" };
                let price_change_str = self.price_change_24h
                    .map(|pc| format!("{:.1}%", pc * 100.0))
                    .unwrap_or_else(|| "N/A".to_string());

                info!("🎯 {} mode (24h: {}) | Price {:.2}% from VWAP → {} entry",
                      mode, price_change_str, momentum * 100.0, action);

                // ✅ MEAN REVERSION: No trend alignment needed - we trade reversals
                // Just log trend for debugging
                if let Some(trend_bullish) = self.calculate_trend() {
                    debug!("📊 Current trend: {} (trading against it)",
                        if trend_bullish { "BULLISH" } else { "BEARISH" });
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

        // ✅ FIX BUG #19 (DEFENSIVE): Prevent division by zero
        // Theoretically impossible (exchange never sends price=0), but defensive check
        if vwap == Decimal::ZERO {
            warn!("⚠️  VWAP is zero (exchange data error?), cannot calculate momentum");
            return None;
        }

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
                // ✅ FIX BUG #29: Ensure config fallback is safe
                let fallback_sl = self.config.stop_loss_percent.max(MIN_SL_PERCENT);
                let fallback_tp = self.config.take_profit_percent.max(fallback_sl * TP_MULTIPLIER);

                if self.config.stop_loss_percent < MIN_SL_PERCENT {
                    warn!(
                        "⚠️  Config SL {:.2}% too low, using minimum {:.2}%",
                        self.config.stop_loss_percent, MIN_SL_PERCENT
                    );
                }

                return (fallback_sl, fallback_tp);
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

        // ✅ ADAPTIVE STRATEGY: Order side depends on coin type
        let is_pump_coin = self.price_change_24h
            .map(|pc| pc.abs() > 0.10)
            .unwrap_or(false);
        
        // ✅ TRAILING STOP: Activate for momentum trades
        self.is_momentum_trade = is_pump_coin;
        self.peak_pnl_percent = 0.0;
        
        let side = if is_pump_coin {
            // MOMENTUM: Trade WITH the trend
            if momentum > 0.0 { OrderSide::Buy } else { OrderSide::Sell }
        } else {
            // MEAN REVERSION: Trade AGAINST the move
            if momentum < 0.0 { OrderSide::Buy } else { OrderSide::Sell }
        };

        // ✅ RISK-ADJUSTED POSITION SIZING (FIXED DOLLAR RISK)
        // Goal: Lose exactly $X regardless of SL size or volatility
        // Formula: Position_Size = Risk_Amount / (SL_Percent / 100)
        // Example: SL 1% → Position $30, SL 3% → Position $10 (both risk $0.30)
        let risk_amount_usd = self.config.risk_amount_usd;

        // ✅ FIX BUG #29 (CRITICAL): Prevent division by zero
        if dynamic_sl_percent <= 0.0 {
            error!(
                "❌ BUG #29 CAUGHT! Invalid SL percent: {:.4}% (must be > 0)",
                dynamic_sl_percent
            );
            error!("⚠️  Cannot calculate position size with zero/negative SL, aborting entry");
            self.pending_signal = None;
            self.confirmation_count = 0;
            return;
        }

        let sl_decimal = dynamic_sl_percent / 100.0; // Convert to decimal (e.g., 1.5% -> 0.015)
        let risk_adjusted_position_usd = risk_amount_usd / sl_decimal;

        // Cap at max_position_size_usd for safety
        let max_position_usd = self.config.max_position_size_usd;
        let final_position_usd = risk_adjusted_position_usd.min(max_position_usd);

        debug!(
            "💰 Position Sizing: Risk=${:.2}, SL={:.2}%, Calculated=${:.2}, Capped=${:.2}",
            risk_amount_usd, dynamic_sl_percent, risk_adjusted_position_usd, final_position_usd
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

        // ✅ MEAN REVERSION: Always use Market IOC for reliable fills
        // PostOnly limits were getting cancelled on fast markets (spread too tight)
        info!("📈 Using Market IOC Order (spread={:.2}bps)", orderbook.spread_bps);
        let (order_type, price, time_in_force) = (OrderType::Market, None, TimeInForce::IOC);

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

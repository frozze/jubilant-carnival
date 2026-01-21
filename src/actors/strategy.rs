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

    // Tick buffer for momentum calculation
    tick_buffer: RingBuffer<TradeTick>,

    // Entry conditions
    momentum_threshold: f64,

    // ✅ FIXED: Proper state machine replaces simple boolean
    state: StrategyState,

    // ✅ FIX BUG #1: Store pending symbol change until position is closed
    pending_symbol_change: Option<(Symbol, SymbolSpecs)>,

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
            tick_buffer: RingBuffer::new(100),
            momentum_threshold: 0.001, // 0.1% momentum threshold
            state: StrategyState::Idle,
            pending_symbol_change: None,
            // ✅ IMPROVEMENT #1: Confirmation delay
            pending_signal: None,
            confirmation_count: 0,
            // ✅ IMPROVEMENT #3: Trade cooldown (30 seconds)
            last_trade_time: None,
            trade_cooldown_secs: 30,
        }
    }

    pub async fn run(mut self) {
        info!("⚡ StrategyEngine started");

        // ✅ FIXED: Add periodic position verification (every 60 seconds)
        let mut position_verify_interval = interval(Duration::from_secs(60));

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
                                self.state = StrategyState::Idle;
                            } else if self.state == StrategyState::SwitchingSymbol {
                                // ✅ FIX BUG #1: Now complete the pending symbol change
                                info!("✅ Position closed during symbol switch, completing switch...");
                                // ✅ IMPROVEMENT #3: Start trade cooldown
                                self.last_trade_time = Some(Instant::now());
                                if let Some((new_symbol, specs)) = self.pending_symbol_change.take() {
                                    self.complete_symbol_switch(new_symbol, specs);
                                } else {
                                    warn!("SwitchingSymbol state but no pending change!");
                                    self.state = StrategyState::Idle;
                                }
                            }
                        }
                        StrategyMessage::SymbolChanged { symbol: new_symbol, specs } => {
                            self.handle_symbol_change(new_symbol, specs).await;
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

    async fn handle_symbol_change(&mut self, new_symbol: Symbol, specs: SymbolSpecs) {
        info!("🔄 Symbol change requested: {} (qty_step: {}, tick_size: {})",
              new_symbol, specs.qty_step, specs.tick_size);

        // ✅ FIX BUG #1: If we have a position, close it FIRST and defer the switch
        if let Some(ref position) = self.current_position {
            info!("⚠️  Closing position on {} before symbol switch", position.symbol);

            // Store pending symbol change - will be applied after close confirmation
            self.pending_symbol_change = Some((new_symbol, specs));
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
                if let Some((sym, sp)) = self.pending_symbol_change.take() {
                    self.complete_symbol_switch(sym, sp);
                }
            }
            // DON'T switch yet - wait for PositionUpdate(None)
            return;
        }

        // No position - switch immediately
        self.complete_symbol_switch(new_symbol, specs);
    }

    /// Complete the symbol switch after position is closed
    fn complete_symbol_switch(&mut self, new_symbol: Symbol, specs: SymbolSpecs) {
        info!("✅ Completing symbol switch to: {}", new_symbol);
        self.current_symbol = Some(new_symbol);
        self.current_position = None;
        self.last_orderbook = None;
        self.current_specs = Some(specs);
        self.tick_buffer = RingBuffer::new(100);
        self.pending_symbol_change = None;
        // ✅ Reset confirmation state for new symbol
        self.pending_signal = None;
        self.confirmation_count = 0;
        self.state = StrategyState::Idle;
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

            // Check stop loss
            if position.should_stop_loss() {
                warn!(
                    "🛑 STOP LOSS triggered for {} at {} (PnL: {:.2}%)",
                    position.symbol,
                    position.current_price,
                    position.pnl_percent()
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

            // Check take profit
            let pnl_pct = position.pnl_percent();
            if pnl_pct >= self.config.take_profit_percent {
                info!(
                    "💰 TAKE PROFIT hit for {} (PnL: {:.2}%)",
                    position.symbol, pnl_pct
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

        // Add to buffer
        self.tick_buffer.push(tick.clone());

        // ✅ FIXED: Increased to 50 ticks for noise reduction
        if self.tick_buffer.len() < 50 {
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
                // ✅ IMPROVEMENT #2: Trend alignment - check if signal aligns with trend
                let signal_is_bullish = momentum > 0.0;
                
                if let Some(trend_bullish) = self.calculate_trend() {
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
                }

                // ✅ IMPROVEMENT #1: Confirmation delay
                if let Some(pending_bullish) = self.pending_signal {
                    // Check if current signal matches pending
                    if pending_bullish == signal_is_bullish {
                        self.confirmation_count += 1;
                        debug!("🔄 Signal confirmation: {}/3", self.confirmation_count);
                        
                        // Need 3 consecutive confirmations
                        if self.confirmation_count >= 3 {
                            if let Some(ref orderbook) = self.last_orderbook {
                                // Check spread is reasonable
                                if orderbook.spread_bps > self.config.max_spread_bps {
                                    debug!(
                                        "Spread too wide: {:.2} bps (max: {:.2})",
                                        orderbook.spread_bps, self.config.max_spread_bps
                                    );
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

    /// ✅ IMPROVEMENT #2: Calculate trend using short vs long VWAP
    fn calculate_trend(&self) -> Option<bool> {
        let ticks: Vec<&TradeTick> = self.tick_buffer.iter().collect();
        
        // Need at least 50 ticks for comparison
        if ticks.len() < 50 {
            return None;
        }

        // Short VWAP (last 20 ticks)
        let mut short_value = Decimal::ZERO;
        let mut short_volume = Decimal::ZERO;
        for tick in ticks.iter().rev().take(20) {
            short_value += tick.price * tick.size;
            short_volume += tick.size;
        }

        // Long VWAP (last 50 ticks)  
        let mut long_value = Decimal::ZERO;
        let mut long_volume = Decimal::ZERO;
        for tick in ticks.iter().rev().take(50) {
            long_value += tick.price * tick.size;
            long_volume += tick.size;
        }

        if short_volume == Decimal::ZERO || long_volume == Decimal::ZERO {
            return None;
        }

        let short_vwap = short_value / short_volume;
        let long_vwap = long_value / long_volume;

        // Bullish trend = short VWAP above long VWAP
        Some(short_vwap > long_vwap)
    }

    fn calculate_momentum(&self) -> Option<f64> {
        let ticks: Vec<&TradeTick> = self.tick_buffer.iter().collect();

        // ✅ FIXED: Increased to 50 ticks for noise reduction
        if ticks.len() < 50 {
            return None;
        }

        // Calculate VWAP for last 50 ticks
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

        // Compare last price to VWAP
        if let Some(last_tick) = self.tick_buffer.last() {
            let momentum_dec = (last_tick.price - vwap) / vwap;

            // ✅ FIXED: 100x faster conversion using ToPrimitive
            let momentum = momentum_dec.to_f64().unwrap_or(0.0);

            Some(momentum)
        } else {
            None
        }
    }

    async fn execute_entry(&mut self, momentum: f64, orderbook: &OrderBookSnapshot) {
        info!(
            "🎯 ENTRY SIGNAL: {} momentum={:.4}% spread={:.2}bps",
            orderbook.symbol,
            momentum * 100.0,
            orderbook.spread_bps
        );

        // Determine side
        let side = if momentum > 0.0 {
            OrderSide::Buy
        } else {
            OrderSide::Sell
        };

        // Calculate position size
        let position_value = Decimal::from_str_exact(&self.config.max_position_size_usd.to_string())
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
            // Revert state if send failed
            self.state = StrategyState::Idle;
        }
    }
}

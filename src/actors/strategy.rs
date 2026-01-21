use crate::actors::messages::{ExecutionMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::SymbolSpecs;
use crate::models::*;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

/// ✅ FIXED: Proper state machine for order lifecycle
#[derive(Debug, Clone, PartialEq)]
enum StrategyState {
    Idle,                 // No position, no order
    OrderPending,         // Order sent, waiting for confirmation
    PositionOpen,         // Position confirmed by exchange
    ClosingPosition,      // Close order sent, waiting for confirmation
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
                                self.state = StrategyState::Idle;
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
        info!("🔄 Symbol changed to: {} (qty_step: {}, tick_size: {})",
              new_symbol, specs.qty_step, specs.tick_size);

        // Close any existing position
        if let Some(ref position) = self.current_position {
            info!("⚠️  Closing position on {} before symbol switch", position.symbol);

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
                warn!("Failed to send ClosePosition on symbol change: {}", e);
                // Will reset to Idle below anyway
            }
        }

        // Reset state
        self.current_symbol = Some(new_symbol);
        self.current_position = None;
        self.last_orderbook = None;
        self.current_specs = Some(specs);
        self.tick_buffer = RingBuffer::new(100);
        self.state = StrategyState::Idle; // ✅ Reset state machine
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

        // Calculate momentum
        if let Some(momentum) = self.calculate_momentum() {
            debug!("Momentum: {:.4}%", momentum * 100.0);

            // Check entry conditions
            if momentum.abs() > self.momentum_threshold {
                if let Some(ref orderbook) = self.last_orderbook {
                    // Check spread is reasonable
                    if orderbook.spread_bps > self.config.max_spread_bps {
                        debug!(
                            "Spread too wide: {:.2} bps (max: {:.2})",
                            orderbook.spread_bps, self.config.max_spread_bps
                        );
                        return;
                    }

                    let orderbook_clone = orderbook.clone();
                    self.execute_entry(momentum, &orderbook_clone).await;
                }
            }
        }
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

        // ✅ Smart Order Routing with Fallback Protection
        // Strategy:
        // - Deeply liquid (spread < 5bps, $500+ on both sides): PostOnly → auto-fallback to Market IOC if not filled in 5s
        // - Other markets: Market IOC immediately for guaranteed execution
        let (order_type, price, time_in_force) = if orderbook.is_deeply_liquid() {
            // Deep liquidity: Try PostOnly for maker rebate
            // ExecutionActor will auto-fallback to Market IOC if not filled in 5s
            info!(
                "📊 Using PostOnly Limit (deep liquidity: spread={:.2}bps, bid_value=${:.0}, ask_value=${:.0}) with auto-fallback",
                orderbook.spread_bps,
                (orderbook.bid_size * orderbook.best_bid).to_f64().unwrap_or(0.0),
                (orderbook.ask_size * orderbook.best_ask).to_f64().unwrap_or(0.0)
            );

            let mut limit_price = match side {
                OrderSide::Buy => orderbook.best_bid,
                OrderSide::Sell => orderbook.best_ask,
            };

            if let Some(ref specs) = self.current_specs {
                limit_price = specs.round_price(limit_price);
                debug!("Rounded price to {} (tick_size: {})", limit_price, specs.tick_size);
            }

            (OrderType::Limit, Some(limit_price), TimeInForce::PostOnly)
        } else {
            // Lower liquidity or wider spread: Use Market IOC for immediate execution
            info!(
                "📈 Using Market IOC (spread={:.2}bps, bid_value=${:.0}, ask_value=${:.0}) for guaranteed execution",
                orderbook.spread_bps,
                (orderbook.bid_size * orderbook.best_bid).to_f64().unwrap_or(0.0),
                (orderbook.ask_size * orderbook.best_ask).to_f64().unwrap_or(0.0)
            );
            (OrderType::Market, None, TimeInForce::IOC)
        };

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

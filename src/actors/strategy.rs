use crate::actors::messages::{ExecutionMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::SymbolSpecs;
use crate::models::*;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use std::time::Instant;
use tracing::{debug, info, warn};

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

    // ✅ CRITICAL: Prevent order spam
    order_in_progress: bool,
    last_order_time: Option<Instant>,
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
            order_in_progress: false,
            last_order_time: None,
        }
    }

    pub async fn run(mut self) {
        info!("⚡ StrategyEngine started");

        let mut tick_interval = interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                Some(msg) = self.message_rx.recv() => {
                    match msg {
                        StrategyMessage::OrderBook(snapshot) => {
                            self.handle_orderbook(snapshot).await;
                        }
                        StrategyMessage::Trade(tick) => {
                            self.handle_trade(tick).await;
                        }
                        StrategyMessage::PositionUpdate(position) => {
                            self.current_position = position;
                        }
                        StrategyMessage::SymbolChanged { symbol: new_symbol, specs } => {
                            self.handle_symbol_change(new_symbol, specs).await;
                        }
                        // ✅ CRITICAL: Feedback from execution
                        StrategyMessage::OrderFilled(symbol) => {
                            info!("✅ Order filled for {}, unfreezing strategy", symbol);
                            self.order_in_progress = false;
                            self.last_order_time = None;
                        }
                        StrategyMessage::OrderFailed(error) => {
                            warn!("❌ Order failed: {}, unfreezing strategy", error);
                            self.order_in_progress = false;
                            self.last_order_time = None;
                            // Also clear position expectation
                            self.current_position = None;
                        }
                    }
                }
                _ = tick_interval.tick() => {
                    // ✅ FIXED: Check for order timeout (Freeze Protection)
                    if self.order_in_progress {
                        if let Some(last_time) = self.last_order_time {
                            if last_time.elapsed() > Duration::from_secs(10) {
                                warn!("⚠️  Order execution TIMEOUT after 10s - forcing unfreeze");
                                self.order_in_progress = false;
                                self.last_order_time = None;
                            }
                        }
                    }
                }
                else => break,
            }
        }
    }

    async fn handle_symbol_change(&mut self, new_symbol: Symbol, specs: SymbolSpecs) {
        info!("🔄 Symbol changed to: {} (qty_step={}, tick_size={})", 
              new_symbol, specs.qty_step, specs.tick_size);

        // Close any existing position
        if let Some(ref position) = self.current_position {
            info!("⚠️  Closing position on {} before symbol switch", position.symbol);

            let _ = self
                .execution_tx
                .send(ExecutionMessage::ClosePosition {
                    symbol: position.symbol.clone(),
                    position_side: position.side,
                })
                .await;
        }

        // Reset state
        self.current_symbol = Some(new_symbol);
        self.current_specs = Some(specs);
        self.current_position = None;
        self.last_orderbook = None;
        self.tick_buffer = RingBuffer::new(100);
        self.order_in_progress = false; // ✅ Reset order lock
        self.last_order_time = None;
    }

    async fn handle_orderbook(&mut self, snapshot: OrderBookSnapshot) {
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

                let _ = self
                    .execution_tx
                    .send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                    .await;

                return;
            }

            // Check take profit
            let pnl_pct = position.pnl_percent();
            if pnl_pct >= self.config.take_profit_percent {
                info!(
                    "💰 TAKE PROFIT hit for {} (PnL: {:.2}%)",
                    position.symbol, pnl_pct
                );

                let _ = self
                    .execution_tx
                    .send(ExecutionMessage::ClosePosition {
                        symbol: position.symbol.clone(),
                        position_side: position.side,
                    })
                    .await;

                return;
            }
        }

        self.last_orderbook = Some(snapshot);
    }

    async fn handle_trade(&mut self, tick: TradeTick) {
        // Add to buffer
        self.tick_buffer.push(tick.clone());

        // ✅ FIXED: Increased to 50 ticks for noise reduction
        if self.tick_buffer.len() < 50 {
            return;
        }

        // ✅ CRITICAL: Skip if order already in progress
        if self.order_in_progress {
            debug!("⏸️  Order in progress, skipping new entry signals");
            return;
        }

        // Skip if we already have a position
        if self.current_position.is_some() {
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
        let (side, position_side) = if momentum > 0.0 {
            (OrderSide::Buy, PositionSide::Long)
        } else {
            (OrderSide::Sell, PositionSide::Short)
        };

        // Calculate position size
        let position_value = Decimal::from_str_exact(&self.config.max_position_size_usd.to_string())
            .unwrap_or(Decimal::from(1000));

        let qty = position_value / orderbook.mid_price;

        // Smart Order Routing based on liquidity
        let (order_type, price, time_in_force) = if orderbook.is_liquid() {
            // Liquid market: Use aggressive IOC market orders
            info!("📈 Using IOC Market Order (liquid market)");
            (OrderType::Market, None, TimeInForce::IOC)
        } else {
            // Wide spread: Try to capture maker rebate with PostOnly limit
            info!("📊 Using PostOnly Limit Order (wide spread)");
            let limit_price = match side {
                OrderSide::Buy => orderbook.best_bid, // Join the bid
                OrderSide::Sell => orderbook.best_ask, // Join the ask
            };
            (OrderType::Limit, Some(limit_price), TimeInForce::PostOnly)
        };

        let order = Order {
            symbol: orderbook.symbol.clone(),
            side,
            order_type,
            qty,
            price,
            time_in_force,
            reduce_only: false,
            qty_step: self.current_specs.as_ref().map(|s| s.qty_step),
            tick_size: self.current_specs.as_ref().map(|s| s.tick_size),
        };

        // ✅ FIXED: Don't set position optimistically - wait for exchange confirmation
        // Position will be set via PositionUpdate message from ExecutionActor

        // ✅ CRITICAL: Lock strategy to prevent order spam
        self.order_in_progress = true;
        self.last_order_time = Some(Instant::now());

        // Send order to execution
        let _ = self
            .execution_tx
            .send(ExecutionMessage::PlaceOrder(order))
            .await;
    }
}

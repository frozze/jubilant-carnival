use crate::actors::messages::{ExecutionMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::BybitClient;
use crate::models::*;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// ExecutionActor - Order placement and position tracking
pub struct ExecutionActor {
    client: BybitClient,
    #[allow(dead_code)]
    config: Arc<Config>,
    message_rx: mpsc::Receiver<ExecutionMessage>,
    strategy_tx: mpsc::Sender<StrategyMessage>,
}

impl ExecutionActor {
    pub fn new(
        client: BybitClient,
        config: Arc<Config>,
        message_rx: mpsc::Receiver<ExecutionMessage>,
        strategy_tx: mpsc::Sender<StrategyMessage>,
    ) -> Self {
        Self {
            client,
            config,
            message_rx,
            strategy_tx,
        }
    }

    pub async fn run(mut self) {
        info!("💼 ExecutionActor started");

        while let Some(msg) = self.message_rx.recv().await {
            match msg {
                ExecutionMessage::PlaceOrder(order) => {
                    self.handle_place_order(order).await;
                }
                ExecutionMessage::ClosePosition { symbol, position_side } => {
                    self.handle_close_position(symbol, position_side).await;
                }
                ExecutionMessage::GetPosition(symbol) => {
                    self.handle_get_position(symbol).await;
                }
                ExecutionMessage::Shutdown => {
                    info!("ExecutionActor shutting down");
                    break;
                }
            }
        }
    }

    async fn handle_place_order(&self, order: Order) {
        let symbol = order.symbol.clone();
        let symbol_str = symbol.0.clone();

        info!(
            "📤 Placing order: {:?} {} {} @ {:?}",
            order.side, order.qty, symbol, order.price
        );

        // Step 1: Place order
        let order_id = match self.client.place_order(&order).await {
            Ok(response) => {
                info!("✅ Order accepted by exchange: {}", response.order_id);
                response.order_id
            }
            Err(e) => {
                let error_msg = format!("Failed to place order: {}", e);
                error!("❌ {}", error_msg);

                // Notify strategy that order failed
                if let Err(e) = self
                    .strategy_tx
                    .send(StrategyMessage::OrderFailed(error_msg))
                    .await
                {
                    error!("Failed to send OrderFailed message: {}", e);
                }
                return;
            }
        };

        // ✅ FIXED: Step 2 - Poll for order confirmation (up to 10 seconds)
        let max_polls = 20; // 20 polls × 500ms = 10 seconds
        let poll_interval = tokio::time::Duration::from_millis(500);

        for attempt in 1..=max_polls {
            tokio::time::sleep(poll_interval).await;

            match self.client.get_order_status(&symbol_str, &order_id).await {
                Ok(order_status) => {
                    info!(
                        "📊 Order {} status: {} (attempt {}/{})",
                        order_id, order_status.order_status, attempt, max_polls
                    );

                    match order_status.order_status.as_str() {
                        "Filled" => {
                            info!("✅ Order {} FILLED", order_id);

                            // Notify strategy
                            if let Err(e) = self
                                .strategy_tx
                                .send(StrategyMessage::OrderFilled(symbol.clone()))
                                .await
                            {
                                error!("Failed to send OrderFilled message: {}", e);
                            }

                            // Query position and send update
                            self.handle_get_position(symbol).await;
                            return;
                        }
                        "Cancelled" | "Rejected" => {
                            let error_msg = format!("Order {} {}", order_id, order_status.order_status);
                            error!("❌ {}", error_msg);

                            if let Err(e) = self
                                .strategy_tx
                                .send(StrategyMessage::OrderFailed(error_msg))
                                .await
                            {
                                error!("Failed to send OrderFailed message: {}", e);
                            }
                            return;
                        }
                        "PartiallyFilled" | "New" => {
                            // Continue polling
                            continue;
                        }
                        _ => {
                            warn!("Unknown order status: {}", order_status.order_status);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to query order status (attempt {}/{}): {}",
                        attempt, max_polls, e
                    );
                    continue;
                }
            }
        }

        // Timeout - order not filled within 10 seconds
        // ✅ FIX BUG #5: Cancel the orphan order before reporting failure
        warn!("⏰ Order {} timeout, attempting to cancel...", order_id);
        if let Err(e) = self.client.cancel_order(&symbol_str, &order_id).await {
            error!("Failed to cancel timed-out order: {}", e);
        }

        let error_msg = format!("Order {} cancelled after timeout", order_id);
        error!("❌ {}", error_msg);

        if let Err(e) = self
            .strategy_tx
            .send(StrategyMessage::OrderFailed(error_msg))
            .await
        {
            error!("Failed to send OrderFailed message: {}", e);
        }
    }

    async fn handle_close_position(&self, symbol: Symbol, position_side: PositionSide) {
        info!("🔒 Closing position for {} {:?}", symbol, position_side);

        // First, get current position to determine size
        match self.client.get_position(&symbol.0).await {
            Ok(positions) => {
                if positions.is_empty() {
                    warn!("No position found for {}", symbol);
                    // ✅ Still send PositionUpdate(None) so Strategy transitions correctly
                    if let Err(e) = self
                        .strategy_tx
                        .send(StrategyMessage::PositionUpdate(None))
                        .await
                    {
                        error!("Failed to send PositionUpdate(None): {}", e);
                    }
                    return;
                }

                for pos_info in positions {
                    let size = Decimal::from_str(&pos_info.size).unwrap_or(Decimal::ZERO);

                    if size == Decimal::ZERO {
                        continue;
                    }

                    // Determine closing side (opposite of position)
                    let close_side = if pos_info.side == "Buy" {
                        OrderSide::Sell
                    } else {
                        OrderSide::Buy
                    };

                    // Create closing market order
                    let close_order = Order {
                        symbol: symbol.clone(),
                        side: close_side,
                        order_type: OrderType::Market,
                        qty: size,
                        price: None,
                        time_in_force: TimeInForce::IOC,
                        reduce_only: true,
                        qty_step: None,
                        tick_size: None,
                    };

                    info!(
                        "📤 Closing order: {:?} {} (reduce_only)",
                        close_side, size
                    );

                    match self.client.place_order(&close_order).await {
                        Ok(response) => {
                            info!("✅ Close order placed: {}", response.order_id);

                            // ✅ FIX BUG #3: Poll for close order confirmation
                            let max_polls = 10; // 5 seconds for close orders
                            let poll_interval = tokio::time::Duration::from_millis(500);

                            for attempt in 1..=max_polls {
                                tokio::time::sleep(poll_interval).await;

                                match self.client.get_order_status(&symbol.0, &response.order_id).await {
                                    Ok(status) => {
                                        match status.order_status.as_str() {
                                            "Filled" => {
                                                info!("✅ Close order FILLED");
                                                if let Err(e) = self
                                                    .strategy_tx
                                                    .send(StrategyMessage::PositionUpdate(None))
                                                    .await
                                                {
                                                    error!("Failed to send PositionUpdate(None): {}", e);
                                                }
                                                return;
                                            }
                                            "Cancelled" | "Rejected" => {
                                                error!("❌ Close order {}: {}", response.order_id, status.order_status);
                                                // Don't send PositionUpdate - position still exists!
                                                return;
                                            }
                                            _ => continue,
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Close order poll {}/{} failed: {}", attempt, max_polls, e);
                                        continue;
                                    }
                                }
                            }

                            // Timeout - assume filled for Market IOC
                            warn!("Close order confirmation timeout, assuming filled");
                            if let Err(e) = self
                                .strategy_tx
                                .send(StrategyMessage::PositionUpdate(None))
                                .await
                            {
                                error!("Failed to send PositionUpdate(None): {}", e);
                            }
                        }
                        Err(e) => {
                            error!("❌ Failed to close position: {}", e);
                            // Don't send PositionUpdate - position still exists!
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to get position for closing: {}", e);
            }
        }
    }

    async fn handle_get_position(&self, symbol: Symbol) {
        match self.client.get_position(&symbol.0).await {
            Ok(positions) => {
                if positions.is_empty() {
                    if let Err(e) = self
                        .strategy_tx
                        .send(StrategyMessage::PositionUpdate(None))
                        .await
                    {
                        error!("Failed to send PositionUpdate(None): {}", e);
                    }
                    return;
                }

                for pos_info in positions {
                    let size = Decimal::from_str(&pos_info.size).unwrap_or(Decimal::ZERO);

                    if size > Decimal::ZERO {
                        let entry_price = Decimal::from_str(&pos_info.avg_price)
                            .unwrap_or(Decimal::ZERO);
                        let is_long = pos_info.side == "Buy";

                        // ✅ FIX BUG #2: Calculate stop_loss based on config
                        let sl_percent = Decimal::from_str(&self.config.stop_loss_percent.to_string())
                            .unwrap_or(Decimal::new(5, 1)); // 0.5% default
                        let sl_multiplier = Decimal::ONE - (sl_percent / Decimal::from(100));
                        let sl_multiplier_short = Decimal::ONE + (sl_percent / Decimal::from(100));
                        
                        let stop_loss = if is_long {
                            entry_price * sl_multiplier  // Long: SL below entry
                        } else {
                            entry_price * sl_multiplier_short  // Short: SL above entry
                        };

                        let position = Position {
                            symbol: symbol.clone(),
                            side: if is_long {
                                PositionSide::Long
                            } else {
                                PositionSide::Short
                            },
                            size,
                            entry_price,
                            current_price: Decimal::from_str(&pos_info.avg_price)
                                .unwrap_or(Decimal::ZERO),
                            unrealized_pnl: Decimal::from_str(&pos_info.unrealised_pnl)
                                .unwrap_or(Decimal::ZERO),
                            stop_loss: Some(stop_loss),  // ✅ Now properly set!
                        };

                        debug!("📊 Position created: {:?}, SL: {}", position.side, stop_loss);

                        if let Err(e) = self
                            .strategy_tx
                            .send(StrategyMessage::PositionUpdate(Some(position)))
                            .await
                        {
                            error!("Failed to send PositionUpdate(Some): {}", e);
                        }

                        return;
                    }
                }

                if let Err(e) = self
                    .strategy_tx
                    .send(StrategyMessage::PositionUpdate(None))
                    .await
                {
                    error!("Failed to send PositionUpdate(None) after loop: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to get position: {}", e);
            }
        }
    }
}

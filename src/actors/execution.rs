use crate::actors::messages::{ExecutionMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::BybitClient;
use crate::models::*;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

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
        let error_msg = format!("Order {} confirmation timeout after 10 seconds", order_id);
        error!("⏰ {}", error_msg);

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
                        qty_step: None,  // Market order, precision not critical
                        tick_size: None,
                    };

                    info!(
                        "📤 Closing order: {:?} {} (reduce_only)",
                        close_side, size
                    );

                    match self.client.place_order(&close_order).await {
                        Ok(response) => {
                            info!("✅ Position closed: {}", response.order_id);

                            // Notify strategy
                            if let Err(e) = self
                                .strategy_tx
                                .send(StrategyMessage::PositionUpdate(None))
                                .await
                            {
                                error!("Failed to send PositionUpdate(None) after close: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("❌ Failed to close position: {}", e);
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
                        let position = Position {
                            symbol: symbol.clone(),
                            side: if pos_info.side == "Buy" {
                                PositionSide::Long
                            } else {
                                PositionSide::Short
                            },
                            size,
                            entry_price: Decimal::from_str(&pos_info.avg_price)
                                .unwrap_or(Decimal::ZERO),
                            current_price: Decimal::from_str(&pos_info.avg_price)
                                .unwrap_or(Decimal::ZERO),
                            unrealized_pnl: Decimal::from_str(&pos_info.unrealised_pnl)
                                .unwrap_or(Decimal::ZERO),
                            stop_loss: None,
                        };

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

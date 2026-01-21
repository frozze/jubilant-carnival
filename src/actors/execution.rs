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
        let is_post_only = order.time_in_force == TimeInForce::PostOnly;

        info!(
            "📤 Placing order: {:?} {} {} @ {:?} ({})",
            order.side, order.qty, symbol, order.price,
            if is_post_only { "PostOnly" } else { "Market/IOC" }
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

        // ✅ Step 2 - Poll for order confirmation
        // PostOnly: 5 seconds timeout then fallback to Market IOC
        // Market/IOC: 10 seconds timeout (should fill much faster)
        let initial_timeout_polls = if is_post_only {
            10 // 5s PostOnly timeout before fallback
        } else {
            20 // 10s total for Market/IOC
        };

        let poll_interval = tokio::time::Duration::from_millis(500);

        for attempt in 1..=initial_timeout_polls {
            tokio::time::sleep(poll_interval).await;

            match self.client.get_order_status(&symbol_str, &order_id).await {
                Ok(order_status) => {
                    info!(
                        "📊 Order {} status: {} (attempt {}/{})",
                        order_id, order_status.order_status, attempt, initial_timeout_polls
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
                        attempt, initial_timeout_polls, e
                    );
                    continue;
                }
            }
        }

        // ✅ FALLBACK: PostOnly didn't fill within 5 seconds
        if is_post_only {
            warn!("⚠️  PostOnly order {} not filled after 5s, falling back to Market IOC", order_id);

            // Cancel the PostOnly order
            match self.client.cancel_order(&symbol_str, &order_id).await {
                Ok(_) => {
                    info!("✅ Cancelled PostOnly order {}", order_id);
                }
                Err(e) => {
                    // Order might already be filled/cancelled, check status
                    warn!("Failed to cancel PostOnly order {}: {}", order_id, e);

                    // Double-check status before fallback
                    if let Ok(status) = self.client.get_order_status(&symbol_str, &order_id).await {
                        if status.order_status == "Filled" {
                            info!("✅ PostOnly order {} was FILLED during cancel", order_id);
                            if let Err(e) = self.strategy_tx.send(StrategyMessage::OrderFilled(symbol.clone())).await {
                                error!("Failed to send OrderFilled: {}", e);
                            }
                            self.handle_get_position(symbol).await;
                            return;
                        }
                    }
                }
            }

            // Create fallback Market IOC order
            info!("🔄 Placing fallback Market IOC order");
            let fallback_order = Order {
                symbol: order.symbol.clone(),
                side: order.side,
                order_type: OrderType::Market,
                qty: order.qty,
                price: None,
                time_in_force: TimeInForce::IOC,
                reduce_only: order.reduce_only,
                qty_step: order.qty_step,
                tick_size: order.tick_size,
            };

            let fallback_order_id = match self.client.place_order(&fallback_order).await {
                Ok(response) => {
                    info!("✅ Fallback Market IOC order accepted: {}", response.order_id);
                    response.order_id
                }
                Err(e) => {
                    let error_msg = format!("Failed to place fallback Market IOC order: {}", e);
                    error!("❌ {}", error_msg);
                    if let Err(e) = self.strategy_tx.send(StrategyMessage::OrderFailed(error_msg)).await {
                        error!("Failed to send OrderFailed: {}", e);
                    }
                    return;
                }
            };

            // Poll fallback order (should fill quickly)
            for attempt in 1..=10 {
                tokio::time::sleep(poll_interval).await;

                match self.client.get_order_status(&symbol_str, &fallback_order_id).await {
                    Ok(order_status) => {
                        info!(
                            "📊 Fallback order {} status: {} (attempt {}/10)",
                            fallback_order_id, order_status.order_status, attempt
                        );

                        match order_status.order_status.as_str() {
                            "Filled" => {
                                info!("✅ Fallback order {} FILLED", fallback_order_id);
                                if let Err(e) = self.strategy_tx.send(StrategyMessage::OrderFilled(symbol.clone())).await {
                                    error!("Failed to send OrderFilled: {}", e);
                                }
                                self.handle_get_position(symbol).await;
                                return;
                            }
                            "Cancelled" | "Rejected" => {
                                let error_msg = format!("Fallback order {} {}", fallback_order_id, order_status.order_status);
                                error!("❌ {}", error_msg);
                                if let Err(e) = self.strategy_tx.send(StrategyMessage::OrderFailed(error_msg)).await {
                                    error!("Failed to send OrderFailed: {}", e);
                                }
                                return;
                            }
                            _ => continue,
                        }
                    }
                    Err(e) => {
                        warn!("Failed to query fallback order status: {}", e);
                        continue;
                    }
                }
            }

            // Fallback order also timed out
            let error_msg = format!("Fallback Market IOC order {} timeout", fallback_order_id);
            error!("⏰ {}", error_msg);
            if let Err(e) = self.strategy_tx.send(StrategyMessage::OrderFailed(error_msg)).await {
                error!("Failed to send OrderFailed: {}", e);
            }
            return;
        }

        // Market/IOC order timed out (shouldn't happen often)
        let error_msg = format!("Market/IOC order {} timeout after 10 seconds", order_id);
        error!("⏰ {}", error_msg);
        if let Err(e) = self.strategy_tx.send(StrategyMessage::OrderFailed(error_msg)).await {
            error!("Failed to send OrderFailed: {}", e);
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

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
        info!(
            "📤 Placing order: {:?} {} {} @ {:?}",
            order.side, order.qty, order.symbol, order.price
        );

        match self.client.place_order(&order).await {
            Ok(response) => {
                info!("✅ Order placed successfully: {}", response.order_id);
            }
            Err(e) => {
                error!("❌ Failed to place order: {}", e);
            }
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
                    };

                    info!(
                        "📤 Closing order: {:?} {} (reduce_only)",
                        close_side, size
                    );

                    match self.client.place_order(&close_order).await {
                        Ok(response) => {
                            info!("✅ Position closed: {}", response.order_id);

                            // Notify strategy
                            let _ = self
                                .strategy_tx
                                .send(StrategyMessage::PositionUpdate(None))
                                .await;
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
                    let _ = self
                        .strategy_tx
                        .send(StrategyMessage::PositionUpdate(None))
                        .await;
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

                        let _ = self
                            .strategy_tx
                            .send(StrategyMessage::PositionUpdate(Some(position)))
                            .await;

                        return;
                    }
                }

                let _ = self
                    .strategy_tx
                    .send(StrategyMessage::PositionUpdate(None))
                    .await;
            }
            Err(e) => {
                error!("Failed to get position: {}", e);
            }
        }
    }
}

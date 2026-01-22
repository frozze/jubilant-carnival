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
        // ✅ FIX BUG #20 & #21: CRITICAL race condition!
        // Between cancel request and response, order might FILL or PARTIALLY FILL
        // We MUST verify final state before reporting failure
        warn!("⏰ Order {} timeout after 10s, attempting to cancel...", order_id);

        if let Err(e) = self.client.cancel_order(&symbol_str, &order_id).await {
            error!("Failed to cancel timed-out order: {}", e);
        }

        // ✅ CRITICAL: Query final order status after cancel
        // The order might have filled DURING the cancel API call!
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await; // Let cancel settle

        match self.client.get_order_status(&symbol_str, &order_id).await {
            Ok(final_status) => {
                match final_status.order_status.as_str() {
                    "Filled" => {
                        // ✅ Order FILLED during cancel! This is the race condition!
                        warn!("⚠️  BUG #20 CAUGHT! Order {} filled DURING cancel attempt", order_id);
                        info!("✅ Order {} FILLED (detected after cancel)", order_id);

                        if let Err(e) = self
                            .strategy_tx
                            .send(StrategyMessage::OrderFilled(symbol.clone()))
                            .await
                        {
                            error!("Failed to send OrderFilled message: {}", e);
                        }

                        // Query position to confirm
                        self.handle_get_position(symbol).await;
                        return;
                    }
                    "PartiallyFilled" => {
                        // ✅ BUG #21: Partial fill exists!
                        warn!("⚠️  BUG #21 CAUGHT! Order {} PARTIALLY filled: {}/{}",
                              order_id, final_status.cum_exec_qty, final_status.qty);

                        // Query position - partial position exists!
                        self.handle_get_position(symbol).await;

                        // Notify strategy that partial fill occurred (not a full failure)
                        let error_msg = format!(
                            "Order {} partially filled ({}/{}), then cancelled",
                            order_id, final_status.cum_exec_qty, final_status.qty
                        );
                        warn!("{}", error_msg);

                        if let Err(e) = self
                            .strategy_tx
                            .send(StrategyMessage::OrderFailed(error_msg))
                            .await
                        {
                            error!("Failed to send OrderFailed message: {}", e);
                        }
                        return;
                    }
                    "Cancelled" | "Rejected" => {
                        // Truly cancelled/rejected - safe to report failure
                        let error_msg = format!("Order {} {} after timeout", order_id, final_status.order_status);
                        info!("✅ Verified: {}", error_msg);

                        if let Err(e) = self
                            .strategy_tx
                            .send(StrategyMessage::OrderFailed(error_msg))
                            .await
                        {
                            error!("Failed to send OrderFailed message: {}", e);
                        }
                        return;
                    }
                    _ => {
                        warn!("Unknown final order status: {}", final_status.order_status);
                    }
                }
            }
            Err(e) => {
                // ✅ DEFENSIVE: If we can't query status, check position anyway
                error!("Failed to verify final order status: {}", e);
                warn!("⚠️  Cannot confirm order state, checking position defensively...");
                self.handle_get_position(symbol).await;

                // Report failure but position check will reveal truth
                let error_msg = format!("Order {} cancel attempted, final state unknown", order_id);
                if let Err(e) = self
                    .strategy_tx
                    .send(StrategyMessage::OrderFailed(error_msg))
                    .await
                {
                    error!("Failed to send OrderFailed message: {}", e);
                }
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

                            // ✅ FIX BUG #22 (CRITICAL): NEVER assume filled!
                            // Market orders CAN be rejected (insufficient liquidity, price protection, risk limits)
                            // If we assume filled but position still exists → Strategy thinks closed → money bleeds!
                            warn!("⏰ Close order {} timeout after 5s, verifying position state...", response.order_id);

                            // Query final order status
                            match self.client.get_order_status(&symbol.0, &response.order_id).await {
                                Ok(final_status) => {
                                    match final_status.order_status.as_str() {
                                        "Filled" => {
                                            info!("✅ Close order {} verified FILLED", response.order_id);
                                            if let Err(e) = self
                                                .strategy_tx
                                                .send(StrategyMessage::PositionUpdate(None))
                                                .await
                                            {
                                                error!("Failed to send PositionUpdate(None): {}", e);
                                            }
                                        }
                                        "PartiallyFilled" => {
                                            warn!("⚠️  Close order {} PARTIALLY filled: {}/{}",
                                                  response.order_id, final_status.cum_exec_qty, final_status.qty);
                                            // Query position - partial position still exists!
                                            self.handle_get_position(symbol.clone()).await;
                                        }
                                        "Cancelled" | "Rejected" => {
                                            error!("❌ Close order {} {}: POSITION STILL EXISTS!",
                                                   response.order_id, final_status.order_status);
                                            // DO NOT send PositionUpdate(None) - position still open!
                                            // Query position to send correct state
                                            self.handle_get_position(symbol.clone()).await;
                                        }
                                        _ => {
                                            warn!("Unknown close order status: {}, querying position...", final_status.order_status);
                                            self.handle_get_position(symbol.clone()).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    // ✅ DEFENSIVE: Cannot verify order status → Query position directly
                                    error!("Failed to verify close order status: {}", e);
                                    warn!("⚠️  BUG #22 PROTECTION: Querying position to verify close...");
                                    self.handle_get_position(symbol.clone()).await;
                                }
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
        // ✅ FIX BUG #23 (HIGH): Empty array ambiguity
        // API can return empty array due to lag even if position exists!
        // This is especially dangerous after OrderFilled where we KNOW position should exist.
        // Solution: Retry 3 times before accepting empty result
        const MAX_RETRIES: u32 = 3;
        const RETRY_DELAY_MS: u64 = 200;

        for retry_attempt in 0..MAX_RETRIES {
            match self.client.get_position(&symbol.0).await {
                Ok(positions) => {
                    if positions.is_empty() {
                        if retry_attempt < MAX_RETRIES - 1 {
                            // Not the last attempt - retry after delay
                            debug!(
                                "Position query returned empty (attempt {}/{}), retrying in {}ms...",
                                retry_attempt + 1, MAX_RETRIES, RETRY_DELAY_MS
                            );
                            tokio::time::sleep(tokio::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                            continue; // Retry
                        } else {
                            // Last attempt still empty - accept as no position
                            info!("✅ Position confirmed empty after {} retries", MAX_RETRIES);
                            if let Err(e) = self
                                .strategy_tx
                                .send(StrategyMessage::PositionUpdate(None))
                                .await
                            {
                                error!("Failed to send PositionUpdate(None): {}", e);
                            }
                            return;
                        }
                    }

                    // Process positions (not empty)
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

                            debug!("📊 Position found: {:?}, SL: {}", position.side, stop_loss);

                            if let Err(e) = self
                                .strategy_tx
                                .send(StrategyMessage::PositionUpdate(Some(position)))
                                .await
                            {
                                error!("Failed to send PositionUpdate(Some): {}", e);
                            }

                            return; // Position found, exit retry loop
                        }
                    }

                    // All positions have size=0 (shouldn't happen but handle it)
                    if retry_attempt < MAX_RETRIES - 1 {
                        debug!("All positions have size=0, retrying...");
                        tokio::time::sleep(tokio::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    } else {
                        warn!("All positions have size=0 after {} retries", MAX_RETRIES);
                        if let Err(e) = self
                            .strategy_tx
                            .send(StrategyMessage::PositionUpdate(None))
                            .await
                        {
                            error!("Failed to send PositionUpdate(None) after loop: {}", e);
                        }
                        return;
                    }
                }
                Err(e) => {
                    if retry_attempt < MAX_RETRIES - 1 {
                        warn!("Failed to get position (attempt {}/{}): {}, retrying...",
                              retry_attempt + 1, MAX_RETRIES, e);
                        tokio::time::sleep(tokio::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                        continue;
                    } else {
                        error!("Failed to get position after {} retries: {}", MAX_RETRIES, e);
                        // Don't send PositionUpdate - we don't know the state!
                        return;
                    }
                }
            }
        }
    }
}

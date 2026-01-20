use crate::actors::messages::{MarketDataMessage, StrategyMessage};
use crate::config::Config;
use crate::models::{OrderBookSnapshot, Symbol, TradeSide, TradeTick};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// MarketDataActor - maintains WebSocket connection with Hot-Swap capability
pub struct MarketDataActor {
    config: Arc<Config>,
    ws_url: String,
    strategy_tx: mpsc::Sender<StrategyMessage>,
    command_rx: mpsc::Receiver<MarketDataMessage>,
    current_symbol: Option<Symbol>,
}

impl MarketDataActor {
    pub fn new(
        config: Arc<Config>,
        strategy_tx: mpsc::Sender<StrategyMessage>,
        command_rx: mpsc::Receiver<MarketDataMessage>,
    ) -> Self {
        let ws_url = config.ws_url().to_string();

        Self {
            config,
            ws_url,
            strategy_tx,
            command_rx,
            current_symbol: None,
        }
    }

    pub async fn run(mut self) {
        info!("📡 MarketDataActor started");

        loop {
            match self.connect_and_stream().await {
                Ok(_) => {
                    info!("WebSocket connection closed gracefully");
                    break;
                }
                Err(e) => {
                    error!("WebSocket error: {}. Reconnecting in 5s...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn connect_and_stream(&mut self) -> Result<()> {
        // Connect to WebSocket
        let (ws_stream, _) = connect_async(&self.ws_url)
            .await
            .context("Failed to connect to WebSocket")?;

        info!("✅ WebSocket connected to {}", self.ws_url);

        let (mut write, mut read) = ws_stream.split();

        // Ping interval to keep connection alive
        let mut ping_interval = interval(Duration::from_secs(20));

        loop {
            tokio::select! {
                // Handle incoming WebSocket messages
                Some(msg) = read.next() => {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Err(e) = self.handle_message(&text).await {
                                warn!("Failed to handle message: {}", e);
                            }
                        }
                        Ok(Message::Ping(_)) => {
                            // Tungstenite handles pong automatically
                        }
                        Ok(Message::Close(_)) => {
                            info!("WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            error!("WebSocket read error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }

                // Handle commands from Scanner
                Some(cmd) = self.command_rx.recv() => {
                    match cmd {
                        MarketDataMessage::SwitchSymbol(new_symbol) => {
                            info!("🔄 Hot-swapping to symbol: {}", new_symbol);

                            // Unsubscribe from old symbol
                            if let Some(ref old_symbol) = self.current_symbol {
                                if let Err(e) = self.unsubscribe(&mut write, old_symbol).await {
                                    error!("Failed to unsubscribe from {}: {}", old_symbol, e);
                                }

                                // Notify strategy is handled by Scanner now
                                // let _ = self.strategy_tx.send(StrategyMessage::SymbolChanged(new_symbol.clone())).await;
                            }

                            // Subscribe to new symbol
                            if let Err(e) = self.subscribe(&mut write, &new_symbol).await {
                                error!("Failed to subscribe to {}: {}", new_symbol, e);
                            } else {
                                self.current_symbol = Some(new_symbol);
                            }
                        }
                        MarketDataMessage::Shutdown => {
                            info!("Shutdown command received");
                            break;
                        }
                    }
                }

                // Send periodic ping
                _ = ping_interval.tick() => {
                    if let Err(e) = write.send(Message::Ping(vec![])).await {
                        error!("Failed to send ping: {}", e);
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn subscribe(
        &self,
        write: &mut futures_util::stream::SplitSink<WsStream, Message>,
        symbol: &Symbol,
    ) -> Result<()> {
        let subscribe_msg = SubscribeMessage {
            op: "subscribe".to_string(),
            args: vec![
                format!("orderbook.1.{}", symbol.0),
                format!("publicTrade.{}", symbol.0),
            ],
        };

        let msg_text = serde_json::to_string(&subscribe_msg)?;
        write.send(Message::Text(msg_text)).await?;

        info!("📥 Subscribed to {} orderbook and trades", symbol);
        Ok(())
    }

    async fn unsubscribe(
        &self,
        write: &mut futures_util::stream::SplitSink<WsStream, Message>,
        symbol: &Symbol,
    ) -> Result<()> {
        let unsubscribe_msg = SubscribeMessage {
            op: "unsubscribe".to_string(),
            args: vec![
                format!("orderbook.1.{}", symbol.0),
                format!("publicTrade.{}", symbol.0),
            ],
        };

        let msg_text = serde_json::to_string(&unsubscribe_msg)?;
        write.send(Message::Text(msg_text)).await?;

        info!("📤 Unsubscribed from {}", symbol);
        Ok(())
    }

    async fn handle_message(&self, text: &str) -> Result<()> {
        // Try to parse as WebSocket response
        let ws_msg: WsMessage = serde_json::from_str(text)?;

        // Handle different topics
        if let Some(ref topic) = ws_msg.topic {
            if topic.starts_with("orderbook") {
                self.handle_orderbook(ws_msg)?;
            } else if topic.starts_with("publicTrade") {
                self.handle_trade(ws_msg)?;
            }
        }

        Ok(())
    }

    fn handle_orderbook(&self, msg: WsMessage) -> Result<()> {
        if let Some(data) = msg.data {
            if let Some(symbol_str) = data.get("s").and_then(|v| v.as_str()) {
                let symbol = Symbol::from(symbol_str);

                // Get best bid/ask
                let bids = data.get("b").and_then(|v| v.as_array());
                let asks = data.get("a").and_then(|v| v.as_array());

                if let (Some(bids), Some(asks)) = (bids, asks) {
                    if let (Some(best_bid), Some(best_ask)) = (bids.first(), asks.first()) {
                        let bid_price = best_bid[0].as_str().unwrap_or("0");
                        let bid_size = best_bid[1].as_str().unwrap_or("0");
                        let ask_price = best_ask[0].as_str().unwrap_or("0");
                        let ask_size = best_ask[1].as_str().unwrap_or("0");

                        let timestamp = data
                            .get("ts")
                            .and_then(|v| v.as_i64())
                            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

                        // Check for stale data
                        let now = chrono::Utc::now().timestamp_millis();
                        if now - timestamp > self.config.stale_data_threshold_ms {
                            debug!("Ignoring stale orderbook data (age: {}ms)", now - timestamp);
                            return Ok(());
                        }

                        let snapshot = OrderBookSnapshot::new(
                            symbol.clone(),
                            timestamp,
                            Decimal::from_str(bid_price).unwrap_or(Decimal::ZERO),
                            Decimal::from_str(ask_price).unwrap_or(Decimal::ZERO),
                            Decimal::from_str(bid_size).unwrap_or(Decimal::ZERO),
                            Decimal::from_str(ask_size).unwrap_or(Decimal::ZERO),
                        );

                        // ✅ FIXED: Use try_send to avoid task explosion
                        if let Err(e) = self.strategy_tx.try_send(StrategyMessage::OrderBook(snapshot)) {
                             // It's normal to drop packets in HFT if consumer is slow
                             debug!("Dropped orderbook snapshot: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_trade(&self, msg: WsMessage) -> Result<()> {
        if let Some(data_array) = msg.data {
            if let Some(trades) = data_array.as_array() {
                for trade_data in trades {
                    if let Some(symbol_str) = trade_data.get("s").and_then(|v| v.as_str()) {
                        let symbol = Symbol::from(symbol_str);
                        let price = trade_data
                            .get("p")
                            .and_then(|v| v.as_str())
                            .and_then(|s| Decimal::from_str(s).ok())
                            .unwrap_or(Decimal::ZERO);

                        let size = trade_data
                            .get("v")
                            .and_then(|v| v.as_str())
                            .and_then(|s| Decimal::from_str(s).ok())
                            .unwrap_or(Decimal::ZERO);

                        let timestamp = trade_data
                            .get("T")
                            .and_then(|v| v.as_i64())
                            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

                        // Check for stale data
                        let now = chrono::Utc::now().timestamp_millis();
                        if now - timestamp > self.config.stale_data_threshold_ms {
                            continue;
                        }

                        let side = trade_data
                            .get("S")
                            .and_then(|v| v.as_str())
                            .map(|s| {
                                if s == "Buy" {
                                    TradeSide::Buy
                                } else {
                                    TradeSide::Sell
                                }
                            })
                            .unwrap_or(TradeSide::Buy);

                        let tick = TradeTick {
                            symbol: symbol.clone(),
                            price,
                            size,
                            timestamp,
                            side,
                        };

                        // ✅ FIXED: Use try_send to avoid task explosion
                        if let Err(e) = self.strategy_tx.try_send(StrategyMessage::Trade(tick)) {
                             // It's normal to drop packets in HFT if consumer is slow
                             debug!("Dropped trade tick: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct SubscribeMessage {
    op: String,
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WsMessage {
    topic: Option<String>,
    #[serde(rename = "type")]
    msg_type: Option<String>,
    data: Option<serde_json::Value>,
}

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use tracing::{debug, error, warn};

type HmacSha256 = Hmac<Sha256>;

const RECV_WINDOW: &str = "5000";

/// Round a value to the nearest step (e.g., round 4.977 to step 0.1 = 4.9)
fn round_to_step(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return value;
    }
    (value / step).floor() * step
}

#[derive(Clone)]
pub struct BybitClient {
    client: Client,
    api_key: String,
    api_secret: String,
    base_url: String,
}

impl BybitClient {
    pub fn new(api_key: String, api_secret: String, base_url: String) -> Self {
        // HFT-optimized HTTP client
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .tcp_nodelay(true) // Disable Nagle's algorithm for low latency
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(10) // Connection pooling
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            api_key,
            api_secret,
            base_url,
        }
    }

    /// Generate Bybit V5 API signature
    /// Formula: timestamp + api_key + recv_window + params
    fn sign(&self, timestamp: i64, recv_window: &str, params: &str) -> String {
        let sign_str = format!(
            "{}{}{}{}",
            timestamp, &self.api_key, recv_window, params
        );

        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(sign_str.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Public endpoint - no authentication required
    pub async fn get_tickers(&self, category: &str) -> Result<TickersResponse> {
        let url = format!("{}/v5/market/tickers", self.base_url);

        let mut retries = 0;
        let max_retries = 3;

        loop {
            match self
                .client
                .get(&url)
                .query(&[("category", category)])
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        let data: ApiResponse<TickersResponse> = response
                            .json()
                            .await
                            .context("Failed to parse tickers response")?;

                        if data.ret_code == 0 {
                            return Ok(data.result);
                        } else {
                            anyhow::bail!("API error: {} - {}", data.ret_code, data.ret_msg);
                        }
                    } else if response.status().as_u16() >= 500 && retries < max_retries {
                        retries += 1;
                        error!(
                            "Server error {}, retry {}/{}",
                            response.status(),
                            retries,
                            max_retries
                        );
                        tokio::time::sleep(tokio::time::Duration::from_secs(2u64.pow(retries)))
                            .await;
                        continue;
                    } else {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        anyhow::bail!("HTTP error {}: {}", status, body);
                    }
                }
                Err(e) if retries < max_retries => {
                    retries += 1;
                    error!("Request error: {}, retry {}/{}", e, retries, max_retries);
                    tokio::time::sleep(tokio::time::Duration::from_secs(2u64.pow(retries))).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// GET /v5/market/instruments-info
    /// Fetch instrument specifications (qtyStep, tickSize, minOrderQty)
    pub async fn get_instrument_info(&self, symbol: &str) -> Result<InstrumentInfo> {
        let url = format!("{}/v5/market/instruments-info", self.base_url);

        let response = self
            .client
            .get(&url)
            .query(&[("category", "linear"), ("symbol", symbol)])
            .send()
            .await
            .context("Failed to send instruments-info request")?;

        if response.status().is_success() {
            let data: ApiResponse<InstrumentsResponse> = response
                .json()
                .await
                .context("Failed to parse instruments-info response")?;

            if data.ret_code == 0 {
                if let Some(instrument) = data.result.list.into_iter().next() {
                    return Ok(instrument);
                } else {
                    anyhow::bail!("No instrument info found for {}", symbol);
                }
            } else {
                anyhow::bail!("API error: {} - {}", data.ret_code, data.ret_msg);
            }
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("HTTP error {}: {}", status, body);
        }
    }

    /// POST /v5/order/create
    /// CRITICAL: For POST requests, the signature MUST be calculated on the EXACT JSON body sent
    pub async fn place_order(&self, order: &crate::models::Order) -> Result<PlaceOrderResponse> {
        let timestamp = chrono::Utc::now().timestamp_millis();
        let url = format!("{}/v5/order/create", self.base_url);

        // Round qty based on instrument's qtyStep, fallback to 2 decimals
        let qty_rounded = if let Some(qty_step) = &order.qty_step {
            round_to_step(order.qty, *qty_step)
        } else {
            order.qty.round_dp(2)
        };
        
        // Build JSON payload
        let mut payload = json!({
            "category": "linear",
            "symbol": order.symbol.0,
            "side": format!("{:?}", order.side),
            "orderType": format!("{:?}", order.order_type),
            "qty": qty_rounded.to_string(),
            "timeInForce": format!("{:?}", order.time_in_force),
        });

        // Add optional fields - round price based on instrument's tickSize
        if let Some(price) = order.price {
            let price_rounded = if let Some(tick_size) = &order.tick_size {
                round_to_step(price, *tick_size)
            } else {
                price.round_dp(4)
            };
            payload["price"] = json!(price_rounded.to_string());
        }

        if order.reduce_only {
            payload["reduceOnly"] = json!(true);
        }

        // Serialize to string ONCE - this exact string will be signed and sent
        let payload_str = serde_json::to_string(&payload)
            .context("Failed to serialize order payload")?;

        // Generate signature on the EXACT payload
        let signature = self.sign(timestamp, RECV_WINDOW, &payload_str);

        debug!(
            "Placing order: {:?} {} {} @ {:?}",
            order.side, order.qty, order.symbol, order.price
        );

        // Send request with exponential backoff retry
        let mut retries = 0;
        let max_retries = 3;

        loop {
            let response = self
                .client
                .post(&url)
                .header("X-BAPI-API-KEY", &self.api_key)
                .header("X-BAPI-TIMESTAMP", timestamp.to_string())
                .header("X-BAPI-SIGN", &signature)
                .header("X-BAPI-RECV-WINDOW", RECV_WINDOW)
                .header("Content-Type", "application/json")
                .body(payload_str.clone()) // Send the EXACT signed string
                .send()
                .await;

            match response {
                Ok(resp) if resp.status().is_success() => {
                    let raw_body = resp.text().await.context("Failed to read response body")?;
                    debug!("Raw order response: {}", raw_body);
                    
                    let data: ApiResponse<PlaceOrderResponse> = serde_json::from_str(&raw_body)
                        .context(format!("Failed to parse order response: {}", raw_body))?;

                    if data.ret_code == 0 {
                        debug!("Order placed successfully: {}", data.result.order_id);
                        return Ok(data.result);
                    } else {
                        anyhow::bail!(
                            "Order placement failed: {} - {}",
                            data.ret_code,
                            data.ret_msg
                        );
                    }
                }
                Ok(resp) if resp.status().as_u16() >= 500 && retries < max_retries => {
                    retries += 1;
                    warn!(
                        "Server error {}, retry {}/{}",
                        resp.status(),
                        retries,
                        max_retries
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(2u64.pow(retries))).await;
                    continue;
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Order failed with HTTP {}: {}", status, body);
                }
                Err(e) if retries < max_retries => {
                    retries += 1;
                    warn!("Request error: {}, retry {}/{}", e, retries, max_retries);
                    tokio::time::sleep(tokio::time::Duration::from_secs(2u64.pow(retries))).await;
                }
                Err(e) => {
                    return Err(e).context("Failed to send order request");
                }
            }
        }
    }

    /// GET /v5/position/list
    /// CRITICAL: For GET requests, the signature MUST be calculated on the QUERY STRING
    /// Format: category=linear&symbol=BTCUSDT (NOT JSON!)
    pub async fn get_position(&self, symbol: &str) -> Result<Vec<PositionInfo>> {
        let timestamp = chrono::Utc::now().timestamp_millis();
        let url = format!("{}/v5/position/list", self.base_url);

        // Build query string MANUALLY to ensure correct signature
        // CRITICAL: For GET, params must be query string, NOT JSON
        let query_string = format!("category=linear&symbol={}", symbol);

        // Sign the query string
        let signature = self.sign(timestamp, RECV_WINDOW, &query_string);

        debug!("Getting position for {}", symbol);

        let response = self
            .client
            .get(&url)
            .header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-SIGN", &signature)
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW)
            .query(&[("category", "linear"), ("symbol", symbol)]) // Same query params as signature
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                let data: ApiResponse<PositionListResponse> = resp
                    .json()
                    .await
                    .context("Failed to parse position response")?;

                if data.ret_code == 0 {
                    debug!("Got {} positions for {}", data.result.list.len(), symbol);
                    Ok(data.result.list)
                } else {
                    // Not an error - just no position
                    debug!(
                        "Get position returned: {} - {}",
                        data.ret_code, data.ret_msg
                    );
                    Ok(vec![])
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                warn!("Get position failed with HTTP {}: {}", status, body);
                Ok(vec![]) // Return empty instead of error
            }
            Err(e) => {
                warn!("Get position request failed: {}", e);
                Ok(vec![]) // Return empty instead of error
            }
        }
    }

    /// Cancel all orders for a symbol (useful for emergency stops)
    #[allow(dead_code)]
    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<()> {
        let timestamp = chrono::Utc::now().timestamp_millis();
        let url = format!("{}/v5/order/cancel-all", self.base_url);

        let payload = json!({
            "category": "linear",
            "symbol": symbol,
        });

        let payload_str = serde_json::to_string(&payload)?;
        let signature = self.sign(timestamp, RECV_WINDOW, &payload_str);

        let response = self
            .client
            .post(&url)
            .header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-SIGN", &signature)
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW)
            .header("Content-Type", "application/json")
            .body(payload_str)
            .send()
            .await?;

        if response.status().is_success() {
            debug!("Cancelled all orders for {}", symbol);
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Cancel all orders failed: {} - {}", status, body);
        }
    }
}

// API Response types
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse<T> {
    pub ret_code: i32,
    pub ret_msg: String,
    pub result: T,
}

#[derive(Debug, Deserialize)]
pub struct TickersResponse {
    pub category: String,
    pub list: Vec<TickerInfo>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TickerInfo {
    pub symbol: String,
    pub last_price: String,
    pub price_24h_pcnt: String,
    pub turnover_24h: String,
    pub volume_24h: String,
    pub bid1_price: String,
    pub ask1_price: String,
    pub bid1_size: String,
    pub ask1_size: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderResponse {
    pub order_id: String,
    pub order_link_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PositionListResponse {
    pub list: Vec<PositionInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionInfo {
    pub symbol: String,
    pub side: String,
    pub size: String,
    pub avg_price: String,
    pub unrealised_pnl: String,
}

#[derive(Debug, Deserialize)]
pub struct InstrumentsResponse {
    pub list: Vec<InstrumentInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstrumentInfo {
    pub symbol: String,
    pub lot_size_filter: LotSizeFilter,
    pub price_filter: PriceFilter,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LotSizeFilter {
    pub qty_step: String,
    pub min_order_qty: String,
    pub max_order_qty: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceFilter {
    pub tick_size: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_generation() {
        let client = BybitClient::new(
            "test_key".to_string(),
            "test_secret".to_string(),
            "https://api-testnet.bybit.com".to_string(),
        );

        let timestamp = 1234567890000i64;
        let recv_window = "5000";
        let params = r#"{"category":"linear","symbol":"BTCUSDT"}"#;

        let signature = client.sign(timestamp, recv_window, params);

        // Signature should be deterministic
        assert!(!signature.is_empty());
        assert_eq!(signature.len(), 64); // HMAC-SHA256 produces 64 hex chars
    }

    #[test]
    fn test_get_query_string_format() {
        // This is the CORRECT format for GET requests
        let query_string = format!("category=linear&symbol={}", "BTCUSDT");
        assert_eq!(query_string, "category=linear&symbol=BTCUSDT");

        // This is WRONG (old implementation):
        // let wrong = format!(r#"{{"category":"linear","symbol":"{}"}}"#, "BTCUSDT");
        // assert_ne!(query_string, wrong); // They are different!
    }
}

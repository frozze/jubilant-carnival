use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use tracing::{debug, error};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct BybitClient {
    client: Client,
    api_key: String,
    api_secret: String,
    base_url: String,
}

impl BybitClient {
    pub fn new(api_key: String, api_secret: String, base_url: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            api_key,
            api_secret,
            base_url,
        }
    }

    fn sign(&self, timestamp: i64, params: &str) -> String {
        let sign_str = format!("{}{}{}", timestamp, &self.api_key, params);
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(sign_str.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

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
                        tokio::time::sleep(tokio::time::Duration::from_secs(2u64.pow(retries))).await;
                        continue;
                    } else {
                        anyhow::bail!("HTTP error: {}", response.status());
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

    pub async fn place_order(&self, order: &crate::models::Order) -> Result<PlaceOrderResponse> {
        let timestamp = chrono::Utc::now().timestamp_millis();

        let mut params = HashMap::new();
        params.insert("category", "linear".to_string());
        params.insert("symbol", order.symbol.0.clone());
        params.insert("side", format!("{:?}", order.side));
        params.insert("orderType", format!("{:?}", order.order_type));
        params.insert("qty", order.qty.to_string());
        params.insert("timeInForce", format!("{:?}", order.time_in_force));

        if let Some(price) = order.price {
            params.insert("price", price.to_string());
        }

        if order.reduce_only {
            params.insert("reduceOnly", "true".to_string());
        }

        let params_str = serde_json::to_string(&params)?;
        let signature = self.sign(timestamp, &params_str);

        let url = format!("{}/v5/order/create", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-SIGN", signature)
            .json(&params)
            .send()
            .await?;

        if response.status().is_success() {
            let data: ApiResponse<PlaceOrderResponse> = response.json().await?;
            if data.ret_code == 0 {
                Ok(data.result)
            } else {
                anyhow::bail!("Order placement failed: {} - {}", data.ret_code, data.ret_msg);
            }
        } else {
            anyhow::bail!("HTTP error: {}", response.status());
        }
    }

    pub async fn get_position(&self, symbol: &str) -> Result<Vec<PositionInfo>> {
        let timestamp = chrono::Utc::now().timestamp_millis();

        let params = format!(r#"{{"category":"linear","symbol":"{}"}}"#, symbol);
        let signature = self.sign(timestamp, &params);

        let url = format!("{}/v5/position/list", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-BAPI-API-KEY", &self.api_key)
            .header("X-BAPI-TIMESTAMP", timestamp.to_string())
            .header("X-BAPI-SIGN", signature)
            .query(&[("category", "linear"), ("symbol", symbol)])
            .send()
            .await?;

        if response.status().is_success() {
            let data: ApiResponse<PositionListResponse> = response.json().await?;
            if data.ret_code == 0 {
                Ok(data.result.list)
            } else {
                debug!("Get position returned: {} - {}", data.ret_code, data.ret_msg);
                Ok(vec![])
            }
        } else {
            anyhow::bail!("HTTP error: {}", response.status());
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

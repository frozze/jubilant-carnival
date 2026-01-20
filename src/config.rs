use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bybit_api_key: String,
    pub bybit_api_secret: String,
    pub testnet: bool,

    // Trading parameters
    pub max_position_size_usd: f64,
    pub stop_loss_percent: f64,
    pub take_profit_percent: f64,

    // Scanner parameters
    pub scan_interval_secs: u64,
    pub min_turnover_24h_usd: f64,
    pub score_threshold_multiplier: f64,

    // Risk management
    pub max_spread_bps: f64,
    pub stale_data_threshold_ms: i64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        Ok(Self {
            bybit_api_key: env::var("BYBIT_API_KEY")
                .context("BYBIT_API_KEY not found in environment")?,
            bybit_api_secret: env::var("BYBIT_API_SECRET")
                .context("BYBIT_API_SECRET not found in environment")?,
            testnet: env::var("BYBIT_TESTNET")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),

            max_position_size_usd: env::var("MAX_POSITION_SIZE_USD")
                .unwrap_or_else(|_| "1000.0".to_string())
                .parse()
                .unwrap_or(1000.0),
            stop_loss_percent: env::var("STOP_LOSS_PERCENT")
                .unwrap_or_else(|_| "0.5".to_string())
                .parse()
                .unwrap_or(0.5),
            take_profit_percent: env::var("TAKE_PROFIT_PERCENT")
                .unwrap_or_else(|_| "1.0".to_string())
                .parse()
                .unwrap_or(1.0),

            scan_interval_secs: env::var("SCAN_INTERVAL_SECS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),
            min_turnover_24h_usd: env::var("MIN_TURNOVER_24H_USD")
                .unwrap_or_else(|_| "10000000.0".to_string())
                .parse()
                .unwrap_or(10_000_000.0),
            score_threshold_multiplier: env::var("SCORE_THRESHOLD_MULTIPLIER")
                .unwrap_or_else(|_| "1.2".to_string())
                .parse()
                .unwrap_or(1.2),

            max_spread_bps: env::var("MAX_SPREAD_BPS")
                .unwrap_or_else(|_| "20.0".to_string())
                .parse()
                .unwrap_or(20.0),
            stale_data_threshold_ms: env::var("STALE_DATA_THRESHOLD_MS")
                .unwrap_or_else(|_| "500".to_string())
                .parse()
                .unwrap_or(500),
        })
    }

    pub fn rest_api_url(&self) -> &str {
        if self.testnet {
            "https://api-testnet.bybit.com"
        } else {
            "https://api.bybit.com"
        }
    }

    pub fn ws_url(&self) -> &str {
        if self.testnet {
            "wss://stream-testnet.bybit.com/v5/public/linear"
        } else {
            "wss://stream.bybit.com/v5/public/linear"
        }
    }
}

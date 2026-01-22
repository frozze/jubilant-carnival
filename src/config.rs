use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bybit_api_key: String,
    pub bybit_api_secret: String,
    pub testnet: bool,

    // ✅ NEW: Custom URLs for Demo Trading / Custom Endpoints
    pub custom_rest_url: Option<String>,
    pub custom_ws_url: Option<String>,

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

    // Strategy parameters
    pub momentum_threshold: f64,
    pub min_trend_strength: f64,

    // ✅ PUMP PROTECTION: Blacklist specific symbols
    pub blacklist_symbols: Vec<String>,
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

            // ✅ NEW: Load custom URLs if provided
            custom_rest_url: env::var("BYBIT_REST_URL").ok(),
            custom_ws_url: env::var("BYBIT_WS_URL").ok(),

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

            momentum_threshold: env::var("MOMENTUM_THRESHOLD")
                .unwrap_or_else(|_| "0.15".to_string())
                .parse()
                .unwrap_or(0.15),

            min_trend_strength: env::var("MIN_TREND_STRENGTH")
                .unwrap_or_else(|_| "0.1".to_string())
                .parse::<f64>()
                .unwrap_or(0.1)
                / 100.0, // Convert percentage to decimal (0.1 → 0.001)

            // ✅ PUMP PROTECTION: Parse blacklist (comma-separated symbols)
            blacklist_symbols: env::var("BLACKLIST_SYMBOLS")
                .unwrap_or_else(|_| "".to_string())
                .split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }

    /// Get REST API URL
    /// Priority: 1. Custom URL (BYBIT_REST_URL)
    ///           2. Testnet URL
    ///           3. Mainnet URL (default)
    pub fn rest_api_url(&self) -> String {
        if let Some(ref custom_url) = self.custom_rest_url {
            // Custom URL takes highest priority (for Demo Trading)
            custom_url.clone()
        } else if self.testnet {
            "https://api-testnet.bybit.com".to_string()
        } else {
            "https://api.bybit.com".to_string()
        }
    }

    /// Get WebSocket URL
    /// Priority: 1. Custom URL (BYBIT_WS_URL)
    ///           2. Testnet URL
    ///           3. Mainnet URL (default)
    pub fn ws_url(&self) -> String {
        if let Some(ref custom_url) = self.custom_ws_url {
            // Custom URL takes highest priority (for Demo Trading)
            custom_url.clone()
        } else if self.testnet {
            "wss://stream-testnet.bybit.com/v5/public/linear".to_string()
        } else {
            "wss://stream.bybit.com/v5/public/linear".to_string()
        }
    }
}

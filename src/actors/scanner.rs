use crate::actors::messages::{MarketDataMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::{BybitClient, SpecsCache, SymbolSpecs};
use crate::models::Symbol;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

/// The "Predator" Scanner - hunts for high-volatility coins
pub struct ScannerActor {
    client: BybitClient,
    config: Arc<Config>,
    market_data_tx: mpsc::Sender<MarketDataMessage>,
    strategy_tx: mpsc::Sender<StrategyMessage>,
    specs_cache: SpecsCache,
    current_symbol: Option<Symbol>,
    current_score: f64,
}

impl ScannerActor {
    pub fn new(
        client: BybitClient,
        config: Arc<Config>,
        market_data_tx: mpsc::Sender<MarketDataMessage>,
        strategy_tx: mpsc::Sender<StrategyMessage>,
    ) -> Self {
        Self {
            client,
            config,
            market_data_tx,
            strategy_tx,
            specs_cache: SpecsCache::new(),
            current_symbol: None,
            current_score: 0.0,
        }
    }

    pub async fn run(mut self) {
        info!("🔍 ScannerActor started");

        let mut scan_interval = interval(Duration::from_secs(self.config.scan_interval_secs));

        // Initial scan
        if let Err(e) = self.scan_and_select().await {
            error!("Initial scan failed: {}", e);
        }

        loop {
            scan_interval.tick().await;

            if let Err(e) = self.scan_and_select().await {
                error!("Scan failed: {}", e);
                // Don't panic, just continue to next iteration
                continue;
            }
        }
    }

    async fn scan_and_select(&mut self) -> Result<()> {
        info!("🎯 Starting market scan...");

        // Fetch all tickers
        let tickers = self.client.get_tickers("linear").await?;

        // Filter and score coins
        let mut candidates: Vec<ScoredCoin> = tickers
            .list
            .iter()
            .filter_map(|ticker| {
                // Parse symbol
                let symbol = ticker.symbol.clone();

                // ✅ FIXED: Only accept USDT pairs
                if !symbol.ends_with("USDT") {
                    return None;
                }

                // Exclude BTC/ETH (too stable for scalping)
                if symbol == "BTCUSDT" || symbol == "ETHUSDT" {
                    return None;
                }

                // Exclude stablecoin pairs (USDCUSDT, BUSDUSDT, etc)
                let base_symbol = symbol.replace("USDT", "");
                if base_symbol == "USDC"
                    || base_symbol == "BUSD"
                    || base_symbol == "DAI"
                    || base_symbol == "TUSD"
                {
                    return None;
                }

                // Parse turnover and price change
                let turnover_24h = ticker.turnover_24h.parse::<f64>().ok()?;
                let price_change_24h = ticker.price_24h_pcnt.parse::<f64>().ok()?;

                // Filter by minimum turnover
                if turnover_24h < self.config.min_turnover_24h_usd {
                    return None;
                }

                // ✅ PURE FORMULA: Turnover * |PriceChange| (NO BIAS)
                let score = turnover_24h * price_change_24h.abs();

                Some(ScoredCoin {
                    symbol,
                    score,
                    turnover_24h,
                    price_change_24h,
                })
            })
            .collect();

        // Sort by score descending
        candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

        // Take top coin
        if let Some(top_coin) = candidates.first() {
            info!(
                "📊 Top coin: {} | Score: {:.2e} | Turnover: ${:.2e} | Change: {:.2}%",
                top_coin.symbol,
                top_coin.score,
                top_coin.turnover_24h,
                top_coin.price_change_24h * 100.0
            );

            // ✅ FIXED: Update current score from live candidates (Solve Zombie Bug)
            let mut current_score_live = 0.0;
            
            if let Some(ref current) = self.current_symbol {
                if let Some(current_candidate) = candidates.iter().find(|c| c.symbol == current.0) {
                    current_score_live = current_candidate.score;
                    // Update internal state to match reality
                    self.current_score = current_score_live;
                } else {
                    // Current symbol dropped out of filter (volume crash?) -> Score 0 to force switch
                    self.current_score = 0.0;
                }
            }

            // Check if we should switch
            let should_switch = if let Some(ref current) = self.current_symbol {
                // Switch if new score is significantly higher (threshold multiplier)
                // Compare against LIVE score, not stale score
                top_coin.score > self.current_score * self.config.score_threshold_multiplier
                    && top_coin.symbol != current.0
            } else {
                // No current symbol, switch to top
                true
            };

            if should_switch {
                info!(
                    "🔄 Switching to new coin: {} (score: {:.2e} -> {:.2e})",
                    top_coin.symbol, self.current_score, top_coin.score
                );

                // Fetch instrument specs if not cached
                let specs = if let Some(cached) = self.specs_cache.get(&top_coin.symbol) {
                    cached
                } else {
                    match self.client.get_instrument_info(&top_coin.symbol).await {
                        Ok(info) => {
                            let specs = SymbolSpecs::from(info);
                            self.specs_cache.insert(specs.clone());
                            specs
                        }
                        Err(e) => {
                            warn!("⚠️ Failed to fetch specs for {}: {}, using defaults", top_coin.symbol, e);
                            self.specs_cache.get_or_default(&top_coin.symbol)
                        }
                    }
                };

                self.current_symbol = Some(Symbol(top_coin.symbol.clone()));
                self.current_score = top_coin.score;

                // Send switch command to MarketDataActor
                if let Err(e) = self
                    .market_data_tx
                    .send(MarketDataMessage::SwitchSymbol(Symbol(
                        top_coin.symbol.clone(),
                    )))
                    .await
                {
                    error!("Failed to send symbol switch message: {}", e);
                }
                
                // Send specs to StrategyEngine
                if let Err(e) = self
                    .strategy_tx
                    .send(StrategyMessage::SymbolChanged { 
                        symbol: Symbol(top_coin.symbol.clone()),
                        specs 
                    })
                    .await
                {
                    error!("Failed to send symbol specs to strategy: {}", e);
                }
            } else {
                info!("✅ Current coin {} still optimal", self.current_symbol.as_ref().unwrap());
            }
        } else {
            warn!("⚠️  No suitable coins found in scan");
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ScoredCoin {
    symbol: String,
    score: f64,
    turnover_24h: f64,
    price_change_24h: f64,
}

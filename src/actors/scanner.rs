use crate::actors::messages::{MarketDataMessage, StrategyMessage};
use crate::config::Config;
use crate::exchange::{BybitClient, SpecsCache, SymbolSpecs};
use crate::models::Symbol;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

/// The "Predator" Scanner - hunts for high-volatility coins
pub struct ScannerActor {
    client: BybitClient,
    config: Arc<Config>,
    market_data_tx: mpsc::Sender<MarketDataMessage>,
    strategy_tx: mpsc::Sender<StrategyMessage>,
    specs_cache: SpecsCache,
    current_symbol: Option<Symbol>,
    current_score: f64,
    // ✅ FIX RECONNECT: Track first scan to ensure subscription after restart
    first_scan: bool,
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
            first_scan: true, // ✅ FIX RECONNECT: Ensure first scan always sends messages
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
        // ✅ MEAN REVERSION: If fixed symbol is set, use it directly (no scanning)
        if let Some(ref fixed_symbol) = self.config.trading_symbol {
            return self.use_fixed_symbol(fixed_symbol.clone()).await;
        }

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

                // ✅ FIX BUG #30: Check blacklist BEFORE selecting symbol
                if self.config.blacklist_symbols.contains(&symbol.to_uppercase()) {
                    debug!("⛔ Symbol {} is blacklisted, excluding from scan", symbol);
                    return None;
                }

                // ✅ MEAN REVERSION SCORING:
                // MODE 1: "STABLE" (Default) - Prefer Stable Coins (SOL, BTC)
                // Formula: turnover / (|change| + 1) -> Penalizes volatility
                
                // MODE 2: "VOLATILE" (Mid-Caps) - Prefer Active Coins (RENDER, SUI)
                // Formula: turnover * (|change|) -> Rewards volatility
                // But filter out extreme pumps (>30%) to avoid suicide
                
                let score = if self.config.scanner_mode == "VOLATILE" {
                    // Mid-Cap Logic:
                    // 1. Must move at least 1.5% (otherwise it's dead)
                    // 2. Must not move more than 30% (otherwise it's a dangerous pump)
                    let abs_change = price_change_24h.abs();
                    
                    if abs_change < 0.015 {
                         0.0 // Too stable (Dead)
                    } else if abs_change > 0.30 {
                         0.0 // Too volatile (Dangerous Pump)
                    } else {
                         // ✅ BELL CURVE SCORING:
                         // Reward volatility up to 10% (0.10).
                         // Penalize volatility above 10%.
                         // This pushes RIVER (e.g. 25%) down, and RENDER (e.g. 5%) up.
                         
                         let volatility_factor = if abs_change > 0.10 {
                             // Penalty zone: 10% -> 30%
                             // Higher change = Lower score
                             0.10 - (abs_change - 0.10) * 2.0 
                         } else {
                             // Reward zone: 1.5% -> 10%
                             abs_change
                         };
                         
                         // Ensure non-negative score
                         let effective_volatility = volatility_factor.max(0.001);
                         
                         turnover_24h * effective_volatility
                    }
                } else {
                    // Stable Logic (Old default):
                    // Penalize volatility. We want liquid coins that don't move much.
                    turnover_24h / (price_change_24h.abs() + 1.0)
                };

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

        // ✅ DEBUG LOGGING: Show top 5 candidates to understand selection logic
        info!("🔍 SCANNER REPORT (Mode: {})", self.config.scanner_mode);
        for (i, coin) in candidates.iter().take(5).enumerate() {
            info!(
                "   #{}: {} | Score: {:.0} | Volatility: {:+.2}% | Vol: ${:.0}M",
                i + 1,
                coin.symbol,
                coin.score,
                coin.price_change_24h * 100.0,
                coin.turnover_24h / 1_000_000.0
            );
        }

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

            // ✅ FIX RECONNECT: Always send messages on first scan (even if same symbol)
            // This ensures WebSocket resubscribes after reconnect
            let should_notify = should_switch || self.first_scan;

            if should_notify {
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

                if should_switch {
                    info!(
                        "🔄 Switching to new coin: {} (score: {:.2e} -> {:.2e})",
                        top_coin.symbol, self.current_score, top_coin.score
                    );

                    self.current_symbol = Some(Symbol(top_coin.symbol.clone()));
                    self.current_score = top_coin.score;

                    // Send switch command to MarketDataActor (only on actual switch)
                    if let Err(e) = self
                        .market_data_tx
                        .send(MarketDataMessage::SwitchSymbol(Symbol(
                            top_coin.symbol.clone(),
                        )))
                        .await
                    {
                        error!("Failed to send symbol switch message: {}", e);
                    }
                } else if self.first_scan {
                    // First scan after start - same symbol but need to notify
                    info!("✅ Current coin {} confirmed optimal (first scan after start)",
                        self.current_symbol.as_ref().unwrap());

                    // Still send SwitchSymbol to ensure WebSocket subscribes
                    if let Err(e) = self
                        .market_data_tx
                        .send(MarketDataMessage::SwitchSymbol(Symbol(
                            top_coin.symbol.clone(),
                        )))
                        .await
                    {
                        error!("Failed to send symbol switch message: {}", e);
                    }
                }

                // Always send specs to StrategyEngine when notifying
                if let Err(e) = self
                    .strategy_tx
                    .send(StrategyMessage::SymbolChanged {
                        symbol: Symbol(top_coin.symbol.clone()),
                        specs,
                        price_change_24h: top_coin.price_change_24h, // Pass 24h change for trend protection
                    })
                    .await
                {
                    error!("Failed to send symbol specs to strategy: {}", e);
                }

                // Clear first_scan flag
                self.first_scan = false;
            } else {
                info!("✅ Current coin {} still optimal", self.current_symbol.as_ref().unwrap());
                
                // ✅ FIX HARMONY: Even if symbol is same, update stats (price_change_24h)
                // This prevents "Silent Pump" bug where strategy keeps Mean Reversion mode
                // while coin pumps +20% during the session!
                if let Some(ref current) = self.current_symbol {
                    if let Err(e) = self
                        .strategy_tx
                        .send(StrategyMessage::UpdateMarketStats {
                            symbol: current.clone(),
                            price_change_24h: top_coin.price_change_24h,
                        })
                        .await
                    {
                         // Don't error log - channel might be full, not critical if one update is missed
                         debug!("Failed to send market stats update: {}", e);
                    }
                }
            }
        } else {
            warn!("⚠️  No suitable coins found in scan");
        }

        Ok(())
    }

    /// ✅ MEAN REVERSION: Use fixed trading symbol (skip scanning)
    async fn use_fixed_symbol(&mut self, symbol: String) -> Result<()> {
        // Only send on first scan or if symbol changed
        let should_notify = self.first_scan 
            || self.current_symbol.as_ref().map(|s| &s.0) != Some(&symbol);

        if !should_notify {
            debug!("📌 Fixed symbol {} already active", symbol);
            return Ok(());
        }

        info!("📌 Using fixed trading symbol: {}", symbol);

        // Fetch instrument specs
        let specs = match self.client.get_instrument_info(&symbol).await {
            Ok(info) => {
                let specs = SymbolSpecs::from(info);
                self.specs_cache.insert(specs.clone());
                specs
            }
            Err(e) => {
                error!("Failed to fetch specs for {}: {}", symbol, e);
                self.specs_cache.get_or_default(&symbol)
            }
        };

        // Get 24h price change (default to 0 for neutral)
        let price_change_24h = self.client.get_tickers("linear").await
            .ok()
            .and_then(|t| t.list.iter()
                .find(|ticker| ticker.symbol == symbol)
                .and_then(|ticker| ticker.price_24h_pcnt.parse::<f64>().ok()))
            .unwrap_or(0.0);

        // Send switch command to MarketDataActor
        if let Err(e) = self.market_data_tx
            .send(MarketDataMessage::SwitchSymbol(Symbol(symbol.clone())))
            .await
        {
            error!("Failed to send symbol switch: {}", e);
        }

        // Send to StrategyEngine
        if let Err(e) = self.strategy_tx
            .send(StrategyMessage::SymbolChanged {
                symbol: Symbol(symbol.clone()),
                specs,
                price_change_24h,
            })
            .await
        {
            error!("Failed to send symbol specs: {}", e);
        }

        self.current_symbol = Some(Symbol(symbol));
        self.first_scan = false;
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

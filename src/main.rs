use anyhow::Result;
use bybit_scalper_bot::actors::*;
use bybit_scalper_bot::alerts::{Alert, AlertSender, TelegramAlerter};
use bybit_scalper_bot::config::Config;
use bybit_scalper_bot::exchange::BybitClient;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize structured logging
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact()
        .init();

    info!("🚀 Bybit Dynamic Scalper Bot - Initializing...");

    // Load configuration
    let config = Arc::new(Config::from_env()?);
    info!("✅ Configuration loaded");
    info!("   - API URL: {}", config.rest_api_url());
    info!("   - WebSocket: {}", config.ws_url());
    info!("   - Max Position: ${}", config.max_position_size_usd);
    info!("   - Stop Loss: {}%", config.stop_loss_percent);
    info!("   - Scan Interval: {}s", config.scan_interval_secs);

    // Setup Telegram alerts (optional)
    let alert_sender = if config.has_telegram() {
        info!("📨 Telegram alerts ENABLED");
        let (alert_tx, alert_rx) = mpsc::channel::<Alert>(100);
        let alerter = TelegramAlerter::new(
            config.telegram_bot_token.clone().unwrap(),
            config.telegram_chat_id.clone().unwrap(),
            alert_rx,
        );

        // Spawn alerter task
        tokio::spawn(async move {
            alerter.run().await;
        });

        Some(AlertSender::new(alert_tx))
    } else {
        info!("📨 Telegram alerts DISABLED (no credentials)");
        None
    };

    // Create Bybit client
    let client = BybitClient::new(
        config.bybit_api_key.clone(),
        config.bybit_api_secret.clone(),
        config.rest_api_url().to_string(),
    );

    // Actor Communication Channels
    // Scanner -> MarketData
    // ✅ FIXED: Increased from 32 to 256 to prevent deadlock
    let (market_data_cmd_tx, market_data_cmd_rx) = mpsc::channel(256);

    // MarketData -> Strategy
    let (strategy_tx, strategy_rx) = mpsc::channel(1000);

    // Strategy -> Execution
    let (execution_tx, execution_rx) = mpsc::channel(100);

    info!("🔧 Setting up Actor System...");

    // Initialize ScannerActor
    let scanner = scanner::ScannerActor::new(
        client.clone(),
        config.clone(),
        market_data_cmd_tx.clone(),
        strategy_tx.clone(),
    );

    // Initialize MarketDataActor
    let market_data = websocket::MarketDataActor::new(
        config.clone(),
        strategy_tx.clone(),
        market_data_cmd_rx,
    );

    // Initialize StrategyEngine
    let strategy = strategy::StrategyEngine::new(
        config.clone(),
        strategy_rx,
        execution_tx.clone(),
        alert_sender.clone(),
    );

    // Initialize ExecutionActor
    let execution = execution::ExecutionActor::new(
        client.clone(),
        config.clone(),
        execution_rx,
        strategy_tx.clone(),
        alert_sender.clone(),
    );

    info!("✅ All actors initialized");

    // Spawn actors as independent tasks
    let scanner_handle = tokio::spawn(async move {
        scanner.run().await;
    });

    let market_data_handle = tokio::spawn(async move {
        market_data.run().await;
    });

    let strategy_handle = tokio::spawn(async move {
        strategy.run().await;
    });

    let execution_handle = tokio::spawn(async move {
        execution.run().await;
    });

    info!("🎯 Bot is now LIVE and hunting for opportunities!");
    info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // Setup graceful shutdown
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        info!("🛑 Shutdown signal received, stopping bot...");
        std::process::exit(0);
    });

    // Wait for all actors (they should run indefinitely)
    let results = tokio::try_join!(
        scanner_handle,
        market_data_handle,
        strategy_handle,
        execution_handle
    );

    if let Err(e) = results {
        error!("Actor task failed: {}", e);
    }

    info!("Bot terminated");
    Ok(())
}

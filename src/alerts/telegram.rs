use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// Telegram alert types
#[derive(Debug, Clone)]
pub enum AlertLevel {
    Info,     // 💡 Informational
    Success,  // ✅ Positive events
    Warning,  // ⚠️ Important but non-critical
    Error,    // 🔴 Critical errors
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub level: AlertLevel,
    pub title: String,
    pub message: String,
}

impl Alert {
    pub fn info(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: AlertLevel::Info,
            title: title.into(),
            message: message.into(),
        }
    }

    pub fn success(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: AlertLevel::Success,
            title: title.into(),
            message: message.into(),
        }
    }

    pub fn warning(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: AlertLevel::Warning,
            title: title.into(),
            message: message.into(),
        }
    }

    pub fn error(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: AlertLevel::Error,
            title: title.into(),
            message: message.into(),
        }
    }

    fn format_telegram(&self) -> String {
        let emoji = match self.level {
            AlertLevel::Info => "💡",
            AlertLevel::Success => "✅",
            AlertLevel::Warning => "⚠️",
            AlertLevel::Error => "🔴",
        };

        format!("{} <b>{}</b>\n\n{}", emoji, self.title, self.message)
    }
}

/// Telegram alerter - sends alerts to Telegram chat
pub struct TelegramAlerter {
    bot_token: String,
    chat_id: String,
    client: Client,
    alert_rx: mpsc::Receiver<Alert>,
}

impl TelegramAlerter {
    pub fn new(
        bot_token: String,
        chat_id: String,
        alert_rx: mpsc::Receiver<Alert>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create Telegram HTTP client");

        Self {
            bot_token,
            chat_id,
            client,
            alert_rx,
        }
    }

    /// Run the alerter loop
    pub async fn run(mut self) {
        debug!("📨 TelegramAlerter started");

        // Send startup notification
        let startup_alert = Alert::info(
            "Bot Started",
            format!("Bybit Scalper Bot is now running\nEnvironment: {}",
                if cfg!(debug_assertions) { "Debug" } else { "Release" }
            ),
        );

        if let Err(e) = self.send_alert(&startup_alert).await {
            error!("Failed to send startup alert: {}", e);
        }

        // Process alerts
        while let Some(alert) = self.alert_rx.recv().await {
            if let Err(e) = self.send_alert(&alert).await {
                error!("Failed to send Telegram alert: {}", e);
            }
        }

        debug!("TelegramAlerter shutting down");
    }

    async fn send_alert(&self, alert: &Alert) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let payload = json!({
            "chat_id": self.chat_id,
            "text": alert.format_telegram(),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });

        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await?;

        if response.status().is_success() {
            debug!("Telegram alert sent: {}", alert.title);
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            warn!("Telegram API error {}: {}", status, body);
            anyhow::bail!("Telegram API error: {}", status)
        }
    }
}

/// Alert channel sender - use this to send alerts from anywhere in the codebase
#[derive(Clone)]
pub struct AlertSender {
    tx: mpsc::Sender<Alert>,
}

impl AlertSender {
    pub fn new(tx: mpsc::Sender<Alert>) -> Self {
        Self { tx }
    }

    /// Send an alert (non-blocking, drops if channel full)
    pub fn send(&self, alert: Alert) {
        if let Err(e) = self.tx.try_send(alert) {
            error!("Failed to send alert (channel full): {}", e);
        }
    }

    /// Send info alert
    pub fn info(&self, title: impl Into<String>, message: impl Into<String>) {
        self.send(Alert::info(title, message));
    }

    /// Send success alert
    pub fn success(&self, title: impl Into<String>, message: impl Into<String>) {
        self.send(Alert::success(title, message));
    }

    /// Send warning alert
    pub fn warning(&self, title: impl Into<String>, message: impl Into<String>) {
        self.send(Alert::warning(title, message));
    }

    /// Send error alert
    pub fn error(&self, title: impl Into<String>, message: impl Into<String>) {
        self.send(Alert::error(title, message));
    }
}

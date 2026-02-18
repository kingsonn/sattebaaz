use crate::config::TelemetryConfig;
use anyhow::Result;
use tracing::{error, info};

/// Sends alerts via Telegram or Discord webhooks.
pub struct AlertManager {
    config: TelemetryConfig,
    http: reqwest::Client,
}

impl AlertManager {
    pub fn new(config: TelemetryConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    /// Send an alert message.
    pub async fn send(&self, message: &str) {
        info!("ALERT: {message}");

        if let Err(e) = self.send_telegram(message).await {
            error!("Telegram alert failed: {e}");
        }

        if let Err(e) = self.send_discord(message).await {
            error!("Discord alert failed: {e}");
        }
    }

    /// Send alert to Telegram.
    async fn send_telegram(&self, message: &str) -> Result<()> {
        let (Some(token), Some(chat_id)) = (&self.config.telegram_bot_token, &self.config.telegram_chat_id) else {
            return Ok(()); // Not configured
        };

        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": format!("🎰 SATTEBAAZ: {message}"),
            "parse_mode": "Markdown"
        });

        self.http.post(&url).json(&body).send().await?;
        Ok(())
    }

    /// Send alert to Discord.
    async fn send_discord(&self, message: &str) -> Result<()> {
        let Some(webhook_url) = &self.config.discord_webhook_url else {
            return Ok(());
        };

        let body = serde_json::json!({
            "content": format!("🎰 **SATTEBAAZ**: {message}")
        });

        self.http.post(webhook_url).json(&body).send().await?;
        Ok(())
    }

    /// Alert on trade execution.
    pub async fn on_trade(&self, summary: &str) {
        if self.config.alert_on_trade {
            self.send(&format!("Trade: {summary}")).await;
        }
    }

    /// Alert on error.
    pub async fn on_error(&self, error: &str) {
        if self.config.alert_on_error {
            self.send(&format!("⚠️ Error: {error}")).await;
        }
    }

    /// Alert on drawdown.
    pub async fn on_drawdown(&self, pct: f64) {
        if self.config.alert_on_drawdown {
            self.send(&format!("🔴 Drawdown: {pct:.1}%")).await;
        }
    }
}

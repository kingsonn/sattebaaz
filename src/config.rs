use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub binance: BinanceConfig,
    pub strategy: StrategyConfig,
    pub risk: RiskConfig,
    pub telemetry: TelemetryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub clob_host: String,
    pub ws_host: String,
    pub gamma_api_host: String,
    pub chain_id: u64,
    pub private_key: String,
    pub funder_address: Option<String>,
    pub signature_type: u8, // 0 = EOA, 1 = Poly Proxy
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceConfig {
    pub ws_url: String,
    pub rest_url: String,
    pub streams: Vec<String>, // e.g. ["btcusdt@trade", "btcusdt@kline_1m"]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub straddle_enabled: bool,
    pub arb_enabled: bool,
    pub lag_exploit_enabled: bool,
    pub market_making_enabled: bool,
    pub momentum_enabled: bool,

    pub straddle_max_combined: f64,   // Max YES+NO sum to enter straddle (e.g. 0.97)
    pub straddle_max_capital_pct: f64, // Max % of capital per straddle (e.g. 0.25)
    pub bias_min_confidence: f64,      // Min confidence to amplify (e.g. 0.35)
    pub bias_max_capital_pct: f64,     // Max % on directional bet (e.g. 0.15)

    pub arb_min_edge: f64,            // Minimum edge in dollars (e.g. 0.02)
    pub arb_min_expected_profit: f64, // Minimum expected profit (e.g. 0.10)

    pub lag_min_edge: f64,            // Minimum mispricing to exploit (e.g. 0.03)
    pub lag_kelly_fraction: f64,      // Fractional Kelly (e.g. 0.25)

    pub mm_base_size_pct: f64,        // Base quote size as % of capital (e.g. 0.10)

    pub momentum_min_signal: f64,     // Min momentum to trade (e.g. 0.003)
    pub momentum_min_divergence: f64, // Min divergence (e.g. 0.02)

    pub lockout_seconds_5m: f64,      // Stop trading N seconds before resolution (e.g. 30)
    pub lockout_seconds_15m: f64,     // (e.g. 30)

    pub capital_allocation: CapitalAllocation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapitalAllocation {
    pub btc_5m_pct: f64,
    pub btc_15m_pct: f64,
    pub eth_15m_pct: f64,
    pub sol_15m_pct: f64,
    pub xrp_15m_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_exposure_pct: f64,        // Max total exposure as % of capital (e.g. 0.50)
    pub max_daily_loss_pct: f64,      // Pause if daily loss exceeds this (e.g. 0.10)
    pub loss_streak_threshold: u32,   // Consecutive losses to trigger size reduction
    pub loss_streak_size_mult: f64,   // Size multiplier during streak (e.g. 0.50)
    pub max_price_deviation: f64,     // Reject orders deviating >X from midpoint
    pub pause_duration_secs: u64,     // Pause duration after drawdown (e.g. 3600)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub log_level: String,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub discord_webhook_url: Option<String>,
    pub alert_on_trade: bool,
    pub alert_on_error: bool,
    pub alert_on_drawdown: bool,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            straddle_enabled: true,
            arb_enabled: true,
            lag_exploit_enabled: true,
            market_making_enabled: true,
            momentum_enabled: true,
            straddle_max_combined: 0.97,
            straddle_max_capital_pct: 0.25,
            bias_min_confidence: 0.35,
            bias_max_capital_pct: 0.15,
            arb_min_edge: 0.02,
            arb_min_expected_profit: 0.10,
            lag_min_edge: 0.03,
            lag_kelly_fraction: 0.25,
            mm_base_size_pct: 0.10,
            momentum_min_signal: 0.003,
            momentum_min_divergence: 0.02,
            lockout_seconds_5m: 30.0,
            lockout_seconds_15m: 30.0,
            capital_allocation: CapitalAllocation::default(),
        }
    }
}

impl Default for CapitalAllocation {
    fn default() -> Self {
        Self {
            btc_5m_pct: 0.40,
            btc_15m_pct: 0.20,
            eth_15m_pct: 0.20,
            sol_15m_pct: 0.10,
            xrp_15m_pct: 0.10,
        }
    }
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_exposure_pct: 0.50,
            max_daily_loss_pct: 0.10,
            loss_streak_threshold: 5,
            loss_streak_size_mult: 0.50,
            max_price_deviation: 0.15,
            pause_duration_secs: 3600,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                clob_host: "https://clob.polymarket.com".into(),
                ws_host: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
                gamma_api_host: "https://gamma-api.polymarket.com".into(),
                chain_id: 137,
                private_key: String::new(),
                funder_address: None,
                signature_type: 0,
            },
            binance: BinanceConfig {
                ws_url: "wss://fstream.binance.com".into(),
                rest_url: "https://fapi.binance.com".into(),
                streams: vec![
                    "btcusdt@aggTrade".into(),
                    "btcusdt@kline_1m".into(),
                    "ethusdt@aggTrade".into(),
                    "ethusdt@kline_1m".into(),
                    "solusdt@aggTrade".into(),
                    "solusdt@kline_1m".into(),
                    "xrpusdt@aggTrade".into(),
                    "xrpusdt@kline_1m".into(),
                    "btcusdt@forceOrder".into(),
                    "ethusdt@forceOrder".into(),
                    "solusdt@forceOrder".into(),
                    "xrpusdt@forceOrder".into(),
                ],
            },
            strategy: StrategyConfig::default(),
            risk: RiskConfig::default(),
            telemetry: TelemetryConfig {
                log_level: "info".into(),
                telegram_bot_token: None,
                telegram_chat_id: None,
                discord_webhook_url: None,
                alert_on_trade: true,
                alert_on_error: true,
                alert_on_drawdown: true,
            },
        }
    }
}

impl Config {
    /// Load configuration from environment variables (.env file) with defaults.
    ///
    /// Required env vars:
    ///   POLYMARKET_PRIVATE_KEY — hex private key for signing
    ///   STARTING_CAPITAL — initial USDC balance (default: 5)
    ///
    /// Optional env vars:
    ///   POLYMARKET_FUNDER_ADDRESS — proxy wallet address
    ///   POLYMARKET_SIGNATURE_TYPE — 0=EOA, 1=PolyProxy (default: 0)
    ///   TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID — for alerts
    ///   DISCORD_WEBHOOK_URL — for alerts
    ///   RUST_LOG — log level (default: info)
    ///   DRY_RUN — set to "true" to use random key (no real orders)
    pub fn load_or_default() -> Self {
        // Load .env file if present
        let _ = dotenv::dotenv();

        let mut config = Self::default();

        // Polymarket credentials
        if let Ok(key) = std::env::var("POLYMARKET_PRIVATE_KEY") {
            if key != "your_private_key_here" {
                config.polymarket.private_key = key;
            }
        }

        if let Ok(addr) = std::env::var("POLYMARKET_FUNDER_ADDRESS") {
            if !addr.is_empty() && addr != "optional_proxy_address" {
                config.polymarket.funder_address = Some(addr);
            }
        }

        if let Ok(sig_type) = std::env::var("POLYMARKET_SIGNATURE_TYPE") {
            config.polymarket.signature_type = sig_type.parse().unwrap_or(0);
        }

        // Starting capital
        if let Ok(capital) = std::env::var("STARTING_CAPITAL") {
            if let Ok(_val) = capital.parse::<f64>() {
                // Stored in config for PositionManager initialization
                // (passed through main.rs)
            }
        }

        // Telegram alerts
        if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
            if !token.is_empty() && token != "your_bot_token" {
                config.telemetry.telegram_bot_token = Some(token);
            }
        }
        if let Ok(chat) = std::env::var("TELEGRAM_CHAT_ID") {
            if !chat.is_empty() && chat != "your_chat_id" {
                config.telemetry.telegram_chat_id = Some(chat);
            }
        }

        // Discord alerts
        if let Ok(url) = std::env::var("DISCORD_WEBHOOK_URL") {
            if !url.is_empty() && url != "your_webhook_url" {
                config.telemetry.discord_webhook_url = Some(url);
            }
        }

        // Log level
        if let Ok(level) = std::env::var("RUST_LOG") {
            config.telemetry.log_level = level;
        }

        // Dry run mode — use random key if no real key provided
        let dry_run = std::env::var("DRY_RUN")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        if config.polymarket.private_key.is_empty() && !dry_run {
            tracing::warn!("No POLYMARKET_PRIVATE_KEY set — entering DRY RUN mode");
            tracing::warn!("Orders will be signed with a random key and will fail on CLOB");
        }

        config
    }

    /// Get starting capital from env, defaulting to 5.0 USDC.
    pub fn starting_capital() -> f64 {
        std::env::var("STARTING_CAPITAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5.0)
    }

    /// Check if running in dry-run mode (no real key).
    pub fn is_dry_run(&self) -> bool {
        self.polymarket.private_key.is_empty()
            || std::env::var("DRY_RUN")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.is_dry_run() {
            tracing::info!("Dry-run mode — skipping private key validation");
        } else {
            anyhow::ensure!(
                !self.polymarket.private_key.is_empty(),
                "POLYMARKET_PRIVATE_KEY must be set (or set DRY_RUN=true)"
            );
        }
        anyhow::ensure!(
            self.risk.max_exposure_pct > 0.0 && self.risk.max_exposure_pct <= 1.0,
            "max_exposure_pct must be between 0 and 1"
        );
        let alloc = &self.strategy.capital_allocation;
        let total = alloc.btc_5m_pct + alloc.btc_15m_pct + alloc.eth_15m_pct
            + alloc.sol_15m_pct + alloc.xrp_15m_pct;
        anyhow::ensure!(
            (total - 1.0).abs() < 0.01,
            "Capital allocation must sum to 1.0, got {total}"
        );
        Ok(())
    }
}

use crate::config::BinanceConfig;
use crate::models::market::Asset;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};

/// Real-time Binance futures data feed.
///
/// Connects to Binance WebSocket for:
///   - Aggregate trades (price updates every ~100ms)
///   - Forced liquidations (for cascade detection)
pub struct BinanceFeed {
    config: BinanceConfig,
    /// Latest prices per asset, updated on every aggTrade
    pub prices: Arc<RwLock<HashMap<Asset, PriceState>>>,
    /// Latest funding rates per asset
    pub funding_rates: Arc<RwLock<HashMap<Asset, f64>>>,
    /// Net liquidations per asset over rolling 60s window (positive = longs liquidated)
    pub net_liquidations: Arc<RwLock<HashMap<Asset, f64>>>,
    /// Price update broadcast (asset, price) for downstream consumers
    pub price_tx: broadcast::Sender<(Asset, f64)>,
}

#[derive(Debug, Clone, Copy)]
pub struct PriceState {
    pub price: f64,
    pub price_1s_ago: f64,
    pub timestamp: DateTime<Utc>,
    last_1s_update: i64, // unix millis of last 1s snapshot
}

impl PriceState {
    pub fn move_pct_1s(&self) -> f64 {
        if self.price_1s_ago == 0.0 {
            return 0.0;
        }
        (self.price - self.price_1s_ago) / self.price_1s_ago
    }
}

impl BinanceFeed {
    pub fn new(config: BinanceConfig) -> Self {
        let (price_tx, _) = broadcast::channel(1024);
        Self {
            config,
            prices: Arc::new(RwLock::new(HashMap::new())),
            funding_rates: Arc::new(RwLock::new(HashMap::new())),
            net_liquidations: Arc::new(RwLock::new(HashMap::new())),
            price_tx,
        }
    }

    /// Start the WebSocket feed. Spawns a background reconnecting task.
    pub fn start(&self, mut shutdown: broadcast::Receiver<()>) {
        let streams: Vec<String> = self.config.streams.clone();
        let ws_base = self.config.ws_url.clone();
        let prices = self.prices.clone();
        let net_liqs = self.net_liquidations.clone();
        let price_tx = self.price_tx.clone();

        tokio::spawn(async move {
            let combined = streams.join("/");
            let ws_url = format!("{}/stream?streams={}", ws_base, combined);
            let mut backoff_ms: u64 = 500;

            loop {
                info!("Connecting to Binance WS: {ws_url}");

                let conn = tokio::select! {
                    result = connect_async(&ws_url) => result,
                    _ = shutdown.recv() => {
                        info!("Binance feed shutdown");
                        return;
                    }
                };

                match conn {
                    Ok((ws_stream, _)) => {
                        info!("Binance WS connected");
                        backoff_ms = 500; // Reset backoff on success

                        let (_, mut read) = ws_stream.split();

                        loop {
                            let msg = tokio::select! {
                                msg = read.next() => msg,
                                _ = shutdown.recv() => {
                                    info!("Binance feed shutdown");
                                    return;
                                }
                            };

                            match msg {
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                    Self::handle_message(
                                        &text,
                                        &prices,
                                        &net_liqs,
                                        &price_tx,
                                    )
                                    .await;
                                }
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_))) => {
                                    debug!("Binance ping");
                                }
                                Some(Ok(_)) => {} // Binary, Pong, Close, Frame
                                Some(Err(e)) => {
                                    warn!("Binance WS error: {e}");
                                    break; // Reconnect
                                }
                                None => {
                                    warn!("Binance WS stream ended");
                                    break; // Reconnect
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Binance WS connection failed: {e}");
                    }
                }

                // Exponential backoff reconnect
                warn!("Reconnecting in {backoff_ms}ms...");
                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        });
    }

    /// Parse and route a combined stream message.
    async fn handle_message(
        text: &str,
        prices: &Arc<RwLock<HashMap<Asset, PriceState>>>,
        net_liqs: &Arc<RwLock<HashMap<Asset, f64>>>,
        price_tx: &broadcast::Sender<(Asset, f64)>,
    ) {
        // Binance combined stream wraps in {"stream":"...", "data":{...}}
        let envelope: CombinedStreamMsg = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        let stream = &envelope.stream;

        if stream.ends_with("@aggTrade") {
            if let Ok(trade) = serde_json::from_value::<AggTradeMsg>(envelope.data) {
                Self::on_agg_trade(trade, prices, price_tx).await;
            }
        } else if stream.contains("@forceOrder") {
            if let Ok(fo) = serde_json::from_value::<ForceOrderWrapper>(envelope.data) {
                Self::on_force_order(fo.o, net_liqs).await;
            }
        }
        // kline messages can be added later
    }

    /// Process an aggregate trade update.
    async fn on_agg_trade(
        trade: AggTradeMsg,
        prices: &Arc<RwLock<HashMap<Asset, PriceState>>>,
        price_tx: &broadcast::Sender<(Asset, f64)>,
    ) {
        let asset = match Self::symbol_to_asset(&trade.symbol) {
            Some(a) => a,
            None => return,
        };

        let price: f64 = match trade.price.parse() {
            Ok(p) => p,
            Err(_) => return,
        };

        let now = Utc::now();
        let now_ms = now.timestamp_millis();

        let mut map = prices.write().await;
        let state = map.entry(asset).or_insert(PriceState {
            price,
            price_1s_ago: price,
            timestamp: now,
            last_1s_update: now_ms,
        });

        // Update 1-second ago snapshot every 1000ms
        if now_ms - state.last_1s_update >= 1000 {
            state.price_1s_ago = state.price;
            state.last_1s_update = now_ms;
        }

        state.price = price;
        state.timestamp = now;
        drop(map);

        // Broadcast to downstream consumers (non-blocking, ignore if no receivers)
        let _ = price_tx.send((asset, price));
    }

    /// Process a forced liquidation event.
    async fn on_force_order(
        order: ForceOrderData,
        net_liqs: &Arc<RwLock<HashMap<Asset, f64>>>,
    ) {
        let asset = match Self::symbol_to_asset(&order.symbol) {
            Some(a) => a,
            None => return,
        };

        let qty: f64 = order.quantity.parse().unwrap_or(0.0);
        let price: f64 = order.price.parse().unwrap_or(0.0);
        let notional = qty * price;

        // side=SELL means long was liquidated (bearish), side=BUY means short liquidated (bullish)
        let signed = if order.side == "SELL" {
            notional // Longs liquidated = positive
        } else {
            -notional // Shorts liquidated = negative
        };

        let mut map = net_liqs.write().await;
        *map.entry(asset).or_insert(0.0) += signed;

        debug!(
            "Liquidation: {:?} {} ${:.0} (net={:.0})",
            asset,
            order.side,
            notional,
            map.get(&asset).unwrap_or(&0.0)
        );
    }

    /// Get current price for an asset.
    pub async fn get_price(&self, asset: Asset) -> Option<f64> {
        self.prices.read().await.get(&asset).map(|s| s.price)
    }

    /// Get 1-second price move percentage for an asset.
    pub async fn get_1s_move_pct(&self, asset: Asset) -> f64 {
        self.prices
            .read()
            .await
            .get(&asset)
            .map(|s| s.move_pct_1s())
            .unwrap_or(0.0)
    }

    /// Get current funding rate for an asset.
    pub async fn get_funding_rate(&self, asset: Asset) -> f64 {
        self.funding_rates
            .read()
            .await
            .get(&asset)
            .copied()
            .unwrap_or(0.0)
    }

    /// Get net liquidations for an asset (positive = longs liquidated = bearish).
    pub async fn get_net_liquidations(&self, asset: Asset) -> f64 {
        self.net_liquidations
            .read()
            .await
            .get(&asset)
            .copied()
            .unwrap_or(0.0)
    }

    /// Reset liquidation accumulator (call periodically, e.g. every 60s).
    pub async fn reset_liquidations(&self) {
        let mut map = self.net_liquidations.write().await;
        for v in map.values_mut() {
            *v *= 0.5; // Decay rather than reset, so recent liqs still have weight
        }
    }

    /// Subscribe to price updates.
    pub fn subscribe_prices(&self) -> broadcast::Receiver<(Asset, f64)> {
        self.price_tx.subscribe()
    }

    /// Start periodic funding rate polling from Binance REST API (every 60s).
    pub fn start_funding_poller(&self, mut shutdown: broadcast::Receiver<()>) {
        let funding = self.funding_rates.clone();

        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let symbols = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT"];
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        for symbol in &symbols {
                            let url = format!(
                                "https://fapi.binance.com/fapi/v1/premiumIndex?symbol={}",
                                symbol
                            );
                            match http.get(&url).send().await {
                                Ok(resp) => {
                                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                                        if let Some(rate_str) = data["lastFundingRate"].as_str() {
                                            if let Ok(rate) = rate_str.parse::<f64>() {
                                                if let Some(asset) = Self::symbol_to_asset(symbol) {
                                                    funding.write().await.insert(asset, rate);
                                                    debug!("Funding rate {:?}: {:.6}", asset, rate);
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    debug!("Funding rate fetch failed for {symbol}: {e}");
                                }
                            }
                        }
                    }
                    _ = shutdown.recv() => break,
                }
            }
        });
    }

    /// Map Binance symbol to our Asset enum.
    pub fn symbol_to_asset(symbol: &str) -> Option<Asset> {
        match symbol.to_uppercase().as_str() {
            "BTCUSDT" => Some(Asset::BTC),
            "ETHUSDT" => Some(Asset::ETH),
            "SOLUSDT" => Some(Asset::SOL),
            "XRPUSDT" => Some(Asset::XRP),
            _ => None,
        }
    }
}

// --- Binance message types ---

#[derive(Debug, Deserialize)]
struct CombinedStreamMsg {
    stream: String,
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AggTradeMsg {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "m")]
    is_buyer_maker: bool,
    #[serde(rename = "E")]
    event_time: u64,
}

#[derive(Debug, Deserialize)]
struct ForceOrderWrapper {
    o: ForceOrderData,
}

#[derive(Debug, Deserialize)]
struct ForceOrderData {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "p")]
    price: String,
}

use crate::config::PolymarketConfig;
use crate::feeds::market_discovery::MarketDiscovery;
use crate::models::market::{Asset, Duration, Market, OrderBook};
use anyhow::Result;
use dashmap::DashMap;
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};

/// Polymarket CLOB data feed.
///
/// Connects to:
///   - REST API for order book snapshots and market discovery
///   - WebSocket for real-time book/trade updates
pub struct PolymarketFeed {
    config: PolymarketConfig,
    /// Order books indexed by token_id
    pub books: Arc<DashMap<String, OrderBook>>,
    /// Active markets indexed by market slug
    pub markets: Arc<DashMap<String, Market>>,
    /// Token IDs we're subscribed to
    pub subscribed_tokens: Arc<DashMap<String, ()>>,
    /// Book update broadcast: (token_id) notifying downstream that a book changed
    pub book_update_tx: broadcast::Sender<String>,
    http_client: reqwest::Client,
    /// Optional filter: only discover these market types. None = all.
    market_filter: Option<Vec<(Asset, Duration)>>,
}

impl PolymarketFeed {
    pub fn new(config: PolymarketConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let (book_update_tx, _) = broadcast::channel(512);

        Self {
            config,
            books: Arc::new(DashMap::new()),
            markets: Arc::new(DashMap::new()),
            subscribed_tokens: Arc::new(DashMap::new()),
            book_update_tx,
            http_client,
            market_filter: None,
        }
    }

    /// Restrict market discovery to specific asset/duration pairs.
    pub fn set_market_filter(&mut self, filter: Vec<(Asset, Duration)>) {
        self.market_filter = Some(filter);
    }

    /// Start the data feed. Spawns:
    ///   1. Market discovery loop (every 5s)
    ///   2. WebSocket connection for real-time book updates
    ///   3. Book refresh loop (every 2s for active tokens)
    pub fn start(&self, shutdown_tx: &broadcast::Sender<()>) {
        info!("Starting Polymarket feed...");

        // Spawn market discovery loop
        self.spawn_market_discovery(shutdown_tx.subscribe());

        // Spawn WebSocket book feed
        self.spawn_ws_feed(shutdown_tx.subscribe());

        // Spawn periodic book refresh
        self.spawn_book_refresh(shutdown_tx.subscribe());
    }

    /// Spawn market discovery: discovers new markets every 5 seconds.
    fn spawn_market_discovery(&self, mut shutdown: broadcast::Receiver<()>) {
        let http = self.http_client.clone();
        let config = self.config.clone();
        let markets = self.markets.clone();
        let books = self.books.clone();
        let subscribed = self.subscribed_tokens.clone();
        let market_types = self.market_filter.clone()
            .unwrap_or_else(MarketDiscovery::all_market_types);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Generate slugs for configured market types (current + next)
                        for (asset, duration) in market_types.iter().copied() {
                            let slugs = MarketDiscovery::upcoming_slugs(asset, duration, 2);
                            for slug in slugs {
                                // Skip if already tracked
                                if markets.contains_key(&slug) {
                                    continue;
                                }

                                // Try to resolve via Gamma API
                                match Self::resolve_market(
                                    &http, &config.gamma_api_host, &slug, asset, duration,
                                ).await {
                                    Ok(Some(market)) => {
                                        info!(
                                            "Discovered market: {} (YES={}, NO={})",
                                            slug,
                                            &market.yes_token_id[..8.min(market.yes_token_id.len())],
                                            &market.no_token_id[..8.min(market.no_token_id.len())],
                                        );

                                        // Pre-fetch books
                                        for token_id in [&market.yes_token_id, &market.no_token_id] {
                                            if let Ok(book) = Self::fetch_book_static(
                                                &http, &config.clob_host, token_id,
                                            ).await {
                                                books.insert(token_id.clone(), book);
                                                subscribed.insert(token_id.clone(), ());
                                            }
                                        }

                                        markets.insert(slug.clone(), market);
                                    }
                                    Ok(None) => {
                                        debug!("Market not yet available: {slug}");
                                    }
                                    Err(e) => {
                                        debug!("Market resolution failed for {slug}: {e}");
                                    }
                                }
                            }

                            // Clean up expired markets
                            let expired: Vec<String> = markets
                                .iter()
                                .filter(|entry| entry.value().time_remaining_secs() < -60.0)
                                .map(|entry| entry.key().clone())
                                .collect();

                            for slug in expired {
                                if let Some((_, market)) = markets.remove(&slug) {
                                    books.remove(&market.yes_token_id);
                                    books.remove(&market.no_token_id);
                                    subscribed.remove(&market.yes_token_id);
                                    subscribed.remove(&market.no_token_id);
                                    debug!("Cleaned up expired market: {slug}");
                                }
                            }
                        }
                    }
                    _ = shutdown.recv() => break,
                }
            }
        });
    }

    /// Spawn WebSocket feed for real-time book updates.
    fn spawn_ws_feed(&self, mut shutdown: broadcast::Receiver<()>) {
        let ws_host = self.config.ws_host.clone();
        let books = self.books.clone();
        let subscribed = self.subscribed_tokens.clone();
        let book_tx = self.book_update_tx.clone();

        tokio::spawn(async move {
            let mut backoff_ms: u64 = 500;

            loop {
                info!("Connecting to Polymarket WS: {ws_host}");

                let conn = tokio::select! {
                    result = connect_async(&ws_host) => result,
                    _ = shutdown.recv() => {
                        info!("Polymarket WS shutdown");
                        return;
                    }
                };

                match conn {
                    Ok((ws_stream, _)) => {
                        info!("Polymarket WS connected");
                        backoff_ms = 500;

                        let (mut write, mut read) = ws_stream.split();

                        // Subscribe to all tracked tokens
                        let tokens: Vec<String> = subscribed
                            .iter()
                            .map(|e| e.key().clone())
                            .collect();

                        for token_id in &tokens {
                            let sub_msg = serde_json::json!({
                                "auth": {},
                                "type": "subscribe",
                                "channel": "market",
                                "assets_ids": [token_id]
                            });
                            if let Ok(msg_str) = serde_json::to_string(&sub_msg) {
                                use futures_util::SinkExt;
                                let _ = write.send(
                                    tokio_tungstenite::tungstenite::Message::Text(msg_str)
                                ).await;
                            }
                        }

                        if !tokens.is_empty() {
                            info!("Subscribed to {} token books", tokens.len());
                        }

                        // Read loop
                        loop {
                            let msg = tokio::select! {
                                msg = read.next() => msg,
                                _ = shutdown.recv() => {
                                    info!("Polymarket WS shutdown");
                                    return;
                                }
                            };

                            match msg {
                                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                    Self::handle_ws_message(&text, &books, &book_tx);
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    warn!("Polymarket WS error: {e}");
                                    break;
                                }
                                None => {
                                    warn!("Polymarket WS stream ended");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Polymarket WS connection failed: {e}");
                    }
                }

                warn!("Polymarket WS reconnecting in {backoff_ms}ms...");
                tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        });
    }

    /// Spawn periodic book refresh via REST (fallback for WS gaps).
    fn spawn_book_refresh(&self, mut shutdown: broadcast::Receiver<()>) {
        let http = self.http_client.clone();
        let clob_host = self.config.clob_host.clone();
        let books = self.books.clone();
        let subscribed = self.subscribed_tokens.clone();
        let book_tx = self.book_update_tx.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(2));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let tokens: Vec<String> = subscribed
                            .iter()
                            .map(|e| e.key().clone())
                            .collect();

                        for token_id in tokens {
                            match Self::fetch_book_static(&http, &clob_host, &token_id).await {
                                Ok(book) => {
                                    books.insert(token_id.clone(), book);
                                    let _ = book_tx.send(token_id);
                                }
                                Err(e) => {
                                    debug!("Book refresh failed for {}: {e}", &token_id[..8.min(token_id.len())]);
                                }
                            }
                        }
                    }
                    _ = shutdown.recv() => break,
                }
            }
        });
    }

    /// Handle a WebSocket message (book update).
    fn handle_ws_message(
        text: &str,
        books: &Arc<DashMap<String, OrderBook>>,
        book_tx: &broadcast::Sender<String>,
    ) {
        // Polymarket WS sends book updates as:
        // [{"asset_id":"...","market":"...","bids":[...],"asks":[...],"timestamp":"...","hash":"..."}]
        let updates: Vec<WsBookUpdate> = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                // Could be a single object or subscription ack
                if let Ok(single) = serde_json::from_str::<WsBookUpdate>(text) {
                    vec![single]
                } else {
                    return;
                }
            }
        };

        for update in updates {
            let Some(asset_id) = update.asset_id else { continue };

            if let Some(mut book) = books.get_mut(&asset_id) {
                // Apply delta updates to existing book
                if let Some(bids) = update.bids {
                    for level in bids {
                        let price = level.price.parse::<Decimal>().unwrap_or_default();
                        let size = level.size.parse::<Decimal>().unwrap_or_default();
                        if size == Decimal::ZERO {
                            book.bids.remove(&price);
                        } else {
                            book.bids.insert(price, size);
                        }
                    }
                }
                if let Some(asks) = update.asks {
                    for level in asks {
                        let price = level.price.parse::<Decimal>().unwrap_or_default();
                        let size = level.size.parse::<Decimal>().unwrap_or_default();
                        if size == Decimal::ZERO {
                            book.asks.remove(&price);
                        } else {
                            book.asks.insert(price, size);
                        }
                    }
                }

                let _ = book_tx.send(asset_id);
            }
        }
    }

    /// Resolve a market slug to a Market struct via Gamma API.
    async fn resolve_market(
        http: &reqwest::Client,
        gamma_host: &str,
        slug: &str,
        asset: Asset,
        duration: Duration,
    ) -> Result<Option<Market>> {
        let url = format!("{}/markets?slug={}", gamma_host, slug);
        let resp = http.get(&url).send().await?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let text = resp.text().await?;
        let infos: Vec<MarketInfo> = serde_json::from_str(&text).unwrap_or_default();

        let info = match infos.into_iter().next() {
            Some(i) => i,
            None => return Ok(None),
        };

        // Extract token IDs — try `tokens` array first, then fall back to
        // `clobTokenIds` + `outcomes` (JSON-encoded strings from Gamma API).
        let tokens = info.tokens.unwrap_or_default();
        let yes_token = tokens
            .iter()
            .find(|t| matches!(t.outcome.as_deref(), Some("Yes") | Some("Up")))
            .and_then(|t| t.token_id.clone());
        let no_token = tokens
            .iter()
            .find(|t| matches!(t.outcome.as_deref(), Some("No") | Some("Down")))
            .and_then(|t| t.token_id.clone());

        let (yes_id, no_id) = match (yes_token, no_token) {
            (Some(y), Some(n)) => (y, n),
            _ => {
                // Fallback: parse clobTokenIds + outcomes JSON strings
                let clob_ids: Vec<String> = info
                    .clob_token_ids
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                let outcomes: Vec<String> = info
                    .outcomes
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();

                if clob_ids.len() >= 2 && outcomes.len() >= 2 {
                    // Map "Up"/"Yes" → yes_token, "Down"/"No" → no_token
                    let up_idx = outcomes.iter().position(|o| o == "Up" || o == "Yes");
                    let down_idx = outcomes.iter().position(|o| o == "Down" || o == "No");
                    match (up_idx, down_idx) {
                        (Some(u), Some(d)) => (clob_ids[u].clone(), clob_ids[d].clone()),
                        _ => return Ok(None),
                    }
                } else {
                    return Ok(None);
                }
            }
        };

        let market = Market::with_condition_id(
            slug.to_string(),
            asset,
            duration,
            yes_id,
            no_id,
            info.condition_id,
        );

        Ok(Some(market))
    }

    /// Static book fetch (no &self, for use in spawned tasks).
    async fn fetch_book_static(
        http: &reqwest::Client,
        clob_host: &str,
        token_id: &str,
    ) -> Result<OrderBook> {
        let url = format!("{}/book?token_id={}", clob_host, token_id);

        let resp: BookResponse = http
            .get(&url)
            .send()
            .await?
            .json()
            .await?;

        let mut book = OrderBook::new(token_id.to_string());

        for level in &resp.bids {
            let price = level.price.parse::<Decimal>().unwrap_or_default();
            let size = level.size.parse::<Decimal>().unwrap_or_default();
            book.bids.insert(price, size);
        }

        for level in &resp.asks {
            let price = level.price.parse::<Decimal>().unwrap_or_default();
            let size = level.size.parse::<Decimal>().unwrap_or_default();
            book.asks.insert(price, size);
        }

        Ok(book)
    }

    /// Fetch order book snapshot via REST API (instance method).
    pub async fn fetch_book(&self, token_id: &str) -> Result<OrderBook> {
        let book = Self::fetch_book_static(&self.http_client, &self.config.clob_host, token_id).await?;
        self.books.insert(token_id.to_string(), book.clone());
        Ok(book)
    }

    /// Get cached order book for a token.
    pub fn get_book(&self, token_id: &str) -> Option<OrderBook> {
        self.books.get(token_id).map(|b| b.clone())
    }

    /// Get cached market by slug.
    pub fn get_market(&self, slug: &str) -> Option<Market> {
        self.markets.get(slug).map(|m| m.clone())
    }

    /// Get the best ask price for a token from cache.
    pub fn best_ask(&self, token_id: &str) -> Option<(Decimal, Decimal)> {
        self.books.get(token_id)?.best_ask()
    }

    /// Get the best bid price for a token from cache.
    pub fn best_bid(&self, token_id: &str) -> Option<(Decimal, Decimal)> {
        self.books.get(token_id)?.best_bid()
    }

    /// Subscribe to book update notifications.
    pub fn subscribe_book_updates(&self) -> broadcast::Receiver<String> {
        self.book_update_tx.subscribe()
    }

    /// Get count of tracked markets.
    pub fn market_count(&self) -> usize {
        self.markets.len()
    }
}

// --- Response types ---

#[derive(Debug, Deserialize)]
pub struct BookResponse {
    #[serde(default)]
    pub bids: Vec<BookLevel>,
    #[serde(default)]
    pub asks: Vec<BookLevel>,
}

#[derive(Debug, Deserialize)]
pub struct BookLevel {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Deserialize)]
pub struct MarketInfo {
    pub id: Option<String>,
    pub slug: Option<String>,
    pub question: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    /// Condition ID for CTF contract (needed for merge/redeem)
    #[serde(rename = "conditionId", default)]
    pub condition_id: Option<String>,
    #[serde(default)]
    pub tokens: Option<Vec<TokenInfo>>,
    /// JSON-encoded array of CLOB token IDs, e.g. "[\"abc\", \"def\"]"
    #[serde(rename = "clobTokenIds", default)]
    pub clob_token_ids: Option<String>,
    /// JSON-encoded array of outcome labels, e.g. "[\"Up\", \"Down\"]"
    #[serde(default)]
    pub outcomes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    pub token_id: Option<String>,
    pub outcome: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WsBookUpdate {
    asset_id: Option<String>,
    market: Option<String>,
    #[serde(default)]
    bids: Option<Vec<BookLevel>>,
    #[serde(default)]
    asks: Option<Vec<BookLevel>>,
    timestamp: Option<String>,
}

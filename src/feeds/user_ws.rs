use crate::models::market::Side;
use crate::models::order::OrderSide;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, info, warn};

/// WebSocket client for the Polymarket CLOB user channel.
///
/// Receives real-time fill events for GTC/GTD orders that don't fill immediately.
/// Also receives order status updates (cancelled, expired, etc.).
///
/// WS endpoint: wss://ws-subscriptions-clob.polymarket.com/ws/user
/// Auth: connect, then send auth message with L1 headers.
pub struct UserWsFeed {
    ws_host: String,
    address: String,
    /// Broadcast channel for fill events
    fill_tx: broadcast::Sender<FillEvent>,
}

/// A fill event received from the CLOB user WebSocket.
#[derive(Debug, Clone)]
pub struct FillEvent {
    pub order_id: String,
    pub token_id: String,
    pub market_id: String,
    pub side: OrderSide,
    pub market_side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
    pub strategy_tag: String,
}

/// Raw WS message from CLOB user channel.
#[derive(Debug, Deserialize)]
struct WsUserMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    // Trade/fill event
    order_id: Option<String>,
    token_id: Option<String>,
    market: Option<String>,
    side: Option<String>,       // "BUY" or "SELL"
    price: Option<String>,
    size: Option<String>,
    fee: Option<String>,
    status: Option<String>,     // "MATCHED", "FILLED", "CANCELLED"
    // Misc
    asset_id: Option<String>,
}

impl UserWsFeed {
    pub fn new(ws_host: &str, address: &str) -> Self {
        let (fill_tx, _) = broadcast::channel(256);

        // User channel endpoint
        let ws_url = if ws_host.ends_with("/ws/user") {
            ws_host.to_string()
        } else {
            format!("{}/ws/user", ws_host.trim_end_matches("/ws/market"))
        };

        Self {
            ws_host: ws_url,
            address: address.to_string(),
            fill_tx,
        }
    }

    /// Subscribe to fill events.
    pub fn subscribe_fills(&self) -> broadcast::Receiver<FillEvent> {
        self.fill_tx.subscribe()
    }

    /// Start the user WebSocket connection with reconnection logic.
    pub fn start(&self, shutdown_tx: &broadcast::Sender<()>) {
        let ws_host = self.ws_host.clone();
        let address = self.address.clone();
        let fill_tx = self.fill_tx.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut backoff_ms = 1000u64;
            let max_backoff = 30_000u64;

            loop {
                info!("Connecting to CLOB user WS: {ws_host}");

                match connect_async(&ws_host).await {
                    Ok((ws_stream, _)) => {
                        info!("CLOB user WS connected");
                        backoff_ms = 1000;

                        let (mut write, mut read) = ws_stream.split();

                        // Send auth/subscribe message
                        let subscribe_msg = serde_json::json!({
                            "auth": {},
                            "type": "subscribe",
                            "channel": "user",
                            "markets": [],
                            "assets_ids": [],
                            "user": address,
                        });

                        if let Err(e) = write
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                subscribe_msg.to_string(),
                            ))
                            .await
                        {
                            error!("Failed to subscribe user WS: {e}");
                            continue;
                        }

                        // Read messages until disconnect
                        loop {
                            tokio::select! {
                                msg = read.next() => {
                                    match msg {
                                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                                            Self::handle_message(&text, &fill_tx);
                                        }
                                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(data))) => {
                                            let _ = write.send(
                                                tokio_tungstenite::tungstenite::Message::Pong(data)
                                            ).await;
                                        }
                                        Some(Ok(_)) => {} // Binary, Close, etc
                                        Some(Err(e)) => {
                                            warn!("User WS error: {e}");
                                            break;
                                        }
                                        None => {
                                            warn!("User WS stream ended");
                                            break;
                                        }
                                    }
                                }
                                _ = shutdown_rx.recv() => {
                                    info!("User WS shutting down");
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("User WS connect failed: {e}");
                    }
                }

                // Reconnect with backoff
                info!("User WS reconnecting in {backoff_ms}ms...");
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)) => {}
                    _ = shutdown_rx.recv() => return,
                }
                backoff_ms = (backoff_ms * 2).min(max_backoff);
            }
        });
    }

    /// Handle an incoming user WS message.
    fn handle_message(text: &str, fill_tx: &broadcast::Sender<FillEvent>) {
        let msg: WsUserMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(_) => return, // Not a parseable message (heartbeat, etc)
        };

        let msg_type = msg.msg_type.as_deref().unwrap_or("");
        let status = msg.status.as_deref().unwrap_or("");

        // We care about trade/fill events
        if msg_type != "trade" && !matches!(status, "MATCHED" | "FILLED") {
            debug!("User WS non-fill: type={msg_type} status={status}");
            return;
        }

        let order_id = match &msg.order_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => return,
        };

        let token_id = msg.token_id.clone().or(msg.asset_id.clone()).unwrap_or_default();
        let market_id = msg.market.clone().unwrap_or_default();

        let price = msg
            .price
            .as_deref()
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::ZERO);

        let size = msg
            .size
            .as_deref()
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::ZERO);

        let fee = msg
            .fee
            .as_deref()
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::ZERO);

        let order_side = match msg.side.as_deref() {
            Some("BUY") => OrderSide::Buy,
            Some("SELL") => OrderSide::Sell,
            _ => OrderSide::Buy,
        };

        if size == Decimal::ZERO {
            return;
        }

        info!(
            "User WS fill: order={} token={} side={:?} price={} size={} fee={}",
            &order_id[..8.min(order_id.len())],
            &token_id[..8.min(token_id.len())],
            order_side,
            price,
            size,
            fee
        );

        let event = FillEvent {
            order_id,
            token_id,
            market_id,
            side: order_side,
            market_side: Side::Yes, // Will be resolved by the consumer
            price,
            size,
            fee,
            strategy_tag: String::new(), // Will be resolved by the consumer
        };

        let _ = fill_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fill_message() {
        let (tx, mut rx) = broadcast::channel(16);

        let msg = r#"{
            "type": "trade",
            "order_id": "0x123abc",
            "token_id": "tok_yes_001",
            "market": "btc-updown-5m-12345",
            "side": "BUY",
            "price": "0.52",
            "size": "10.00",
            "fee": "0.01",
            "status": "MATCHED"
        }"#;

        UserWsFeed::handle_message(msg, &tx);

        let event = rx.try_recv().unwrap();
        assert_eq!(event.order_id, "0x123abc");
        assert_eq!(event.token_id, "tok_yes_001");
        assert_eq!(event.price, Decimal::from_str("0.52").unwrap());
        assert_eq!(event.size, Decimal::from_str("10.00").unwrap());
    }

    #[test]
    fn test_ignore_non_fill() {
        let (tx, mut rx) = broadcast::channel(16);

        let msg = r#"{"type": "heartbeat"}"#;
        UserWsFeed::handle_message(msg, &tx);

        assert!(rx.try_recv().is_err());
    }
}

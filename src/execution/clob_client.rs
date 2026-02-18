use crate::config::PolymarketConfig;
use crate::execution::clob_auth::ClobAuth;
use crate::execution::order_builder::SignedOrder;
use crate::models::order::{OrderResult, OrderStatus, OrderType};
use anyhow::Result;
use chrono::Utc;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// REST client for Polymarket CLOB API.
///
/// Handles order submission, cancellation, and book queries.
/// Uses connection pooling and L1/L2 authentication.
pub struct ClobClient {
    config: PolymarketConfig,
    http: reqwest::Client,
    auth: Arc<RwLock<ClobAuth>>,
}

#[derive(Debug, Serialize)]
struct PostOrderRequest {
    order: SignedOrder,
    #[serde(rename = "orderType")]
    order_type: String,
    owner: String,
    #[serde(rename = "postOnly", skip_serializing_if = "Option::is_none")]
    post_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PostOrderResponse {
    success: Option<bool>,
    #[serde(rename = "orderID")]
    order_id: Option<String>,
    #[serde(rename = "errorMsg")]
    error_msg: Option<String>,
    /// API returns "error" on rejections (different from "errorMsg" on success responses)
    error: Option<String>,
}

impl ClobClient {
    pub fn new(config: PolymarketConfig) -> Self {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        let auth = ClobAuth::new(&config.private_key, config.chain_id);

        Self {
            config,
            http,
            auth: Arc::new(RwLock::new(auth)),
        }
    }

    /// Initialize authentication: derive API key for L2 auth.
    pub async fn init_auth(&self) -> Result<()> {
        let mut auth = self.auth.write().await;
        match auth.derive_api_key(&self.config.clob_host).await {
            Ok(_creds) => {
                info!("L2 API key auth initialized");
                Ok(())
            }
            Err(e) => {
                info!("L2 key derivation failed ({e}), falling back to L1 auth");
                Ok(())
            }
        }
    }

    /// Build an authenticated request.
    async fn auth_request(
        &self,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let url = format!("{}{}", self.config.clob_host, path);
        let auth = self.auth.read().await;

        let builder = match method.to_uppercase().as_str() {
            "POST" => self.http.post(&url),
            "DELETE" => self.http.delete(&url),
            "GET" => self.http.get(&url),
            _ => self.http.get(&url),
        };

        // Prefer L2 auth if available, fall back to L1
        if auth.has_api_key() {
            let headers = auth.l2_headers(method, path, body)?;
            Ok(headers.apply(builder))
        } else {
            drop(auth); // Release read lock before async L1
            let auth = self.auth.read().await;
            let headers = auth.l1_headers().await?;
            Ok(headers.apply(builder))
        }
    }

    /// Submit a single order to the CLOB.
    pub async fn post_order(
        &self,
        signed: SignedOrder,
        order_type: OrderType,
        post_only: bool,
    ) -> Result<OrderResult> {
        let ot_str = match order_type {
            OrderType::GTC => "GTC",
            OrderType::GTD => "GTD",
            OrderType::FOK => "FOK",
            OrderType::FAK => "FAK",
        };

        // Get the API key (owner) from auth credentials
        let owner = {
            let auth = self.auth.read().await;
            auth.api_key().unwrap_or_default()
        };

        let req_body = PostOrderRequest {
            order: signed.clone(),
            order_type: ot_str.to_string(),
            owner,
            post_only: if post_only { Some(true) } else { None },
        };

        // Parse original order size from maker_amount for remaining_size tracking
        let original_size = signed.taker_amount.parse::<u64>().unwrap_or(0) as f64 / 1_000_000.0;
        let original_size_dec = Decimal::from_f64_retain(original_size).unwrap_or(Decimal::ZERO);

        let body_json = serde_json::to_string(&req_body)?;
        let request = self.auth_request("POST", "/order", &body_json).await?;

        let resp = request
            .header("Content-Type", "application/json")
            .body(body_json)
            .send()
            .await?;

        let status_code = resp.status();
        let resp_text = resp.text().await?;

        if !status_code.is_success() {
            error!("Order HTTP {status_code}: {resp_text}");
        }

        let body: PostOrderResponse = serde_json::from_str(&resp_text).unwrap_or(PostOrderResponse {
            success: None,
            order_id: None,
            error_msg: Some(format!("HTTP {status_code} — {resp_text}")),
            error: None,
        });

        if body.success.unwrap_or(false) {
            info!("Order submitted: id={}", body.order_id.as_deref().unwrap_or("?"));
            Ok(OrderResult {
                order_id: body.order_id.unwrap_or_default(),
                token_id: signed.token_id,
                status: OrderStatus::Open,
                filled_size: Decimal::ZERO,
                avg_fill_price: Decimal::ZERO,
                remaining_size: original_size_dec,
                timestamp: Utc::now(),
                error_msg: None,
            })
        } else {
            // API returns "error" on rejections, "errorMsg" on other failures — check both
            let err = body.error
                .or(body.error_msg)
                .unwrap_or_else(|| format!("HTTP {status_code}"));
            error!("Order rejected: {err}");
            Ok(OrderResult {
                order_id: String::new(),
                token_id: signed.token_id,
                status: OrderStatus::Rejected,
                filled_size: Decimal::ZERO,
                avg_fill_price: Decimal::ZERO,
                remaining_size: Decimal::ZERO,
                timestamp: Utc::now(),
                error_msg: Some(err),
            })
        }
    }

    /// Submit a batch of orders (preferred for arb legs).
    pub async fn post_orders(
        &self,
        orders: Vec<(SignedOrder, OrderType, bool)>,
    ) -> Result<Vec<OrderResult>> {
        let mut results = Vec::with_capacity(orders.len());
        for (signed, ot, po) in orders {
            let result = self.post_order(signed, ot, po).await?;
            results.push(result);
        }
        Ok(results)
    }

    /// Cancel all open orders.
    pub async fn cancel_all(&self) -> Result<()> {
        let request = self.auth_request("DELETE", "/cancel-all", "").await?;
        let resp = request.send().await?;

        if resp.status().is_success() {
            info!("All orders cancelled");
        } else {
            error!("Failed to cancel all: HTTP {}", resp.status());
        }

        Ok(())
    }

    /// Cancel a specific order by ID.
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let path = format!("/order/{}", order_id);
        let request = self.auth_request("DELETE", &path, "").await?;
        let resp = request.send().await?;

        if resp.status().is_success() {
            debug!("Cancelled order {order_id}");
        } else {
            error!("Failed to cancel {order_id}: HTTP {}", resp.status());
        }

        Ok(())
    }

    /// Get order status by ID. Returns (status_string, size_matched).
    /// Status: "LIVE", "MATCHED", "CANCELLED", "DELAYED", etc.
    pub async fn get_order(&self, order_id: &str) -> Result<(String, f64)> {
        let path = format!("/order/{}", order_id);
        let request = self.auth_request("GET", &path, "").await?;
        let resp = request.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Get order failed: HTTP {status} — {body}");
        }

        let val: serde_json::Value = resp.json().await?;
        let status = val.get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();
        let size_matched = val.get("size_matched")
            .and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()))
            .unwrap_or(0.0);

        Ok((status, size_matched))
    }

    /// Get server time (for clock synchronization).
    pub async fn get_server_time(&self) -> Result<u64> {
        let url = format!("{}/time", self.config.clob_host);
        let resp: serde_json::Value = self.http.get(&url).send().await?.json().await?;
        let ts = resp.as_f64().unwrap_or(0.0) as u64;
        Ok(ts)
    }

    /// Check if a token requires neg risk exchange signing.
    /// Returns true for neg risk markets (e.g., multi-outcome), false otherwise.
    pub async fn fetch_neg_risk(&self, token_id: &str) -> Result<bool> {
        let url = format!("{}/neg-risk?token_id={}", self.config.clob_host, token_id);
        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            info!("Neg risk endpoint returned {}, defaulting to false", resp.status());
            return Ok(false);
        }

        let text = resp.text().await.unwrap_or_default();
        info!("Neg risk raw response for {}...: {}", &token_id[..20.min(token_id.len())], text);

        let val: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let neg_risk = val
            .get("neg_risk")
            .and_then(|v| v.as_bool().or_else(|| v.as_str().map(|s| s == "true")))
            .unwrap_or(false);

        info!("Neg risk for {}...: {}", &token_id[..20.min(token_id.len())], neg_risk);
        Ok(neg_risk)
    }

    /// Fetch the fee rate (in basis points) for a specific token.
    /// Fee-enabled markets (15-min crypto) return 1000, fee-free return 0.
    /// Formula: fee_per_share = p × (1-p) × (fee_rate_bps / 10000)
    pub async fn fetch_fee_rate(&self, token_id: &str) -> Result<u32> {
        let url = format!("{}/fee-rate?token_id={}", self.config.clob_host, token_id);
        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            info!("Fee rate endpoint returned {}, defaulting to 1000", resp.status());
            return Ok(1000);
        }

        let text = resp.text().await.unwrap_or_default();
        info!("Fee rate raw response for {}...: {}", &token_id[..20.min(token_id.len())], text);

        let val: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        // Try parsing fee_rate_bps as string ("1000"), number (1000), or from root value
        let bps = val
            .get("fee_rate_bps")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .or_else(|| v.as_f64().map(|f| f as u64))
            })
            .or_else(|| {
                // Maybe the response IS just the number or string directly
                val.as_u64()
                    .or_else(|| val.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(1000) as u32; // default 1000 for crypto markets

        info!("Fee rate for {}...: {} bps", &token_id[..20.min(token_id.len())], bps);
        Ok(bps)
    }

    /// Fetch available USDC balance from Polymarket profile.
    /// Uses authenticated GET /balance-allowance?asset_type=COLLATERAL endpoint.
    /// Response: { "balance": "5.123456", "allowance": "..." }
    pub async fn fetch_balance(&self) -> Result<f64> {
        let sig_type = self.config.signature_type;
        let path = format!("/balance-allowance?asset_type=COLLATERAL&signature_type={sig_type}");
        let request = self.auth_request("GET", &path, "").await?;
        let resp = request.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Balance fetch failed: HTTP {status} — {body}");
        }

        // Response: {"balance": "5.123456", "allowance": "..."}
        let text = resp.text().await?;
        let val: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();

        let raw = if let Some(b) = val.get("balance") {
            b.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| b.as_f64())
                .unwrap_or(0.0)
        } else if let Some(b) = val.as_f64() {
            b
        } else {
            text.trim().parse::<f64>().unwrap_or(0.0)
        };

        // API returns balance in micro-units (USDC has 6 decimals)
        let balance = raw / 1_000_000.0;
        Ok(balance)
    }
}

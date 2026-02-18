use crate::execution::clob_client::ClobClient;
use crate::execution::order_builder::OrderBuilder;
use crate::models::order::{OrderIntent, OrderResult};
use anyhow::Result;
use tokio::sync::RwLock;
use tracing::info;

/// Handles batch order submission with pre-flight validation.
///
/// This is the single serialized execution point — all strategy order intents
/// funnel through here to prevent conflicting orders.
pub struct BatchSubmitter {
    order_builder: RwLock<OrderBuilder>,
    clob_client: ClobClient,
}

impl BatchSubmitter {
    pub fn new(order_builder: OrderBuilder, clob_client: ClobClient) -> Self {
        Self {
            order_builder: RwLock::new(order_builder),
            clob_client,
        }
    }

    /// Submit a batch of order intents.
    ///
    /// 1. Build and sign all orders
    /// 2. Submit as batch to CLOB
    /// 3. Return results
    pub async fn submit(&self, intents: &[OrderIntent]) -> Result<Vec<OrderResult>> {
        if intents.is_empty() {
            return Ok(Vec::new());
        }

        info!("Submitting batch of {} orders", intents.len());

        // Build and sign
        let builder = self.order_builder.read().await;
        let signed = builder.build_batch(intents).await?;
        drop(builder);

        // Pair with order types
        let orders: Vec<_> = signed
            .into_iter()
            .zip(intents.iter())
            .map(|(s, i)| (s, i.order_type, i.post_only))
            .collect();

        // Submit
        let results = self.clob_client.post_orders(orders).await?;

        // Log summary
        let filled = results.iter().filter(|r| r.is_success()).count();
        let rejected = results.len() - filled;
        info!("Batch result: {filled} success, {rejected} rejected");

        Ok(results)
    }

    /// Get the wallet address used for signing.
    pub fn address(&self) -> String {
        let builder = self.order_builder.blocking_read();
        format!("{:?}", builder.address())
    }

    /// Set the fee rate (bps) on the order builder.
    pub async fn set_fee_rate_bps(&self, bps: u32) {
        self.order_builder.write().await.set_fee_rate_bps(bps);
    }

    /// Initialize CLOB authentication (derive L2 API key).
    pub async fn init_auth(&self) -> Result<()> {
        self.clob_client.init_auth().await
    }

    /// Emergency cancel all orders.
    pub async fn cancel_all(&self) -> Result<()> {
        self.clob_client.cancel_all().await
    }

    /// Cancel a specific order.
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        self.clob_client.cancel_order(order_id).await
    }

    /// Fetch real USDC balance from Polymarket.
    pub async fn fetch_balance(&self) -> Result<f64> {
        self.clob_client.fetch_balance().await
    }

    /// Fetch fee rate (bps) for a token from CLOB API.
    pub async fn fetch_fee_rate(&self, token_id: &str) -> Result<u32> {
        self.clob_client.fetch_fee_rate(token_id).await
    }
}

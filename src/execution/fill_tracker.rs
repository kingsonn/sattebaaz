use crate::models::order::{Fill, OrderResult, OrderStatus};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{debug, info};

/// Tracks order fills via WebSocket user channel.
///
/// Maintains a map of order_id → fill status, updated in real-time.
pub struct FillTracker {
    /// Active orders being tracked: order_id → OrderResult (updated on fill)
    pub active_orders: Arc<DashMap<String, OrderResult>>,
    /// Completed fills
    pub fills: Arc<DashMap<String, Vec<Fill>>>,
}

impl FillTracker {
    pub fn new() -> Self {
        Self {
            active_orders: Arc::new(DashMap::new()),
            fills: Arc::new(DashMap::new()),
        }
    }

    /// Register an order for fill tracking.
    pub fn watch(&self, result: OrderResult) {
        if !result.order_id.is_empty() {
            debug!("Tracking order: {}", result.order_id);
            self.active_orders.insert(result.order_id.clone(), result);
        }
    }

    /// Process a fill event (called from WebSocket handler).
    pub fn on_fill(&self, fill: Fill) {
        let order_id = fill.order_id.clone();

        // Update order status
        if let Some(mut order) = self.active_orders.get_mut(&order_id) {
            order.filled_size += fill.size;
            if order.remaining_size > fill.size {
                order.remaining_size -= fill.size;
                order.status = OrderStatus::PartiallyFilled;
            } else {
                order.remaining_size = Decimal::ZERO;
                order.status = OrderStatus::Filled;
            }
            order.avg_fill_price = fill.price; // Simplified; should be weighted avg
            info!(
                "Fill: order={} size={} price={} status={:?}",
                order_id, fill.size, fill.price, order.status
            );
        }

        // Store fill
        self.fills
            .entry(order_id)
            .or_insert_with(Vec::new)
            .push(fill);
    }

    /// Check if an order is fully filled.
    pub fn is_filled(&self, order_id: &str) -> bool {
        self.active_orders
            .get(order_id)
            .map(|o| o.status == OrderStatus::Filled)
            .unwrap_or(false)
    }

    /// Get total filled size for an order.
    pub fn filled_size(&self, order_id: &str) -> Decimal {
        self.active_orders
            .get(order_id)
            .map(|o| o.filled_size)
            .unwrap_or(Decimal::ZERO)
    }

    /// Clean up completed/old orders to prevent memory growth.
    pub fn cleanup_completed(&self) {
        self.active_orders.retain(|_, v| {
            !matches!(
                v.status,
                OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
            )
        });
    }
}

impl Default for FillTracker {
    fn default() -> Self {
        Self::new()
    }
}

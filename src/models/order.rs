use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::market::Side;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    GTC, // Good-Til-Cancelled: standard limit
    GTD, // Good-Til-Date: expires at timestamp
    FOK, // Fill-Or-Kill: all or nothing
    FAK, // Fill-And-Kill: partial fills OK, rest cancelled
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    pub token_id: String,
    pub market_side: Side,
    pub order_side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub order_type: OrderType,
    pub post_only: bool,
    pub expiration: Option<u64>,
    pub strategy_tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    pub order_id: String,
    pub token_id: String,
    pub status: OrderStatus,
    pub filled_size: Decimal,
    pub avg_fill_price: Decimal,
    pub remaining_size: Decimal,
    pub timestamp: DateTime<Utc>,
    pub error_msg: Option<String>,
}

impl OrderResult {
    pub fn is_success(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Filled | OrderStatus::PartiallyFilled | OrderStatus::Open
        )
    }

    pub fn fill_ratio(&self) -> f64 {
        if self.filled_size + self.remaining_size == Decimal::ZERO {
            return 0.0;
        }
        let total = self.filled_size + self.remaining_size;
        (self.filled_size / total)
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: String,
    pub token_id: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub timestamp: DateTime<Utc>,
    pub fee: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchOrderRequest {
    pub orders: Vec<OrderIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchOrderResponse {
    pub results: Vec<OrderResult>,
}

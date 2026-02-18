use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::market::Side;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub market_id: String,
    pub token_id: String,
    pub side: Side,
    pub size: Decimal,
    pub avg_entry_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub strategy_tag: String,
    pub opened_at: DateTime<Utc>,
}

impl Position {
    pub fn cost_basis(&self) -> Decimal {
        self.size * self.avg_entry_price
    }

    pub fn max_payout(&self) -> Decimal {
        self.size // Each token pays $1 if correct
    }

    pub fn potential_profit(&self) -> Decimal {
        self.max_payout() - self.cost_basis()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StraddlePosition {
    pub market_id: String,
    pub yes_size: Decimal,
    pub no_size: Decimal,
    pub yes_avg_price: Decimal,
    pub no_avg_price: Decimal,
    pub combined_cost: Decimal,
    pub guaranteed_profit: Decimal, // min(yes_size, no_size) * (1.0 - combined_price)
    pub opened_at: DateTime<Utc>,
}

impl StraddlePosition {
    pub fn new(
        market_id: String,
        yes_size: Decimal,
        no_size: Decimal,
        yes_avg_price: Decimal,
        no_avg_price: Decimal,
    ) -> Self {
        let matched = yes_size.min(no_size);
        let combined_price = yes_avg_price + no_avg_price;
        let guaranteed = matched * (Decimal::ONE - combined_price);

        Self {
            market_id,
            yes_size,
            no_size,
            yes_avg_price,
            no_avg_price,
            combined_cost: yes_size * yes_avg_price + no_size * no_avg_price,
            guaranteed_profit: guaranteed,
            opened_at: Utc::now(),
        }
    }

    pub fn is_balanced(&self) -> bool {
        let ratio = if self.yes_size > Decimal::ZERO && self.no_size > Decimal::ZERO {
            let min = self.yes_size.min(self.no_size);
            let max = self.yes_size.max(self.no_size);
            min / max
        } else {
            Decimal::ZERO
        };
        ratio >= Decimal::new(80, 2) // At least 80% balanced
    }

    pub fn imbalance(&self) -> Decimal {
        (self.yes_size - self.no_size).abs()
    }

    pub fn excess_side(&self) -> Option<Side> {
        if self.yes_size > self.no_size {
            Some(Side::Yes)
        } else if self.no_size > self.yes_size {
            Some(Side::No)
        } else {
            None
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Portfolio {
    pub capital: Decimal,
    pub starting_capital: Decimal,
    pub positions: Vec<Position>,
    pub straddles: Vec<StraddlePosition>,
    pub daily_pnl: Decimal,
    pub total_pnl: Decimal,
    pub consecutive_losses: u32,
    pub total_trades: u64,
    pub winning_trades: u64,
}

impl Portfolio {
    pub fn new(capital: Decimal) -> Self {
        Self {
            capital,
            starting_capital: capital,
            ..Default::default()
        }
    }

    pub fn total_exposure(&self) -> Decimal {
        self.positions.iter().map(|p| p.cost_basis()).sum::<Decimal>()
            + self.straddles.iter().map(|s| s.combined_cost).sum::<Decimal>()
    }

    pub fn exposure_ratio(&self) -> Decimal {
        if self.capital == Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.total_exposure() / self.capital
    }

    pub fn win_rate(&self) -> f64 {
        if self.total_trades == 0 {
            return 0.0;
        }
        self.winning_trades as f64 / self.total_trades as f64
    }

    pub fn daily_return_pct(&self) -> Decimal {
        if self.starting_capital == Decimal::ZERO {
            return Decimal::ZERO;
        }
        (self.daily_pnl / self.starting_capital) * Decimal::from(100)
    }
}

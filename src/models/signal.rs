use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::market::{Asset, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolRegime {
    Dead,
    Low,
    Medium,
    High,
    Extreme,
}

impl VolRegime {
    pub fn from_atr(asset: Asset, atr_1m: f64) -> Self {
        match asset {
            Asset::BTC => match atr_1m {
                x if x < 15.0 => VolRegime::Dead,
                x if x < 50.0 => VolRegime::Low,
                x if x < 150.0 => VolRegime::Medium,
                x if x < 300.0 => VolRegime::High,
                _ => VolRegime::Extreme,
            },
            Asset::ETH => match atr_1m {
                x if x < 1.0 => VolRegime::Dead,
                x if x < 3.0 => VolRegime::Low,
                x if x < 10.0 => VolRegime::Medium,
                x if x < 20.0 => VolRegime::High,
                _ => VolRegime::Extreme,
            },
            Asset::SOL => match atr_1m {
                x if x < 0.05 => VolRegime::Dead,
                x if x < 0.15 => VolRegime::Low,
                x if x < 0.50 => VolRegime::Medium,
                x if x < 1.00 => VolRegime::High,
                _ => VolRegime::Extreme,
            },
            Asset::XRP => match atr_1m {
                x if x < 0.001 => VolRegime::Dead,
                x if x < 0.003 => VolRegime::Low,
                x if x < 0.008 => VolRegime::Medium,
                x if x < 0.015 => VolRegime::High,
                _ => VolRegime::Extreme,
            },
        }
    }

    pub fn mm_half_spread(&self) -> f64 {
        match self {
            VolRegime::Dead => 0.015,
            VolRegime::Low => 0.020,
            VolRegime::Medium => 0.030,
            VolRegime::High => 0.050,
            VolRegime::Extreme => 0.0, // PULL quotes
        }
    }

    pub fn mm_size_multiplier(&self) -> f64 {
        match self {
            VolRegime::Dead => 2.0,
            VolRegime::Low => 1.5,
            VolRegime::Medium => 1.0,
            VolRegime::High => 0.3,
            VolRegime::Extreme => 0.0,
        }
    }

    pub fn arb_min_edge(&self) -> f64 {
        match self {
            VolRegime::Dead => 0.05,
            VolRegime::Low => 0.03,
            VolRegime::Medium => 0.02,
            VolRegime::High => 0.02,
            VolRegime::Extreme => 0.02,
        }
    }

    pub fn lag_min_edge(&self) -> Option<f64> {
        match self {
            VolRegime::Dead => None, // Don't trade lag in dead vol
            VolRegime::Low => Some(0.03),
            VolRegime::Medium => Some(0.02),
            VolRegime::High => Some(0.03),
            VolRegime::Extreme => Some(0.05),
        }
    }

    pub fn position_size_cap(&self) -> f64 {
        match self {
            VolRegime::Dead => 0.30,
            VolRegime::Low => 0.25,
            VolRegime::Medium => 0.20,
            VolRegime::High => 0.15,
            VolRegime::Extreme => 0.10,
        }
    }

    pub fn fill_probability_penalty(&self) -> f64 {
        match self {
            VolRegime::Dead => 0.95,
            VolRegime::Low => 0.90,
            VolRegime::Medium => 0.80,
            VolRegime::High => 0.65,
            VolRegime::Extreme => 0.50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BiasDirection {
    Up,
    Down,
    Neutral,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BiasSignal {
    pub direction: BiasDirection,
    pub confidence: f64,       // 0.0 - 1.0
    pub momentum_score: f64,   // raw momentum component
    pub trend_score: f64,      // raw trend component
    pub flow_score: f64,       // raw order flow component
    pub funding_score: f64,    // raw funding rate component
    pub liquidation_score: f64, // raw liquidation component
    pub timestamp: DateTime<Utc>,
}

impl BiasSignal {
    pub fn is_actionable(&self) -> bool {
        self.direction != BiasDirection::Neutral && self.confidence > 0.35
    }

    pub fn favored_side(&self) -> Option<Side> {
        match self.direction {
            BiasDirection::Up => Some(Side::Yes),
            BiasDirection::Down => Some(Side::No),
            BiasDirection::Neutral => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbSignal {
    pub yes_ask: f64,
    pub no_ask: f64,
    pub combined: f64,
    pub edge: f64,           // 1.0 - combined
    pub executable_size: f64,
    pub expected_profit: f64,
    pub timestamp: DateTime<Utc>,
}

impl ArbSignal {
    pub fn is_profitable(&self, min_edge: f64, min_profit: f64) -> bool {
        self.edge >= min_edge && self.expected_profit >= min_profit
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MomentumSignal {
    pub momentum: f64,
    pub acceleration: f64,
    pub divergence: f64,     // fair_prob - current_market_prob
    pub velocity_5s: f64,
    pub velocity_15s: f64,
    pub velocity_30s: f64,
    pub exhausted: bool,
    pub timestamp: DateTime<Utc>,
}

impl MomentumSignal {
    pub fn is_entry_signal(&self) -> bool {
        self.momentum.abs() > 0.003
            && self.divergence.abs() > 0.02
            && !self.exhausted
            && self.momentum.signum() == self.divergence.signum()
    }

    pub fn direction(&self) -> BiasDirection {
        if self.momentum > 0.0 && self.divergence > 0.0 {
            BiasDirection::Up
        } else if self.momentum < 0.0 && self.divergence < 0.0 {
            BiasDirection::Down
        } else {
            BiasDirection::Neutral
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionState {
    Normal,
    Compressing,       // BBW percentile < 10
    BreakoutDetected,  // BBW expanding from compression
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionSignal {
    pub state: CompressionState,
    pub bbw_percentile: f64,
    pub bbw_current: f64,
    pub bbw_previous: f64,
    pub timestamp: DateTime<Utc>,
}

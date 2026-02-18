use crate::models::signal::MomentumSignal;
use chrono::Utc;
use std::collections::VecDeque;

/// Tracks probability price velocity and acceleration to detect momentum.
pub struct MomentumDetector {
    price_history: VecDeque<(f64, f64)>, // (timestamp_secs, price)
    momentum_history: VecDeque<f64>,
    max_history: usize,
}

impl MomentumDetector {
    pub fn new(max_history: usize) -> Self {
        Self {
            price_history: VecDeque::with_capacity(max_history),
            momentum_history: VecDeque::with_capacity(max_history),
            max_history,
        }
    }

    /// Push a new price observation (called on every Polymarket book update).
    pub fn push_price(&mut self, timestamp_secs: f64, price: f64) {
        if self.price_history.len() >= self.max_history {
            self.price_history.pop_front();
        }
        self.price_history.push_back((timestamp_secs, price));
    }

    /// Get price at approximately N seconds ago.
    fn price_at_ago(&self, seconds_ago: f64) -> Option<f64> {
        if self.price_history.is_empty() {
            return None;
        }
        let now = self.price_history.back()?.0;
        let target_time = now - seconds_ago;

        // Find closest price to target time
        self.price_history
            .iter()
            .min_by(|a, b| {
                (a.0 - target_time)
                    .abs()
                    .partial_cmp(&(b.0 - target_time).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|&(_, p)| p)
    }

    /// Compute momentum signal from recent price history.
    ///
    /// - `fair_prob`: our model's fair probability (for divergence calculation)
    pub fn detect(&mut self, fair_prob: f64) -> Option<MomentumSignal> {
        let current_price = self.price_history.back()?.1;
        let price_5s = self.price_at_ago(5.0)?;
        let price_15s = self.price_at_ago(15.0)?;
        let price_30s = self.price_at_ago(30.0)?;

        // Velocities (price change per second)
        let velocity_5s = (current_price - price_5s) / 5.0;
        let velocity_15s = (current_price - price_15s) / 15.0;
        let velocity_30s = (current_price - price_30s) / 30.0;

        // Acceleration
        let acceleration = velocity_5s - velocity_15s;

        // Composite momentum score
        let momentum = velocity_5s * 0.5 + acceleration * 0.3 + (velocity_5s - velocity_30s) * 0.2;

        // Track momentum history for exhaustion detection
        if self.momentum_history.len() >= self.max_history {
            self.momentum_history.pop_front();
        }
        self.momentum_history.push_back(momentum);

        // Divergence from fair value
        let divergence = fair_prob - current_price;

        // Exhaustion detection
        let exhausted = self.detect_exhaustion();

        Some(MomentumSignal {
            momentum,
            acceleration,
            divergence,
            velocity_5s,
            velocity_15s,
            velocity_30s,
            exhausted,
            timestamp: Utc::now(),
        })
    }

    /// Detect if momentum is exhausting (peaked and declining).
    fn detect_exhaustion(&self) -> bool {
        if self.momentum_history.len() < 5 {
            return false;
        }

        let recent_len = 10.min(self.momentum_history.len());
        let recent: Vec<f64> = self
            .momentum_history
            .iter()
            .rev()
            .take(recent_len)
            .copied()
            .collect();

        let peak = recent
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
            .abs();
        let current = recent[0].abs();

        // Exhaustion: peak was significant and current is <40% of peak
        peak > 0.005 && current < peak * 0.4
    }

    pub fn reset(&mut self) {
        self.price_history.clear();
        self.momentum_history.clear();
    }
}

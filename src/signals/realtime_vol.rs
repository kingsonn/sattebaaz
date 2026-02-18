use crate::models::market::Asset;
use crate::models::signal::VolRegime;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Real-time volatility regime tracker.
///
/// Computes ATR(1m) from a rolling window of price ticks,
/// then classifies into VolRegime for each asset.
pub struct RealtimeVolTracker {
    /// Rolling price windows per asset (last 60 prices = ~1min at 1/sec)
    windows: Arc<RwLock<HashMap<Asset, PriceWindow>>>,
}

struct PriceWindow {
    prices: VecDeque<f64>,
    timestamps_ms: VecDeque<i64>,
    max_size: usize,
    /// Computed 1-minute ATR (average of |close - open| over rolling 1-min bars)
    atr_1m: f64,
    /// Current regime
    regime: VolRegime,
}

impl PriceWindow {
    fn new(max_size: usize) -> Self {
        Self {
            prices: VecDeque::with_capacity(max_size),
            timestamps_ms: VecDeque::with_capacity(max_size),
            max_size,
            atr_1m: 0.0,
            regime: VolRegime::Medium,
        }
    }

    fn push(&mut self, price: f64, timestamp_ms: i64) {
        if self.prices.len() >= self.max_size {
            self.prices.pop_front();
            self.timestamps_ms.pop_front();
        }
        self.prices.push_back(price);
        self.timestamps_ms.push_back(timestamp_ms);
        self.recompute();
    }

    fn recompute(&mut self) {
        if self.prices.len() < 10 {
            return;
        }

        // Compute realized volatility over the window
        // Method: average absolute return per tick, annualized
        let mut sum_abs_returns = 0.0;
        let mut count = 0;

        for i in 1..self.prices.len() {
            let ret = (self.prices[i] - self.prices[i - 1]).abs() / self.prices[i - 1];
            sum_abs_returns += ret;
            count += 1;
        }

        if count == 0 {
            return;
        }

        let avg_abs_return = sum_abs_returns / count as f64;

        // Estimate 1-minute ATR:
        // If we have N ticks over T seconds, scale to 60 seconds
        let first_ts = self.timestamps_ms.front().copied().unwrap_or(0);
        let last_ts = self.timestamps_ms.back().copied().unwrap_or(0);
        let window_secs = (last_ts - first_ts) as f64 / 1000.0;

        if window_secs < 5.0 {
            return;
        }

        // Ticks per second * 60 = ticks per minute
        let ticks_per_sec = count as f64 / window_secs;
        let ticks_per_min = ticks_per_sec * 60.0;

        // ATR(1m) ≈ avg_abs_return * sqrt(ticks_per_min) * current_price
        // But simpler: just scale the total absolute move over 1 minute
        let current_price = self.prices.back().copied().unwrap_or(1.0);
        self.atr_1m = avg_abs_return * ticks_per_min.sqrt() * current_price;

        // Classify regime based on ATR/price ratio
        let atr_pct = self.atr_1m / current_price;
        self.regime = Self::classify(atr_pct, current_price);
    }

    /// Classify volatility regime based on ATR as percentage of price.
    fn classify(atr_pct: f64, _price: f64) -> VolRegime {
        // Thresholds from docs/04_RISK_AND_EXECUTION.md
        // ATR(1m) thresholds as % of price:
        //   Dead:    < 0.01%
        //   Low:     0.01% - 0.03%
        //   Medium:  0.03% - 0.08%
        //   High:    0.08% - 0.15%
        //   Extreme: > 0.15%
        match atr_pct {
            x if x < 0.0001 => VolRegime::Dead,
            x if x < 0.0003 => VolRegime::Low,
            x if x < 0.0008 => VolRegime::Medium,
            x if x < 0.0015 => VolRegime::High,
            _ => VolRegime::Extreme,
        }
    }
}

impl RealtimeVolTracker {
    pub fn new() -> Self {
        Self {
            windows: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a price tick.
    pub async fn on_price(&self, asset: Asset, price: f64, timestamp_ms: i64) {
        let mut windows = self.windows.write().await;
        let window = windows
            .entry(asset)
            .or_insert_with(|| PriceWindow::new(300)); // ~5 min at 1/sec
        window.push(price, timestamp_ms);
    }

    /// Get current volatility regime for an asset.
    pub async fn regime(&self, asset: Asset) -> VolRegime {
        self.windows
            .read()
            .await
            .get(&asset)
            .map(|w| w.regime)
            .unwrap_or(VolRegime::Medium) // Default to medium until enough data
    }

    /// Get current ATR(1m) estimate for an asset.
    pub async fn atr_1m(&self, asset: Asset) -> f64 {
        self.windows
            .read()
            .await
            .get(&asset)
            .map(|w| w.atr_1m)
            .unwrap_or(0.0)
    }

    /// Get data point count for an asset.
    pub async fn data_points(&self, asset: Asset) -> usize {
        self.windows
            .read()
            .await
            .get(&asset)
            .map(|w| w.prices.len())
            .unwrap_or(0)
    }
}

impl Default for RealtimeVolTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_regime_default() {
        let tracker = RealtimeVolTracker::new();
        assert_eq!(tracker.regime(Asset::BTC).await, VolRegime::Medium);
    }

    #[tokio::test]
    async fn test_low_vol() {
        let tracker = RealtimeVolTracker::new();
        let base_price = 100_000.0;

        // Feed 60 ticks with very small moves (low vol)
        for i in 0..60 {
            let jitter = (i as f64 * 0.1).sin() * 2.0; // tiny oscillation
            tracker
                .on_price(Asset::BTC, base_price + jitter, i * 1000)
                .await;
        }

        let regime = tracker.regime(Asset::BTC).await;
        // Should be Dead or Low with such tiny moves
        assert!(
            matches!(regime, VolRegime::Dead | VolRegime::Low),
            "Expected Dead/Low, got {:?}",
            regime
        );
    }

    #[tokio::test]
    async fn test_high_vol() {
        let tracker = RealtimeVolTracker::new();
        let base_price = 100_000.0;

        // Feed 60 ticks with large moves (high vol)
        for i in 0..60 {
            let jitter = if i % 2 == 0 { 200.0 } else { -200.0 }; // $200 swings
            tracker
                .on_price(Asset::BTC, base_price + jitter, i * 1000)
                .await;
        }

        let regime = tracker.regime(Asset::BTC).await;
        // Should be High or Extreme with $200 oscillations on $100K
        assert!(
            matches!(regime, VolRegime::High | VolRegime::Extreme),
            "Expected High/Extreme, got {:?}",
            regime
        );
    }
}

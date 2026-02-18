use crate::models::candle::IndicatorEngine;
use crate::models::signal::{BiasDirection, BiasSignal};
use chrono::Utc;

/// Bias detector combining multiple signals to determine directional bias.
///
/// Signals and weights:
///   - 1-minute momentum:    0.30
///   - Short-term trend:     0.25
///   - Order flow delta:     0.20
///   - Funding rate:         0.10
///   - Liquidation cascade:  0.15
pub struct BiasDetector {
    min_confidence: f64,
}

impl BiasDetector {
    pub fn new(min_confidence: f64) -> Self {
        Self { min_confidence }
    }

    /// Compute bias signal from available data.
    ///
    /// - `indicators`: candle-based indicator engine with recent data
    /// - `funding_rate`: current perpetual funding rate (positive = longs pay)
    /// - `net_liquidations`: long_liqs - short_liqs in USD over last 60s
    ///   (positive = more longs liquidated = bearish pressure)
    pub fn detect(
        &self,
        indicators: &IndicatorEngine,
        funding_rate: f64,
        net_liquidations: f64,
    ) -> BiasSignal {
        // 1. Momentum (weight: 0.30)
        // Normalize: map raw momentum to [-1, 1] range
        let momentum_raw = indicators.momentum_pct().unwrap_or(0.0);
        let momentum_score = Self::normalize_momentum(momentum_raw);

        // 2. Trend (weight: 0.25)
        // EMA(5) - EMA(20), normalized
        let trend_raw = indicators.trend_signal().unwrap_or(0.0);
        let latest_price = indicators.latest().map(|c| c.close).unwrap_or(1.0);
        let trend_score = if latest_price > 0.0 {
            (trend_raw / latest_price * 100.0).clamp(-1.0, 1.0)
        } else {
            0.0
        };

        // 3. Order flow (weight: 0.20)
        let flow_raw = indicators.order_flow_delta(3).unwrap_or(0.0); // last 3 candles
        let total_volume = indicators
            .latest()
            .map(|c| c.buy_volume + c.sell_volume)
            .unwrap_or(1.0)
            .max(1.0);
        let flow_score = (flow_raw / total_volume / 3.0).clamp(-1.0, 1.0);

        // 4. Funding rate (weight: 0.10)
        // Mild positive = continuation UP. Extreme positive = contrarian DOWN.
        let funding_score = if funding_rate.abs() > 0.0005 {
            // Extreme funding → contrarian signal
            -funding_rate.signum() * 0.8
        } else {
            // Normal funding → continuation signal
            funding_rate.signum() * (funding_rate.abs() / 0.0005).min(1.0)
        };

        // 5. Liquidation cascade (weight: 0.15)
        // net_liquidations > 0 means longs being liquidated (bearish cascade)
        // Normalize: assume $5M is a significant cascade event
        let liq_score = (-net_liquidations / 5_000_000.0).clamp(-1.0, 1.0);

        // Weighted composite [-1.0, +1.0]
        let composite = momentum_score * 0.30
            + trend_score * 0.25
            + flow_score * 0.20
            + funding_score * 0.10
            + liq_score * 0.15;

        let (direction, confidence) = if composite.abs() > self.min_confidence {
            let dir = if composite > 0.0 {
                BiasDirection::Up
            } else {
                BiasDirection::Down
            };
            (dir, composite.abs().min(1.0))
        } else {
            (BiasDirection::Neutral, 0.0)
        };

        BiasSignal {
            direction,
            confidence,
            momentum_score,
            trend_score,
            flow_score,
            funding_score,
            liquidation_score: liq_score,
            timestamp: Utc::now(),
        }
    }

    /// Normalize momentum percentage to [-1, 1] range.
    /// 0.15% move maps to ~0.5, 0.30% maps to ~1.0
    fn normalize_momentum(pct_move: f64) -> f64 {
        // Sigmoid-like mapping: tanh(pct_move / 0.003)
        // 0.1% → tanh(0.33) ≈ 0.32
        // 0.2% → tanh(0.67) ≈ 0.58
        // 0.3% → tanh(1.0)  ≈ 0.76
        // 0.5% → tanh(1.67) ≈ 0.93
        (pct_move / 0.003).tanh()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::candle::{Candle, IndicatorEngine};
    use chrono::Utc;

    fn make_candle(close: f64, buy_vol: f64, sell_vol: f64) -> Candle {
        Candle {
            open: close - 10.0,
            high: close + 5.0,
            low: close - 15.0,
            close,
            volume: buy_vol + sell_vol,
            buy_volume: buy_vol,
            sell_volume: sell_vol,
            trades: 100,
            open_time: Utc::now(),
            close_time: Utc::now(),
        }
    }

    #[test]
    fn test_neutral_in_flat_market() {
        let mut engine = IndicatorEngine::new(100);
        for i in 0..30 {
            engine.push(make_candle(100_000.0, 50.0, 50.0));
        }

        let detector = BiasDetector::new(0.35);
        let signal = detector.detect(&engine, 0.0, 0.0);
        assert_eq!(signal.direction, BiasDirection::Neutral);
    }

    #[test]
    fn test_bullish_on_uptrend() {
        let mut engine = IndicatorEngine::new(100);
        for i in 0..30 {
            let price = 100_000.0 + (i as f64 * 50.0); // Steady uptrend
            engine.push(make_candle(price, 80.0, 20.0)); // Buy-heavy flow
        }

        let detector = BiasDetector::new(0.20);
        let signal = detector.detect(&engine, 0.0001, 0.0);
        assert_eq!(signal.direction, BiasDirection::Up);
        assert!(signal.confidence > 0.2);
    }
}

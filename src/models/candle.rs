use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub buy_volume: f64,
    pub sell_volume: f64,
    pub trades: u64,
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
}

impl Candle {
    pub fn true_range(&self, prev_close: Option<f64>) -> f64 {
        let hl = self.high - self.low;
        match prev_close {
            Some(pc) => {
                let hc = (self.high - pc).abs();
                let lc = (self.low - pc).abs();
                hl.max(hc).max(lc)
            }
            None => hl,
        }
    }

    pub fn body(&self) -> f64 {
        (self.close - self.open).abs()
    }

    pub fn is_bullish(&self) -> bool {
        self.close > self.open
    }

    pub fn order_flow_delta(&self) -> f64 {
        self.buy_volume - self.sell_volume
    }

    pub fn order_flow_imbalance(&self) -> f64 {
        let total = self.buy_volume + self.sell_volume;
        if total == 0.0 {
            return 0.0;
        }
        (self.buy_volume - self.sell_volume) / total
    }
}

/// Rolling indicator calculator using a ring buffer of candles.
#[derive(Debug)]
pub struct IndicatorEngine {
    candles: VecDeque<Candle>,
    max_size: usize,
    atr_period: usize,
    ema_fast: usize,
    ema_slow: usize,
    bbw_period: usize,
    bbw_std: f64,
}

impl IndicatorEngine {
    pub fn new(max_size: usize) -> Self {
        Self {
            candles: VecDeque::with_capacity(max_size),
            max_size,
            atr_period: 14,
            ema_fast: 5,
            ema_slow: 20,
            bbw_period: 20,
            bbw_std: 2.0,
        }
    }

    pub fn push(&mut self, candle: Candle) {
        if self.candles.len() >= self.max_size {
            self.candles.pop_front();
        }
        self.candles.push_back(candle);
    }

    pub fn len(&self) -> usize {
        self.candles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candles.is_empty()
    }

    pub fn latest(&self) -> Option<&Candle> {
        self.candles.back()
    }

    /// Calculate 1-minute ATR over the last `period` candles
    pub fn atr(&self, period: usize) -> Option<f64> {
        if self.candles.len() < period + 1 {
            return None;
        }
        let start = self.candles.len() - period;
        let mut sum_tr = 0.0;
        for i in start..self.candles.len() {
            let prev_close = if i > 0 {
                Some(self.candles[i - 1].close)
            } else {
                None
            };
            sum_tr += self.candles[i].true_range(prev_close);
        }
        Some(sum_tr / period as f64)
    }

    /// Simple ATR using default period
    pub fn atr_default(&self) -> Option<f64> {
        self.atr(self.atr_period)
    }

    /// EMA calculation
    fn ema(values: &[f64], period: usize) -> Option<f64> {
        if values.len() < period {
            return None;
        }
        let multiplier = 2.0 / (period as f64 + 1.0);
        let mut ema = values[..period].iter().sum::<f64>() / period as f64;
        for &val in &values[period..] {
            ema = (val - ema) * multiplier + ema;
        }
        Some(ema)
    }

    /// EMA(fast) - EMA(slow) on close prices = trend signal
    pub fn trend_signal(&self) -> Option<f64> {
        let closes: Vec<f64> = self.candles.iter().map(|c| c.close).collect();
        let ema_fast = Self::ema(&closes, self.ema_fast)?;
        let ema_slow = Self::ema(&closes, self.ema_slow)?;
        Some(ema_fast - ema_slow)
    }

    /// Bollinger Band Width (BBW) for compression/breakout detection
    pub fn bbw(&self) -> Option<f64> {
        if self.candles.len() < self.bbw_period {
            return None;
        }
        let start = self.candles.len() - self.bbw_period;
        let closes: Vec<f64> = self.candles.iter().skip(start).map(|c| c.close).collect();
        let mean = closes.iter().sum::<f64>() / closes.len() as f64;
        let variance = closes.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / closes.len() as f64;
        let std_dev = variance.sqrt();
        let upper = mean + self.bbw_std * std_dev;
        let lower = mean - self.bbw_std * std_dev;
        if mean == 0.0 {
            return None;
        }
        Some((upper - lower) / mean)
    }

    /// BBW percentile rank over lookback period
    pub fn bbw_percentile(&self, lookback: usize) -> Option<f64> {
        if self.candles.len() < self.bbw_period + lookback {
            return None;
        }
        let current_bbw = self.bbw()?;

        let mut bbw_history = Vec::with_capacity(lookback);
        for i in 0..lookback {
            let end = self.candles.len() - i;
            if end < self.bbw_period {
                break;
            }
            let start = end - self.bbw_period;
            let closes: Vec<f64> = self.candles.iter().skip(start).take(self.bbw_period).map(|c| c.close).collect();
            let mean = closes.iter().sum::<f64>() / closes.len() as f64;
            if mean == 0.0 {
                continue;
            }
            let variance = closes.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / closes.len() as f64;
            let std_dev = variance.sqrt();
            let upper = mean + self.bbw_std * std_dev;
            let lower = mean - self.bbw_std * std_dev;
            bbw_history.push((upper - lower) / mean);
        }

        if bbw_history.is_empty() {
            return None;
        }

        let count_below = bbw_history.iter().filter(|&&b| b < current_bbw).count();
        Some(count_below as f64 / bbw_history.len() as f64 * 100.0)
    }

    /// Rolling order flow delta over last N candles
    pub fn order_flow_delta(&self, period: usize) -> Option<f64> {
        if self.candles.len() < period {
            return None;
        }
        let start = self.candles.len() - period;
        let delta: f64 = self.candles.iter().skip(start).map(|c| c.order_flow_delta()).sum();
        Some(delta)
    }

    /// 1-minute momentum: latest close - previous close
    pub fn momentum_1m(&self) -> Option<f64> {
        if self.candles.len() < 2 {
            return None;
        }
        let curr = self.candles[self.candles.len() - 1].close;
        let prev = self.candles[self.candles.len() - 2].close;
        Some(curr - prev)
    }

    /// Percentage momentum
    pub fn momentum_pct(&self) -> Option<f64> {
        if self.candles.len() < 2 {
            return None;
        }
        let curr = self.candles[self.candles.len() - 1].close;
        let prev = self.candles[self.candles.len() - 2].close;
        if prev == 0.0 {
            return None;
        }
        Some((curr - prev) / prev)
    }
}

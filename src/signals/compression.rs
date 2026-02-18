use crate::models::candle::IndicatorEngine;
use crate::models::signal::{CompressionSignal, CompressionState};
use chrono::Utc;

/// Detects Bollinger Band Width compression and breakout transitions.
///
/// Compression → Breakout is the most profitable volatility pattern:
/// spreads explode, mispricings surge, arb edges widen.
pub struct CompressionDetector {
    previous_bbw: Option<f64>,
    compression_threshold_pct: f64, // BBW percentile below which = compression (default 10)
    breakout_expansion_ratio: f64,  // BBW must expand by this ratio to confirm breakout (default 1.5)
}

impl CompressionDetector {
    pub fn new() -> Self {
        Self {
            previous_bbw: None,
            compression_threshold_pct: 10.0,
            breakout_expansion_ratio: 1.5,
        }
    }

    /// Analyze current compression state from indicator engine.
    pub fn analyze(&mut self, indicators: &IndicatorEngine) -> Option<CompressionSignal> {
        let bbw_current = indicators.bbw()?;
        let bbw_percentile = indicators.bbw_percentile(100).unwrap_or(50.0);

        let state = if bbw_percentile < self.compression_threshold_pct {
            // Check if breaking out of compression
            if let Some(prev) = self.previous_bbw {
                if bbw_current > prev * self.breakout_expansion_ratio {
                    CompressionState::BreakoutDetected
                } else {
                    CompressionState::Compressing
                }
            } else {
                CompressionState::Compressing
            }
        } else {
            CompressionState::Normal
        };

        let bbw_previous = self.previous_bbw.unwrap_or(bbw_current);
        self.previous_bbw = Some(bbw_current);

        Some(CompressionSignal {
            state,
            bbw_percentile,
            bbw_current,
            bbw_previous,
            timestamp: Utc::now(),
        })
    }

    pub fn reset(&mut self) {
        self.previous_bbw = None;
    }
}

impl Default for CompressionDetector {
    fn default() -> Self {
        Self::new()
    }
}

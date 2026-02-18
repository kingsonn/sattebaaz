use crate::models::candle::IndicatorEngine;
use crate::models::market::Asset;
use crate::models::signal::VolRegime;

/// Classifies the current volatility regime based on ATR(1m) for a given asset.
pub struct VolatilityClassifier;

impl VolatilityClassifier {
    /// Classify volatility regime from the indicator engine.
    pub fn classify(asset: Asset, indicators: &IndicatorEngine) -> VolRegime {
        match indicators.atr_default() {
            Some(atr) => VolRegime::from_atr(asset, atr),
            None => {
                // Not enough data yet — assume LOW as conservative default
                VolRegime::Low
            }
        }
    }

    /// Classify from a raw ATR value.
    pub fn classify_raw(asset: Asset, atr_1m: f64) -> VolRegime {
        VolRegime::from_atr(asset, atr_1m)
    }

    /// Check if we're in a compression→breakout transition.
    /// This is the most profitable volatility pattern.
    pub fn is_breakout(indicators: &IndicatorEngine) -> bool {
        let Some(bbw_pct) = indicators.bbw_percentile(100) else {
            return false;
        };
        let Some(bbw_current) = indicators.bbw() else {
            return false;
        };

        // Need at least 2 candles of BBW history to detect expansion
        // For simplicity, check if BBW percentile was <10 recently and is now expanding
        // A full implementation would track BBW over time
        bbw_pct < 15.0 && bbw_current > 0.0 // Placeholder: needs previous BBW comparison
    }

    /// Check if we're in compression (low BBW percentile).
    pub fn is_compression(indicators: &IndicatorEngine) -> bool {
        indicators
            .bbw_percentile(100)
            .map(|pct| pct < 10.0)
            .unwrap_or(false)
    }
}

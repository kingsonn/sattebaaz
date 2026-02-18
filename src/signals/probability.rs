use statrs::distribution::{ContinuousCDF, Normal};

/// Price-to-implied-probability model using Black-Scholes-like approach.
///
/// For a binary "UP/DOWN from open" market:
///   P(UP at expiry) = Φ(d)
///   where d = pct_move / (σ_per_min × √(minutes_remaining))
pub struct ProbabilityModel {
    normal: Normal,
}

impl ProbabilityModel {
    pub fn new() -> Self {
        Self {
            normal: Normal::new(0.0, 1.0).expect("valid normal distribution"),
        }
    }

    /// Calculate fair probability that price will be UP at expiry.
    ///
    /// - `current_price`: current underlying price (e.g. Binance BTC/USDT)
    /// - `open_price`: reference price at market open
    /// - `minutes_remaining`: time until market resolution in minutes
    /// - `vol_per_min`: per-minute volatility (annual_vol / sqrt(525600))
    /// - `momentum_adj`: optional momentum adjustment factor [-0.1, 0.1]
    pub fn fair_prob_up(
        &self,
        current_price: f64,
        open_price: f64,
        minutes_remaining: f64,
        vol_per_min: f64,
        momentum_adj: f64,
    ) -> f64 {
        if minutes_remaining <= 0.0 {
            // Already resolved: check if price is above open
            return if current_price > open_price { 1.0 } else { 0.0 };
        }

        if open_price == 0.0 || vol_per_min == 0.0 {
            return 0.5;
        }

        let pct_move = (current_price - open_price) / open_price;
        let remaining_vol = vol_per_min * minutes_remaining.sqrt();

        if remaining_vol == 0.0 {
            return if pct_move > 0.0 { 1.0 } else { 0.0 };
        }

        let z_score = pct_move / remaining_vol;
        let adjusted_z = z_score + momentum_adj;

        // Clamp to [0.01, 0.99] to avoid extreme probabilities
        self.normal.cdf(adjusted_z).clamp(0.01, 0.99)
    }

    /// Calculate fair probability that price will be DOWN at expiry.
    pub fn fair_prob_down(
        &self,
        current_price: f64,
        open_price: f64,
        minutes_remaining: f64,
        vol_per_min: f64,
        momentum_adj: f64,
    ) -> f64 {
        1.0 - self.fair_prob_up(current_price, open_price, minutes_remaining, vol_per_min, momentum_adj)
    }

    /// Calculate the mispricing between our fair value and market price.
    ///
    /// Returns (yes_mispricing, no_mispricing).
    /// Positive = token is underpriced (buy opportunity).
    pub fn mispricing(
        &self,
        current_price: f64,
        open_price: f64,
        minutes_remaining: f64,
        vol_per_min: f64,
        momentum_adj: f64,
        yes_market_price: f64,
        no_market_price: f64,
    ) -> (f64, f64) {
        let fair_up = self.fair_prob_up(
            current_price,
            open_price,
            minutes_remaining,
            vol_per_min,
            momentum_adj,
        );
        let fair_down = 1.0 - fair_up;

        let yes_mispricing = fair_up - yes_market_price;
        let no_mispricing = fair_down - no_market_price;

        (yes_mispricing, no_mispricing)
    }

    /// Calculate Kelly optimal fraction for a binary bet.
    ///
    /// - `edge`: our estimated edge (fair_prob - market_price)
    /// - `market_price`: the price we'd pay for the token
    /// - `base_win_prob`: base estimated win probability
    /// - `kelly_fraction`: fractional Kelly multiplier (e.g. 0.25 for quarter-Kelly)
    pub fn kelly_size(
        &self,
        edge: f64,
        market_price: f64,
        base_win_prob: f64,
        kelly_fraction: f64,
    ) -> f64 {
        if market_price <= 0.0 || market_price >= 1.0 {
            return 0.0;
        }

        // Payout odds: if we buy at price P and it pays $1, odds = (1-P)/P
        let payout_odds = (1.0 - market_price) / market_price;

        // Adjusted win probability based on edge
        let win_prob = (base_win_prob + edge * 2.0).clamp(0.0, 0.95);
        let lose_prob = 1.0 - win_prob;

        // Kelly: f* = (b*p - q) / b
        let kelly = (payout_odds * win_prob - lose_prob) / payout_odds;

        if kelly <= 0.0 {
            return 0.0;
        }

        // Apply fractional Kelly for safety
        (kelly * kelly_fraction).clamp(0.0, 0.50)
    }
}

impl Default for ProbabilityModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fair_value_at_open() {
        let model = ProbabilityModel::new();
        // At open, price == reference → 50/50
        let prob = model.fair_prob_up(100_000.0, 100_000.0, 5.0, 0.000758, 0.0);
        assert!((prob - 0.5).abs() < 0.01, "At open, prob should be ~0.50, got {prob}");
    }

    #[test]
    fn test_fair_value_price_up() {
        let model = ProbabilityModel::new();
        // BTC moved +0.10% from open, 4 min remaining
        let open = 100_000.0;
        let current = 100_100.0; // +0.1%
        let prob = model.fair_prob_up(current, open, 4.0, 0.000758, 0.0);
        assert!(prob > 0.60, "With +0.1% move and 4 min left, prob should be >0.60, got {prob}");
        assert!(prob < 0.80, "Should not be too extreme, got {prob}");
    }

    #[test]
    fn test_fair_value_price_down() {
        let model = ProbabilityModel::new();
        let open = 100_000.0;
        let current = 99_800.0; // -0.2%
        let prob = model.fair_prob_up(current, open, 2.0, 0.000758, 0.0);
        assert!(prob < 0.15, "With -0.2% move and 2 min left, prob should be <0.15, got {prob}");
    }

    #[test]
    fn test_fair_value_near_expiry() {
        let model = ProbabilityModel::new();
        let open = 100_000.0;
        let current = 100_050.0; // +0.05%
        // With only 0.5 min (30s) remaining
        let prob = model.fair_prob_up(current, open, 0.5, 0.000758, 0.0);
        assert!(prob > 0.75, "Near expiry with positive drift should be high prob, got {prob}");
    }

    #[test]
    fn test_kelly_positive_edge() {
        let model = ProbabilityModel::new();
        let size = model.kelly_size(0.05, 0.45, 0.62, 0.25);
        assert!(size > 0.0, "Positive edge should give positive Kelly size");
        assert!(size < 0.50, "Should be bounded by max");
    }

    #[test]
    fn test_kelly_no_edge() {
        let model = ProbabilityModel::new();
        let size = model.kelly_size(-0.05, 0.50, 0.45, 0.25);
        assert_eq!(size, 0.0, "Negative edge should give zero size");
    }
}

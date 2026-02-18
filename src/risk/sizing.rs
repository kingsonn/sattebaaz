use crate::models::signal::VolRegime;

/// Position sizing utilities.
///
/// Implements Kelly criterion and capital-tier-based sizing.
pub struct Sizing;

impl Sizing {
    /// Capital-tier-based maximum position size as fraction of capital.
    pub fn max_position_fraction(capital: f64) -> f64 {
        match capital {
            c if c < 50.0 => 1.00,
            c if c < 500.0 => 0.50,
            c if c < 5_000.0 => 0.25,
            c if c < 50_000.0 => 0.10,
            _ => 0.10,
        }
    }

    /// Kelly optimal fraction for a binary bet.
    ///
    /// f* = (b*p - q) / b
    /// where b = payout odds, p = win probability, q = 1-p
    pub fn kelly_fraction(win_prob: f64, payout_odds: f64, fractional: f64) -> f64 {
        if payout_odds <= 0.0 || win_prob <= 0.0 || win_prob >= 1.0 {
            return 0.0;
        }

        let lose_prob = 1.0 - win_prob;
        let kelly = (payout_odds * win_prob - lose_prob) / payout_odds;

        if kelly <= 0.0 {
            return 0.0;
        }

        (kelly * fractional).clamp(0.0, 0.50)
    }

    /// Compute payout odds for a binary token bought at `price`.
    /// If price = 0.40, token pays $1 → odds = (1-0.40)/0.40 = 1.5
    pub fn payout_odds(price: f64) -> f64 {
        if price <= 0.0 || price >= 1.0 {
            return 0.0;
        }
        (1.0 - price) / price
    }

    /// Apply volatility regime cap to a position size.
    pub fn apply_vol_cap(size: f64, capital: f64, vol_regime: VolRegime) -> f64 {
        let cap = capital * vol_regime.position_size_cap();
        size.min(cap)
    }

    /// Apply risk manager size multiplier.
    pub fn apply_risk_mult(size: f64, multiplier: f64) -> f64 {
        size * multiplier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kelly_positive_edge() {
        // Win 65% of the time, 1.5:1 odds → strong positive Kelly
        let frac = Sizing::kelly_fraction(0.65, 1.5, 0.25);
        assert!(frac > 0.0);
        assert!(frac < 0.50);
    }

    #[test]
    fn test_kelly_no_edge() {
        // Win 40% at even money → negative expectancy → 0
        let frac = Sizing::kelly_fraction(0.40, 1.0, 0.25);
        assert_eq!(frac, 0.0);
    }

    #[test]
    fn test_payout_odds() {
        let odds = Sizing::payout_odds(0.40);
        assert!((odds - 1.5).abs() < 0.001);

        let odds = Sizing::payout_odds(0.50);
        assert!((odds - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_capital_tier() {
        assert_eq!(Sizing::max_position_fraction(10.0), 1.00);
        assert_eq!(Sizing::max_position_fraction(100.0), 0.50);
        assert_eq!(Sizing::max_position_fraction(1000.0), 0.25);
        assert_eq!(Sizing::max_position_fraction(10000.0), 0.10);
    }
}

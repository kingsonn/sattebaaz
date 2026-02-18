use crate::models::market::{Asset, Duration, Market};
use chrono::Utc;

/// Generates market slugs and discovers active markets.
///
/// Polymarket short-duration markets follow the pattern:
///   {asset}-updown-{duration}-{unix_timestamp}
///
/// Where unix_timestamp is the interval start time, aligned to clean boundaries.
pub struct MarketDiscovery;

impl MarketDiscovery {
    /// Generate the slug for the currently active market.
    pub fn current_slug(asset: Asset, duration: Duration) -> String {
        let now = Utc::now().timestamp() as u64;
        let interval = duration.interval_seconds();
        let interval_start = (now / interval) * interval;
        Market::generate_slug(asset, duration, interval_start)
    }

    /// Generate slugs for the next N upcoming markets.
    pub fn upcoming_slugs(asset: Asset, duration: Duration, count: usize) -> Vec<String> {
        let now = Utc::now().timestamp() as u64;
        let interval = duration.interval_seconds();
        let current_start = (now / interval) * interval;

        (0..count)
            .map(|i| {
                let ts = current_start + (i as u64 * interval);
                Market::generate_slug(asset, duration, ts)
            })
            .collect()
    }

    /// Generate slugs for recent + current + upcoming markets.
    /// Useful for scanning across a window (to catch late-discovered markets).
    pub fn scan_window_slugs(
        asset: Asset,
        duration: Duration,
        past_count: usize,
        future_count: usize,
    ) -> Vec<(String, u64)> {
        let now = Utc::now().timestamp() as u64;
        let interval = duration.interval_seconds();
        let current_start = (now / interval) * interval;

        let mut slugs = Vec::new();

        // Past intervals
        for i in (1..=past_count).rev() {
            let ts = current_start - (i as u64 * interval);
            slugs.push((Market::generate_slug(asset, duration, ts), ts));
        }

        // Current interval
        slugs.push((Market::generate_slug(asset, duration, current_start), current_start));

        // Future intervals
        for i in 1..=future_count {
            let ts = current_start + (i as u64 * interval);
            slugs.push((Market::generate_slug(asset, duration, ts), ts));
        }

        slugs
    }

    /// Calculate time remaining in the current interval.
    pub fn time_remaining_in_current(duration: Duration) -> f64 {
        let now = Utc::now().timestamp() as u64;
        let interval = duration.interval_seconds();
        let current_start = (now / interval) * interval;
        let current_end = current_start + interval;
        (current_end - now) as f64
    }

    /// Calculate seconds until the next interval starts.
    pub fn seconds_until_next(duration: Duration) -> f64 {
        Self::time_remaining_in_current(duration)
    }

    /// Get all asset/duration combinations we trade.
    pub fn all_market_types() -> Vec<(Asset, Duration)> {
        vec![
            (Asset::BTC, Duration::FiveMin),
            (Asset::BTC, Duration::FifteenMin),
            (Asset::ETH, Duration::FifteenMin),
            (Asset::SOL, Duration::FifteenMin),
            (Asset::XRP, Duration::FifteenMin),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slug_generation() {
        let slug = Market::generate_slug(Asset::BTC, Duration::FiveMin, 1770933900);
        assert_eq!(slug, "btc-updown-5m-1770933900");
    }

    #[test]
    fn test_slug_15m() {
        let slug = Market::generate_slug(Asset::ETH, Duration::FifteenMin, 1768502700);
        assert_eq!(slug, "eth-updown-15m-1768502700");
    }

    #[test]
    fn test_current_slug_not_empty() {
        let slug = MarketDiscovery::current_slug(Asset::BTC, Duration::FiveMin);
        assert!(slug.starts_with("btc-updown-5m-"));
    }

    #[test]
    fn test_upcoming_slugs() {
        let slugs = MarketDiscovery::upcoming_slugs(Asset::BTC, Duration::FiveMin, 3);
        assert_eq!(slugs.len(), 3);
        for s in &slugs {
            assert!(s.starts_with("btc-updown-5m-"));
        }
    }

    #[test]
    fn test_scan_window() {
        let window = MarketDiscovery::scan_window_slugs(Asset::BTC, Duration::FiveMin, 2, 2);
        assert_eq!(window.len(), 5); // 2 past + 1 current + 2 future
    }

    #[test]
    fn test_all_market_types() {
        let types = MarketDiscovery::all_market_types();
        assert_eq!(types.len(), 5);
    }
}

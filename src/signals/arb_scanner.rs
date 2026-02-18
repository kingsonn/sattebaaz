use crate::models::market::OrderBook;
use crate::models::signal::{ArbSignal, VolRegime};
use chrono::Utc;

/// Scans for YES+NO < $1.00 arbitrage opportunities.
///
/// This is an O(1) operation per market: check best ask on both sides, sum them.
/// Should run on every orderbook update (~20-100ms).
pub struct ArbScanner;

impl ArbScanner {
    /// Scan a single market for arbitrage.
    ///
    /// Returns `Some(ArbSignal)` if a profitable arb exists.
    pub fn scan(
        yes_book: &OrderBook,
        no_book: &OrderBook,
        vol_regime: VolRegime,
        min_profit: f64,
    ) -> Option<ArbSignal> {
        let (yes_ask_price, _yes_ask_size) = yes_book.best_ask()?;
        let (no_ask_price, _no_ask_size) = no_book.best_ask()?;

        let yes_ask = yes_ask_price.to_string().parse::<f64>().ok()?;
        let no_ask = no_ask_price.to_string().parse::<f64>().ok()?;

        let combined = yes_ask + no_ask;
        let edge = 1.0 - combined;

        let min_edge = vol_regime.arb_min_edge();

        if edge < min_edge {
            return None;
        }

        // Calculate executable size (minimum depth on both sides within 2c tolerance)
        let tolerance = rust_decimal::Decimal::new(2, 2); // 0.02
        let yes_depth = yes_book.ask_depth_within(tolerance);
        let no_depth = no_book.ask_depth_within(tolerance);
        let executable = yes_depth.min(no_depth);
        let executable_f64 = executable.to_string().parse::<f64>().unwrap_or(0.0);

        // Conservative fill rate based on volatility
        let fill_penalty = vol_regime.fill_probability_penalty();
        let expected_fill = executable_f64 * fill_penalty;
        let expected_profit = expected_fill * edge;

        if expected_profit < min_profit {
            return None;
        }

        Some(ArbSignal {
            yes_ask,
            no_ask,
            combined,
            edge,
            executable_size: executable_f64,
            expected_profit,
            timestamp: Utc::now(),
        })
    }

    /// Quick check without full signal computation.
    /// Returns true if combined price < threshold.
    pub fn quick_check(
        yes_book: &OrderBook,
        no_book: &OrderBook,
        max_combined: f64,
    ) -> bool {
        let yes_ask = match yes_book.best_ask() {
            Some((p, _)) => p.to_string().parse::<f64>().unwrap_or(1.0),
            None => return false,
        };
        let no_ask = match no_book.best_ask() {
            Some((p, _)) => p.to_string().parse::<f64>().unwrap_or(1.0),
            None => return false,
        };
        yes_ask + no_ask < max_combined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::market::OrderBook;
    use rust_decimal::Decimal;

    fn make_book(token_id: &str, best_bid: f64, bid_size: f64, best_ask: f64, ask_size: f64) -> OrderBook {
        let mut book = OrderBook::new(token_id.to_string());
        book.bids.insert(
            Decimal::from_f64_retain(best_bid).unwrap(),
            Decimal::from_f64_retain(bid_size).unwrap(),
        );
        book.asks.insert(
            Decimal::from_f64_retain(best_ask).unwrap(),
            Decimal::from_f64_retain(ask_size).unwrap(),
        );
        book
    }

    #[test]
    fn test_arb_detected() {
        let yes_book = make_book("yes", 0.43, 100.0, 0.45, 100.0);
        let no_book = make_book("no", 0.41, 100.0, 0.43, 80.0);
        // combined = 0.45 + 0.43 = 0.88, edge = 0.12

        let signal = ArbScanner::scan(&yes_book, &no_book, VolRegime::Medium, 0.10);
        assert!(signal.is_some(), "Should detect arb with 12% edge");

        let sig = signal.unwrap();
        assert!((sig.edge - 0.12).abs() < 0.001);
        assert!(sig.expected_profit > 0.0);
    }

    #[test]
    fn test_no_arb_when_fair() {
        let yes_book = make_book("yes", 0.48, 100.0, 0.52, 100.0);
        let no_book = make_book("no", 0.48, 100.0, 0.52, 100.0);
        // combined = 0.52 + 0.52 = 1.04, no arb

        let signal = ArbScanner::scan(&yes_book, &no_book, VolRegime::Medium, 0.10);
        assert!(signal.is_none(), "No arb when combined > 1.0");
    }

    #[test]
    fn test_quick_check() {
        let yes_book = make_book("yes", 0.43, 100.0, 0.45, 100.0);
        let no_book = make_book("no", 0.41, 100.0, 0.43, 80.0);
        assert!(ArbScanner::quick_check(&yes_book, &no_book, 0.95));
        assert!(!ArbScanner::quick_check(&yes_book, &no_book, 0.85));
    }
}

use crate::config::StrategyConfig;
use crate::models::market::{LifecyclePhase, Market, OrderBook, Side};
use crate::models::order::{OrderIntent, OrderSide, OrderType};
use crate::models::signal::{ArbSignal, VolRegime};
use crate::signals::arb_scanner::ArbScanner;
use rust_decimal::Decimal;
use tracing::{debug, info};

/// Pure YES+NO arbitrage engine.
///
/// Detects when YES_ask + NO_ask < $1.00 and buys both sides
/// to lock in risk-free profit. The simplest and safest strategy.
pub struct PureArbEngine {
    config: StrategyConfig,
}

impl PureArbEngine {
    pub fn new(config: StrategyConfig) -> Self {
        Self { config }
    }

    /// Evaluate and produce arb orders if opportunity exists.
    pub fn evaluate(
        &self,
        market: &Market,
        yes_book: &OrderBook,
        no_book: &OrderBook,
        vol_regime: VolRegime,
        available_capital: f64,
    ) -> Vec<OrderIntent> {
        let phase = market.lifecycle_phase();

        // Don't arb in lockout (too close to resolution, fills may not settle)
        if matches!(phase, LifecyclePhase::Lockout | LifecyclePhase::Resolved) {
            return Vec::new();
        }

        let signal = match ArbScanner::scan(
            yes_book,
            no_book,
            vol_regime,
            self.config.arb_min_expected_profit,
        ) {
            Some(s) => s,
            None => return Vec::new(),
        };

        if signal.edge < self.config.arb_min_edge {
            return Vec::new();
        }

        self.build_arb_orders(market, &signal, vol_regime, available_capital)
    }

    fn build_arb_orders(
        &self,
        market: &Market,
        signal: &ArbSignal,
        _vol_regime: VolRegime,
        available_capital: f64,
    ) -> Vec<OrderIntent> {
        // Position sizing by capital tier
        let size_cap_pct = Self::size_cap_for_capital(available_capital);
        let max_from_capital = available_capital * size_cap_pct / signal.combined;
        let size = signal.executable_size.min(max_from_capital).max(0.0);

        if size < 1.0 {
            debug!("Arb size too small: {size:.2}");
            return Vec::new();
        }

        let size_dec = Decimal::from_f64_retain(size).unwrap_or(Decimal::ZERO);
        let yes_price = Decimal::from_f64_retain(signal.yes_ask).unwrap_or(Decimal::ZERO);
        let no_price = Decimal::from_f64_retain(signal.no_ask).unwrap_or(Decimal::ZERO);

        info!(
            "ARB: market={} YES@{:.3}+NO@{:.3}={:.3} edge={:.3} size={:.1} profit={:.2}",
            market.slug, signal.yes_ask, signal.no_ask, signal.combined, signal.edge, size,
            size * signal.edge
        );

        vec![
            OrderIntent {
                token_id: market.yes_token_id.clone(),
                market_side: Side::Yes,
                order_side: OrderSide::Buy,
                price: yes_price,
                size: size_dec,
                order_type: OrderType::FAK,
                post_only: false,
                expiration: None,
                strategy_tag: "arb_yes".into(),
            },
            OrderIntent {
                token_id: market.no_token_id.clone(),
                market_side: Side::No,
                order_side: OrderSide::Buy,
                price: no_price,
                size: size_dec,
                order_type: OrderType::FAK,
                post_only: false,
                expiration: None,
                strategy_tag: "arb_no".into(),
            },
        ]
    }

    /// Capital-tier-based position sizing.
    fn size_cap_for_capital(capital: f64) -> f64 {
        match capital {
            c if c < 50.0 => 1.00,   // $5-50: all-in on every arb
            c if c < 500.0 => 0.50,  // $50-500: half capital
            c if c < 5_000.0 => 0.25, // $500-5K: quarter
            c if c < 50_000.0 => 0.10, // $5K-50K: 10%
            _ => 0.10,                 // $50K+: 10% or depth-limited
        }
    }
}

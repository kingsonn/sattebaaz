use crate::config::StrategyConfig;
use crate::models::market::{LifecyclePhase, Market, OrderBook, Side};
use crate::models::order::{OrderIntent, OrderSide, OrderType};
use crate::models::signal::{BiasDirection, MomentumSignal, VolRegime};
use rust_decimal::Decimal;
use tracing::info;

/// Probability momentum capture engine.
///
/// Detects when Polymarket probability is accelerating in one direction,
/// enters in the direction of acceleration, exits on exhaustion.
pub struct MomentumCaptureEngine {
    config: StrategyConfig,
}

impl MomentumCaptureEngine {
    pub fn new(config: StrategyConfig) -> Self {
        Self { config }
    }

    /// Evaluate momentum opportunity and produce order if signal is strong.
    pub fn evaluate(
        &self,
        market: &Market,
        yes_book: &OrderBook,
        no_book: &OrderBook,
        signal: &MomentumSignal,
        vol_regime: VolRegime,
        available_capital: f64,
    ) -> Vec<OrderIntent> {
        let phase = market.lifecycle_phase();

        // Only trade in prime and mature phases
        if !matches!(
            phase,
            LifecyclePhase::EarlyArbs | LifecyclePhase::PrimeZone | LifecyclePhase::MaturePhase
        ) {
            return Vec::new();
        }

        // Check minimum signal thresholds
        if !signal.is_entry_signal() {
            return Vec::new();
        }

        // Don't enter on DEAD vol (no real momentum)
        if matches!(vol_regime, VolRegime::Dead) {
            return Vec::new();
        }

        let direction = signal.direction();
        if direction == BiasDirection::Neutral {
            return Vec::new();
        }

        // Select token and book based on direction
        let (token_id, book, side) = match direction {
            BiasDirection::Up => (&market.yes_token_id, yes_book, Side::Yes),
            BiasDirection::Down => (&market.no_token_id, no_book, Side::No),
            BiasDirection::Neutral => return Vec::new(),
        };

        let (ask_price, _) = match book.best_ask() {
            Some(p) => p,
            None => return Vec::new(),
        };
        let ask_f64 = ask_price.to_string().parse::<f64>().unwrap_or(1.0);

        // Size calculation: scale with divergence and momentum strength
        let base = available_capital * 0.10;
        let divergence_mult = (signal.divergence.abs() / 0.05).min(2.0);
        let momentum_mult = (signal.momentum.abs() / 0.005).min(1.5);
        let mut size = base * divergence_mult * momentum_mult;

        // Cap by vol regime
        let max_size = available_capital * vol_regime.position_size_cap();
        size = size.min(max_size);

        if size < 0.50 {
            return Vec::new();
        }

        let side_str = match side {
            Side::Yes => "YES",
            Side::No => "NO",
        };

        info!(
            "MOMENTUM: market={} buy {side_str}@{ask_f64:.3} momentum={:.4} divergence={:.3} size={size:.1}",
            market.slug, signal.momentum, signal.divergence
        );

        vec![OrderIntent {
            token_id: token_id.clone(),
            market_side: side,
            order_side: OrderSide::Buy,
            price: ask_price,
            size: Decimal::from_f64_retain(size).unwrap_or(Decimal::ZERO),
            order_type: OrderType::FAK,
            post_only: false,
            expiration: None,
            strategy_tag: "momentum".into(),
        }]
    }

    /// Check if we should exit an existing momentum position.
    ///
    /// Returns true if position should be closed.
    pub fn should_exit(
        &self,
        market: &Market,
        signal: &MomentumSignal,
    ) -> bool {
        // Exit on exhaustion
        if signal.exhausted {
            info!("MOMENTUM EXIT: exhaustion detected on {}", market.slug);
            return true;
        }

        // If < 45 seconds remaining, hold to resolution (don't try to exit)
        if market.time_remaining_secs() < 45.0 {
            return false;
        }

        // Exit if momentum reversed (sign flipped)
        // This would need position-tracking context; placeholder for now
        false
    }
}

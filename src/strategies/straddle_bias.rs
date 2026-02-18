use crate::config::StrategyConfig;
use crate::models::market::{LifecyclePhase, Market, OrderBook, Side};
use crate::models::order::{OrderIntent, OrderSide, OrderType};
use crate::models::signal::{ArbSignal, BiasSignal, VolRegime};
use rust_decimal::Decimal;
use tracing::{debug, info};

/// The core strategy: Straddle-First Bias Engine.
///
/// Phase 1: Buy BOTH YES and NO when combined price < $1.00 (guaranteed profit).
/// Phase 2: If directional bias detected, amplify by buying more of the favored side
///          at the point of maximum counter-movement.
pub struct StraddleBiasEngine {
    config: StrategyConfig,
}

impl StraddleBiasEngine {
    pub fn new(config: StrategyConfig) -> Self {
        Self { config }
    }

    /// Evaluate whether to enter a straddle on this market.
    ///
    /// Returns a vec of OrderIntents (0, 2, or 3 orders).
    pub fn evaluate(
        &self,
        market: &Market,
        yes_book: &OrderBook,
        no_book: &OrderBook,
        arb_signal: Option<&ArbSignal>,
        bias_signal: Option<&BiasSignal>,
        vol_regime: VolRegime,
        available_capital: f64,
    ) -> Vec<OrderIntent> {
        let mut orders = Vec::new();

        let phase = market.lifecycle_phase();

        // Don't trade in lockout or if already resolved
        if matches!(phase, LifecyclePhase::Lockout | LifecyclePhase::Resolved) {
            return orders;
        }

        // === PHASE 1: STRADDLE ===
        if let Some(arb) = arb_signal {
            if arb.combined < self.config.straddle_max_combined {
                let straddle_orders =
                    self.build_straddle(market, yes_book, no_book, arb, vol_regime, available_capital);
                orders.extend(straddle_orders);
            }
        }

        // === PHASE 2: BIAS AMPLIFICATION ===
        // Only if we're past the alpha window and have a bias signal
        if matches!(
            phase,
            LifecyclePhase::EarlyArbs | LifecyclePhase::PrimeZone | LifecyclePhase::MaturePhase
        ) {
            if let Some(bias) = bias_signal {
                if bias.is_actionable() && bias.confidence > self.config.bias_min_confidence {
                    if let Some(amp_order) = self.build_bias_amplification(
                        market,
                        yes_book,
                        no_book,
                        bias,
                        vol_regime,
                        available_capital,
                        arb_signal.map(|a| a.edge * a.executable_size).unwrap_or(0.0),
                    ) {
                        orders.push(amp_order);
                    }
                }
            }
        }

        orders
    }

    /// Build the straddle leg orders (buy YES + buy NO).
    fn build_straddle(
        &self,
        market: &Market,
        _yes_book: &OrderBook,
        _no_book: &OrderBook,
        arb: &ArbSignal,
        _vol_regime: VolRegime,
        available_capital: f64,
    ) -> Vec<OrderIntent> {
        let mut orders = Vec::new();

        // Size: min of both sides' depth, capped by capital allocation
        let max_capital = available_capital * self.config.straddle_max_capital_pct;
        let max_affordable = max_capital / arb.combined;
        let size = arb.executable_size.min(max_affordable).max(0.0);

        if size < 1.0 {
            debug!(
                "Straddle size too small: {size:.2} (capital={available_capital:.2}, combined={:.3})",
                arb.combined
            );
            return orders;
        }

        let size_dec = Decimal::from_f64_retain(size).unwrap_or(Decimal::ZERO);
        let yes_price = Decimal::from_f64_retain(arb.yes_ask).unwrap_or(Decimal::ZERO);
        let no_price = Decimal::from_f64_retain(arb.no_ask).unwrap_or(Decimal::ZERO);

        info!(
            "STRADDLE: market={} YES@{} + NO@{} = {:.3} | edge={:.3} | size={:.1}",
            market.slug, arb.yes_ask, arb.no_ask, arb.combined, arb.edge, size
        );

        // YES leg
        orders.push(OrderIntent {
            token_id: market.yes_token_id.clone(),
            market_side: Side::Yes,
            order_side: OrderSide::Buy,
            price: yes_price,
            size: size_dec,
            order_type: OrderType::FAK, // Fill what you can, cancel rest
            post_only: false,
            expiration: None,
            strategy_tag: "straddle_yes".into(),
        });

        // NO leg
        orders.push(OrderIntent {
            token_id: market.no_token_id.clone(),
            market_side: Side::No,
            order_side: OrderSide::Buy,
            price: no_price,
            size: size_dec,
            order_type: OrderType::FAK,
            post_only: false,
            expiration: None,
            strategy_tag: "straddle_no".into(),
        });

        orders
    }

    /// Build the directional bias amplification order.
    fn build_bias_amplification(
        &self,
        market: &Market,
        yes_book: &OrderBook,
        no_book: &OrderBook,
        bias: &BiasSignal,
        _vol_regime: VolRegime,
        available_capital: f64,
        guaranteed_straddle_profit: f64,
    ) -> Option<OrderIntent> {
        let favored_side = bias.favored_side()?;
        let (token_id, book) = match favored_side {
            Side::Yes => (&market.yes_token_id, yes_book),
            Side::No => (&market.no_token_id, no_book),
        };

        let (ask_price, _) = book.best_ask()?;
        let ask_f64 = ask_price.to_string().parse::<f64>().ok()?;

        // Size the directional bet
        // Cap at: 15% of capital, 3× straddle profit, or available depth
        let max_from_capital = available_capital * self.config.bias_max_capital_pct;
        let max_from_straddle = if guaranteed_straddle_profit > 0.0 {
            guaranteed_straddle_profit * 3.0
        } else {
            max_from_capital
        };
        let max_from_depth = {
            let tolerance = rust_decimal::Decimal::new(1, 2); // 0.01
            let depth = book.ask_depth_within(tolerance);
            depth.to_string().parse::<f64>().unwrap_or(0.0)
        };

        let size = max_from_capital
            .min(max_from_straddle)
            .min(max_from_depth)
            .max(0.0);

        if size < 1.0 {
            return None;
        }

        // Scale size by confidence
        let confidence_mult = (bias.confidence - self.config.bias_min_confidence)
            / (1.0 - self.config.bias_min_confidence);
        let final_size = size * confidence_mult.clamp(0.3, 1.0);

        if final_size < 0.5 {
            return None;
        }

        info!(
            "BIAS AMP: market={} side={:?} confidence={:.2} price={} size={:.1}",
            market.slug, favored_side, bias.confidence, ask_f64, final_size
        );

        Some(OrderIntent {
            token_id: token_id.clone(),
            market_side: favored_side,
            order_side: OrderSide::Buy,
            price: ask_price,
            size: Decimal::from_f64_retain(final_size).unwrap_or(Decimal::ZERO),
            order_type: OrderType::FAK,
            post_only: false,
            expiration: None,
            strategy_tag: "bias_amplify".into(),
        })
    }
}

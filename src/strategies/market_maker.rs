use crate::config::StrategyConfig;
use crate::models::market::{LifecyclePhase, Market, OrderBook, Side};
use crate::models::order::{OrderIntent, OrderSide, OrderType};
use crate::models::signal::VolRegime;
use crate::signals::probability::ProbabilityModel;
use rust_decimal::Decimal;
use tracing::debug;

/// Micro market-making engine.
///
/// Posts two-sided quotes (bid + ask) around fair value.
/// Captures spread on thin books. Manages inventory via quote skew.
/// Pulls quotes on adverse selection signals.
pub struct MarketMakerEngine {
    config: StrategyConfig,
    prob_model: ProbabilityModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdverseSelectionAction {
    Normal,
    WidenSpread,
    PullQuotes,
}

impl MarketMakerEngine {
    pub fn new(config: StrategyConfig) -> Self {
        Self {
            config,
            prob_model: ProbabilityModel::new(),
        }
    }

    /// Evaluate and produce market-making quotes.
    ///
    /// - `binance_price`: real-time underlying price
    /// - `net_yes_inventory`: our current YES holdings minus NO holdings
    /// - `binance_1s_move_pct`: absolute % move of Binance price in last 1 second
    /// - `order_flow_imbalance`: buy/sell ratio over last 5 seconds
    /// - `liquidation_active`: whether a liquidation cascade is detected
    pub fn evaluate(
        &self,
        market: &Market,
        yes_book: &OrderBook,
        binance_price: f64,
        vol_regime: VolRegime,
        available_capital: f64,
        net_yes_inventory: f64,
        binance_1s_move_pct: f64,
        order_flow_imbalance: f64,
        liquidation_active: bool,
    ) -> Vec<OrderIntent> {
        // Should we market-make at all?
        if !self.should_mm(market, vol_regime, available_capital, yes_book) {
            return Vec::new();
        }

        // Check for adverse selection
        let action = self.detect_adverse_selection(
            binance_1s_move_pct,
            order_flow_imbalance,
            liquidation_active,
        );

        if action == AdverseSelectionAction::PullQuotes {
            debug!("MM: pulling quotes due to adverse selection");
            // Return cancel intent (caller handles cancellation)
            return Vec::new();
        }

        let time_remaining_min = market.time_remaining_secs() / 60.0;
        let vol_per_min = market.asset.vol_per_minute();

        // Calculate fair value
        let fair_value = self.prob_model.fair_prob_up(
            binance_price,
            market.reference_price,
            time_remaining_min,
            vol_per_min,
            0.0, // No momentum adjustment for MM
        );

        // Calculate spread
        let mut half_spread = vol_regime.mm_half_spread();

        // Widen on adverse selection signal
        if action == AdverseSelectionAction::WidenSpread {
            half_spread *= 2.0;
        }

        // Widen near expiry (gamma risk)
        let time_remaining_secs = market.time_remaining_secs();
        if time_remaining_secs < 120.0 {
            let time_factor = 1.0 + (120.0 - time_remaining_secs) / 120.0 * 0.5;
            half_spread *= time_factor;
        }

        // Inventory skew: shift both bid and ask to offload excess
        let max_inventory = available_capital * 0.50;
        let skew = if max_inventory > 0.0 {
            (net_yes_inventory / max_inventory * 0.02).clamp(-0.03, 0.03)
        } else {
            0.0
        };

        let bid_price = (fair_value - half_spread - skew).clamp(0.01, 0.99);
        let ask_price = (fair_value + half_spread - skew).clamp(0.01, 0.99);

        // Don't post if bid >= ask
        if bid_price >= ask_price {
            return Vec::new();
        }

        // Quote size
        let base_size = available_capital * self.config.mm_base_size_pct;
        let size_mult = vol_regime.mm_size_multiplier();
        let quote_size = base_size * size_mult;

        if quote_size < 0.10 {
            return Vec::new();
        }

        let bid_dec = Decimal::from_f64_retain(bid_price).unwrap_or(Decimal::ZERO);
        let ask_dec = Decimal::from_f64_retain(ask_price).unwrap_or(Decimal::ZERO);
        let size_dec = Decimal::from_f64_retain(quote_size).unwrap_or(Decimal::ZERO);

        // Round to tick size
        let tick = market.tick_size;
        let bid_rounded = (bid_dec / tick).floor() * tick;
        let ask_rounded = (ask_dec / tick).ceil() * tick;

        debug!(
            "MM: market={} fair={fair_value:.3} bid={bid_price:.3} ask={ask_price:.3} spread={:.3} skew={skew:.4} size={quote_size:.1}",
            market.slug,
            ask_price - bid_price
        );

        vec![
            // Bid (buy YES)
            OrderIntent {
                token_id: market.yes_token_id.clone(),
                market_side: Side::Yes,
                order_side: OrderSide::Buy,
                price: bid_rounded,
                size: size_dec,
                order_type: OrderType::GTC,
                post_only: true, // Ensure maker execution
                expiration: None,
                strategy_tag: "mm_bid".into(),
            },
            // Ask (sell YES)
            OrderIntent {
                token_id: market.yes_token_id.clone(),
                market_side: Side::Yes,
                order_side: OrderSide::Sell,
                price: ask_rounded,
                size: size_dec,
                order_type: OrderType::GTC,
                post_only: true,
                expiration: None,
                strategy_tag: "mm_ask".into(),
            },
        ]
    }

    fn should_mm(
        &self,
        market: &Market,
        vol_regime: VolRegime,
        capital: f64,
        yes_book: &OrderBook,
    ) -> bool {
        let time_remaining = market.time_remaining_secs();

        // Don't MM in last 30 seconds
        if time_remaining < 30.0 {
            return false;
        }

        // Don't MM in EXTREME vol
        if matches!(vol_regime, VolRegime::Extreme) {
            return false;
        }

        // Don't MM in HIGH vol with very small capital
        if matches!(vol_regime, VolRegime::High) && capital < 3.0 {
            return false;
        }

        // Don't MM if spread already < 1 cent
        if let Some(spread) = yes_book.spread() {
            let spread_f64 = spread.to_string().parse::<f64>().unwrap_or(0.0);
            if spread_f64 < 0.01 {
                return false;
            }
        }

        // Don't MM if resolved or in lockout
        !matches!(
            market.lifecycle_phase(),
            LifecyclePhase::Lockout | LifecyclePhase::Resolved
        )
    }

    fn detect_adverse_selection(
        &self,
        binance_1s_move_pct: f64,
        order_flow_imbalance: f64,
        liquidation_active: bool,
    ) -> AdverseSelectionAction {
        // Liquidation cascade = full retreat
        if liquidation_active {
            return AdverseSelectionAction::PullQuotes;
        }

        // Fast Binance move = pull quotes
        if binance_1s_move_pct.abs() > 0.0002 {
            return AdverseSelectionAction::PullQuotes;
        }

        // Large one-sided flow = widen spread
        if order_flow_imbalance.abs() > 3.0 {
            return AdverseSelectionAction::WidenSpread;
        }

        AdverseSelectionAction::Normal
    }
}

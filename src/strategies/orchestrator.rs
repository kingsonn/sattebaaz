use crate::config::StrategyConfig;
use crate::models::market::{Asset, Duration, LifecyclePhase, Market, OrderBook};
use crate::models::order::OrderIntent;
use crate::models::signal::{ArbSignal, BiasSignal, MomentumSignal, VolRegime};
use crate::signals::arb_scanner::ArbScanner;
use crate::strategies::lag_exploit::LagExploitEngine;
use crate::strategies::market_maker::MarketMakerEngine;
use crate::strategies::momentum_capture::MomentumCaptureEngine;
use crate::strategies::pure_arb::PureArbEngine;
use crate::strategies::straddle_bias::StraddleBiasEngine;

/// Orchestrates all sub-strategies for a given market cycle.
///
/// Decides which strategies run based on:
///   - Volatility regime
///   - Market lifecycle phase
///   - Capital tier
///   - Available signals
pub struct StrategyOrchestrator {
    straddle: StraddleBiasEngine,
    arb: PureArbEngine,
    lag: LagExploitEngine,
    mm: MarketMakerEngine,
    momentum: MomentumCaptureEngine,
    config: StrategyConfig,
}

impl StrategyOrchestrator {
    pub fn new(config: StrategyConfig) -> Self {
        Self {
            straddle: StraddleBiasEngine::new(config.clone()),
            arb: PureArbEngine::new(config.clone()),
            lag: LagExploitEngine::new(config.clone()),
            mm: MarketMakerEngine::new(config.clone()),
            momentum: MomentumCaptureEngine::new(config.clone()),
            config,
        }
    }

    /// Run all eligible strategies for a market and collect order intents.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        &self,
        market: &Market,
        yes_book: &OrderBook,
        no_book: &OrderBook,
        vol_regime: VolRegime,
        available_capital: f64,
        binance_price: f64,
        arb_signal: Option<&ArbSignal>,
        bias_signal: Option<&BiasSignal>,
        momentum_signal: Option<&MomentumSignal>,
        net_yes_inventory: f64,
        binance_1s_move_pct: f64,
        order_flow_imbalance: f64,
        liquidation_active: bool,
    ) -> Vec<OrderIntent> {
        let mut all_orders: Vec<OrderIntent> = Vec::new();
        let phase = market.lifecycle_phase();

        if matches!(phase, LifecyclePhase::Lockout | LifecyclePhase::Resolved) {
            return all_orders;
        }

        let capital_for_market = self.capital_for_market(market, available_capital);

        // Pre-compute arb signal if not provided externally
        let computed_arb = if arb_signal.is_none() {
            ArbScanner::scan(
                yes_book,
                no_book,
                vol_regime,
                self.config.arb_min_expected_profit,
            )
        } else {
            None
        };
        let effective_arb = arb_signal.or(computed_arb.as_ref());

        // Strategy priority order depends on vol regime and phase
        let priority = self.strategy_priority(vol_regime, &phase);

        for strategy in &priority {
            // Don't exceed capital allocation
            let remaining_capital = capital_for_market - self.total_order_cost(&all_orders);
            if remaining_capital < 0.50 {
                break;
            }

            match strategy {
                StrategyId::StraddleBias => {
                    if self.config.straddle_enabled {
                        let orders = self.straddle.evaluate(
                            market,
                            yes_book,
                            no_book,
                            effective_arb,
                            bias_signal,
                            vol_regime,
                            remaining_capital,
                        );
                        all_orders.extend(orders);
                    }
                }
                StrategyId::PureArb => {
                    if self.config.arb_enabled {
                        let orders = self.arb.evaluate(
                            market,
                            yes_book,
                            no_book,
                            vol_regime,
                            remaining_capital,
                        );
                        all_orders.extend(orders);
                    }
                }
                StrategyId::LagExploit => {
                    if self.config.lag_exploit_enabled {
                        let momentum_adj = bias_signal
                            .map(|b| b.momentum_score * 0.05)
                            .unwrap_or(0.0);
                        let orders = self.lag.evaluate(
                            market,
                            yes_book,
                            no_book,
                            binance_price,
                            vol_regime,
                            remaining_capital,
                            momentum_adj,
                        );
                        all_orders.extend(orders);
                    }
                }
                StrategyId::MarketMaking => {
                    if self.config.market_making_enabled {
                        let orders = self.mm.evaluate(
                            market,
                            yes_book,
                            binance_price,
                            vol_regime,
                            remaining_capital,
                            net_yes_inventory,
                            binance_1s_move_pct,
                            order_flow_imbalance,
                            liquidation_active,
                        );
                        all_orders.extend(orders);
                    }
                }
                StrategyId::Momentum => {
                    if self.config.momentum_enabled {
                        if let Some(sig) = momentum_signal {
                            let orders = self.momentum.evaluate(
                                market,
                                yes_book,
                                no_book,
                                sig,
                                vol_regime,
                                remaining_capital,
                            );
                            all_orders.extend(orders);
                        }
                    }
                }
            }
        }

        all_orders
    }

    /// Determine strategy execution priority based on conditions.
    fn strategy_priority(&self, vol_regime: VolRegime, _phase: &LifecyclePhase) -> Vec<StrategyId> {
        match vol_regime {
            VolRegime::Dead => vec![
                StrategyId::MarketMaking,
                StrategyId::PureArb,
                StrategyId::StraddleBias,
            ],
            VolRegime::Low => vec![
                StrategyId::StraddleBias,
                StrategyId::MarketMaking,
                StrategyId::PureArb,
                StrategyId::LagExploit,
            ],
            VolRegime::Medium => vec![
                StrategyId::LagExploit,
                StrategyId::StraddleBias,
                StrategyId::MarketMaking,
                StrategyId::Momentum,
                StrategyId::PureArb,
            ],
            VolRegime::High => vec![
                StrategyId::PureArb,
                StrategyId::LagExploit,
                StrategyId::StraddleBias,
                StrategyId::Momentum,
            ],
            VolRegime::Extreme => vec![
                StrategyId::PureArb,
                StrategyId::StraddleBias,
            ],
        }
    }

    /// Calculate capital allocation for a specific market type.
    fn capital_for_market(&self, market: &Market, total_capital: f64) -> f64 {
        let alloc = &self.config.capital_allocation;
        let pct = match (market.asset, market.duration) {
            (Asset::BTC, Duration::FiveMin) => alloc.btc_5m_pct,
            (Asset::BTC, Duration::FifteenMin) => alloc.btc_15m_pct,
            (Asset::ETH, Duration::FifteenMin) => alloc.eth_15m_pct,
            (Asset::SOL, Duration::FifteenMin) => alloc.sol_15m_pct,
            (Asset::XRP, Duration::FifteenMin) => alloc.xrp_15m_pct,
            // 5-min markets for non-BTC assets (future expansion)
            _ => 0.05,
        };
        total_capital * pct
    }

    /// Estimate total cost of pending orders (for capital budgeting).
    fn total_order_cost(&self, orders: &[OrderIntent]) -> f64 {
        orders
            .iter()
            .map(|o| {
                let price = o.price.to_string().parse::<f64>().unwrap_or(0.0);
                let size = o.size.to_string().parse::<f64>().unwrap_or(0.0);
                price * size
            })
            .sum()
    }
}

#[derive(Debug, Clone, Copy)]
enum StrategyId {
    StraddleBias,
    PureArb,
    LagExploit,
    MarketMaking,
    Momentum,
}

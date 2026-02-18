use crate::models::market::Side;
use crate::models::order::{Fill, OrderSide};
use crate::models::position::{Portfolio, Position};
use chrono::Utc;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

/// Tracks all positions across all active markets.
///
/// Thread-safe via RwLock — reads are concurrent, writes are serialized.
pub struct PositionManager {
    pub portfolio: Arc<RwLock<Portfolio>>,
}

impl PositionManager {
    pub fn new(starting_capital: Decimal) -> Self {
        Self {
            portfolio: Arc::new(RwLock::new(Portfolio::new(starting_capital))),
        }
    }

    /// Record a new fill and update positions.
    pub async fn record_fill(&self, fill: &Fill, market_id: &str, side: Side, strategy_tag: &str) {
        let mut portfolio = self.portfolio.write().await;

        // Check if we already have a position in this token
        let existing = portfolio
            .positions
            .iter_mut()
            .find(|p| p.token_id == fill.token_id && p.market_id == market_id);

        match fill.side {
            OrderSide::Buy => {
                if let Some(pos) = existing {
                    // Add to existing position (average in)
                    let total_cost = pos.avg_entry_price * pos.size + fill.price * fill.size;
                    pos.size += fill.size;
                    if pos.size > Decimal::ZERO {
                        pos.avg_entry_price = total_cost / pos.size;
                    }
                } else {
                    // New position
                    portfolio.positions.push(Position {
                        market_id: market_id.to_string(),
                        token_id: fill.token_id.clone(),
                        side,
                        size: fill.size,
                        avg_entry_price: fill.price,
                        unrealized_pnl: Decimal::ZERO,
                        strategy_tag: strategy_tag.to_string(),
                        opened_at: Utc::now(),
                    });
                }

                // Deduct capital
                let cost = fill.price * fill.size + fill.fee;
                portfolio.capital -= cost;
            }
            OrderSide::Sell => {
                if let Some(pos) = existing {
                    let sell_proceeds = fill.price * fill.size - fill.fee;
                    let cost_basis = pos.avg_entry_price * fill.size;
                    let pnl = sell_proceeds - cost_basis;

                    pos.size -= fill.size;

                    // Add proceeds back to capital
                    portfolio.capital += sell_proceeds;
                    portfolio.daily_pnl += pnl;
                    portfolio.total_pnl += pnl;
                    portfolio.total_trades += 1;

                    if pnl > Decimal::ZERO {
                        portfolio.winning_trades += 1;
                        portfolio.consecutive_losses = 0;
                    } else {
                        portfolio.consecutive_losses += 1;
                    }

                    info!(
                        "Closed position: market={market_id} pnl={pnl} daily_pnl={}",
                        portfolio.daily_pnl
                    );
                }
            }
        }
    }

    /// Record a market resolution (payout).
    /// - If we hold YES tokens and market resolves UP: payout = size * $1
    /// - If we hold NO tokens and market resolves DOWN: payout = size * $1
    /// - Otherwise: tokens are worthless
    pub async fn record_resolution(
        &self,
        market_id: &str,
        winning_side: Side,
    ) {
        let mut portfolio = self.portfolio.write().await;

        let mut pnl = Decimal::ZERO;
        let mut capital_delta = Decimal::ZERO;
        let mut wins: u64 = 0;
        let mut losses: u32 = 0;
        let mut trades: u64 = 0;

        // First pass: compute resolution results from positions
        for pos in portfolio.positions.iter().filter(|p| p.market_id == market_id) {
            trades += 1;
            if pos.side == winning_side {
                let payout = pos.size;
                let profit = payout - pos.cost_basis();
                pnl += profit;
                capital_delta += payout;
                wins += 1;
            } else {
                let loss = pos.cost_basis();
                pnl -= loss;
                losses += 1;
            }
        }

        // Remove resolved positions
        portfolio.positions.retain(|p| p.market_id != market_id);

        // First pass: compute resolution results from straddles
        for s in portfolio.straddles.iter().filter(|s| s.market_id == market_id) {
            trades += 1;
            wins += 1;

            let matched = s.yes_size.min(s.no_size);
            let straddle_payout = matched;
            let straddle_cost = matched * (s.yes_avg_price + s.no_avg_price);
            let straddle_profit = straddle_payout - straddle_cost;

            let excess = s.imbalance();
            let excess_pnl = if let Some(excess_side) = s.excess_side() {
                if excess_side == winning_side {
                    excess
                } else {
                    Decimal::ZERO
                }
            } else {
                Decimal::ZERO
            };
            let excess_cost = if s.yes_size > s.no_size {
                excess * s.yes_avg_price
            } else {
                excess * s.no_avg_price
            };

            pnl += straddle_profit + excess_pnl - excess_cost;
            capital_delta += straddle_payout + excess_pnl;
        }

        // Remove resolved straddles
        portfolio.straddles.retain(|s| s.market_id != market_id);

        // Apply aggregated mutations
        portfolio.capital += capital_delta;
        portfolio.total_trades += trades;
        portfolio.winning_trades += wins;
        if losses > 0 {
            portfolio.consecutive_losses += losses;
        } else if wins > 0 {
            portfolio.consecutive_losses = 0;
        }
        portfolio.daily_pnl += pnl;
        portfolio.total_pnl += pnl;

        info!(
            "Resolution: market={market_id} winner={:?} pnl={pnl} capital={}",
            winning_side, portfolio.capital
        );
    }

    /// Get current available capital.
    pub async fn available_capital(&self) -> f64 {
        let portfolio = self.portfolio.read().await;
        portfolio
            .capital
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0)
    }

    /// Get total exposure.
    pub async fn total_exposure(&self) -> Decimal {
        self.portfolio.read().await.total_exposure()
    }

    /// Get net YES inventory for a specific market.
    /// Positive = net long YES, negative = net long NO.
    /// Used by market-making to skew quotes away from inventory.
    pub async fn net_yes_inventory(&self, market_id: &str) -> f64 {
        let portfolio = self.portfolio.read().await;
        let mut net = 0.0;

        for pos in &portfolio.positions {
            if pos.market_id == market_id {
                let size_f64 = pos.size.to_string().parse::<f64>().unwrap_or(0.0);
                match pos.side {
                    Side::Yes => net += size_f64,
                    Side::No => net -= size_f64,
                }
            }
        }

        // Straddles are neutral by design, but excess counts
        for s in &portfolio.straddles {
            if s.market_id == market_id {
                let yes_f = s.yes_size.to_string().parse::<f64>().unwrap_or(0.0);
                let no_f = s.no_size.to_string().parse::<f64>().unwrap_or(0.0);
                net += yes_f - no_f;
            }
        }

        net
    }

    /// Sync capital from on-chain USDC balance (for compounding).
    /// Only updates if the fetched balance is reasonable (>0 and different from current).
    pub async fn sync_capital_from_balance(&self, on_chain_balance: f64) {
        if on_chain_balance <= 0.0 {
            return;
        }

        let mut portfolio = self.portfolio.write().await;
        let current = portfolio.capital.to_string().parse::<f64>().unwrap_or(0.0);

        // Only sync if there's a meaningful difference (>1 cent)
        // This prevents overwriting in-flight capital deductions
        let exposure = portfolio.total_exposure().to_string().parse::<f64>().unwrap_or(0.0);
        let expected = on_chain_balance + exposure; // on-chain = free cash, we track cash + positions

        if (expected - current).abs() > 0.01 {
            let new_capital = Decimal::from_f64_retain(on_chain_balance).unwrap_or(portfolio.capital);
            tracing::info!(
                "Capital sync: on_chain=${on_chain_balance:.2} exposure=${exposure:.2} old=${current:.2} new={}",
                new_capital
            );
            portfolio.capital = new_capital;
        }
    }

    /// Reset daily P&L and consecutive losses (for paper trading between cycles).
    pub async fn reset_daily_pnl(&self) {
        let mut portfolio = self.portfolio.write().await;
        portfolio.daily_pnl = Decimal::ZERO;
        portfolio.consecutive_losses = 0;
    }

    /// Get count of open positions for a market.
    pub async fn position_count(&self, market_id: &str) -> usize {
        let portfolio = self.portfolio.read().await;
        portfolio
            .positions
            .iter()
            .filter(|p| p.market_id == market_id)
            .count()
            + portfolio
                .straddles
                .iter()
                .filter(|s| s.market_id == market_id)
                .count()
    }
}

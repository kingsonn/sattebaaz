use crate::risk::position_manager::PositionManager;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::info;

/// Real-time P&L tracking per strategy and overall.
pub struct PnlTracker {
    position_mgr: Arc<PositionManager>,
    strategy_pnl: dashmap::DashMap<String, Decimal>,
    trade_log: Arc<tokio::sync::RwLock<Vec<TradeRecord>>>,
}

#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub timestamp: DateTime<Utc>,
    pub market_slug: String,
    pub strategy: String,
    pub side: String,
    pub entry_price: f64,
    pub size: f64,
    pub pnl: f64,
    pub cumulative_pnl: f64,
}

impl PnlTracker {
    pub fn new(position_mgr: Arc<PositionManager>) -> Self {
        Self {
            position_mgr,
            strategy_pnl: dashmap::DashMap::new(),
            trade_log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    /// Record a completed trade's P&L.
    pub async fn record_trade(&self, record: TradeRecord) {
        let strategy = record.strategy.clone();
        let pnl = Decimal::from_f64_retain(record.pnl).unwrap_or(Decimal::ZERO);

        self.strategy_pnl
            .entry(strategy.clone())
            .and_modify(|v| *v += pnl)
            .or_insert(pnl);

        self.trade_log.write().await.push(record);
    }

    /// Get P&L for a specific strategy.
    pub fn strategy_pnl(&self, strategy: &str) -> Decimal {
        self.strategy_pnl
            .get(strategy)
            .map(|v| *v)
            .unwrap_or(Decimal::ZERO)
    }

    /// Print summary to log.
    pub async fn log_summary(&self) {
        let portfolio = self.position_mgr.portfolio.read().await;
        info!(
            "=== P&L SUMMARY === capital={} daily_pnl={} total_pnl={} trades={} win_rate={:.1}%",
            portfolio.capital,
            portfolio.daily_pnl,
            portfolio.total_pnl,
            portfolio.total_trades,
            portfolio.win_rate() * 100.0,
        );

        for entry in self.strategy_pnl.iter() {
            info!("  Strategy {}: P&L = {}", entry.key(), entry.value());
        }
    }

    /// Record a fill event (from user WS or immediate fill).
    pub async fn record_fill(
        &self,
        token_id: &str,
        price: Decimal,
        size: Decimal,
        side: crate::models::order::OrderSide,
    ) {
        let side_str = match side {
            crate::models::order::OrderSide::Buy => "BUY",
            crate::models::order::OrderSide::Sell => "SELL",
        };
        let price_f = price.to_string().parse::<f64>().unwrap_or(0.0);
        let size_f = size.to_string().parse::<f64>().unwrap_or(0.0);

        info!(
            "Fill recorded: token={} side={} price={:.4} size={:.2}",
            &token_id[..8.min(token_id.len())],
            side_str,
            price_f,
            size_f
        );
    }

    /// Get total trade count.
    pub async fn trade_count(&self) -> usize {
        self.trade_log.read().await.len()
    }
}

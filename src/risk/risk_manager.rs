use crate::config::RiskConfig;
use crate::models::order::OrderIntent;
use crate::risk::position_manager::PositionManager;
use anyhow::Result;
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Risk manager with kill switch, exposure limits, and drawdown protection.
///
/// Runs as an independent watchdog — can halt trading even if strategies malfunction.
pub struct RiskManager {
    config: RiskConfig,
    position_mgr: Arc<PositionManager>,
    /// Global kill switch — when true, no new orders are allowed
    pub killed: Arc<AtomicBool>,
    /// Whether we're in a loss-streak size reduction mode
    pub size_reduction_active: Arc<AtomicBool>,
    pub size_multiplier: Arc<RwLock<f64>>,
}

impl RiskManager {
    pub fn new(config: RiskConfig, position_mgr: Arc<PositionManager>) -> Self {
        Self {
            config,
            position_mgr,
            killed: Arc::new(AtomicBool::new(false)),
            size_reduction_active: Arc::new(AtomicBool::new(false)),
            size_multiplier: Arc::new(RwLock::new(1.0)),
        }
    }

    /// Pre-flight check before submitting an order.
    /// Returns Ok(()) if order is safe to submit, Err otherwise.
    pub async fn check_order(&self, order: &OrderIntent) -> Result<()> {
        // Kill switch check
        if self.killed.load(Ordering::Relaxed) {
            anyhow::bail!("Kill switch is active — no new orders");
        }

        // Exposure limit check
        // Use starting_capital (not current) to prevent paired orders from breaking
        // when the first leg reduces capital and the second leg's limit shrinks
        let portfolio = self.position_mgr.portfolio.read().await;
        let current_exposure = portfolio.total_exposure();
        let order_cost = order.price * order.size;
        let new_exposure = current_exposure + order_cost;
        let base_capital = portfolio.starting_capital.max(portfolio.capital);
        let max_exposure =
            base_capital * Decimal::from_f64_retain(self.config.max_exposure_pct).unwrap_or(Decimal::ONE);

        if new_exposure > max_exposure {
            anyhow::bail!(
                "Exposure limit: current={current_exposure} + order={order_cost} > max={max_exposure}"
            );
        }

        // Daily loss check
        let daily_loss_limit = portfolio.starting_capital
            * Decimal::from_f64_retain(self.config.max_daily_loss_pct).unwrap_or(Decimal::ONE);
        if portfolio.daily_pnl < -daily_loss_limit {
            anyhow::bail!(
                "Daily loss limit breached: pnl={} < -{}",
                portfolio.daily_pnl,
                daily_loss_limit
            );
        }

        // Balance check
        let required = order.price * order.size;
        if required > portfolio.capital {
            anyhow::bail!(
                "Insufficient balance: need={required} have={}",
                portfolio.capital
            );
        }

        Ok(())
    }

    /// Periodic risk check (called every 500ms by watchdog task).
    pub async fn periodic_check(&self) -> RiskAction {
        let portfolio = self.position_mgr.portfolio.read().await;

        // Check exposure
        let exposure_ratio = portfolio.exposure_ratio();
        let max_ratio =
            Decimal::from_f64_retain(self.config.max_exposure_pct).unwrap_or(Decimal::ONE);
        if exposure_ratio > max_ratio {
            error!(
                "RISK: Exposure ratio {exposure_ratio} exceeds max {max_ratio} — KILLING"
            );
            self.killed.store(true, Ordering::Relaxed);
            return RiskAction::KillSwitch;
        }

        // Check daily drawdown
        let daily_loss_limit = portfolio.starting_capital
            * Decimal::from_f64_retain(self.config.max_daily_loss_pct).unwrap_or(Decimal::ONE);
        if portfolio.daily_pnl < -daily_loss_limit {
            warn!(
                "RISK: Daily loss {:.2} exceeds limit {:.2} — PAUSING",
                portfolio.daily_pnl, daily_loss_limit
            );
            return RiskAction::Pause(self.config.pause_duration_secs);
        }

        // Check loss streak
        if portfolio.consecutive_losses >= self.config.loss_streak_threshold {
            warn!(
                "RISK: {} consecutive losses — reducing size",
                portfolio.consecutive_losses
            );
            self.size_reduction_active.store(true, Ordering::Relaxed);
            *self.size_multiplier.write().await = self.config.loss_streak_size_mult;
            return RiskAction::ReduceSize(self.config.loss_streak_size_mult);
        } else if self.size_reduction_active.load(Ordering::Relaxed) {
            // Reset size reduction after streak ends
            self.size_reduction_active.store(false, Ordering::Relaxed);
            *self.size_multiplier.write().await = 1.0;
        }

        RiskAction::Continue
    }

    /// Get current size multiplier (for strategies to query).
    pub async fn current_size_multiplier(&self) -> f64 {
        *self.size_multiplier.read().await
    }

    /// Manually trigger kill switch.
    pub fn kill(&self) {
        error!("RISK: Manual kill switch activated");
        self.killed.store(true, Ordering::Relaxed);
    }

    /// Reset kill switch (manual recovery).
    pub fn reset_kill(&self) {
        info!("RISK: Kill switch reset");
        self.killed.store(false, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub enum RiskAction {
    Continue,
    ReduceSize(f64),
    Pause(u64),
    KillSwitch,
}

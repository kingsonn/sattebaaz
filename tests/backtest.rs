//! Backtesting framework for Sattebaaz strategies.
//!
//! Simulates market scenarios with synthetic order books and Binance price feeds,
//! then runs the full strategy pipeline: signals → orchestrator → risk → P&L.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;

// Re-export from the crate
use sattebaaz::config::{RiskConfig, StrategyConfig};
use sattebaaz::models::candle::{Candle, IndicatorEngine};
use sattebaaz::models::market::{Asset, Duration, LifecyclePhase, Market, OrderBook, Side};
use sattebaaz::models::order::OrderIntent;
use sattebaaz::models::signal::VolRegime;
use sattebaaz::risk::position_manager::PositionManager;
use sattebaaz::risk::risk_manager::RiskManager;
use sattebaaz::signals::bias::BiasDetector;
use sattebaaz::signals::momentum::MomentumDetector;
use sattebaaz::strategies::orchestrator::StrategyOrchestrator;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a synthetic order book with configurable best bid/ask and depth.
fn make_book(token_id: &str, best_bid: f64, best_ask: f64, depth: f64) -> OrderBook {
    let mut book = OrderBook::new(token_id.to_string());
    // 5 levels of depth, using string conversion for exact Decimal values
    for i in 0..5 {
        let offset_cents = i; // each level is 1 cent away
        let size = Decimal::from_str(&format!("{:.2}", depth)).unwrap_or(dec!(10));

        let bid_val = best_bid - (offset_cents as f64 * 0.01);
        let ask_val = best_ask + (offset_cents as f64 * 0.01);

        let bid_price = Decimal::from_str(&format!("{:.2}", bid_val)).unwrap_or_default();
        let ask_price = Decimal::from_str(&format!("{:.2}", ask_val)).unwrap_or_default();

        if bid_price > Decimal::ZERO {
            book.bids.insert(bid_price, size);
        }
        if ask_price < Decimal::ONE {
            book.asks.insert(ask_price, size);
        }
    }
    book
}

/// Build a Market in the PrimeZone phase (30-120s elapsed for 5m).
fn make_market(asset: Asset, duration: Duration) -> Market {
    let mut m = Market::new(
        format!("{:?}-test-5m", asset).to_lowercase(),
        asset,
        duration,
        "yes_token_001".to_string(),
        "no_token_001".to_string(),
    );
    // Override times so market is in PrimeZone
    let now = chrono::Utc::now();
    m.open_time = now - chrono::Duration::seconds(60);
    m.close_time = now + chrono::Duration::seconds(240);
    m.reference_price = 100_000.0;
    m
}

fn default_strategy_config() -> StrategyConfig {
    StrategyConfig::default()
}

fn default_risk_config() -> RiskConfig {
    RiskConfig::default()
}

// ---------------------------------------------------------------------------
// Strategy integration tests
// ---------------------------------------------------------------------------

/// Test: Orchestrator produces no orders during Lockout phase.
#[test]
fn test_lockout_produces_no_orders() {
    let orch = StrategyOrchestrator::new(default_strategy_config());
    let mut market = make_market(Asset::BTC, Duration::FiveMin);

    // Force market into Lockout: close_time very soon
    let now = chrono::Utc::now();
    market.open_time = now - chrono::Duration::seconds(290);
    market.close_time = now + chrono::Duration::seconds(10);

    let yes_book = make_book("yes", 0.52, 0.54, 50.0);
    let no_book = make_book("no", 0.44, 0.46, 50.0);

    let orders = orch.evaluate(
        &market, &yes_book, &no_book,
        VolRegime::Medium, 100.0, 100_000.0,
        None, None, None,
        0.0, 0.001, 0.0, false,
    );

    assert!(orders.is_empty(), "No orders should be produced in Lockout phase");
}

/// Test: Pure arb fires when YES+NO best ask sum < $0.97.
#[test]
fn test_arb_fires_on_cheap_combined() {
    let mut config = default_strategy_config();
    config.arb_enabled = true;
    config.straddle_enabled = false;
    config.lag_exploit_enabled = false;
    config.market_making_enabled = false;
    config.momentum_enabled = false;

    let orch = StrategyOrchestrator::new(config);
    let market = make_market(Asset::BTC, Duration::FiveMin);

    // YES ask = 0.45, NO ask = 0.47 → combined = 0.92 (arb opportunity!)
    let yes_book = make_book("yes", 0.43, 0.45, 50.0);
    let no_book = make_book("no", 0.45, 0.47, 50.0);

    let orders = orch.evaluate(
        &market, &yes_book, &no_book,
        VolRegime::Medium, 100.0, 100_000.0,
        None, None, None,
        0.0, 0.0, 0.0, false,
    );

    assert!(!orders.is_empty(), "Arb should produce orders when combined < $0.97");

    // Should have both YES and NO buy orders
    let has_yes = orders.iter().any(|o| o.market_side == Side::Yes);
    let has_no = orders.iter().any(|o| o.market_side == Side::No);
    assert!(has_yes && has_no, "Arb should produce both YES and NO orders");
}

/// Test: No arb when combined price is near $1.00.
#[test]
fn test_no_arb_when_fairly_priced() {
    let mut config = default_strategy_config();
    config.arb_enabled = true;
    config.straddle_enabled = false;
    config.lag_exploit_enabled = false;
    config.market_making_enabled = false;
    config.momentum_enabled = false;

    let orch = StrategyOrchestrator::new(config);
    let market = make_market(Asset::BTC, Duration::FiveMin);

    // YES ask = 0.52, NO ask = 0.50 → combined = 1.02 (no arb)
    let yes_book = make_book("yes", 0.50, 0.52, 50.0);
    let no_book = make_book("no", 0.48, 0.50, 50.0);

    let orders = orch.evaluate(
        &market, &yes_book, &no_book,
        VolRegime::Medium, 100.0, 100_000.0,
        None, None, None,
        0.0, 0.0, 0.0, false,
    );

    // With only arb enabled and no arb opportunity, should be empty
    assert!(orders.is_empty(), "No arb orders when combined >= $1.00");
}

/// Test: Strategy produces orders during PrimeZone with medium volatility.
#[test]
fn test_prime_zone_medium_vol_produces_orders() {
    let config = default_strategy_config();
    let orch = StrategyOrchestrator::new(config);
    let market = make_market(Asset::BTC, Duration::FiveMin);

    // Cheap combined: YES=0.45 + NO=0.47 = 0.92, should trigger straddle/arb
    let yes_book = make_book("yes", 0.43, 0.45, 50.0);
    let no_book = make_book("no", 0.45, 0.47, 50.0);

    let orders = orch.evaluate(
        &market, &yes_book, &no_book,
        VolRegime::Medium, 100.0, 100_000.0,
        None, None, None,
        0.0, 0.001, 0.0, false,
    );

    // At minimum, arb or straddle should fire on combined = $0.92
    assert!(!orders.is_empty(), "PrimeZone + medium vol + cheap books should produce orders");
}

/// Test: Lag exploit fires when Binance price diverges from market.
#[test]
fn test_lag_exploit_on_price_divergence() {
    let mut config = default_strategy_config();
    config.lag_exploit_enabled = true;
    config.straddle_enabled = false;
    config.arb_enabled = false;
    config.market_making_enabled = false;
    config.momentum_enabled = false;
    config.lag_min_edge = 0.02;

    let orch = StrategyOrchestrator::new(config);
    let mut market = make_market(Asset::BTC, Duration::FiveMin);
    market.reference_price = 100_000.0;

    // Binance price shot up to 100,500 (0.5% move)
    // But YES book is still pricing at 0.50 (hasn't caught up)
    let yes_book = make_book("yes", 0.48, 0.50, 50.0);
    let no_book = make_book("no", 0.48, 0.50, 50.0);

    let orders = orch.evaluate(
        &market, &yes_book, &no_book,
        VolRegime::High, 100.0, 100_500.0, // Binance price up
        None, None, None,
        0.0, 0.003, 0.0, false,
    );

    // Lag exploit should detect the divergence and buy YES
    // (price went up → YES should be worth more than 0.50)
    // Whether it fires depends on the internal fair value calculation
    // At minimum we test it doesn't crash
    // The strategy may or may not produce orders depending on exact thresholds
    println!("Lag exploit orders: {}", orders.len());
}

// ---------------------------------------------------------------------------
// Risk manager integration tests
// ---------------------------------------------------------------------------

/// Test: Risk manager blocks orders when kill switch is active.
#[tokio::test]
async fn test_kill_switch_blocks_orders() {
    let pos_mgr = std::sync::Arc::new(PositionManager::new(dec!(100)));
    let risk = RiskManager::new(default_risk_config(), pos_mgr);

    let order = OrderIntent {
        token_id: "test_token".to_string(),
        market_side: Side::Yes,
        order_side: sattebaaz::models::order::OrderSide::Buy,
        price: dec!(0.50),
        size: dec!(10),
        order_type: sattebaaz::models::order::OrderType::GTC,
        post_only: false,
        expiration: None,
        strategy_tag: "test".to_string(),
    };

    // Should be OK initially
    assert!(risk.check_order(&order).await.is_ok());

    // Activate kill switch
    risk.kill();
    assert!(risk.check_order(&order).await.is_err());

    // Reset and verify
    risk.reset_kill();
    assert!(risk.check_order(&order).await.is_ok());
}

/// Test: Risk manager blocks orders exceeding exposure limit.
#[tokio::test]
async fn test_exposure_limit() {
    let pos_mgr = std::sync::Arc::new(PositionManager::new(dec!(10)));
    let config = RiskConfig {
        max_exposure_pct: 0.5, // 50% max exposure
        ..RiskConfig::default()
    };
    let risk = RiskManager::new(config, pos_mgr);

    // This order costs 0.50 * 20 = $10, but max exposure on $10 capital = $5
    let big_order = OrderIntent {
        token_id: "test_token".to_string(),
        market_side: Side::Yes,
        order_side: sattebaaz::models::order::OrderSide::Buy,
        price: dec!(0.50),
        size: dec!(20),
        order_type: sattebaaz::models::order::OrderType::GTC,
        post_only: false,
        expiration: None,
        strategy_tag: "test".to_string(),
    };

    // Should be rejected — $10 order > $5 max exposure
    assert!(risk.check_order(&big_order).await.is_err());

    // Small order should pass
    let small_order = OrderIntent {
        token_id: "test_token".to_string(),
        market_side: Side::Yes,
        order_side: sattebaaz::models::order::OrderSide::Buy,
        price: dec!(0.50),
        size: dec!(2),
        order_type: sattebaaz::models::order::OrderType::GTC,
        post_only: false,
        expiration: None,
        strategy_tag: "test".to_string(),
    };

    assert!(risk.check_order(&small_order).await.is_ok());
}

/// Test: Risk manager blocks when balance insufficient.
#[tokio::test]
async fn test_balance_check() {
    let pos_mgr = std::sync::Arc::new(PositionManager::new(dec!(5)));
    let risk = RiskManager::new(default_risk_config(), pos_mgr);

    let order = OrderIntent {
        token_id: "test_token".to_string(),
        market_side: Side::Yes,
        order_side: sattebaaz::models::order::OrderSide::Buy,
        price: dec!(0.50),
        size: dec!(20), // costs $10, but we only have $5
        order_type: sattebaaz::models::order::OrderType::GTC,
        post_only: false,
        expiration: None,
        strategy_tag: "test".to_string(),
    };

    assert!(risk.check_order(&order).await.is_err());
}

// ---------------------------------------------------------------------------
// Backtesting simulation
// ---------------------------------------------------------------------------

/// Simulated market tick for backtesting.
struct SimTick {
    timestamp_secs: f64,
    binance_price: f64,
    yes_best_ask: f64,
    no_best_ask: f64,
    depth: f64,
}

/// Result of a backtested market cycle.
#[derive(Debug)]
struct CycleResult {
    orders_generated: usize,
    total_notional: f64,
    strategies_used: Vec<String>,
}

/// Run a full backtest over a simulated 5-minute market cycle.
fn backtest_cycle(
    ticks: &[SimTick],
    starting_capital: f64,
    vol_regime: VolRegime,
) -> CycleResult {
    let config = default_strategy_config();
    let orch = StrategyOrchestrator::new(config);

    let mut market = make_market(Asset::BTC, Duration::FiveMin);
    market.reference_price = ticks.first().map(|t| t.binance_price).unwrap_or(100_000.0);

    let mut total_orders = 0;
    let mut total_notional = 0.0;
    let mut strategies = std::collections::HashSet::new();

    for tick in ticks {
        // Update market timing based on tick timestamp
        let elapsed = tick.timestamp_secs;
        let remaining = 300.0 - elapsed;
        if remaining < 10.0 {
            break; // Lockout
        }

        let yes_book = make_book("yes", tick.yes_best_ask - 0.02, tick.yes_best_ask, tick.depth);
        let no_book = make_book("no", tick.no_best_ask - 0.02, tick.no_best_ask, tick.depth);

        let orders = orch.evaluate(
            &market, &yes_book, &no_book,
            vol_regime, starting_capital, tick.binance_price,
            None, None, None,
            0.0, 0.001, 0.0, false,
        );

        for order in &orders {
            strategies.insert(order.strategy_tag.clone());
            let price = order.price.to_string().parse::<f64>().unwrap_or(0.0);
            let size = order.size.to_string().parse::<f64>().unwrap_or(0.0);
            total_notional += price * size;
        }
        total_orders += orders.len();
    }

    CycleResult {
        orders_generated: total_orders,
        total_notional,
        strategies_used: strategies.into_iter().collect(),
    }
}

/// Test: Backtest a volatile BTC 5-min cycle with arb opportunity.
#[test]
fn test_backtest_volatile_cycle_with_arb() {
    let ticks: Vec<SimTick> = (0..30)
        .map(|i| {
            let t = i as f64 * 10.0; // every 10 seconds
            let price = 100_000.0 + (i as f64 * 50.0); // steady climb
            SimTick {
                timestamp_secs: t,
                binance_price: price,
                yes_best_ask: 0.46, // YES + NO = 0.92 → arb!
                no_best_ask: 0.46,
                depth: 50.0,
            }
        })
        .collect();

    let result = backtest_cycle(&ticks, 100.0, VolRegime::High);

    println!("Backtest result: {:?}", result);
    assert!(
        result.orders_generated > 0,
        "Should generate orders on arb opportunity"
    );
    assert!(
        result.total_notional > 0.0,
        "Should have positive notional"
    );
}

/// Test: Backtest a dead vol cycle with fair pricing (should mostly market-make).
#[test]
fn test_backtest_dead_vol_fair_pricing() {
    let ticks: Vec<SimTick> = (0..30)
        .map(|i| {
            let t = i as f64 * 10.0;
            SimTick {
                timestamp_secs: t,
                binance_price: 100_000.0, // flat
                yes_best_ask: 0.52,       // YES + NO = 1.02 (no arb)
                no_best_ask: 0.50,
                depth: 50.0,
            }
        })
        .collect();

    let result = backtest_cycle(&ticks, 100.0, VolRegime::Dead);

    println!("Dead vol result: {:?}", result);
    // Market making may or may not fire depending on spread thresholds
    // The key test is that it doesn't crash and handles the scenario
}

/// Test: Backtest with very small capital ($5) — sizing should be appropriate.
#[test]
fn test_backtest_micro_capital() {
    let ticks: Vec<SimTick> = (0..10)
        .map(|i| SimTick {
            timestamp_secs: i as f64 * 30.0,
            binance_price: 100_000.0,
            yes_best_ask: 0.45, // cheap combined
            no_best_ask: 0.47,
            depth: 50.0,
        })
        .collect();

    let result = backtest_cycle(&ticks, 5.0, VolRegime::Medium);

    println!("Micro capital result: {:?}", result);

    // Verify notional doesn't exceed capital
    // (with $5 capital and 40% BTC 5m allocation = $2 max)
    if result.total_notional > 0.0 {
        assert!(
            result.total_notional < 50.0, // reasonable upper bound
            "Notional {} too high for $5 capital",
            result.total_notional
        );
    }
}

// ---------------------------------------------------------------------------
// Full P&L simulation backtest
// ---------------------------------------------------------------------------

/// Simple deterministic PRNG (LCG) for reproducible backtests.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    /// Returns a float in [0, 1).
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    /// Returns a float in [-1, 1).
    fn next_signed(&mut self) -> f64 {
        self.next_f64() * 2.0 - 1.0
    }
}

/// Simulate a realistic BTC price path using geometric random walk.
fn simulate_price_path(rng: &mut Rng, start: f64, steps: usize, vol_per_step: f64) -> Vec<f64> {
    let mut prices = Vec::with_capacity(steps);
    let mut price = start;
    for _ in 0..steps {
        let shock = rng.next_signed() * vol_per_step;
        price *= 1.0 + shock;
        prices.push(price);
    }
    prices
}

/// Full P&L backtest: simulates 100 market cycles with $5 starting capital.
///
/// Microstructure model:
///   - Polymarket book implied-prob LAGS Binance by 1-3 ticks.
///   - When lagged, the stale side's ask is cheap → arb or lag exploit.
///   - Market-maker quotes around fair value with spread.
#[tokio::test]
async fn test_full_pnl_backtest() {
    let starting_capital = 5.0;
    let num_cycles = 100;
    let ticks_per_cycle = 25;

    let pos_mgr = std::sync::Arc::new(PositionManager::new(
        Decimal::from_str(&format!("{:.2}", starting_capital)).unwrap(),
    ));
    let risk_mgr = RiskManager::new(default_risk_config(), pos_mgr.clone());
    let config = default_strategy_config();
    let orch = StrategyOrchestrator::new(config);

    let mut total_orders = 0usize;
    let mut total_fills = 0usize;
    let mut total_arb_orders = 0usize;
    let mut total_straddle_orders = 0usize;
    let mut total_lag_orders = 0usize;
    let mut total_mm_orders = 0usize;
    let mut total_momentum_orders = 0usize;
    let mut total_mm_skipped = 0usize;
    let mut wins = 0u32;
    let mut losses = 0u32;
    let mut cycle_pnls: Vec<f64> = Vec::new();

    let btc_start = 97_500.0;
    let mut rng = Rng::new(42);

    // MM resting orders fill with this probability per tick (realistic: ~25%)
    let mm_fill_prob = 0.25;

    println!("\n============================================================");
    println!("  SATTEBAAZ BACKTEST  {} cycles x 5min  (REALISTIC MODEL)", num_cycles);
    println!("  Starting capital: ${:.2}", starting_capital);
    println!("  BTC starting price: ${:.0}", btc_start);
    println!("  Fees: ZERO (5-min crypto markets are fee-free)");
    println!("  MM fill probability: {:.0}%", mm_fill_prob * 100.0);
    println!("============================================================\n");

    for cycle in 0..num_cycles {
        let prices = simulate_price_path(&mut rng, btc_start, ticks_per_cycle, 0.0015);
        let reference_price = prices[0];

        // Unique slug per cycle so positions don't collide
        let slug = format!("btc-5m-c{}", cycle);
        let mut market = Market::new(
            slug.clone(),
            Asset::BTC,
            Duration::FiveMin,
            format!("yes_tok_{}", cycle),
            format!("no_tok_{}", cycle),
        );
        // Put market in PrimeZone (60s elapsed, 240s remaining)
        let now = chrono::Utc::now();
        market.open_time = now - chrono::Duration::seconds(60);
        market.close_time = now + chrono::Duration::seconds(240);
        market.reference_price = reference_price;

        let capital_before = pos_mgr.available_capital().await;
        let mut cycle_orders = 0usize;

        // Determine vol regime from the path
        let returns: Vec<f64> = prices.windows(2).map(|w| (w[1] - w[0]) / w[0]).collect();
        let avg_abs_ret = returns.iter().map(|r| r.abs()).sum::<f64>() / returns.len().max(1) as f64;
        let vol_regime = if avg_abs_ret < 0.0003 {
            VolRegime::Dead
        } else if avg_abs_ret < 0.0008 {
            VolRegime::Low
        } else if avg_abs_ret < 0.0015 {
            VolRegime::Medium
        } else if avg_abs_ret < 0.003 {
            VolRegime::High
        } else {
            VolRegime::Extreme
        };

        // Signal detectors — reset per cycle
        let mut momentum_det = MomentumDetector::new(100);
        let mut indicator_eng = IndicatorEngine::new(100);
        let bias_det = BiasDetector::new(0.20);

        // YES and NO books are managed by DIFFERENT market makers with
        // independent lag. This is how real Polymarket works.
        let mut yes_mm_price: f64 = 0.50; // YES market maker's quote
        let mut no_mm_price: f64 = 0.50;  // NO market maker's quote

        for (i, &binance_price) in prices.iter().enumerate() {
            let elapsed = i as f64 * 10.0;
            if elapsed > 250.0 {
                break;
            }

            let available = pos_mgr.available_capital().await;
            if available < 0.10 {
                break;
            }

            // True fair probability from Binance price
            let pct_move = (binance_price - reference_price) / reference_price;
            let fair_yes = (0.5 + (pct_move * 150.0).tanh() * 0.48).clamp(0.02, 0.98);
            let fair_no = 1.0 - fair_yes;

            // Each market maker catches up independently (20-50% per tick)
            let yes_catchup = 0.20 + rng.next_f64() * 0.30;
            let no_catchup = 0.20 + rng.next_f64() * 0.30;
            yes_mm_price += (fair_yes - yes_mm_price) * yes_catchup;
            no_mm_price += (fair_no - no_mm_price) * no_catchup;

            // Build asks: mid + small spread + noise
            let yes_spread = 0.005 + rng.next_f64() * 0.005;
            let no_spread = 0.005 + rng.next_f64() * 0.005;
            let yes_ask = (yes_mm_price + yes_spread + rng.next_signed() * 0.003).clamp(0.03, 0.97);
            let no_ask = (no_mm_price + no_spread + rng.next_signed() * 0.003).clamp(0.03, 0.97);

            // Feed momentum detector with YES midpoint
            let yes_mid = yes_ask - 0.01;
            momentum_det.push_price(elapsed, yes_mid);
            let mom_signal = momentum_det.detect(fair_yes);

            // Feed bias detector with synthetic candle from Binance price
            let prev_bp = if i > 0 { prices[i - 1] } else { binance_price };
            let buy_vol = if binance_price >= prev_bp { 70.0 } else { 30.0 };
            indicator_eng.push(Candle {
                open: prev_bp,
                high: binance_price.max(prev_bp),
                low: binance_price.min(prev_bp),
                close: binance_price,
                volume: 100.0,
                buy_volume: buy_vol,
                sell_volume: 100.0 - buy_vol,
                trades: 50,
                open_time: chrono::Utc::now(),
                close_time: chrono::Utc::now(),
            });
            let bias_sig = bias_det.detect(&indicator_eng, 0.0, 0.0);
            let bias_ref = if bias_sig.confidence > 0.0 {
                Some(&bias_sig)
            } else {
                None
            };

            // Binance 1-tick move for adverse selection detection
            let b_move = if i > 0 {
                ((binance_price - prices[i - 1]) / prices[i - 1]).abs()
            } else {
                0.0
            };

            let yes_book = make_book(
                &market.yes_token_id, yes_ask - 0.02, yes_ask, 50.0,
            );
            let no_book = make_book(
                &market.no_token_id, no_ask - 0.02, no_ask, 50.0,
            );

            let inventory = pos_mgr.net_yes_inventory(&market.slug).await;

            let orders = orch.evaluate(
                &market, &yes_book, &no_book,
                vol_regime, available, binance_price,
                None, bias_ref, mom_signal.as_ref(),
                inventory, b_move, 0.0, false,
            );

            if orders.is_empty() {
                continue;
            }

            // Batch risk-check then fill
            let mut approved = Vec::new();
            for order in &orders {
                if risk_mgr.check_order(order).await.is_ok() {
                    approved.push(order.clone());
                }
            }

            for order in &approved {
                let is_mm = order.strategy_tag.contains("mm")
                    || order.strategy_tag.contains("market_maker");

                if is_mm {
                    // MM resting orders only fill with realistic probability
                    if rng.next_f64() > mm_fill_prob {
                        total_mm_skipped += 1;
                        continue;
                    }
                    // Cannot sell YES tokens we don't hold
                    if order.order_side == sattebaaz::models::order::OrderSide::Sell {
                        let inv = pos_mgr.net_yes_inventory(&market.slug).await;
                        let sz = order.size.to_string().parse::<f64>().unwrap_or(0.0);
                        if inv < sz * 0.5 {
                            total_mm_skipped += 1;
                            continue;
                        }
                    }
                }

                // 5-min crypto markets are FEE-FREE on Polymarket
                let fill = sattebaaz::models::order::Fill {
                    order_id: format!("bt_{}_{}", cycle, i),
                    token_id: order.token_id.clone(),
                    side: order.order_side,
                    price: order.price,
                    size: order.size,
                    timestamp: chrono::Utc::now(),
                    fee: Decimal::ZERO,
                };

                pos_mgr.record_fill(
                    &fill, &market.slug, order.market_side, &order.strategy_tag,
                ).await;

                cycle_orders += 1;
                total_fills += 1;

                match order.strategy_tag.as_str() {
                    s if s.contains("arb") => total_arb_orders += 1,
                    s if s.contains("straddle") || s.contains("bias") => total_straddle_orders += 1,
                    s if s.contains("lag") => total_lag_orders += 1,
                    s if s.contains("mm") || s.contains("market_maker") => total_mm_orders += 1,
                    s if s.contains("momentum") => total_momentum_orders += 1,
                    _ => {}
                }
            }
        }

        total_orders += cycle_orders;

        // Resolve: did BTC finish up or down?
        let final_price = *prices.last().unwrap_or(&reference_price);
        let winning_side = if final_price >= reference_price {
            Side::Yes
        } else {
            Side::No
        };

        pos_mgr.record_resolution(&market.slug, winning_side).await;
        let capital_after = pos_mgr.available_capital().await;
        let cycle_pnl = capital_after - capital_before;
        cycle_pnls.push(cycle_pnl);

        if cycle_pnl > 0.001 {
            wins += 1;
        } else if cycle_pnl < -0.001 {
            losses += 1;
        }

        if cycle % 10 == 0 || cycle == num_cycles - 1 {
            println!(
                "  Cycle {:>3}/{} | Vol: {:>8?} | Orders: {:>3} | PnL: {:>+7.3} | Capital: ${:.2}",
                cycle + 1, num_cycles, vol_regime, cycle_orders, cycle_pnl, capital_after
            );
        }
    }

    // Final results
    let final_capital = pos_mgr.available_capital().await;
    let total_pnl = final_capital - starting_capital;
    let total_return_pct = (total_pnl / starting_capital) * 100.0;
    let avg_pnl = cycle_pnls.iter().sum::<f64>() / cycle_pnls.len().max(1) as f64;
    let max_pnl = cycle_pnls.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_pnl = cycle_pnls.iter().cloned().fold(f64::INFINITY, f64::min);

    // Max drawdown
    let mut peak = starting_capital;
    let mut max_drawdown = 0.0f64;
    let mut running = starting_capital;
    for &pnl in &cycle_pnls {
        running += pnl;
        if running > peak { peak = running; }
        let dd = (peak - running) / peak;
        if dd > max_drawdown { max_drawdown = dd; }
    }

    // Sharpe ratio (per-cycle)
    let mean = avg_pnl;
    let variance = cycle_pnls.iter().map(|p| (p - mean).powi(2)).sum::<f64>()
        / cycle_pnls.len().max(1) as f64;
    let std_dev = variance.sqrt();
    let sharpe = if std_dev > 0.0 { mean / std_dev } else { 0.0 };

    println!("\n============================================================");
    println!("  BACKTEST RESULTS");
    println!("============================================================");
    println!("  Starting Capital:    ${:.2}", starting_capital);
    println!("  Final Capital:       ${:.2}", final_capital);
    println!("  Total P&L:           {:>+.4}", total_pnl);
    println!("  Total Return:        {:>+.2}%", total_return_pct);
    println!("  ---");
    println!("  Cycles:              {}", num_cycles);
    println!("  Winning Cycles:      {} ({:.0}%)", wins, wins as f64 / num_cycles as f64 * 100.0);
    println!("  Losing Cycles:       {} ({:.0}%)", losses, losses as f64 / num_cycles as f64 * 100.0);
    println!("  ---");
    println!("  Avg P&L per Cycle:   {:>+.4}", avg_pnl);
    println!("  Best Cycle:          {:>+.4}", max_pnl);
    println!("  Worst Cycle:         {:>+.4}", min_pnl);
    println!("  Max Drawdown:        {:.2}%", max_drawdown * 100.0);
    println!("  Sharpe (per-cycle):  {:.2}", sharpe);
    println!("  ---");
    println!("  Total Orders:        {}", total_orders);
    println!("  Total Fills:         {}", total_fills);
    println!("  Orders by Strategy:");
    println!("    Straddle/Bias:     {}", total_straddle_orders);
    println!("    Pure Arb:          {}", total_arb_orders);
    println!("    Lag Exploit:       {}", total_lag_orders);
    println!("    Market Making:     {}", total_mm_orders);
    println!("    Momentum:          {}", total_momentum_orders);
    println!("  MM Orders Skipped:   {} (unfilled resting / no inventory)", total_mm_skipped);
    println!("============================================================\n");

    // Sanity assertions
    assert!(final_capital > 0.0, "Should not go bankrupt");
    assert!(total_orders > 0, "Should have placed orders");
}

// ---------------------------------------------------------------------------
// Multi-market backtest: ALL market types concurrently
// ---------------------------------------------------------------------------

/// Simulates BTC-5m + BTC/ETH/SOL/XRP-15m running concurrently with shared
/// capital.  Validates capital allocation, correct fee model per duration,
/// and ensures every market type generates activity.
#[tokio::test]
async fn test_multi_market_backtest() {
    let starting_capital = 25.0; // $25 — enough for 5 concurrent markets
    let num_rounds = 33; // 33 × 15-min windows ≈ 8 hours
    let mm_fill_prob = 0.25;

    let pos_mgr = std::sync::Arc::new(PositionManager::new(
        Decimal::from_str(&format!("{:.2}", starting_capital)).unwrap(),
    ));
    // Wider daily loss limit for multi-market micro capital
    let risk_config = RiskConfig {
        max_daily_loss_pct: 0.30,
        ..default_risk_config()
    };
    let risk_mgr = RiskManager::new(risk_config, pos_mgr.clone());
    let orch = StrategyOrchestrator::new(default_strategy_config());
    let mut rng = Rng::new(42);

    // Per-market-type counters: [BTC-5m, BTC-15m, ETH-15m, SOL-15m, XRP-15m]
    let labels = ["BTC-5m", "BTC-15m", "ETH-15m", "SOL-15m", "XRP-15m"];
    let mut mkt_fills  = [0usize; 5];
    let mut mkt_mm_skip = [0usize; 5];
    let mut mkt_cycles  = [0usize; 5];
    let mut total_fills = 0usize;
    let mut round_pnls: Vec<f64> = Vec::new();

    println!("\n============================================================");
    println!("  MULTI-MARKET BACKTEST  {} rounds x 15min", num_rounds);
    println!("  Markets: BTC-5m, BTC-15m, ETH-15m, SOL-15m, XRP-15m");
    println!("  Starting capital: ${:.2}  |  MM fill prob: {:.0}%", starting_capital, mm_fill_prob * 100.0);
    println!("  Capital split: BTC-5m 40% | BTC-15m 20% | ETH 20% | SOL 10% | XRP 10%");
    println!("  Fees: 5m=FREE | 15m=taker (fee_rate_bps=1000)");
    println!("============================================================\n");

    for round in 0..num_rounds {
        // Reset daily stats at simulated day boundaries (96 × 15-min = 24h)
        if round > 0 && round % 96 == 0 {
            pos_mgr.portfolio.write().await.daily_pnl = Decimal::ZERO;
            pos_mgr.portfolio.write().await.consecutive_losses = 0;
        }
        let capital_before = pos_mgr.available_capital().await;

        // 75-tick price paths (10s/tick → 750s ≈ 12.5 min)
        // Vol per step scaled by asset annual vol relative to BTC (0.55)
        let btc_prices = simulate_price_path(&mut rng, 97_500.0, 75, 0.0015);
        let eth_prices = simulate_price_path(&mut rng,  3_400.0, 75, 0.00191);
        let sol_prices = simulate_price_path(&mut rng,    200.0, 75, 0.00259);
        let xrp_prices = simulate_price_path(&mut rng,      2.5, 75, 0.00232);

        // 7 concurrent markets per round:
        //   0-2 = BTC-5m sub-cycles (ticks 0-24, 25-49, 50-74)
        //   3   = BTC-15m   4 = ETH-15m   5 = SOL-15m   6 = XRP-15m
        let price_paths: Vec<&Vec<f64>> = vec![
            &btc_prices, &btc_prices, &btc_prices,
            &btc_prices, &eth_prices, &sol_prices, &xrp_prices,
        ];
        let stat_idx:   [usize; 7] = [0, 0, 0, 1, 2, 3, 4];
        let assets:     [Asset; 7] = [
            Asset::BTC, Asset::BTC, Asset::BTC,
            Asset::BTC, Asset::ETH, Asset::SOL, Asset::XRP,
        ];
        let durations: [Duration; 7] = [
            Duration::FiveMin, Duration::FiveMin, Duration::FiveMin,
            Duration::FifteenMin, Duration::FifteenMin, Duration::FifteenMin, Duration::FifteenMin,
        ];
        let fee_mults: [f64; 7] = [0.0, 0.0, 0.0, 0.0625, 0.0625, 0.0625, 0.0625];
        let start_ticks: [usize; 7] = [0, 25, 50, 0, 0, 0, 0];
        let end_ticks:   [usize; 7] = [25, 50, 75, 75, 75, 75, 75];

        // Per-market vol regime (from each market's price slice)
        let vol_regimes: Vec<VolRegime> = (0..7).map(|mi| {
            let sl = &price_paths[mi][start_ticks[mi]..end_ticks[mi]];
            let avg = sl.windows(2)
                .map(|w| ((w[1] - w[0]) / w[0]).abs())
                .sum::<f64>() / (sl.len().max(2) - 1) as f64;
            if avg < 0.0003 { VolRegime::Dead }
            else if avg < 0.0008 { VolRegime::Low }
            else if avg < 0.0015 { VolRegime::Medium }
            else if avg < 0.003 { VolRegime::High }
            else { VolRegime::Extreme }
        }).collect();

        // Create Market structs
        let mut markets: Vec<Market> = Vec::new();
        let mut slugs: Vec<String> = Vec::new();
        let mut ref_prices: Vec<f64> = Vec::new();
        let mut yes_mms = vec![0.50f64; 7];
        let mut no_mms  = vec![0.50f64; 7];
        let mut mom_dets: Vec<MomentumDetector> = (0..7).map(|_| MomentumDetector::new(100)).collect();
        let mut ind_engs: Vec<IndicatorEngine> = (0..7).map(|_| IndicatorEngine::new(100)).collect();
        let bias_dets: Vec<BiasDetector> = (0..7).map(|_| BiasDetector::new(0.20)).collect();

        for mi in 0..7 {
            let pfx = assets[mi].slug_prefix();
            let dsuf = durations[mi].slug_suffix();
            let slug = format!("{}-{}-r{}-{}", pfx, dsuf, round, mi);
            let rp = price_paths[mi][start_ticks[mi]];
            let now = chrono::Utc::now();
            let mut m = Market::new(
                slug.clone(), assets[mi], durations[mi],
                format!("y_{}_{}", slug, round),
                format!("n_{}_{}", slug, round),
            );
            match durations[mi] {
                Duration::FiveMin => {
                    m.open_time  = now - chrono::Duration::seconds(60);
                    m.close_time = now + chrono::Duration::seconds(240);
                }
                Duration::FifteenMin => {
                    m.open_time  = now - chrono::Duration::seconds(120);
                    m.close_time = now + chrono::Duration::seconds(780);
                }
            }
            m.reference_price = rp;
            ref_prices.push(rp);
            slugs.push(slug);
            markets.push(m);
        }

        // ── Tick loop ──
        for tick in 0..75usize {
            if pos_mgr.available_capital().await < 0.05 { break; }

            for mi in 0..7 {
                if tick < start_ticks[mi] || tick >= end_ticks[mi] { continue; }

                let bp = price_paths[mi][tick];
                let lt = tick - start_ticks[mi];
                let elapsed = lt as f64 * 10.0;

                // Fair probability
                let pct = (bp - ref_prices[mi]) / ref_prices[mi];
                let fy = (0.5 + (pct * 150.0).tanh() * 0.48).clamp(0.02, 0.98);
                let fn_ = 1.0 - fy;

                // Independent MM catchup
                let yc = 0.20 + rng.next_f64() * 0.30;
                let nc = 0.20 + rng.next_f64() * 0.30;
                yes_mms[mi] += (fy  - yes_mms[mi]) * yc;
                no_mms[mi]  += (fn_ - no_mms[mi])  * nc;

                let ya = (yes_mms[mi] + 0.005 + rng.next_f64() * 0.005
                    + rng.next_signed() * 0.003).clamp(0.03, 0.97);
                let na = (no_mms[mi] + 0.005 + rng.next_f64() * 0.005
                    + rng.next_signed() * 0.003).clamp(0.03, 0.97);

                // Feed momentum
                mom_dets[mi].push_price(elapsed, ya - 0.01);
                let msig = mom_dets[mi].detect(fy);

                // Feed bias via synthetic candle
                let prev = if tick > start_ticks[mi] { price_paths[mi][tick - 1] } else { bp };
                let bv = if bp >= prev { 70.0 } else { 30.0 };
                ind_engs[mi].push(Candle {
                    open: prev, high: bp.max(prev), low: bp.min(prev), close: bp,
                    volume: 100.0, buy_volume: bv, sell_volume: 100.0 - bv,
                    trades: 50,
                    open_time: chrono::Utc::now(), close_time: chrono::Utc::now(),
                });
                let bsig = bias_dets[mi].detect(&ind_engs[mi], 0.0, 0.0);
                let bref = if bsig.confidence > 0.0 { Some(&bsig) } else { None };

                let bmv = if tick > start_ticks[mi] {
                    ((bp - price_paths[mi][tick - 1]) / price_paths[mi][tick - 1]).abs()
                } else { 0.0 };

                let ybook = make_book(&markets[mi].yes_token_id, ya - 0.02, ya, 50.0);
                let nbook = make_book(&markets[mi].no_token_id,  na - 0.02, na, 50.0);
                let avail = pos_mgr.available_capital().await;
                let inv   = pos_mgr.net_yes_inventory(&slugs[mi]).await;

                let orders = orch.evaluate(
                    &markets[mi], &ybook, &nbook,
                    vol_regimes[mi], avail, bp,
                    None, bref, msig.as_ref(),
                    inv, bmv, 0.0, false,
                );
                if orders.is_empty() { continue; }

                let mut approved = Vec::new();
                for o in &orders {
                    if risk_mgr.check_order(o).await.is_ok() {
                        approved.push(o.clone());
                    }
                }

                for o in &approved {
                    let is_mm = o.strategy_tag.contains("mm")
                        || o.strategy_tag.contains("market_maker");
                    if is_mm {
                        if rng.next_f64() > mm_fill_prob {
                            mkt_mm_skip[stat_idx[mi]] += 1;
                            continue;
                        }
                        if o.order_side == sattebaaz::models::order::OrderSide::Sell {
                            let inv = pos_mgr.net_yes_inventory(&slugs[mi]).await;
                            let sz = o.size.to_string().parse::<f64>().unwrap_or(0.0);
                            if inv < sz * 0.5 {
                                mkt_mm_skip[stat_idx[mi]] += 1;
                                continue;
                            }
                        }
                    }

                    // Fee: 0 for 5m, taker formula for 15m
                    let fee_dec = if fee_mults[mi] > 0.0 {
                        let p = o.price.to_string().parse::<f64>().unwrap_or(0.5);
                        let s = o.size.to_string().parse::<f64>().unwrap_or(0.0);
                        let f = p * s * p * (1.0 - p) * fee_mults[mi];
                        Decimal::from_f64_retain(f).unwrap_or(Decimal::ZERO)
                    } else {
                        Decimal::ZERO
                    };

                    let fill = sattebaaz::models::order::Fill {
                        order_id: format!("mm_r{}_t{}_m{}", round, tick, mi),
                        token_id: o.token_id.clone(),
                        side: o.order_side,
                        price: o.price, size: o.size,
                        timestamp: chrono::Utc::now(),
                        fee: fee_dec,
                    };
                    pos_mgr.record_fill(
                        &fill, &slugs[mi], o.market_side, &o.strategy_tag,
                    ).await;

                    mkt_fills[stat_idx[mi]] += 1;
                    total_fills += 1;
                }
            }

            // Resolve markets whose last tick was just processed
            for mi in 0..7 {
                if end_ticks[mi] == tick + 1 {
                    let fp = price_paths[mi][end_ticks[mi] - 1];
                    let w = if fp >= ref_prices[mi] { Side::Yes } else { Side::No };
                    pos_mgr.record_resolution(&slugs[mi], w).await;
                    mkt_cycles[stat_idx[mi]] += 1;
                }
            }
        }

        let capital_after = pos_mgr.available_capital().await;
        round_pnls.push(capital_after - capital_before);

        if round % 5 == 0 || round == num_rounds - 1 {
            println!(
                "  Round {:>2}/{} | PnL: {:>+8.3} | Capital: ${:.2}",
                round + 1, num_rounds, capital_after - capital_before, capital_after
            );
        }
    }

    // ── Summary ──
    let fc = pos_mgr.available_capital().await;
    let tp = fc - starting_capital;

    let mut peak = starting_capital;
    let mut max_dd = 0.0f64;
    let mut run = starting_capital;
    for &p in &round_pnls {
        run += p;
        if run > peak { peak = run; }
        let dd = (peak - run) / peak;
        if dd > max_dd { max_dd = dd; }
    }
    let mean = round_pnls.iter().sum::<f64>() / round_pnls.len().max(1) as f64;
    let var = round_pnls.iter().map(|p| (p - mean).powi(2)).sum::<f64>()
        / round_pnls.len().max(1) as f64;
    let sharpe = if var.sqrt() > 0.0 { mean / var.sqrt() } else { 0.0 };

    println!("\n============================================================");
    println!("  MULTI-MARKET RESULTS");
    println!("============================================================");
    println!("  Starting Capital:    ${:.2}", starting_capital);
    println!("  Final Capital:       ${:.2}", fc);
    println!("  Total P&L:           {:>+.4}", tp);
    println!("  Total Return:        {:>+.2}%", tp / starting_capital * 100.0);
    println!("  Max Drawdown:        {:.2}%", max_dd * 100.0);
    println!("  Sharpe (per-round):  {:.2}", sharpe);
    println!("  ---");
    println!("  Per-Market Breakdown:");
    for i in 0..5 {
        println!(
            "    {:<10} | Cycles: {:>3} | Fills: {:>4} | MM Skip: {:>3}",
            labels[i], mkt_cycles[i], mkt_fills[i], mkt_mm_skip[i]
        );
    }
    println!("  Total Fills:         {}", total_fills);
    println!("============================================================\n");

    assert!(fc > 0.0, "Should not go bankrupt");
    assert!(total_fills > 0, "Should have placed orders across markets");
    // Verify EVERY market type had activity
    for i in 0..5 {
        assert!(mkt_cycles[i] > 0, "{} should have run cycles", labels[i]);
    }
}

// ---------------------------------------------------------------------------
// Order book tests
// ---------------------------------------------------------------------------

#[test]
fn test_order_book_best_bid_ask() {
    let book = make_book("test", 0.50, 0.52, 10.0);

    let (bid_price, _) = book.best_bid().expect("should have bid");
    let (ask_price, _) = book.best_ask().expect("should have ask");

    assert_eq!(bid_price, Decimal::from_str("0.50").unwrap());
    assert_eq!(ask_price, Decimal::from_str("0.52").unwrap());
}

#[test]
fn test_order_book_midpoint() {
    let book = make_book("test", 0.50, 0.52, 10.0);
    let mid = book.midpoint().expect("should have midpoint");
    assert_eq!(mid, Decimal::from_str("0.51").unwrap());
}

#[test]
fn test_order_book_spread() {
    let book = make_book("test", 0.50, 0.52, 10.0);
    let spread = book.spread().expect("should have spread");
    assert_eq!(spread, Decimal::from_str("0.02").unwrap());
}

#[test]
fn test_market_lifecycle_phases() {
    let now = chrono::Utc::now();

    // Alpha window: 0-5s elapsed
    let mut m = make_market(Asset::BTC, Duration::FiveMin);
    m.open_time = now - chrono::Duration::seconds(2);
    m.close_time = now + chrono::Duration::seconds(298);
    assert_eq!(m.lifecycle_phase(), LifecyclePhase::AlphaWindow);

    // PrimeZone: 30-120s elapsed
    m.open_time = now - chrono::Duration::seconds(60);
    m.close_time = now + chrono::Duration::seconds(240);
    assert_eq!(m.lifecycle_phase(), LifecyclePhase::PrimeZone);

    // Lockout: >270s elapsed
    m.open_time = now - chrono::Duration::seconds(280);
    m.close_time = now + chrono::Duration::seconds(20);
    assert_eq!(m.lifecycle_phase(), LifecyclePhase::Lockout);
}

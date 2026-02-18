#![allow(dead_code)]

mod config;
mod execution;
mod feeds;
mod models;
mod risk;
mod signals;
mod strategies;
mod telemetry;

use crate::config::Config;
use crate::models::market::Asset;
use crate::execution::batch_submitter::BatchSubmitter;
use crate::execution::clob_client::ClobClient;
use crate::execution::fill_tracker::FillTracker;
use crate::execution::order_builder::OrderBuilder;
use crate::feeds::binance::BinanceFeed;
use crate::feeds::market_discovery::MarketDiscovery;
use crate::feeds::polymarket::PolymarketFeed;
use crate::feeds::user_ws::UserWsFeed;
use crate::risk::position_manager::PositionManager;
use crate::risk::risk_manager::RiskManager;
use crate::strategies::orchestrator::StrategyOrchestrator;
use crate::signals::realtime_vol::RealtimeVolTracker;
use crate::telemetry::alerts::AlertManager;
use crate::telemetry::latency::LatencyTracker;
use crate::telemetry::pnl::PnlTracker;

use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment variables
    dotenv::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_thread_ids(true)
        .init();

    info!("================================================");
    info!("  SATTEBAAZ — Polymarket Trading Bot v0.1.0");
    info!("  Short-duration prediction market specialist");
    info!("================================================");

    // Load and validate config (reads .env automatically)
    let config = Config::load_or_default();

    if let Err(e) = config.validate() {
        error!("Config validation failed: {e}");
        info!("Running in dry-run / analysis mode...");
    }

    let dry_run = config.is_dry_run();
    if dry_run {
        warn!("DRY RUN MODE — orders will be signed with random key");
    }

    // Starting capital
    let starting_capital = Config::starting_capital();
    let starting_decimal = Decimal::from_f64_retain(starting_capital)
        .unwrap_or(Decimal::new(5, 0));

    info!("Starting capital: ${starting_capital}");

    // === Initialize components ===

    // Shutdown signal
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Data feeds
    let binance_feed = Arc::new(BinanceFeed::new(config.binance.clone()));
    let polymarket_feed = Arc::new(PolymarketFeed::new(config.polymarket.clone()));

    // Position management
    let position_mgr = Arc::new(PositionManager::new(starting_decimal));

    // Risk management
    let risk_mgr = Arc::new(RiskManager::new(
        config.risk.clone(),
        position_mgr.clone(),
    ));

    // Execution
    let mut order_builder = OrderBuilder::new(
        config.polymarket.chain_id,
        config.polymarket.private_key.clone(),
        config.polymarket.funder_address.clone(),
        config.polymarket.signature_type,
    );
    // All Polymarket up/down markets use the Neg Risk CTF Exchange adapter
    order_builder.set_neg_risk(true);
    let clob_client = ClobClient::new(config.polymarket.clone());
    let batch_submitter = Arc::new(BatchSubmitter::new(order_builder, clob_client));
    let fill_tracker = Arc::new(FillTracker::new());

    // Strategy orchestrator
    let orchestrator = Arc::new(StrategyOrchestrator::new(config.strategy.clone()));

    // Real-time volatility tracker
    let vol_tracker = Arc::new(RealtimeVolTracker::new());

    // Telemetry
    let latency_tracker = Arc::new(LatencyTracker::new(1000));
    let pnl_tracker = Arc::new(PnlTracker::new(position_mgr.clone()));
    let alert_mgr = Arc::new(AlertManager::new(config.telemetry.clone()));

    // === Print market discovery info ===
    info!("--- Active market types ---");
    for (asset, duration) in MarketDiscovery::all_market_types() {
        let slug = MarketDiscovery::current_slug(asset, duration);
        let remaining = MarketDiscovery::time_remaining_in_current(duration);
        info!(
            "  {:?} {:?}: slug={} remaining={:.0}s",
            asset, duration, slug, remaining
        );
    }

    info!("--- Strategy configuration ---");
    info!("  Straddle+Bias: {}", config.strategy.straddle_enabled);
    info!("  Pure Arb:      {}", config.strategy.arb_enabled);
    info!("  Lag Exploit:   {}", config.strategy.lag_exploit_enabled);
    info!("  Market-Making: {}", config.strategy.market_making_enabled);
    info!("  Momentum:      {}", config.strategy.momentum_enabled);
    info!("  Max combined:  {}", config.strategy.straddle_max_combined);
    info!("  Arb min edge:  {}", config.strategy.arb_min_edge);
    info!("  Lag min edge:  {}", config.strategy.lag_min_edge);

    info!("--- Risk configuration ---");
    info!("  Max exposure:    {}%", config.risk.max_exposure_pct * 100.0);
    info!("  Max daily loss:  {}%", config.risk.max_daily_loss_pct * 100.0);
    info!("  Loss streak cap: {} consecutive", config.risk.loss_streak_threshold);

    // === Initialize CLOB authentication ===
    // Try to derive L2 API key for faster auth on order submissions
    if let Err(e) = batch_submitter.init_auth().await {
        warn!("CLOB auth init failed: {e} — will use L1 auth");
    }

    // === Start data feeds ===
    binance_feed.start(shutdown_tx.subscribe());
    binance_feed.start_funding_poller(shutdown_tx.subscribe());
    info!("Binance feed started (WS + funding poller)");

    polymarket_feed.start(&shutdown_tx);
    info!("Polymarket feed started");

    // Start CLOB user WebSocket for real-time fill events
    let user_ws = UserWsFeed::new(
        &config.polymarket.ws_host,
        &batch_submitter.address(),
    );
    user_ws.start(&shutdown_tx);
    info!("CLOB user WS started");

    // === Spawn fill consumer (from user WS) ===
    {
        let mut fill_rx = user_ws.subscribe_fills();
        let tracker = fill_tracker.clone();
        let pos_mgr = position_mgr.clone();
        let pnl = pnl_tracker.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = fill_rx.recv() => {
                        let event = match event {
                            Ok(e) => e,
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Fill channel lagged by {n} messages");
                                continue;
                            }
                            Err(_) => break,
                        };

                        // Record in fill tracker
                        let fill = crate::models::order::Fill {
                            order_id: event.order_id.clone(),
                            token_id: event.token_id.clone(),
                            side: event.side,
                            price: event.price,
                            size: event.size,
                            timestamp: chrono::Utc::now(),
                            fee: event.fee,
                        };
                        tracker.on_fill(fill.clone());

                        // Record in position manager
                        if !event.market_id.is_empty() {
                            pos_mgr.record_fill(
                                &fill,
                                &event.market_id,
                                event.market_side,
                                &event.strategy_tag,
                            ).await;
                        }

                        // Track P&L
                        pnl.record_fill(&event.token_id, event.price, event.size, event.side).await;
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    // === Spawn risk watchdog (every 500ms) ===
    {
        let risk = risk_mgr.clone();
        let submitter = batch_submitter.clone();
        let alerts = alert_mgr.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let action = risk.periodic_check().await;
                        match action {
                            crate::risk::risk_manager::RiskAction::KillSwitch => {
                                error!("KILL SWITCH — cancelling all orders");
                                let _ = submitter.cancel_all().await;
                                alerts.send("KILL SWITCH activated").await;
                            }
                            crate::risk::risk_manager::RiskAction::Pause(secs) => {
                                warn!("Risk pause for {secs}s");
                                alerts.send(&format!("Risk pause for {secs}s")).await;
                            }
                            crate::risk::risk_manager::RiskAction::ReduceSize(mult) => {
                                warn!("Size reduction active: {mult}x");
                            }
                            crate::risk::risk_manager::RiskAction::Continue => {}
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    // === Spawn telemetry loop (every 30s) ===
    {
        let pnl = pnl_tracker.clone();
        let latency = latency_tracker.clone();
        let binance = binance_feed.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        pnl.log_summary().await;
                        latency.log_summary();
                        // Decay liquidation counters
                        binance.reset_liquidations().await;
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    // === Spawn balance sync loop (every 15s — dynamic position sizing + compounding) ===
    {
        let submitter = batch_submitter.clone();
        let pos_mgr = position_mgr.clone();
        let _alerts = alert_mgr.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            // Wait 5s for auth to initialize before first balance fetch
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(15));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        match submitter.fetch_balance().await {
                            Ok(balance) => {
                                pos_mgr.sync_capital_from_balance(balance).await;
                            }
                            Err(e) => {
                                debug!("Balance fetch failed: {e}");
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    // === Spawn strategy execution loop (driven by price updates) ===
    {
        let mut price_rx = binance_feed.subscribe_prices();
        let orch = orchestrator.clone();
        let binance = binance_feed.clone();
        let poly = polymarket_feed.clone();
        let risk = risk_mgr.clone();
        let submitter = batch_submitter.clone();
        let tracker = fill_tracker.clone();
        let pos_mgr = position_mgr.clone();
        let latency = latency_tracker.clone();
        let alerts = alert_mgr.clone();
        let vol = vol_tracker.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            // Throttle: evaluate at most once per 200ms per asset
            let mut last_eval: HashMap<Asset, tokio::time::Instant> =
                HashMap::new();
            let eval_cooldown = tokio::time::Duration::from_millis(200);

            loop {
                tokio::select! {
                    price_update = price_rx.recv() => {
                        let (asset, binance_price) = match price_update {
                            Ok(p) => p,
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Price channel lagged by {n} messages");
                                continue;
                            }
                            Err(_) => break,
                        };

                        // Feed price to vol tracker
                        let now_ms = chrono::Utc::now().timestamp_millis();
                        vol.on_price(asset, binance_price, now_ms).await;

                        // Throttle per-asset
                        let now = tokio::time::Instant::now();
                        if let Some(last) = last_eval.get(&asset) {
                            if now.duration_since(*last) < eval_cooldown {
                                continue;
                            }
                        }
                        last_eval.insert(asset, now);

                        // Skip if kill switch active
                        if risk.killed.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }

                        // Get market types for this asset
                        let market_types: Vec<_> = MarketDiscovery::all_market_types()
                            .into_iter()
                            .filter(|(a, _)| *a == asset)
                            .collect();

                        let available_capital = pos_mgr.available_capital().await;

                        for (_asset, duration) in &market_types {
                            let slug = MarketDiscovery::current_slug(asset, *duration);
                            let remaining = MarketDiscovery::time_remaining_in_current(*duration);

                            // Skip if too close to resolution
                            if remaining < 10.0 {
                                continue;
                            }

                            // Look up market from Polymarket feed cache
                            let mut market = match poly.get_market(&slug) {
                                Some(m) => m,
                                None => continue, // Not yet discovered
                            };

                            // Set reference price from Binance on first tick
                            market.set_reference_price(binance_price);

                            // Get order books
                            let yes_book = match poly.get_book(&market.yes_token_id) {
                                Some(b) => b,
                                None => continue,
                            };
                            let no_book = match poly.get_book(&market.no_token_id) {
                                Some(b) => b,
                                None => continue,
                            };

                            // Compute signals
                            let vol_regime = vol.regime(asset).await;
                            let move_1s = binance.get_1s_move_pct(asset).await;
                            let net_liqs = binance.get_net_liquidations(asset).await;
                            let funding = binance.get_funding_rate(asset).await;
                            let liq_active = net_liqs.abs() > 100_000.0;
                            let inventory = pos_mgr.net_yes_inventory(&slug).await;

                            // Evaluate all strategies via orchestrator
                            let orders = orch.evaluate(
                                &market,
                                &yes_book,
                                &no_book,
                                vol_regime,
                                available_capital,
                                binance_price,
                                None,  // arb_signal: computed inside pure_arb
                                None,  // bias_signal: computed inside straddle_bias
                                None,  // momentum_signal: computed inside momentum_capture
                                inventory,
                                move_1s,
                                funding, // use funding rate as order flow proxy
                                liq_active,
                            );

                            if orders.is_empty() {
                                continue;
                            }

                            // Risk-check each order
                            let mut approved_orders = Vec::new();
                            for order in &orders {
                                match risk.check_order(order).await {
                                    Ok(()) => approved_orders.push(order.clone()),
                                    Err(e) => {
                                        debug!("Order rejected by risk: {e}");
                                    }
                                }
                            }

                            if approved_orders.is_empty() {
                                continue;
                            }

                            // Apply size multiplier from risk manager
                            let size_mult = risk.current_size_multiplier().await;
                            if size_mult < 1.0 {
                                for order in &mut approved_orders {
                                    let current = order.size.to_string().parse::<f64>().unwrap_or(0.0);
                                    order.size = Decimal::from_f64_retain(current * size_mult)
                                        .unwrap_or(Decimal::ZERO);
                                }
                            }

                            // Submit
                            let _timer = latency.start_timer("order_submit");
                            match submitter.submit(&approved_orders).await {
                                Ok(results) => {
                                    let mut success = 0usize;
                                    for (result, intent) in results.iter().zip(approved_orders.iter()) {
                                        if result.is_success() {
                                            tracker.watch(result.clone());
                                            success += 1;

                                            // Record fill with position manager
                                            // For GTC/GTD orders, fills arrive later via WS.
                                            // For FOK/FAK, the initial result is the fill.
                                            if result.filled_size > Decimal::ZERO {
                                                let fill = crate::models::order::Fill {
                                                    order_id: result.order_id.clone(),
                                                    token_id: result.token_id.clone(),
                                                    side: intent.order_side,
                                                    price: result.avg_fill_price,
                                                    size: result.filled_size,
                                                    timestamp: result.timestamp,
                                                    fee: Decimal::ZERO, // CLOB charges taker fee separately
                                                };
                                                pos_mgr.record_fill(
                                                    &fill,
                                                    &slug,
                                                    intent.market_side,
                                                    &intent.strategy_tag,
                                                ).await;
                                            }
                                        }
                                    }
                                    if success > 0 {
                                        info!(
                                            "Submitted {success}/{} orders for {slug}",
                                            approved_orders.len()
                                        );
                                    }
                                }
                                Err(e) => {
                                    error!("Order submission failed for {slug}: {e}");
                                    alerts.send(&format!("Submit error: {e}")).await;
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    // === Spawn market resolution tracker (every 5s) ===
    {
        let poly = polymarket_feed.clone();
        let binance = binance_feed.clone();
        let pos_mgr = position_mgr.clone();
        let _pnl = pnl_tracker.clone();
        let alerts = alert_mgr.clone();
        let tracker = fill_tracker.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            // Track markets we've already resolved to avoid double-settling
            let mut resolved_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Check all market types for resolution
                        for (asset, duration) in MarketDiscovery::all_market_types() {
                            let slug = MarketDiscovery::current_slug(asset, duration);
                            let remaining = MarketDiscovery::time_remaining_in_current(duration);

                            // Market has resolved (past close time)
                            if remaining <= 0.0 && !resolved_slugs.contains(&slug) {
                                // Check if we have positions in this market
                                let pos_count = pos_mgr.position_count(&slug).await;
                                if pos_count == 0 {
                                    resolved_slugs.insert(slug.clone());
                                    continue;
                                }

                                // Get the market from Polymarket feed cache
                                let market = match poly.get_market(&slug) {
                                    Some(m) => m,
                                    None => {
                                        resolved_slugs.insert(slug.clone());
                                        continue;
                                    }
                                };

                                // Determine winner: compare current Binance price vs reference
                                let current_price = match binance.get_price(asset).await {
                                    Some(p) => p,
                                    None => continue,
                                };
                                let ref_price = market.reference_price;

                                if ref_price == 0.0 {
                                    continue;
                                }

                                let winning_side = if current_price >= ref_price {
                                    crate::models::market::Side::Yes // Price went up
                                } else {
                                    crate::models::market::Side::No  // Price went down
                                };

                                info!(
                                    "Market resolved: {slug} ref={ref_price:.2} final={current_price:.2} winner={winning_side:?}"
                                );

                                // Settle positions
                                pos_mgr.record_resolution(&slug, winning_side).await;

                                // Clean up fill tracker
                                tracker.cleanup_completed();

                                // Alert
                                let capital = pos_mgr.available_capital().await;
                                alerts.send(&format!(
                                    "Resolved {slug}: {winning_side:?} won | Capital: ${capital:.2}"
                                )).await;

                                resolved_slugs.insert(slug);
                            }

                            // Clean up old slugs from the set (keep it bounded)
                            if resolved_slugs.len() > 100 {
                                resolved_slugs.clear();
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => break,
                }
            }
        });
    }

    info!("=== SATTEBAAZ running ===");
    info!("All systems active: Binance WS, Polymarket WS, strategies, risk, resolution tracker");
    info!("See docs/ for complete strategy documentation.");
    info!("Press Ctrl+C to shutdown.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received. Cleaning up...");
    let _ = shutdown_tx.send(());

    // Cancel all open orders on shutdown
    if let Err(e) = batch_submitter.cancel_all().await {
        error!("Failed to cancel orders on shutdown: {e}");
    }

    // Final P&L summary
    pnl_tracker.log_summary().await;
    latency_tracker.log_summary();

    info!("SATTEBAAZ shutdown complete.");
    Ok(())
}

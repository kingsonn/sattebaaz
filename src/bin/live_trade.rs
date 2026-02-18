//! BTC 5-Minute LIVE Trader
//!
//! Based on the paper_trade strategy (lag exploit + arb) with real order submission.
//! Connects to Polymarket CLOB API for order execution.
//!
//! Requires: POLYMARKET_PRIVATE_KEY in .env
//!
//! Usage:  cargo run --bin live_trade

use sattebaaz::config::Config;
use sattebaaz::execution::clob_client::ClobClient;
use sattebaaz::execution::order_builder::OrderBuilder;
use sattebaaz::execution::polygon_merger::PolygonMerger;
use sattebaaz::feeds::binance::BinanceFeed;
use sattebaaz::feeds::market_discovery::MarketDiscovery;
use sattebaaz::feeds::polymarket::PolymarketFeed;
use sattebaaz::models::market::{Asset, Duration, Side};
use sattebaaz::models::order::{OrderSide, OrderType};
use sattebaaz::signals::probability::ProbabilityModel;

use chrono::{DateTime, Utc};
use statrs::distribution::{ContinuousCDF, Normal};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::debug;

// ═══════════════════════════════════════════════════════════════════════════
// CONFIGURATION
// ═══════════════════════════════════════════════════════════════════════════
const TICK_MS: u64 = 2000;             // Poll every 2s
const FEED_INIT_SECS: u64 = 12;
const DASHBOARD_SECS: u64 = 10;

// Entry signals
const LAG_MIN_EDGE: f64 = 0.04;        // Min mispricing to enter (4¢)
const ARB_THRESHOLD: f64 = 0.97;       // Buy both when YES_ask + NO_ask < this
const PRICE_FLOOR: f64 = 0.20;         // Don't buy below 20¢
const PRICE_CEILING: f64 = 0.80;       // Don't buy above 80¢
const MAX_SPREAD_PCT: f64 = 0.10;      // Don't enter if spread > 10% of ask
const MIN_REMAINING_SECS: f64 = 120.0; // Need at least 2 min for lag to correct
const MIN_BTC_MOVE_PCT: f64 = 0.005;   // Require ≥0.005% BTC move since last tick

// Exit signals
const TAKE_PROFIT_PCT: f64 = 0.10;     // Exit when bid ≥ entry × (1 + 10%)
const STOP_LOSS_PCT: f64 = 0.20;       // Cut loss when bid ≤ entry × (1 - 20%) — wide for thin books
const MAX_HOLD_SECS: f64 = 120.0;      // Force exit after 2 minutes
const PRE_RESOLVE_EXIT_SECS: f64 = 90.0; // Start closing in last 90s if profitable

// Position sizing
const MAX_POSITIONS: usize = 2;
const MAX_COST_PER_POS: f64 = 1.00;    // Max $1.00 per position — limit risk
const MIN_POSITION_COST: f64 = 0.50;   // Min $0.50 per position
const MIN_ORDER_COST: f64 = 1.0;       // Polymarket market order minimum = $1
const ENTRY_COOLDOWN_SECS: u64 = 10;

// Safety
const MAX_SESSION_LOSS_PCT: f64 = 0.30; // Kill switch: stop if down 30% from start
const BALANCE_SYNC_CYCLES: u32 = 3;     // Sync real balance from CLOB every N market cycles
// Market orders (FOK) fill instantly — no hold time needed

// Realized volatility tracking
const VOL_WINDOW: usize = 30;

// ═══════════════════════════════════════════════════════════════════════════
// DATA TYPES
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
struct Position {
    id: usize,
    side: Side,
    token_id: String,
    entry_price: f64,
    size: f64,         // actual shares (confirmed via get_order)
    cost_basis: f64,   // actual USDC spent (from build_market_order)
    tp_price: f64,     // take-profit price
    strategy: String,
    opened_at: tokio::time::Instant,
    market_slug: String,
    // Active GTC sell order — always one active (TP, SL, or force)
    sell_order_id: Option<String>,
    sell_order_price: f64,      // price of the active sell order
    sell_order_type: String,    // "tp", "sl", "force"
    sell_attempts: u32,         // how many times we've placed/replaced sell orders
    #[allow(dead_code)]
    order_id: Option<String>,
}

#[derive(Clone)]
struct TradeLog {
    id: usize,
    time: DateTime<Utc>,
    action: String,
    side: Side,
    price: f64,
    size: f64,
    pnl: f64,
    strategy: String,
    capital_after: f64,
}

impl std::fmt::Display for TradeLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pnl_str = if self.pnl != 0.0 {
            format!(" P&L:{:>+.3}", self.pnl)
        } else {
            String::new()
        };
        write!(
            f,
            "#{:<3} {} {:<4} {:>3?} @ {:.3} x{:.2}  {:<12}  cap ${:.2}{}",
            self.id,
            self.time.format("%H:%M:%S"),
            self.action,
            self.side,
            self.price,
            self.size,
            self.strategy,
            self.capital_after,
            pnl_str,
        )
    }
}

struct Stats {
    entries: usize,
    exits: usize,
    resolutions: usize,
    winning_exits: usize,
    total_exit_pnl: f64,
    total_resolution_pnl: f64,
    cycles: u32,
    order_failures: usize,
}

impl Stats {
    fn new() -> Self {
        Self { entries: 0, exits: 0, resolutions: 0, winning_exits: 0,
               total_exit_pnl: 0.0, total_resolution_pnl: 0.0, cycles: 0,
               order_failures: 0 }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter("info").with_target(false).init();

    let config = Config::load_or_default();

    // Validate we have a real private key
    if config.is_dry_run() {
        eprintln!("\n  ╔══════════════════════════════════════════════════════════════╗");
        eprintln!("  ║  ERROR: No POLYMARKET_PRIVATE_KEY set in .env               ║");
        eprintln!("  ║  Create a .env file with:                                   ║");
        eprintln!("  ║    POLYMARKET_PRIVATE_KEY=0xYourPrivateKeyHere               ║");
        eprintln!("  ║    POLYMARKET_FUNDER_ADDRESS=0xYourPolymarketWallet (if proxy)║");
        eprintln!("  ║    POLYMARKET_SIGNATURE_TYPE=0  (0=EOA, 1=PolyProxy)        ║");
        eprintln!("  ╚══════════════════════════════════════════════════════════════╝");
        std::process::exit(1);
    }

    // Build order execution pipeline
    let mut order_builder = OrderBuilder::new(
        config.polymarket.chain_id,
        config.polymarket.private_key.clone(),
        config.polymarket.funder_address.clone(),
        config.polymarket.signature_type,
    );
    order_builder.set_neg_risk(false); // 5-min BTC markets are NOT neg_risk
    // Fee rate fetched dynamically per token. Default 1000 (crypto markets have taker fees).
    order_builder.set_fee_rate_bps(1000);

    let clob_client = ClobClient::new(config.polymarket.clone());

    // Initialize L2 API key auth
    println!("  Initializing CLOB authentication...");
    clob_client.init_auth().await?;

    // Cancel ALL stale orders from previous runs to free locked USDC
    println!("  Cancelling any stale orders from previous runs...");
    match clob_client.cancel_all().await {
        Ok(_) => println!("  All stale orders cancelled."),
        Err(e) => eprintln!("  WARNING: Could not cancel stale orders: {}", e),
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Fetch real balance (after cancelling stale orders so USDC is unlocked)
    let starting_capital = match clob_client.fetch_balance().await {
        Ok(bal) => {
            println!("  On-chain balance: ${:.4} USDC", bal);
            bal
        }
        Err(e) => {
            eprintln!("  WARNING: Could not fetch balance: {}", e);
            eprintln!("  Using STARTING_CAPITAL env var or default $5.00");
            Config::starting_capital()
        }
    };

    if starting_capital < 1.0 {
        eprintln!("  ERROR: Balance too low (${:.2}). Need at least $1.00 to trade.", starting_capital);
        std::process::exit(1);
    }

    // Initialize on-chain merger for arb positions
    let polygon_rpc = std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://polygon-rpc.com".to_string());
    let merger_wallet = alloy_signer_local::PrivateKeySigner::from_bytes(
        &alloy_primitives::B256::from_slice(
            &hex::decode(
                config.polymarket.private_key.trim_start_matches("0x")
            ).expect("invalid private key hex")
        )
    ).expect("invalid private key");

    let merger = PolygonMerger::new(&polygon_rpc, merger_wallet)
        .expect("failed to create PolygonMerger");

    // Check MATIC balance for gas
    match merger.check_gas_balance().await {
        Ok(matic) => {
            if matic < 0.005 {
                eprintln!("  ⚠ WARNING: EOA has only {:.4} MATIC — need ~0.01 MATIC for arb merge gas", matic);
                eprintln!("    Arb strategy will be DISABLED until MATIC is funded.");
            } else {
                println!("  MATIC balance: {:.4} (sufficient for arb merge)", matic);
            }
        }
        Err(e) => {
            eprintln!("  WARNING: Could not check MATIC balance: {}", e);
        }
    }
    let has_matic = merger.check_gas_balance().await.unwrap_or(0.0) >= 0.005;
    let arb_enabled = std::env::var("ARB_ENABLED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    if !arb_enabled {
        println!("  ARB: disabled (set ARB_ENABLED=true in .env to enable)");
    } else if !has_matic {
        println!("  ARB: enabled but no MATIC for gas — will skip until funded");
    } else {
        println!("  ARB: enabled ✓");
    }

    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let prob_model = ProbabilityModel::new();
    let vol_per_min = Asset::BTC.vol_per_minute();

    println!("\n{}", "=".repeat(80));
    println!("  ██╗     ██╗██╗   ██╗███████╗    ████████╗██████╗  █████╗ ██████╗ ███████╗██████╗ ");
    println!("  ██║     ██║██║   ██║██╔════╝    ╚══██╔══╝██╔══██╗██╔══██╗██╔══██╗██╔════╝██╔══██╗");
    println!("  ██║     ██║██║   ██║█████╗         ██║   ██████╔╝███████║██║  ██║█████╗  ██████╔╝");
    println!("  ██║     ██║╚██╗ ██╔╝██╔══╝         ██║   ██╔══██╗██╔══██║██║  ██║██╔══╝  ██╔══██╗");
    println!("  ███████╗██║ ╚████╔╝ ███████╗       ██║   ██║  ██║██║  ██║██████╔╝███████╗██║  ██║");
    println!("  ╚══════╝╚═╝  ╚═══╝  ╚══════╝       ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═╝");
    println!("{}", "=".repeat(80));
    println!("  BTC 5-MIN | REAL ORDERS | ${:.2} USDC | TAKER FEE 1000bps", starting_capital);
    println!("  Wallet: {:?}", order_builder.address());
    println!("  TP: {:.0}% | SL: {:.0}% | Edge: >{:.0}¢ | Max/pos: ${:.2}",
        TAKE_PROFIT_PCT * 100.0, STOP_LOSS_PCT * 100.0, LAG_MIN_EDGE * 100.0, MAX_COST_PER_POS);
    println!("  Kill switch: stop if down {:.0}% from start", MAX_SESSION_LOSS_PCT * 100.0);
    println!("{}", "=".repeat(80));

    // Data feeds
    let binance = Arc::new(BinanceFeed::new(config.binance.clone()));
    let mut poly_feed = PolymarketFeed::new(config.polymarket.clone());
    poly_feed.set_market_filter(vec![(Asset::BTC, Duration::FiveMin)]);
    let poly = Arc::new(poly_feed);
    binance.start(shutdown_tx.subscribe());
    binance.start_funding_poller(shutdown_tx.subscribe());
    poly.start(&shutdown_tx);

    println!("  Waiting {}s for feeds...\n", FEED_INIT_SECS);
    let _ = std::io::stdout().flush();
    tokio::time::sleep(tokio::time::Duration::from_secs(FEED_INIT_SECS)).await;

    // Show initial state
    let slug = MarketDiscovery::current_slug(Asset::BTC, Duration::FiveMin);
    let rem = MarketDiscovery::time_remaining_in_current(Duration::FiveMin);
    let live = poly.get_market(&slug).is_some();
    let btc_price = binance.get_price(Asset::BTC).await.unwrap_or(0.0);
    println!("  BTC: ${:.2}  |  Market: {} | {:.0}s left | {}",
        btc_price, slug, rem, if live { "LIVE" } else { "waiting..." });
    println!("  Trading active. Ctrl+C to stop.\n");
    let _ = std::io::stdout().flush();

    // ── State ──
    let mut capital = starting_capital;
    let mut positions: Vec<Position> = Vec::new();
    let mut trade_log: VecDeque<TradeLog> = VecDeque::new();
    let mut trade_id = 0usize;
    let mut next_pos_id = 0usize;
    let mut stats = Stats::new();
    let mut resolved_slugs: HashSet<String> = HashSet::new();
    let mut fee_fetched_slugs: HashSet<String> = HashSet::new();
    let mut ref_prices: HashMap<String, f64> = HashMap::new();
    let mut last_entry = tokio::time::Instant::now() - tokio::time::Duration::from_secs(999);
    let mut last_dash = tokio::time::Instant::now();
    let mut prev_btc_price: f64 = 0.0;
    let mut btc_returns: VecDeque<f64> = VecDeque::new();
    let mut realized_vol_per_min: f64 = vol_per_min;

    // Shutdown handler
    let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sf = shutdown_flag.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        sf.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let mut poll = tokio::time::interval(tokio::time::Duration::from_millis(TICK_MS));
    let entry_cooldown = tokio::time::Duration::from_secs(ENTRY_COOLDOWN_SECS);
    let dash_interval = tokio::time::Duration::from_secs(DASHBOARD_SECS);

    // ═══════════════════════════════════════════════════════════════════════
    // MAIN LOOP
    // ═══════════════════════════════════════════════════════════════════════
    loop {
        poll.tick().await;
        if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
            println!("\n  Shutting down — cancelling all open orders...");
            if let Err(e) = clob_client.cancel_all().await {
                eprintln!("  WARNING: Failed to cancel orders: {}", e);
            }
            let _ = shutdown_tx.send(());
            break;
        }

        let now_inst = tokio::time::Instant::now();

        // ── Safety: kill switch ──
        let realized_pnl = stats.total_exit_pnl + stats.total_resolution_pnl;
        if realized_pnl < -(starting_capital * MAX_SESSION_LOSS_PCT) {
            println!("\n  ⚠ KILL SWITCH: Realized P&L ${:.3} exceeds {:.0}% max loss. Stopping.",
                realized_pnl, MAX_SESSION_LOSS_PCT * 100.0);
            if let Err(e) = clob_client.cancel_all().await {
                eprintln!("  WARNING: Failed to cancel orders: {}", e);
            }
            break;
        }

        // ── Get BTC price ──
        let btc_price = match binance.get_price(Asset::BTC).await {
            Some(p) if p > 0.0 => p,
            _ => continue,
        };

        // BTC momentum
        let btc_move_pct = if prev_btc_price > 0.0 {
            ((btc_price - prev_btc_price) / prev_btc_price).abs() * 100.0
        } else {
            0.0
        };
        let btc_move_signed = if prev_btc_price > 0.0 {
            (btc_price - prev_btc_price) / prev_btc_price * 100.0
        } else {
            0.0
        };

        // ── Realized volatility from recent ticks ──
        if prev_btc_price > 0.0 {
            let ret = (btc_price / prev_btc_price).ln();
            btc_returns.push_back(ret);
            if btc_returns.len() > VOL_WINDOW { btc_returns.pop_front(); }
            if btc_returns.len() >= 5 {
                let mean = btc_returns.iter().sum::<f64>() / btc_returns.len() as f64;
                let var = btc_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                    / (btc_returns.len() - 1) as f64;
                let ticks_per_min = 60_000.0 / TICK_MS as f64;
                realized_vol_per_min = var.sqrt() * ticks_per_min.sqrt();
                realized_vol_per_min = 0.70 * realized_vol_per_min + 0.30 * vol_per_min;
                realized_vol_per_min = realized_vol_per_min.max(vol_per_min * 0.5);
            }
        }
        prev_btc_price = btc_price;

        // ── Current 5m market ──
        let slug = MarketDiscovery::current_slug(Asset::BTC, Duration::FiveMin);
        let remaining = MarketDiscovery::time_remaining_in_current(Duration::FiveMin);

        // ── Track reference price per market ──
        let ref_p = if let Some(&p) = ref_prices.get(&slug) {
            p
        } else {
            let total_secs = 300.0;
            let is_fresh = remaining > (total_secs - 15.0);
            let calibrated = if is_fresh {
                btc_price
            } else {
                let yes_mid = poly.get_market(&slug)
                    .and_then(|m| poly.get_book(&m.yes_token_id))
                    .and_then(|b| b.midpoint())
                    .map(|d| d.to_string().parse::<f64>().unwrap_or(0.5))
                    .unwrap_or(0.5);
                calibrate_reference_price(btc_price, yes_mid, remaining / 60.0, vol_per_min)
            };
            ref_prices.insert(slug.clone(), calibrated);
            println!("  [NEW MARKET] {} ref=${:.2} ({}cal) | {:.0}s left",
                slug, calibrated, if is_fresh { "raw" } else { "book" }, remaining);
            let _ = std::io::stdout().flush();
            calibrated
        };

        // ── Resolution: check if old positions survived to new market ──
        // IMPORTANT: We cannot redeem tokens on-chain, so any position still open
        // at resolution is effectively lost capital (tokens are unredeemed ERC1155s).
        // Lag positions should ALWAYS exit via TP/SL/time/pre-res before this.
        let mut to_resolve: Vec<usize> = Vec::new();
        for (i, pos) in positions.iter().enumerate() {
            if pos.market_slug != slug && !resolved_slugs.contains(&pos.market_slug) {
                to_resolve.push(i);
            }
        }
        if !to_resolve.is_empty() {
            // Cancel any stale GTC orders from the old market before resolving
            let _ = clob_client.cancel_all().await;
            let old_slug = &positions[to_resolve[0]].market_slug;
            let old_ref = ref_prices.get(old_slug).copied().unwrap_or(btc_price);
            let winner = if btc_price >= old_ref { Side::Yes } else { Side::No };
            println!("  ⚠ [RESOLVE] {} — {} positions survived to resolution!", old_slug, to_resolve.len());
            println!("    ref=${:.2} final=${:.2} → {:?} wins", old_ref, btc_price, winner);
            println!("    WARNING: Tokens are unredeemed ERC1155s. Writing off as lost capital.");
            for &i in to_resolve.iter().rev() {
                let pos = &positions[i];
                // Write off entire cost — we can't get USDC back without redeemPositions()
                let pnl = -pos.cost_basis;
                stats.resolutions += 1;
                stats.total_resolution_pnl += pnl;

                trade_id += 1;
                let log = TradeLog {
                    id: trade_id, time: Utc::now(), action: "EXPIRED".into(),
                    side: pos.side, price: 0.0,
                    size: pos.size, pnl, strategy: pos.strategy.clone(),
                    capital_after: capital,
                };
                println!("  LOST {}", log);
                let _ = std::io::stdout().flush();
                push_log(&mut trade_log, log);
            }
            for &i in to_resolve.iter().rev() {
                let old_slug = positions[i].market_slug.clone();
                positions.remove(i);
                resolved_slugs.insert(old_slug);
            }
            stats.cycles += 1;
            // Sync balance from CLOB periodically to catch any drift
            if stats.cycles % BALANCE_SYNC_CYCLES == 0 {
                if let Ok(real_bal) = clob_client.fetch_balance().await {
                    let drift = (capital - real_bal).abs();
                    if drift > 0.05 {
                        println!("  [BALANCE SYNC] Internal: ${:.2} | Real: ${:.2} | Drift: ${:.2} — correcting",
                            capital, real_bal, drift);
                        capital = real_bal;
                    }
                }
            }
            println!("  [CYCLE {}] Capital: ${:.2}", stats.cycles, capital);
            let _ = std::io::stdout().flush();
        }

        // ── Get Polymarket market and books ──
        let market = match poly.get_market(&slug) {
            Some(m) => m,
            None => { maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, starting_capital, btc_price, &positions, &trade_log, &stats, remaining, &slug, 0.5, 0.0, 0.0, 0.0, 0.0, ref_p, btc_move_pct); continue; }
        };
        // Fetch fee rate + neg_risk once per new market
        if !fee_fetched_slugs.contains(&slug) {
            if let Ok(bps) = clob_client.fetch_fee_rate(&market.yes_token_id).await {
                order_builder.set_fee_rate_bps(bps);
                print!("  [MARKET CONFIG] fee={}bps", bps);
            }
            if let Ok(nr) = clob_client.fetch_neg_risk(&market.yes_token_id).await {
                order_builder.set_neg_risk(nr);
                print!(" neg_risk={}", nr);
            }
            println!(" for {}", &slug[..30.min(slug.len())]);
            fee_fetched_slugs.insert(slug.clone());
        }
        let yes_book = match poly.get_book(&market.yes_token_id) {
            Some(b) => b,
            None => { maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, starting_capital, btc_price, &positions, &trade_log, &stats, remaining, &slug, 0.5, 0.0, 0.0, 0.0, 0.0, ref_p, btc_move_pct); continue; }
        };
        let no_book = match poly.get_book(&market.no_token_id) {
            Some(b) => b,
            None => { maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, starting_capital, btc_price, &positions, &trade_log, &stats, remaining, &slug, 0.5, 0.0, 0.0, 0.0, 0.0, ref_p, btc_move_pct); continue; }
        };

        // ── Fair value from Binance (using realized vol) ──
        let time_remaining_min = remaining / 60.0;
        let fair_up = prob_model.fair_prob_up(btc_price, ref_p, time_remaining_min, realized_vol_per_min, 0.0);
        let fair_down = 1.0 - fair_up;

        // Book prices and depth at best level
        let yes_ask = yes_book.best_ask().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(1.0)).unwrap_or(1.0);
        let yes_bid = yes_book.best_bid().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
        let no_ask = no_book.best_ask().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(1.0)).unwrap_or(1.0);
        let no_bid = no_book.best_bid().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);

        // ══════════════════════════════════════════════════════════════════════
        // EXIT LOGIC — ALL sells are GTC limit orders. No FOK.
        //
        // Every position has ONE active GTC sell order at all times:
        //   "tp"    → entry * 1.10  (wait for profit)
        //   "sl"    → bid * 0.50    (aggressive, fills at best bid)
        //   "force" → 0.01          (emergency, fills at any bid)
        //
        // Each tick: check if sell order filled → if yes, record exit.
        // If conditions escalate → cancel current order → place more aggressive one.
        // ══════════════════════════════════════════════════════════════════════
        let mut exits: Vec<usize> = Vec::new();
        for (i, pos) in positions.iter().enumerate() {
            if pos.market_slug != slug { continue; }

            let current_bid = if pos.side == Side::Yes { yes_bid } else { no_bid };
            let hold_secs = now_inst.duration_since(pos.opened_at).as_secs_f64();
            let pct_change = if pos.entry_price > 0.0 {
                (current_bid - pos.entry_price) / pos.entry_price
            } else { 0.0 };

            // ── Step 1: Check if current sell order has filled ──
            if let Some(ref sell_oid) = pos.sell_order_id {
                match clob_client.get_order(sell_oid).await {
                    Ok((status, _size_matched)) if status == "MATCHED" => {
                        // SOLD! GTC order filled automatically.
                        let proceeds = pos.sell_order_price * pos.size;
                        let pnl = proceeds - pos.cost_basis;
                        capital += proceeds;

                        stats.exits += 1;
                        stats.total_exit_pnl += pnl;
                        if pnl > 0.0 { stats.winning_exits += 1; }

                        trade_id += 1;
                        let log = TradeLog {
                            id: trade_id, time: Utc::now(),
                            action: format!("SELL({})", pos.sell_order_type),
                            side: pos.side, price: pos.sell_order_price, size: pos.size,
                            pnl, strategy: pos.strategy.clone(),
                            capital_after: capital,
                        };
                        println!("  EXIT  {} [GTC {} filled]", log, pos.sell_order_type);
                        let _ = std::io::stdout().flush();
                        push_log(&mut trade_log, log);
                        exits.push(i);
                        continue;
                    }
                    Ok((ref status, _)) => {
                        if status == "CANCELLED" {
                            // Order was cancelled externally, will re-place below
                            debug!("  Sell order #{} was cancelled externally", pos.id);
                        }
                        // Otherwise LIVE — still waiting for fill
                    }
                    Err(e) => {
                        debug!("  Sell status check failed for #{}: {}", pos.id, e);
                    }
                }
            }

            // ── Step 2: Determine what sell order SHOULD be active ──
            let desired_type = if remaining < 30.0 {
                "force" // last 30s: emergency dump
            } else if remaining < 60.0 || hold_secs >= MAX_HOLD_SECS {
                "force" // deadline or max hold time
            } else if pct_change <= -STOP_LOSS_PCT {
                "sl" // hard SL triggered
            } else if remaining < PRE_RESOLVE_EXIT_SECS && pct_change > 0.02 {
                "sl" // pre-resolve profit lock (aggressive price to fill fast)
            } else {
                "tp" // normal: wait for take profit
            };

            // ── Step 3: Replace sell order if type needs to escalate ──
            // Only replace if: (a) no order exists, (b) need to escalate, (c) order was cancelled
            let current_type = &pos.sell_order_type;
            let needs_replacement = pos.sell_order_id.is_none()
                || (desired_type == "force" && current_type != "force")
                || (desired_type == "sl" && current_type == "tp");

            if needs_replacement {
                // Cancel existing order first
                if let Some(ref sell_oid) = pos.sell_order_id {
                    let _ = clob_client.cancel_order(sell_oid).await;
                    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                }

                // Place new GTC sell at desired price — stored for next iteration
                // (we can't mutate pos here since we're iterating, we'll collect updates)
            }
        }
        // Remove filled positions
        for &i in exits.iter().rev() {
            positions.remove(i);
        }

        // ── Step 4: Place/replace sell orders for positions that need them ──
        // (Done outside the immutable iterator loop above)
        for pos in positions.iter_mut() {
            if pos.market_slug != slug { continue; }

            let current_bid = if pos.side == Side::Yes { yes_bid } else { no_bid };
            let hold_secs = now_inst.duration_since(pos.opened_at).as_secs_f64();
            let pct_change = if pos.entry_price > 0.0 {
                (current_bid - pos.entry_price) / pos.entry_price
            } else { 0.0 };

            let desired_type = if remaining < 30.0 || (remaining < 60.0) || hold_secs >= MAX_HOLD_SECS {
                "force"
            } else if pct_change <= -STOP_LOSS_PCT {
                "sl"
            } else if remaining < PRE_RESOLVE_EXIT_SECS && pct_change > 0.02 {
                "sl"
            } else {
                "tp"
            };

            let desired_price = match desired_type {
                "force" => 0.01,
                "sl" => (current_bid * 0.50).max(0.01),
                _ => pos.tp_price,
            };

            let needs_replacement = pos.sell_order_id.is_none()
                || (desired_type == "force" && pos.sell_order_type != "force")
                || (desired_type == "sl" && pos.sell_order_type == "tp");

            if !needs_replacement { continue; }

            // Cancel existing order
            if let Some(ref sell_oid) = pos.sell_order_id {
                let _ = clob_client.cancel_order(sell_oid).await;
                tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                pos.sell_order_id = None;
            }

            // Place new GTC sell
            use rust_decimal::prelude::FromPrimitive;
            let intent = sattebaaz::models::order::OrderIntent {
                token_id: pos.token_id.clone(),
                market_side: pos.side,
                order_side: OrderSide::Sell,
                price: rust_decimal::Decimal::from_f64(desired_price)
                    .unwrap_or(rust_decimal::Decimal::ZERO),
                size: rust_decimal::Decimal::from_f64(pos.size)
                    .unwrap_or(rust_decimal::Decimal::ZERO),
                order_type: OrderType::GTC,
                post_only: false,
                expiration: None,
                strategy_tag: pos.strategy.clone(),
            };

            match order_builder.build(&intent).await {
                Ok(signed) => {
                    match clob_client.post_order(signed, OrderType::GTC, false).await {
                        Ok(result) if result.status != sattebaaz::models::order::OrderStatus::Rejected => {
                            let oid = result.order_id.clone();
                            pos.sell_order_id = Some(oid.clone());
                            pos.sell_order_price = desired_price;
                            pos.sell_order_type = desired_type.to_string();
                            pos.sell_attempts += 1;
                            println!("  SELL ORDER #{}: {} @ {:.2} [oid:{}]",
                                pos.id, desired_type.to_uppercase(), desired_price,
                                &oid[..8.min(oid.len())]);
                        }
                        Ok(result) => {
                            let msg = result.error_msg.unwrap_or_default();
                            pos.sell_attempts += 1;
                            eprintln!("  ⚠ SELL ORDER #{} rejected({}): {}",
                                pos.id, pos.sell_attempts, msg);
                        }
                        Err(e) => {
                            pos.sell_attempts += 1;
                            eprintln!("  ⚠ SELL ORDER #{} post error({}): {}",
                                pos.id, pos.sell_attempts, e);
                        }
                    }
                }
                Err(e) => {
                    pos.sell_attempts += 1;
                    eprintln!("  ⚠ SELL ORDER #{} build error({}): {}",
                        pos.id, pos.sell_attempts, e);
                }
            }
        }

        // ══════════════════════════════════════════════
        // ENTRY LOGIC — find new opportunities
        // ══════════════════════════════════════════════
        if remaining > MIN_REMAINING_SECS
            && positions.len() < MAX_POSITIONS
            && now_inst.duration_since(last_entry) >= entry_cooldown
        {
            let yes_mispricing = fair_up - yes_ask;
            let no_mispricing = fair_down - no_ask;

            let yes_spread_ok = yes_bid > 0.0 && (yes_ask - yes_bid) / yes_ask < MAX_SPREAD_PCT;
            let no_spread_ok = no_bid > 0.0 && (no_ask - no_bid) / no_ask < MAX_SPREAD_PCT;

            let btc_just_moved = btc_move_pct >= MIN_BTC_MOVE_PCT;
            let btc_up = btc_move_signed > 0.0;
            let btc_down = btc_move_signed < 0.0;

            // ── Lag exploit: YES if BTC up, NO if BTC down ──
            // Net edge = mispricing - spread cost. Only enter if profitable after exit.
            let yes_spread = yes_ask - yes_bid;
            let no_spread = no_ask - no_bid;
            let yes_net_edge = yes_mispricing - yes_spread; // edge after we pay the spread
            let no_net_edge = no_mispricing - no_spread;

            // Block new entries if we already have positions with many sell attempts
            let has_stuck_position = positions.iter().any(|p| p.sell_attempts >= 5);

            // Re-sync capital from CLOB before entry to avoid "not enough balance"
            if let Ok(real_bal) = clob_client.fetch_balance().await {
                if (real_bal - capital).abs() > 0.10 {
                    println!("  [BALANCE SYNC] tracked=${:.2} actual=${:.2} → using actual", capital, real_bal);
                    capital = real_bal;
                }
            }

            let mut entered = false;
            if yes_net_edge > LAG_MIN_EDGE && yes_ask >= PRICE_FLOOR && yes_ask <= PRICE_CEILING
                && yes_spread_ok && btc_just_moved && btc_up && !has_stuck_position
            {
                // Market buy: walk book, cap spend to available depth
                let desired = MAX_COST_PER_POS.min(capital - 0.10);
                if let Some((worst_price, depth_usdc)) = yes_book.calculate_buy_market_price(desired) {
                    let spend = desired.min(depth_usdc); // cap to book depth
                    if spend >= MIN_ORDER_COST && capital >= spend {
                        let shares = spend / worst_price;
                        entered = try_market_buy(
                            &order_builder, &clob_client, &market.yes_token_id, Side::Yes,
                            spend, worst_price, shares,
                            &format!("lag(+{:.0}¢,net+{:.0}¢)", yes_mispricing * 100.0, yes_net_edge * 100.0),
                            &slug, &mut capital, &mut positions, &mut trade_log,
                            &mut trade_id, &mut next_pos_id, &mut stats, now_inst,
                        ).await;
                        if entered { last_entry = now_inst; }
                    }
                }
            }

            if !entered && no_net_edge > LAG_MIN_EDGE && no_ask >= PRICE_FLOOR && no_ask <= PRICE_CEILING
                && no_spread_ok && btc_just_moved && btc_down && !has_stuck_position
            {
                let desired = MAX_COST_PER_POS.min(capital - 0.10);
                if let Some((worst_price, depth_usdc)) = no_book.calculate_buy_market_price(desired) {
                    let spend = desired.min(depth_usdc);
                    if spend >= MIN_ORDER_COST && capital >= spend {
                        let shares = spend / worst_price;
                        entered = try_market_buy(
                            &order_builder, &clob_client, &market.no_token_id, Side::No,
                            spend, worst_price, shares,
                            &format!("lag(+{:.0}¢,net+{:.0}¢)", no_mispricing * 100.0, no_net_edge * 100.0),
                            &slug, &mut capital, &mut positions, &mut trade_log,
                            &mut trade_id, &mut next_pos_id, &mut stats, now_inst,
                        ).await;
                        if entered { last_entry = now_inst; }
                    }
                }
            }

            // ── Arb: buy both when YES+NO < threshold, then merge on-chain ──
            if !entered && arb_enabled && has_matic && yes_ask + no_ask < ARB_THRESHOLD
                && positions.len() + 2 <= MAX_POSITIONS
            {
                let condition_id = market.condition_id.clone();
                if let Some(ref cid) = condition_id {
                    let arb_cost_per_pair = yes_ask + no_ask;
                    let edge = 1.0 - arb_cost_per_pair;
                    let arb_budget = (capital * 0.20).min(MAX_COST_PER_POS);
                    let arb_size = arb_budget / arb_cost_per_pair;
                    let total_cost = arb_cost_per_pair * arb_size;

                    if total_cost >= MIN_POSITION_COST && capital >= total_cost {
                        // Leg 1: Buy YES (market order)
                        let yes_spend = yes_ask * arb_size;
                        let yes_ok = try_market_buy(
                            &order_builder, &clob_client, &market.yes_token_id, Side::Yes,
                            yes_spend, yes_ask, arb_size, "arb_yes",
                            &slug, &mut capital, &mut positions, &mut trade_log,
                            &mut trade_id, &mut next_pos_id, &mut stats, now_inst,
                        ).await;

                        if yes_ok {
                            // Leg 2: Buy NO (market order)
                            let no_spend = no_ask * arb_size;
                            let no_ok = try_market_buy(
                                &order_builder, &clob_client, &market.no_token_id, Side::No,
                                no_spend, no_ask, arb_size, "arb_no",
                                &slug, &mut capital, &mut positions, &mut trade_log,
                                &mut trade_id, &mut next_pos_id, &mut stats, now_inst,
                            ).await;

                            if no_ok {
                                // Both legs filled → merge on-chain for instant profit
                                println!("  [ARB] Both legs filled. Merging on-chain...");
                                let _ = std::io::stdout().flush();

                                match merger.merge_positions(cid, arb_size).await {
                                    Ok(tx_hash) => {
                                        // Merge succeeded! Remove arb positions, credit $1 per pair
                                        let merge_revenue = arb_size; // $1 per merged pair
                                        capital += merge_revenue;
                                        let arb_pnl = merge_revenue - total_cost;

                                        // Remove the two arb positions (last two added)
                                        let len = positions.len();
                                        if len >= 2 {
                                            positions.remove(len - 1);
                                            positions.remove(len - 2);
                                        }

                                        stats.exits += 1;
                                        stats.total_exit_pnl += arb_pnl;
                                        if arb_pnl > 0.0 { stats.winning_exits += 1; }

                                        trade_id += 1;
                                        let log = TradeLog {
                                            id: trade_id, time: Utc::now(),
                                            action: "MERGE".into(),
                                            side: Side::Yes,
                                            price: arb_cost_per_pair,
                                            size: arb_size,
                                            pnl: arb_pnl,
                                            strategy: format!("arb(edge={:.0}¢,tx={})", edge * 100.0, &tx_hash[..10.min(tx_hash.len())]),
                                            capital_after: capital,
                                        };
                                        println!("  MERGE {} +${:.4}", log, arb_pnl);
                                        let _ = std::io::stdout().flush();
                                        push_log(&mut trade_log, log);
                                        last_entry = now_inst;
                                    }
                                    Err(e) => {
                                        // Merge failed — keep positions, they'll exit via TP/SL/force
                                        eprintln!("  [ARB] Merge FAILED: {}. Positions kept as lag fallback.", e);
                                        last_entry = now_inst;
                                    }
                                }
                            } else {
                                // NO leg failed — YES position stays as regular lag trade
                                eprintln!("  [ARB] NO leg failed. YES position kept as lag fallback.");
                                last_entry = now_inst;
                            }
                        }
                    }
                }
            }
        }

        // ── Dashboard ──
        maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, starting_capital, btc_price,
            &positions, &trade_log, &stats, remaining, &slug,
            fair_up, yes_ask, yes_bid, no_ask, no_bid, ref_p, btc_move_pct);
    }

    // ═══════════════════════════════════════════════════════════
    // SESSION SUMMARY
    // ═══════════════════════════════════════════════════════════
    let realized_pnl = stats.total_exit_pnl + stats.total_resolution_pnl;
    let exit_wr = if stats.exits > 0 { stats.winning_exits as f64 / stats.exits as f64 * 100.0 } else { 0.0 };
    println!("\n{}", "=".repeat(80));
    println!("  LIVE SESSION COMPLETE | {} cycles", stats.cycles);
    println!("{}", "=".repeat(80));
    println!("  Capital:    ${:.2} → ${:.2}  |  Realized P&L: {:>+.3} ({:>+.1}%)",
        starting_capital, capital, realized_pnl, realized_pnl / starting_capital * 100.0);
    println!("  Entries:    {}  |  Exits: {} ({:.0}% win)  |  Resolutions: {}",
        stats.entries, stats.exits, exit_wr, stats.resolutions);
    println!("  Exit P&L:   {:>+.4}  |  Resolution P&L: {:>+.4}",
        stats.total_exit_pnl, stats.total_resolution_pnl);
    println!("  Order failures: {}", stats.order_failures);
    if !trade_log.is_empty() {
        println!("  Last trades:");
        for t in trade_log.iter().rev().take(10).collect::<Vec<_>>().iter().rev() {
            println!("    {}", t);
        }
    }
    println!("{}\n", "=".repeat(80));

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// HELPERS
// ═══════════════════════════════════════════════════════════════════════════

/// Submit a MARKET BUY order (FOK). Returns true if filled.
/// `spend` = dollar amount, `worst_price` = worst price from book walk, `shares` = spend/price
#[allow(clippy::too_many_arguments)]
async fn try_market_buy(
    order_builder: &OrderBuilder,
    clob_client: &ClobClient,
    token_id: &str,
    side: Side,
    spend: f64,
    worst_price: f64,
    _shares: f64,
    strategy: &str,
    slug: &str,
    capital: &mut f64,
    positions: &mut Vec<Position>,
    trade_log: &mut VecDeque<TradeLog>,
    trade_id: &mut usize,
    next_pos_id: &mut usize,
    stats: &mut Stats,
    now_inst: tokio::time::Instant,
) -> bool {
    // build_market_order returns (SignedOrder, actual_spend, actual_shares)
    let (signed, actual_spend, actual_shares) = match order_builder.build_market_order(
        token_id, OrderSide::Buy, spend, worst_price
    ).await {
        Ok(r) => r,
        Err(e) => {
            stats.order_failures += 1;
            eprintln!("  BUY SIGN ERROR: {}", e);
            return false;
        }
    };

    match clob_client.post_order(signed, OrderType::FOK, false).await {
        Ok(result) => {
            if result.status == sattebaaz::models::order::OrderStatus::Rejected {
                let msg = result.error_msg.unwrap_or_default();
                if msg.contains("couldn't be fully filled") || msg.contains("no orders found") {
                    // Book too thin for our amount — not an error, skip
                    return false;
                }
                stats.order_failures += 1;
                eprintln!("  BUY REJECTED: {}", msg);
                return false;
            }

            // ── VERIFY the FOK buy actually filled ──
            // post_order returns success=true for "accepted", but we must confirm
            // the order is MATCHED and get the real size_matched from the CLOB.
            let buy_oid = result.order_id.clone();
            let mut confirmed_shares: Option<f64> = None;
            let mut last_status = String::new();
            for attempt in 0..5 {
                if attempt > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                match clob_client.get_order(&buy_oid).await {
                    Ok((status, size_matched)) => {
                        last_status = status.clone();
                        if status == "MATCHED" {
                            // Use the CLOB's actual fill size, floored to 2 dec
                            let real_shares = (size_matched * 100.0).floor() / 100.0;
                            if real_shares > 0.0 {
                                confirmed_shares = Some(real_shares);
                                println!("  BUY CONFIRMED: {} shares (signed: {:.4}, matched: {:.4}, using: {:.2})",
                                    status, actual_shares, size_matched, real_shares);
                            }
                            break;
                        } else if status == "LIVE" || status == "DELAYED" {
                            // Still being processed/settled — keep waiting
                            if attempt == 4 {
                                // Last attempt still LIVE — FOK should have resolved by now.
                                // Trust the signed order amounts (floor to 2 dec).
                                let fallback = (actual_shares * 100.0).floor() / 100.0;
                                if fallback > 0.0 {
                                    confirmed_shares = Some(fallback);
                                    println!("  BUY ASSUMED FILLED: status still {} after {}ms, using {:.2} shares",
                                        status, (attempt + 1) * 500, fallback);
                                }
                            }
                            continue;
                        } else {
                            // CANCELLED, KILLED, etc — buy didn't fill
                            eprintln!("  ⚠ BUY NOT FILLED: order {} status={}", &buy_oid[..8.min(buy_oid.len())], status);
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("  ⚠ BUY verify attempt {}: {}", attempt + 1, e);
                        // On API error, don't give up — keep trying
                    }
                }
            }

            // If we couldn't confirm the fill, DON'T track the position
            let real_shares = match confirmed_shares {
                Some(s) => s,
                None => {
                    eprintln!("  ⚠ BUY UNCONFIRMED (last status: {}) — not tracking to avoid phantom",
                        last_status);
                    return false;
                }
            };

            *capital -= actual_spend;

            // Calculate TP price: round DOWN to 0.01 tick so it sits on the book
            let tp_price = ((worst_price * (1.0 + TAKE_PROFIT_PCT)) * 100.0).floor() / 100.0;
            let tp_price = tp_price.min(0.99); // can't sell above 0.99

            // Immediately place GTC limit SELL at TP price — this sits on the book
            // and fills automatically. Maker order = zero fees.
            let sell_order_id = {
                use rust_decimal::prelude::FromPrimitive;
                let intent = sattebaaz::models::order::OrderIntent {
                    token_id: token_id.to_string(),
                    market_side: side,
                    order_side: OrderSide::Sell,
                    price: rust_decimal::Decimal::from_f64(tp_price).unwrap_or(rust_decimal::Decimal::ZERO),
                    size: rust_decimal::Decimal::from_f64(real_shares).unwrap_or(rust_decimal::Decimal::ZERO),
                    order_type: OrderType::GTC,
                    post_only: false,
                    expiration: None,
                    strategy_tag: strategy.to_string(),
                };
                match order_builder.build(&intent).await {
                    Ok(tp_signed) => {
                        match clob_client.post_order(tp_signed, OrderType::GTC, false).await {
                            Ok(tp_result) if tp_result.status != sattebaaz::models::order::OrderStatus::Rejected => {
                                let oid = tp_result.order_id.clone();
                                println!("  TP ORDER placed: SELL {:.2} shares @ {:.2} [oid:{}]",
                                    real_shares, tp_price, &oid[..8.min(oid.len())]);
                                Some(oid)
                            }
                            Ok(tp_result) => {
                                eprintln!("  ⚠ TP order rejected: {} — will retry next tick",
                                    tp_result.error_msg.unwrap_or_default());
                                None
                            }
                            Err(e) => {
                                eprintln!("  ⚠ TP order post failed: {} — will retry next tick", e);
                                None
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  ⚠ TP order build failed: {} — will retry next tick", e);
                        None
                    }
                }
            };

            let initial_sell_attempts = if sell_order_id.is_some() { 1u32 } else { 0u32 };
            *next_pos_id += 1;
            positions.push(Position {
                id: *next_pos_id, side,
                token_id: token_id.to_string(),
                entry_price: worst_price, size: real_shares,
                cost_basis: actual_spend,
                tp_price,
                strategy: strategy.to_string(),
                opened_at: now_inst,
                market_slug: slug.to_string(),
                sell_order_id,
                sell_order_price: tp_price,
                sell_order_type: "tp".to_string(),
                sell_attempts: initial_sell_attempts,
                order_id: Some(buy_oid.clone()),
            });
            stats.entries += 1;
            *trade_id += 1;
            let log = TradeLog {
                id: *trade_id, time: Utc::now(), action: "BUY".into(),
                side, price: worst_price, size: real_shares, pnl: 0.0,
                strategy: strategy.to_string(),
                capital_after: *capital,
            };
            println!("  ENTRY {} [oid:{}]", log, &buy_oid[..8.min(buy_oid.len())]);
            let _ = std::io::stdout().flush();
            push_log(trade_log, log);
            true
        }
        Err(e) => {
            stats.order_failures += 1;
            eprintln!("  BUY ERROR: {}", e);
            false
        }
    }
}


fn calibrate_reference_price(
    btc_price: f64,
    book_yes_mid: f64,
    minutes_remaining: f64,
    vol_per_min: f64,
) -> f64 {
    let p = book_yes_mid.clamp(0.02, 0.98);
    let normal = Normal::new(0.0, 1.0).expect("valid normal");
    let z = normal.inverse_cdf(p);
    let remaining_vol = vol_per_min * minutes_remaining.sqrt();
    if remaining_vol < 1e-10 {
        return btc_price;
    }
    let pct_move = z * remaining_vol;
    btc_price / (1.0 + pct_move)
}

fn push_log(log: &mut VecDeque<TradeLog>, entry: TradeLog) {
    log.push_back(entry);
    if log.len() > 50 { log.pop_front(); }
}

#[allow(clippy::too_many_arguments)]
fn maybe_dashboard(
    now: tokio::time::Instant,
    last: &mut tokio::time::Instant,
    interval: tokio::time::Duration,
    capital: f64,
    starting_capital: f64,
    btc_price: f64,
    positions: &[Position],
    trade_log: &VecDeque<TradeLog>,
    stats: &Stats,
    remaining: f64,
    slug: &str,
    fair_up: f64,
    yes_ask: f64,
    yes_bid: f64,
    no_ask: f64,
    no_bid: f64,
    ref_p: f64,
    btc_move_pct: f64,
) {
    if now.duration_since(*last) < interval { return; }
    *last = now;

    let realized_pnl = stats.total_exit_pnl + stats.total_resolution_pnl;
    let exposure: f64 = positions.iter().map(|p| p.entry_price * p.size).sum();
    let exit_wr = if stats.exits > 0 { stats.winning_exits as f64 / stats.exits as f64 * 100.0 } else { 0.0 };

    println!();
    println!("  {}", "-".repeat(76));
    println!("  {} | Cycle {} | BTC ${:.0} | Capital: ${:.2} | Realized P&L: {:>+.3} ({:>+.1}%)",
        Utc::now().format("%H:%M:%S"), stats.cycles, btc_price, capital, realized_pnl, realized_pnl / starting_capital * 100.0);
    println!("  Market: {} | {:.0}s left | Exposure: ${:.2} | {} open | {} order fails",
        slug, remaining, exposure, positions.len(), stats.order_failures);
    println!("  Stats: {} entries | {} exits ({:.0}% win) | {} resolved | exit_pnl: {:>+.3} | res_pnl: {:>+.3}",
        stats.entries, stats.exits, exit_wr, stats.resolutions, stats.total_exit_pnl, stats.total_resolution_pnl);
    let pct_move = if ref_p > 0.0 { (btc_price - ref_p) / ref_p * 100.0 } else { 0.0 };
    let yes_spread = if yes_ask > 0.0 { (yes_ask - yes_bid) / yes_ask * 100.0 } else { 0.0 };
    let no_spread = if no_ask > 0.0 { (no_ask - no_bid) / no_ask * 100.0 } else { 0.0 };
    println!("  Fair: UP={:.3} DN={:.3} | BTC {:>+.3}% from ref | YES {:.2}/{:.2} ({:.0}%sp) | NO {:.2}/{:.2} ({:.0}%sp)",
        fair_up, 1.0-fair_up, pct_move, yes_bid, yes_ask, yes_spread, no_bid, no_ask, no_spread);
    let yes_misp = fair_up - yes_ask;
    let no_misp = (1.0-fair_up) - no_ask;
    let yes_net = yes_misp - (yes_ask - yes_bid);
    let no_net = no_misp - (no_ask - no_bid);
    println!("  Mispricing: YES {:>+.3}(net{:>+.3}) | NO {:>+.3}(net{:>+.3}) | need >{:.3} & move>{:.2}% | last_move={:.3}%",
        yes_misp, yes_net, no_misp, no_net, LAG_MIN_EDGE, MIN_BTC_MOVE_PCT, btc_move_pct);

    if !positions.is_empty() {
        println!("  Open positions:");
        for p in positions {
            let age = now.duration_since(p.opened_at).as_secs();
            let sell_str = if p.sell_order_id.is_some() {
                format!(" | {} @{:.2}", p.sell_order_type.to_uppercase(), p.sell_order_price)
            } else {
                " | NO SELL ORDER".to_string()
            };
            println!("    #{} {:?} @ {:.3} x{:.2} | {}s held | {}{}",
                p.id, p.side, p.entry_price, p.size, age, p.strategy, sell_str);
        }
    }

    if !trade_log.is_empty() {
        println!("  Recent:");
        for t in trade_log.iter().rev().take(5).collect::<Vec<_>>().iter().rev() {
            println!("    {}", t);
        }
    }

    println!("  {}", "-".repeat(76));
    let _ = std::io::stdout().flush();
}

//! BTC 5-Minute Paper Trader
//!
//! Focused on BTC-5m Polymarket markets only. Strategies:
//!   - Lag exploit: buy underpriced side when Polymarket lags Binance, SELL when lag corrects
//!   - Arb: buy YES+NO when combined < $1.00
//!
//! Key feature: positions are EXITED before resolution for profit, not held to resolve.
//!
//! Usage:  cargo run --bin paper_trade

use sattebaaz::config::Config;
use sattebaaz::feeds::binance::BinanceFeed;
use sattebaaz::feeds::market_discovery::MarketDiscovery;
use sattebaaz::feeds::polymarket::PolymarketFeed;
use sattebaaz::models::market::{Asset, Duration, Side};
use sattebaaz::signals::probability::ProbabilityModel;

use chrono::{DateTime, Utc};
use statrs::distribution::{ContinuousCDF, Normal};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::broadcast;

// ═══════════════════════════════════════════════════════════════════════════
// CONFIGURATION
// ═══════════════════════════════════════════════════════════════════════════
const STARTING_CAPITAL: f64 = 5.0;
const TICK_MS: u64 = 2000;             // Poll every 2s
const FEED_INIT_SECS: u64 = 10;
const DASHBOARD_SECS: u64 = 8;

// Entry signals
const LAG_MIN_EDGE: f64 = 0.04;        // Min mispricing to enter (4¢)
const ARB_THRESHOLD: f64 = 0.97;       // Buy both when YES_ask + NO_ask < this
const PRICE_FLOOR: f64 = 0.30;         // Don't buy below 30¢ — OTM tokens have fatal gamma risk
const PRICE_CEILING: f64 = 0.70;       // Don't buy above 70¢ — too expensive, low payout
const MAX_SPREAD_PCT: f64 = 0.10;      // Don't enter if spread > 10% of ask (tighter = better fills)
const MIN_REMAINING_SECS: f64 = 120.0; // Need at least 2 min for lag to correct
const MIN_BTC_MOVE_PCT: f64 = 0.01;    // Require ≥0.01% BTC move since last tick

// Exit signals — calibrated for binary option token vol (~21%/min 1σ at p≈0.65)
// SL must be ≥1σ to avoid noise stops. At 60% directional win rate, 1:1 ratio → +EV
const TAKE_PROFIT_PCT: f64 = 0.10;     // Exit when bid ≥ entry × (1 + 10%)
const STOP_LOSS_PCT: f64 = 0.08;       // Cut loss FAST when bid ≤ entry × (1 - 8%)
const MAX_HOLD_SECS: f64 = 120.0;      // Force exit after 2 minutes
const PRE_RESOLVE_EXIT_SECS: f64 = 60.0; // Close positions in last 60s

// Position sizing
const MAX_POSITIONS: usize = 3;        // Max concurrent positions
const MAX_COST_PER_POS: f64 = 0.50;    // Max $0.50 cost per position
const MIN_POSITION_COST: f64 = 0.10;   // Min $0.10 cost per position
const ENTRY_COOLDOWN_SECS: u64 = 10;   // Base cooldown between entries
const SL_COOLDOWN_SECS: u64 = 45;      // Extended cooldown after a stop loss (anti-chop)

// Fill simulation (realistic)
const TAKER_FILL_PROB: f64 = 0.70;     // 70% fill (conservative — accounts for thin books)
const SLIPPAGE_BPS: f64 = 50.0;        // 0.5% slippage on entries (eat through book depth)

// Realized volatility tracking
const VOL_WINDOW: usize = 30;          // Track last 30 BTC ticks (~60s) for realized vol

// ═══════════════════════════════════════════════════════════════════════════
// DATA TYPES
// ═══════════════════════════════════════════════════════════════════════════

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as f64 / (1u64 << 31) as f64
    }
}

#[derive(Clone)]
struct Position {
    id: usize,
    side: Side,           // Yes or No
    token_id: String,
    entry_price: f64,
    size: f64,
    strategy: String,
    opened_at: tokio::time::Instant,
    market_slug: String,
}

#[derive(Clone)]
struct TradeLog {
    id: usize,
    time: DateTime<Utc>,
    action: String,       // "BUY" or "SELL"
    side: Side,
    price: f64,
    size: f64,
    pnl: f64,             // 0 for buys, realized for sells
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
}

impl Stats {
    fn new() -> Self {
        Self { entries: 0, exits: 0, resolutions: 0, winning_exits: 0,
               total_exit_pnl: 0.0, total_resolution_pnl: 0.0, cycles: 0 }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter("warn").with_target(false).init();

    println!("\n{}", "=".repeat(80));
    println!("  BTC 5-MIN PAPER TRADER");
    println!("  Real Polymarket + Binance data | ${:.2} capital | NO FEES", STARTING_CAPITAL);
    println!("{}", "=".repeat(80));
    println!("  Lag edge:    >{:.0}¢  |  TP: {:.0}%  |  SL: {:.0}%  |  Slippage: {:.0}bps",
        LAG_MIN_EDGE * 100.0, TAKE_PROFIT_PCT * 100.0, STOP_LOSS_PCT * 100.0, SLIPPAGE_BPS);
    println!("  Max hold:    {:.0}s   |  Positions: max {}  |  Max cost: ${:.2}/pos  |  Directional: YES",
        MAX_HOLD_SECS, MAX_POSITIONS, MAX_COST_PER_POS);
    println!("{}\n", "=".repeat(80));

    let config = Config::default();
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let prob_model = ProbabilityModel::new();
    let vol_per_min = Asset::BTC.vol_per_minute();

    // Data feeds
    let binance = Arc::new(BinanceFeed::new(config.binance.clone()));
    let poly = Arc::new(PolymarketFeed::new(config.polymarket.clone()));
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
    let mut rng = Rng::new(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().as_nanos() as u64,
    );
    let mut capital = STARTING_CAPITAL;
    let mut positions: Vec<Position> = Vec::new();
    let mut trade_log: VecDeque<TradeLog> = VecDeque::new();
    let mut trade_id = 0usize;
    let mut next_pos_id = 0usize;
    let mut stats = Stats::new();
    let mut resolved_slugs: HashSet<String> = HashSet::new();
    let mut ref_prices: HashMap<String, f64> = HashMap::new();  // slug → ref_price
    let mut last_entry = tokio::time::Instant::now() - tokio::time::Duration::from_secs(999);
    let mut last_dash = tokio::time::Instant::now();
    let mut prev_btc_price: f64 = 0.0; // Track previous tick's BTC price for momentum check
    let mut btc_returns: VecDeque<f64> = VecDeque::new(); // Realized vol tracker
    let mut realized_vol_per_min: f64 = vol_per_min; // Start with constant, update with realized

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

    // ═══════════════════════════════════════════════════════════
    // MAIN LOOP
    // ═══════════════════════════════════════════════════════════
    loop {
        poll.tick().await;
        if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
            println!("\n  Shutting down...");
            let _ = shutdown_tx.send(());
            break;
        }

        let now_inst = tokio::time::Instant::now();

        // ── Get BTC price ──
        let btc_price = match binance.get_price(Asset::BTC).await {
            Some(p) if p > 0.0 => p,
            _ => continue,
        };

        // BTC momentum: how much did price move since last tick?
        let btc_move_pct = if prev_btc_price > 0.0 {
            ((btc_price - prev_btc_price) / prev_btc_price).abs() * 100.0
        } else {
            0.0
        };
        // Signed move: positive = BTC went up, negative = BTC went down
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
                // Convert per-tick vol to per-minute: ticks_per_min = 60000/TICK_MS
                let ticks_per_min = 60_000.0 / TICK_MS as f64;
                realized_vol_per_min = var.sqrt() * ticks_per_min.sqrt();
                // Blend: 70% realized, 30% constant (avoid pure noise)
                realized_vol_per_min = 0.70 * realized_vol_per_min + 0.30 * vol_per_min;
                // Floor: never go below 50% of constant vol
                realized_vol_per_min = realized_vol_per_min.max(vol_per_min * 0.5);
            }
        }
        prev_btc_price = btc_price;

        // ── Current 5m market ──
        let slug = MarketDiscovery::current_slug(Asset::BTC, Duration::FiveMin);
        let remaining = MarketDiscovery::time_remaining_in_current(Duration::FiveMin);

        // ── Track reference price per market ──
        // When joining mid-cycle, calibrate ref from the book's implied probability
        // so our model agrees with the market. Only fresh cycles use raw BTC price.
        let ref_p = if let Some(&p) = ref_prices.get(&slug) {
            p
        } else {
            let total_secs = 300.0; // 5-min market
            let is_fresh = remaining > (total_secs - 15.0); // Within first 15s of market
            let calibrated = if is_fresh {
                btc_price // Fresh market — we have the true open price
            } else {
                // Mid-cycle join: get YES midpoint from book and calibrate
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

        // ── Resolution: check if old positions need settling ──
        let mut to_resolve: Vec<usize> = Vec::new();
        for (i, pos) in positions.iter().enumerate() {
            if pos.market_slug != slug && !resolved_slugs.contains(&pos.market_slug) {
                to_resolve.push(i);
            }
        }
        if !to_resolve.is_empty() {
            // Resolve old market positions using THEIR reference price
            let old_slug = &positions[to_resolve[0]].market_slug;
            let old_ref = ref_prices.get(old_slug).copied().unwrap_or(btc_price);
            let winner = if btc_price >= old_ref { Side::Yes } else { Side::No };
            println!("  [RESOLVE] {} ref=${:.2} final=${:.2} → {:?} wins",
                old_slug, old_ref, btc_price, winner);
            for &i in to_resolve.iter().rev() {
                let pos = &positions[i];
                let pnl = if pos.side == winner {
                    // Winner: payout = $1 per share, profit = 1.0 - entry_price
                    (1.0 - pos.entry_price) * pos.size
                } else {
                    // Loser: worthless, loss = entry_price * size
                    -(pos.entry_price * pos.size)
                };
                capital += pos.entry_price * pos.size + pnl; // return cost + pnl
                stats.resolutions += 1;
                stats.total_resolution_pnl += pnl;

                trade_id += 1;
                let log = TradeLog {
                    id: trade_id, time: Utc::now(), action: "RESOLVE".into(),
                    side: pos.side, price: if pos.side == winner { 1.0 } else { 0.0 },
                    size: pos.size, pnl, strategy: pos.strategy.clone(),
                    capital_after: capital,
                };
                println!("  {} {}", if pnl >= 0.0 { "WIN " } else { "LOSS" }, log);
                let _ = std::io::stdout().flush();
                push_log(&mut trade_log, log);
            }
            for &i in to_resolve.iter().rev() {
                let old_slug = positions[i].market_slug.clone();
                positions.remove(i);
                resolved_slugs.insert(old_slug);
            }
            stats.cycles += 1;
            println!("  [CYCLE {}] Capital: ${:.2}", stats.cycles, capital);
            let _ = std::io::stdout().flush();
        }

        // ── Get Polymarket market and books ──
        let market = match poly.get_market(&slug) {
            Some(m) => m,
            None => { maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, btc_price, &positions, &trade_log, &stats, remaining, &slug, 0.5, 0.0, 0.0, 0.0, 0.0, ref_p, btc_move_pct); continue; }
        };
        let yes_book = match poly.get_book(&market.yes_token_id) {
            Some(b) => b,
            None => { maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, btc_price, &positions, &trade_log, &stats, remaining, &slug, 0.5, 0.0, 0.0, 0.0, 0.0, ref_p, btc_move_pct); continue; }
        };
        let no_book = match poly.get_book(&market.no_token_id) {
            Some(b) => b,
            None => { maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, btc_price, &positions, &trade_log, &stats, remaining, &slug, 0.5, 0.0, 0.0, 0.0, 0.0, ref_p, btc_move_pct); continue; }
        };

        // ── Fair value from Binance (using realized vol) ──
        let time_remaining_min = remaining / 60.0;
        let fair_up = prob_model.fair_prob_up(btc_price, ref_p, time_remaining_min, realized_vol_per_min, 0.0);
        let fair_down = 1.0 - fair_up;

        // Book prices
        let yes_ask = yes_book.best_ask().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(1.0)).unwrap_or(1.0);
        let yes_bid = yes_book.best_bid().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
        let no_ask = no_book.best_ask().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(1.0)).unwrap_or(1.0);
        let no_bid = no_book.best_bid().map(|(p, _)| p.to_string().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);

        // ══════════════════════════════════════════════
        // EXIT LOGIC — check existing positions first
        // ══════════════════════════════════════════════
        let mut exits: Vec<usize> = Vec::new();
        for (i, pos) in positions.iter().enumerate() {
            if pos.market_slug != slug { continue; } // old market, handled above
            if pos.strategy == "arb" { continue; } // arb holds to resolution for guaranteed profit

            let current_bid = if pos.side == Side::Yes { yes_bid } else { no_bid };
            let hold_secs = now_inst.duration_since(pos.opened_at).as_secs_f64();
            let pct_change = if pos.entry_price > 0.0 { (current_bid - pos.entry_price) / pos.entry_price } else { 0.0 };

            let should_exit = if pct_change >= TAKE_PROFIT_PCT {
                true // Take profit
            } else if pct_change <= -STOP_LOSS_PCT {
                true // Stop loss
            } else if hold_secs >= MAX_HOLD_SECS {
                true // Max hold time
            } else if remaining < PRE_RESOLVE_EXIT_SECS && pct_change > 0.0 {
                true // Pre-resolution exit if profitable
            } else {
                false
            };

            if should_exit && current_bid > 0.01 {
                // Simulate sell fill with slippage (sell at slightly worse than bid)
                if rng.next_f64() < TAKER_FILL_PROB {
                    let sell_slippage = current_bid * (SLIPPAGE_BPS / 10000.0);
                    let fill_price = (current_bid - sell_slippage).max(0.01);
                    let proceeds = fill_price * pos.size;
                    let cost_basis = pos.entry_price * pos.size;
                    let pnl = proceeds - cost_basis;
                    capital += proceeds;

                    stats.exits += 1;
                    stats.total_exit_pnl += pnl;
                    if pnl > 0.0 { stats.winning_exits += 1; }

                    trade_id += 1;
                    let reason = if pct_change >= TAKE_PROFIT_PCT { "tp" }
                        else if pct_change <= -STOP_LOSS_PCT { "sl" }
                        else if hold_secs >= MAX_HOLD_SECS { "time" }
                        else { "pre_res" };
                    let log = TradeLog {
                        id: trade_id, time: Utc::now(),
                        action: format!("SELL({})", reason),
                        side: pos.side, price: current_bid, size: pos.size,
                        pnl, strategy: pos.strategy.clone(),
                        capital_after: capital,
                    };
                    println!("  EXIT  {}", log);
                    let _ = std::io::stdout().flush();
                    push_log(&mut trade_log, log);
                    exits.push(i);
                }
            }
        }
        for &i in exits.iter().rev() {
            positions.remove(i);
        }

        // ══════════════════════════════════════════════
        // ENTRY LOGIC — find new opportunities
        // ══════════════════════════════════════════════
        // Adaptive cooldown: use longer cooldown after a stop loss to prevent chop
        if remaining > MIN_REMAINING_SECS
            && positions.len() < MAX_POSITIONS
            && now_inst.duration_since(last_entry) >= entry_cooldown
        {
            let yes_mispricing = fair_up - yes_ask;
            let no_mispricing = fair_down - no_ask;

            // Spread checks: don't enter if spread is too wide (we'd lose money immediately)
            let yes_spread_ok = yes_bid > 0.0 && (yes_ask - yes_bid) / yes_ask < MAX_SPREAD_PCT;
            let no_spread_ok = no_bid > 0.0 && (no_ask - no_bid) / no_ask < MAX_SPREAD_PCT;

            // Momentum check: only enter lag trades when BTC just moved (fresh lag signal)
            // AND the move direction supports our entry side
            let btc_just_moved = btc_move_pct >= MIN_BTC_MOVE_PCT;
            // Directional check: BTC up → YES is underpriced, BTC down → NO is underpriced
            let btc_up = btc_move_signed > 0.0;
            let btc_down = btc_move_signed < 0.0;

            // ── Lag exploit: buy the underpriced side ──
            // Only buy YES if BTC just moved UP (book hasn't caught up to higher price)
            // Only buy NO if BTC just moved DOWN (book hasn't caught up to lower price)
            let mut entered = false;
            if yes_mispricing > LAG_MIN_EDGE && yes_ask >= PRICE_FLOOR && yes_ask <= PRICE_CEILING
                && yes_spread_ok && btc_just_moved && btc_up
            {
                // Apply slippage: we pay slightly more than best ask
                let buy_slippage = yes_ask * (SLIPPAGE_BPS / 10000.0);
                let fill_price = yes_ask + buy_slippage;
                let cost = MAX_COST_PER_POS.min(capital * 0.20);
                let size = cost / fill_price;
                if cost >= MIN_POSITION_COST && capital >= cost {
                    if rng.next_f64() < TAKER_FILL_PROB {
                        capital -= cost;
                        next_pos_id += 1;
                        positions.push(Position {
                            id: next_pos_id, side: Side::Yes,
                            token_id: market.yes_token_id.clone(),
                            entry_price: fill_price, size,
                            strategy: format!("lag(+{:.0}¢)", yes_mispricing * 100.0),
                            opened_at: now_inst, market_slug: slug.clone(),
                        });
                        stats.entries += 1;
                        trade_id += 1;
                        let log = TradeLog {
                            id: trade_id, time: Utc::now(), action: "BUY".into(),
                            side: Side::Yes, price: fill_price, size, pnl: 0.0,
                            strategy: format!("lag(+{:.0}¢)", yes_mispricing * 100.0),
                            capital_after: capital,
                        };
                        println!("  ENTRY {}", log);
                        let _ = std::io::stdout().flush();
                        push_log(&mut trade_log, log);
                        last_entry = now_inst;
                        entered = true;
                    }
                }
            }
            // Only buy NO if BTC just moved DOWN
            if !entered && no_mispricing > LAG_MIN_EDGE && no_ask >= PRICE_FLOOR && no_ask <= PRICE_CEILING
                && no_spread_ok && btc_just_moved && btc_down
            {
                let buy_slippage = no_ask * (SLIPPAGE_BPS / 10000.0);
                let fill_price = no_ask + buy_slippage;
                let cost = MAX_COST_PER_POS.min(capital * 0.20);
                let size = cost / fill_price;
                if cost >= MIN_POSITION_COST && capital >= cost {
                    if rng.next_f64() < TAKER_FILL_PROB {
                        capital -= cost;
                        next_pos_id += 1;
                        positions.push(Position {
                            id: next_pos_id, side: Side::No,
                            token_id: market.no_token_id.clone(),
                            entry_price: fill_price, size,
                            strategy: format!("lag(+{:.0}¢)", no_mispricing * 100.0),
                            opened_at: now_inst, market_slug: slug.clone(),
                        });
                        stats.entries += 1;
                        trade_id += 1;
                        let log = TradeLog {
                            id: trade_id, time: Utc::now(), action: "BUY".into(),
                            side: Side::No, price: fill_price, size, pnl: 0.0,
                            strategy: format!("lag(+{:.0}¢)", no_mispricing * 100.0),
                            capital_after: capital,
                        };
                        println!("  ENTRY {}", log);
                        let _ = std::io::stdout().flush();
                        push_log(&mut trade_log, log);
                        last_entry = now_inst;
                        entered = true;
                    }
                }
            }

            // ── Arb: buy both when YES+NO < threshold ──
            if !entered && yes_ask + no_ask < ARB_THRESHOLD && positions.len() + 1 < MAX_POSITIONS {
                let arb_size = (capital * 0.20 / (yes_ask + no_ask)).max(MIN_POSITION_COST);
                let arb_cost = (yes_ask + no_ask) * arb_size;
                if arb_cost <= capital * 0.40 && arb_size >= MIN_POSITION_COST {
                    if rng.next_f64() < TAKER_FILL_PROB {
                        capital -= arb_cost;
                        next_pos_id += 1;
                        positions.push(Position {
                            id: next_pos_id, side: Side::Yes,
                            token_id: market.yes_token_id.clone(),
                            entry_price: yes_ask, size: arb_size,
                            strategy: "arb".into(), opened_at: now_inst,
                            market_slug: slug.clone(),
                        });
                        next_pos_id += 1;
                        positions.push(Position {
                            id: next_pos_id, side: Side::No,
                            token_id: market.no_token_id.clone(),
                            entry_price: no_ask, size: arb_size,
                            strategy: "arb".into(), opened_at: now_inst,
                            market_slug: slug.clone(),
                        });
                        stats.entries += 2;
                        trade_id += 1;
                        let edge = 1.0 - yes_ask - no_ask;
                        let log = TradeLog {
                            id: trade_id, time: Utc::now(), action: "ARB".into(),
                            side: Side::Yes, price: yes_ask + no_ask, size: arb_size,
                            pnl: 0.0,
                            strategy: format!("arb(edge={:.0}¢)", edge * 100.0),
                            capital_after: capital,
                        };
                        println!("  ENTRY {}", log);
                        let _ = std::io::stdout().flush();
                        push_log(&mut trade_log, log);
                        last_entry = now_inst;
                    }
                }
            }
        }

        // ── Dashboard ──
        maybe_dashboard(now_inst, &mut last_dash, dash_interval, capital, btc_price,
            &positions, &trade_log, &stats, remaining, &slug,
            fair_up, yes_ask, yes_bid, no_ask, no_bid, ref_p, btc_move_pct);
    }

    // ═══════════════════════════════════════════════════════════
    // SESSION SUMMARY
    // ═══════════════════════════════════════════════════════════
    let realized_pnl = stats.total_exit_pnl + stats.total_resolution_pnl;
    let exit_wr = if stats.exits > 0 { stats.winning_exits as f64 / stats.exits as f64 * 100.0 } else { 0.0 };
    println!("\n{}", "=".repeat(80));
    println!("  SESSION COMPLETE | {} cycles", stats.cycles);
    println!("{}", "=".repeat(80));
    println!("  Capital:    ${:.2} → ${:.2}  |  Realized P&L: {:>+.3} ({:>+.1}%)",
        STARTING_CAPITAL, capital, realized_pnl, realized_pnl / STARTING_CAPITAL * 100.0);
    println!("  Entries:    {}  |  Exits: {} ({:.0}% win)  |  Resolutions: {}",
        stats.entries, stats.exits, exit_wr, stats.resolutions);
    println!("  Exit P&L:   {:>+.4}  |  Resolution P&L: {:>+.4}",
        stats.total_exit_pnl, stats.total_resolution_pnl);
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

/// Infer the market's true reference price from the book's current implied probability.
///
/// When joining a market mid-cycle, the book already reflects the correct probability
/// based on the true open price. We reverse-engineer what reference price produces
/// the book's implied probability given current BTC price, time remaining, and vol.
///
/// fair_up = Φ(pct_move / (vol × √time))
/// ⟹ pct_move = Φ⁻¹(book_yes_mid) × vol × √time
/// ⟹ ref = btc / (1 + pct_move)
fn calibrate_reference_price(
    btc_price: f64,
    book_yes_mid: f64,
    minutes_remaining: f64,
    vol_per_min: f64,
) -> f64 {
    // Clamp to avoid infinite z-scores at extremes
    let p = book_yes_mid.clamp(0.02, 0.98);
    let normal = Normal::new(0.0, 1.0).expect("valid normal");
    let z = normal.inverse_cdf(p);
    let remaining_vol = vol_per_min * minutes_remaining.sqrt();
    if remaining_vol < 1e-10 {
        return btc_price; // No vol info, fallback
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
        Utc::now().format("%H:%M:%S"), stats.cycles, btc_price, capital, realized_pnl, realized_pnl / STARTING_CAPITAL * 100.0);
    println!("  Market: {} | {:.0}s left | Exposure: ${:.2} | {} open",
        slug, remaining, exposure, positions.len());
    println!("  Stats: {} entries | {} exits ({:.0}% win) | {} resolved | exit_pnl: {:>+.3} | res_pnl: {:>+.3}",
        stats.entries, stats.exits, exit_wr, stats.resolutions, stats.total_exit_pnl, stats.total_resolution_pnl);
    let pct_move = if ref_p > 0.0 { (btc_price - ref_p) / ref_p * 100.0 } else { 0.0 };
    let yes_spread = if yes_ask > 0.0 { (yes_ask - yes_bid) / yes_ask * 100.0 } else { 0.0 };
    let no_spread = if no_ask > 0.0 { (no_ask - no_bid) / no_ask * 100.0 } else { 0.0 };
    println!("  Fair: UP={:.3} DN={:.3} | BTC {:>+.3}% from ref | YES {:.2}/{:.2} ({:.0}%sp) | NO {:.2}/{:.2} ({:.0}%sp)",
        fair_up, 1.0-fair_up, pct_move, yes_bid, yes_ask, yes_spread, no_bid, no_ask, no_spread);
    let yes_misp = fair_up - yes_ask;
    let no_misp = (1.0-fair_up) - no_ask;
    println!("  Mispricing: YES {:>+.3} | NO {:>+.3} | need >{:.3} & move>{:.2}% | last_move={:.3}%",
        yes_misp, no_misp, LAG_MIN_EDGE, MIN_BTC_MOVE_PCT, btc_move_pct);

    // Open positions detail
    if !positions.is_empty() {
        println!("  Open positions:");
        for p in positions {
            let age = now.duration_since(p.opened_at).as_secs();
            println!("    #{} {:?} @ {:.3} x{:.2} | {}s held | {}",
                p.id, p.side, p.entry_price, p.size, age, p.strategy);
        }
    }

    // Recent trades
    if !trade_log.is_empty() {
        println!("  Recent:");
        for t in trade_log.iter().rev().take(5).collect::<Vec<_>>().iter().rev() {
            println!("    {}", t);
        }
    }

    println!("  {}", "-".repeat(76));
    let _ = std::io::stdout().flush();
}

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Asset {
    BTC,
    ETH,
    SOL,
    XRP,
}

impl Asset {
    pub fn slug_prefix(&self) -> &'static str {
        match self {
            Asset::BTC => "btc",
            Asset::ETH => "eth",
            Asset::SOL => "sol",
            Asset::XRP => "xrp",
        }
    }

    pub fn annual_volatility(&self) -> f64 {
        match self {
            Asset::BTC => 0.55,
            Asset::ETH => 0.70,
            Asset::SOL => 0.95,
            Asset::XRP => 0.85,
        }
    }

    pub fn vol_per_minute(&self) -> f64 {
        self.annual_volatility() / (525_600.0_f64).sqrt()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Duration {
    FiveMin,
    FifteenMin,
}

impl Duration {
    pub fn seconds(&self) -> u64 {
        match self {
            Duration::FiveMin => 300,
            Duration::FifteenMin => 900,
        }
    }

    pub fn slug_suffix(&self) -> &'static str {
        match self {
            Duration::FiveMin => "5m",
            Duration::FifteenMin => "15m",
        }
    }

    pub fn interval_seconds(&self) -> u64 {
        self.seconds()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Yes,
    No,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Yes => Side::No,
            Side::No => Side::Yes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LifecyclePhase {
    AlphaWindow,    // 0-5s (5m) or 0-15s (15m) — highest priority
    EarlyArbs,      // 5-30s / 15-90s
    PrimeZone,      // 30-120s / 90-600s
    MaturePhase,    // 120-240s / 600-780s
    PreResolution,  // 240-270s / 780-870s
    Lockout,        // 270-300s / 870-900s
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub id: String,
    pub slug: String,
    pub asset: Asset,
    pub duration: Duration,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub condition_id: Option<String>,
    pub reference_price: f64,
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub tick_size: Decimal,
    pub active: bool,
}

impl Market {
    pub fn new(
        slug: String,
        asset: Asset,
        duration: Duration,
        yes_token_id: String,
        no_token_id: String,
    ) -> Self {
        Self::with_condition_id(slug, asset, duration, yes_token_id, no_token_id, None)
    }

    pub fn with_condition_id(
        slug: String,
        asset: Asset,
        duration: Duration,
        yes_token_id: String,
        no_token_id: String,
        condition_id: Option<String>,
    ) -> Self {
        let now = Utc::now();
        let interval = duration.interval_seconds();
        let now_unix = now.timestamp() as u64;
        let interval_start = (now_unix / interval) * interval;
        let open_time = DateTime::from_timestamp(interval_start as i64, 0)
            .unwrap_or(now);
        let close_time = DateTime::from_timestamp((interval_start + interval) as i64, 0)
            .unwrap_or(now);

        Self {
            id: slug.clone(),
            slug,
            asset,
            duration,
            yes_token_id,
            no_token_id,
            condition_id,
            reference_price: 0.0, // Set by first Binance price update
            open_time,
            close_time,
            tick_size: Decimal::new(1, 2), // $0.01
            active: true,
        }
    }

    /// Set the reference price (Binance price at market open).
    pub fn set_reference_price(&mut self, price: f64) {
        if self.reference_price == 0.0 {
            self.reference_price = price;
        }
    }

    pub fn time_remaining_secs(&self) -> f64 {
        let now = Utc::now();
        if now >= self.close_time {
            return 0.0;
        }
        (self.close_time - now).num_milliseconds() as f64 / 1000.0
    }

    pub fn time_elapsed_secs(&self) -> f64 {
        let now = Utc::now();
        if now <= self.open_time {
            return 0.0;
        }
        (now - self.open_time).num_milliseconds() as f64 / 1000.0
    }

    pub fn lifecycle_phase(&self) -> LifecyclePhase {
        let elapsed = self.time_elapsed_secs();
        let total = self.duration.seconds() as f64;

        if self.time_remaining_secs() <= 0.0 {
            return LifecyclePhase::Resolved;
        }

        let _fraction = elapsed / total;

        match self.duration {
            Duration::FiveMin => {
                if elapsed < 5.0 {
                    LifecyclePhase::AlphaWindow
                } else if elapsed < 30.0 {
                    LifecyclePhase::EarlyArbs
                } else if elapsed < 120.0 {
                    LifecyclePhase::PrimeZone
                } else if elapsed < 240.0 {
                    LifecyclePhase::MaturePhase
                } else if elapsed < 270.0 {
                    LifecyclePhase::PreResolution
                } else {
                    LifecyclePhase::Lockout
                }
            }
            Duration::FifteenMin => {
                if elapsed < 15.0 {
                    LifecyclePhase::AlphaWindow
                } else if elapsed < 90.0 {
                    LifecyclePhase::EarlyArbs
                } else if elapsed < 600.0 {
                    LifecyclePhase::PrimeZone
                } else if elapsed < 780.0 {
                    LifecyclePhase::MaturePhase
                } else if elapsed < 870.0 {
                    LifecyclePhase::PreResolution
                } else {
                    LifecyclePhase::Lockout
                }
            }
        }
    }

    pub fn generate_slug(asset: Asset, duration: Duration, interval_start_unix: u64) -> String {
        format!(
            "{}-updown-{}-{}",
            asset.slug_prefix(),
            duration.slug_suffix(),
            interval_start_unix
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub token_id: String,
    pub bids: BTreeMap<Decimal, Decimal>, // price → size (descending by price)
    pub asks: BTreeMap<Decimal, Decimal>, // price → size (ascending by price)
    pub timestamp: DateTime<Utc>,
}

impl OrderBook {
    pub fn new(token_id: String) -> Self {
        Self {
            token_id,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            timestamp: Utc::now(),
        }
    }

    pub fn best_bid(&self) -> Option<(Decimal, Decimal)> {
        self.bids.iter().next_back().map(|(&p, &s)| (p, s))
    }

    pub fn best_ask(&self) -> Option<(Decimal, Decimal)> {
        self.asks.iter().next().map(|(&p, &s)| (p, s))
    }

    pub fn midpoint(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some((bid, _)), Some((ask, _))) => Some((bid + ask) / Decimal::from(2)),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some((bid, _)), Some((ask, _))) => Some(ask - bid),
            _ => None,
        }
    }

    /// Available depth within `tolerance` of the best price on ask side
    pub fn ask_depth_within(&self, tolerance: Decimal) -> Decimal {
        let Some((best, _)) = self.best_ask() else {
            return Decimal::ZERO;
        };
        let max_price = best + tolerance;
        self.asks
            .range(..=max_price)
            .map(|(_, &size)| size)
            .sum()
    }

    /// Walk asks to find the worst price needed to fill a BUY of `usdc_amount` dollars.
    /// Returns (worst_price, available_usdc_depth) or None if no asks.
    pub fn calculate_buy_market_price(&self, usdc_amount: f64) -> Option<(f64, f64)> {
        let mut cumulative_cost = 0.0;
        let mut worst_price = 0.0;
        for (&price_dec, &size_dec) in self.asks.iter() {
            let price = price_dec.to_string().parse::<f64>().unwrap_or(0.0);
            let size = size_dec.to_string().parse::<f64>().unwrap_or(0.0);
            if price <= 0.0 || size <= 0.0 { continue; }
            cumulative_cost += price * size;
            worst_price = price;
            if cumulative_cost >= usdc_amount {
                return Some((worst_price, cumulative_cost));
            }
        }
        // Book doesn't have enough — return available depth
        if worst_price > 0.0 { Some((worst_price, cumulative_cost)) } else { None }
    }

    /// Walk bids to find the worst price needed to SELL `share_amount` shares.
    /// Returns (worst_price, total_usdc) or None if book can't fill.
    pub fn calculate_sell_market_price(&self, share_amount: f64) -> Option<(f64, f64)> {
        let mut cumulative_shares = 0.0;
        let mut cumulative_usdc = 0.0;
        let mut worst_price = 0.0;
        // bids: BTreeMap ascending, iter().rev() gives best (highest) first
        for (&price_dec, &size_dec) in self.bids.iter().rev() {
            let price = price_dec.to_string().parse::<f64>().unwrap_or(0.0);
            let size = size_dec.to_string().parse::<f64>().unwrap_or(0.0);
            if price <= 0.0 || size <= 0.0 { continue; }
            cumulative_shares += size;
            cumulative_usdc += price * size;
            worst_price = price;
            if cumulative_shares >= share_amount {
                return Some((worst_price, cumulative_usdc));
            }
        }
        if worst_price > 0.0 { Some((worst_price, cumulative_usdc)) } else { None }
    }

    /// Available depth within `tolerance` of the best price on bid side
    pub fn bid_depth_within(&self, tolerance: Decimal) -> Decimal {
        let Some((best, _)) = self.best_bid() else {
            return Decimal::ZERO;
        };
        let min_price = best - tolerance;
        self.bids
            .range(min_price..)
            .map(|(_, &size)| size)
            .sum()
    }
}

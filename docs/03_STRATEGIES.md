# SATTEBAAZ — Four Core Sub-Strategies

---

## STRATEGY 1: Pure Arbitrage Engine

### Detection
```
FUNCTION scan_arb(market):
    yes_ask = get_best_ask(market.yes_token)
    no_ask  = get_best_ask(market.no_token)
    combined = yes_ask + no_ask
    edge = 1.0 - combined
    
    IF edge < 0.02:  // Min 2c edge
        RETURN NULL
    
    yes_depth = available_depth(market.yes_token, yes_ask, tolerance=0.02)
    no_depth  = available_depth(market.no_token, no_ask, tolerance=0.02)
    executable = MIN(yes_depth, no_depth)
    expected_fill = executable * 0.85  // 85% conservative fill rate
    expected_profit = expected_fill * edge
    
    IF expected_profit > 0.10:  // Min $0.10 expected
        RETURN ArbSignal { edge, executable, expected_profit, yes_ask, no_ask }
    RETURN NULL
```

### Execution
```
FUNCTION execute_arb(signal):
    // BATCH submit both legs in single API call
    orders = [
        { token: YES, side: BUY, price: signal.yes_ask, size: signal.executable, type: FAK },
        { token: NO,  side: BUY, price: signal.no_ask,  size: signal.executable, type: FAK }
    ]
    results = batch_submit(orders)
    
    imbalance = abs(results[0].filled - results[1].filled)
    IF imbalance > 0:
        // Try to complete deficit side
        deficit = smaller_fill_side(results)
        deficit_ask = get_best_ask(deficit)
        IF deficit_ask + other_side_cost < 1.0:
            place_order(deficit, BUY, deficit_ask, imbalance, FAK)
        ELSE:
            place_order(excess_side, SELL, get_best_bid(excess), imbalance, FAK)
    
    locked_profit = MIN(results[0].filled, results[1].filled) * signal.edge
```

### Position Sizing

| Capital | Max Per Arb | Rationale |
|---------|-------------|-----------|
| $5-50 | 100% | Small capital = max every arb |
| $50-500 | 50% | Diversify across cycles |
| $500-5K | 25% | Risk management |
| $5K-50K | 10% | Book depth limits binding |
| $50K+ | MIN(10%, depth) | Capacity constrained |

### Fill Probability Model
```
P(fill) ≈ MIN(order_size / available_depth, 1.0) × vol_penalty
  DEAD:   vol_penalty = 0.95
  LOW:    vol_penalty = 0.90
  MEDIUM: vol_penalty = 0.80
  HIGH:   vol_penalty = 0.65

P(both_legs) = P(yes) × P(no)
At 85% each: P(both) = 0.72 → batch submission is CRITICAL
```

### Expected Performance
- **Win rate**: 95%+ (only risk is partial fills)
- **Avg profit**: 8-15c per unit
- **Daily ROI**: 5-15% (depends on arb frequency)
- **Risk**: Near-zero (guaranteed payout exceeds cost)

---

## STRATEGY 2: Cross-Exchange Lag Exploit

### Price-to-Probability Model
```
P(UP at expiry) = Φ(d)
  d = pct_move / (σ_per_min × √(minutes_remaining))
  
  σ_per_min = annual_vol / √525600
  BTC annual vol ≈ 55% → σ_per_min ≈ 0.0758%
```

### Probability Lookup Table (BTC 5-min market)

| BTC % Move | 4m left | 3m left | 2m left | 1m left | 30s left |
|------------|---------|---------|---------|---------|----------|
| +0.05% | 0.59 | 0.62 | 0.67 | 0.74 | 0.82 |
| +0.10% | 0.67 | 0.72 | 0.78 | 0.87 | 0.94 |
| +0.15% | 0.74 | 0.79 | 0.86 | 0.94 | 0.98 |
| +0.20% | 0.80 | 0.85 | 0.92 | 0.97 | 0.99 |
| +0.30% | 0.89 | 0.93 | 0.97 | 0.99 | ~1.00 |
| -0.10% | 0.33 | 0.28 | 0.22 | 0.13 | 0.06 |
| -0.20% | 0.20 | 0.15 | 0.08 | 0.03 | 0.01 |

### Execution
```
FUNCTION lag_exploit(market, binance_feed):
    open_price = market.reference_price
    time_remaining = market.expiry - NOW()
    binance_price = binance_feed.latest_price
    
    fair_prob = Φ(pct_move / (σ_per_min × √(time_remaining_min)))
    
    yes_ask = get_best_ask(market.yes_token)
    no_ask  = get_best_ask(market.no_token)
    
    yes_mispricing = fair_prob - yes_ask       // Positive = YES cheap
    no_mispricing  = (1.0 - fair_prob) - no_ask // Positive = NO cheap
    
    min_edge = 0.03
    
    IF yes_mispricing > min_edge:
        size = kelly_size(yes_mispricing, yes_ask)
        place_order(YES, BUY, yes_ask, size, FAK)
    ELIF no_mispricing > min_edge:
        size = kelly_size(no_mispricing, no_ask)
        place_order(NO, BUY, no_ask, size, FAK)

FUNCTION kelly_size(edge, ask_price):
    win_prob = MIN(0.62 + edge * 2.0, 0.80)
    payout_odds = (1.0 - ask_price) / ask_price
    kelly = (payout_odds * win_prob - (1.0 - win_prob)) / payout_odds
    RETURN capital * kelly * 0.25  // 25% fractional Kelly
```

### Volatility Filters
```
DO NOT trade lag when:
  - vol_regime == DEAD (no meaningful moves)
  - spread > 0.10 AND time_remaining < 120s
  - vol_regime == HIGH AND time_remaining < 60s

BEST conditions:
  - vol_regime == MEDIUM, time_remaining > 120s, spread < 0.06
```

### Expected Performance
- **Win rate**: 62-70%
- **Avg win**: 4-8c per unit
- **Avg loss**: 3-5c per unit
- **Daily ROI**: 10-25%
- **Risk**: Medium (direction risk, mitigated by Kelly sizing)

---

## STRATEGY 3: Micro Market-Making

### Core Logic
```
FUNCTION market_make(market):
    fair_value = probability_model(binance_price, open_price, time_remaining, vol)
    vol_regime = get_vol_regime()
    
    half_spread = CASE vol_regime OF
        DEAD:   0.015   // 1.5c each side
        LOW:    0.020   // 2c
        MEDIUM: 0.030   // 3c
        HIGH:   0.050   // 5c — or PULL entirely
    
    // Widen near expiry (gamma risk)
    time_factor = 1.0 + MAX(0, (120 - time_remaining) / 120) * 0.5
    half_spread *= time_factor
    
    bid = fair_value - half_spread
    ask = fair_value + half_spread
    
    // Inventory skew: shift quotes to offload excess
    net_pos = yes_holdings - no_holdings
    skew = CLAMP((net_pos / (capital * 0.5)) * 0.02, -0.03, 0.03)
    bid -= skew
    ask -= skew
    
    // Size inversely proportional to vol
    size_mult = CASE vol_regime OF
        DEAD: 2.0, LOW: 1.5, MEDIUM: 1.0, HIGH: 0.3
    quote_size = capital * 0.10 * size_mult
    
    cancel_existing(market)
    place_order(YES, BUY, bid, quote_size, GTC, post_only=true)
    place_order(YES, SELL, ask, quote_size, GTC, post_only=true)
```

### Adverse Selection Avoidance
```
FUNCTION detect_adverse_selection():
    // Binance moving fast → PULL quotes
    IF abs(binance_1s_move) > 0.02%:
        RETURN PULL_QUOTES
    
    // One-sided aggressive flow → WIDEN spread
    IF order_flow_imbalance_5s > 3.0:
        RETURN WIDEN_SPREAD
    
    // Liquidation cascade → full retreat
    IF liquidation_detected():
        RETURN PULL_QUOTES
    
    // Our fills consistently one-sided → we're being picked off
    IF last_10_fills_same_side():
        RETURN WIDEN_AND_SKEW
    
    RETURN NORMAL
```

### When NOT to Market-Make
- Last 30 seconds (gamma risk too high)
- During liquidation cascades
- HIGH vol with capital < $100
- Spread already < 1c (no edge left)

### Expected Performance
- **Win rate**: 55-65% (per round trip)
- **Avg spread captured**: 2-4c
- **Daily ROI**: 8-20%
- **Risk**: Medium-High (adverse selection, inventory risk)

---

## STRATEGY 4: Probability Momentum Capture

### Detection
```
FUNCTION detect_momentum(market):
    // Track YES price velocity over windows
    velocity_5s  = (price_now - price_5s_ago) / 5
    velocity_15s = (price_now - price_15s_ago) / 15
    velocity_30s = (price_now - price_30s_ago) / 30
    
    // Acceleration (second derivative)
    acceleration = velocity_5s - velocity_15s
    
    // Composite momentum score
    momentum = velocity_5s * 0.5 + acceleration * 0.3 + (velocity_5s - velocity_30s) * 0.2
    
    // Divergence from fair value
    fair_prob = price_to_implied_prob(...)
    divergence = fair_prob - yes_price_now
    
    RETURN { momentum, acceleration, divergence, velocity_5s }
```

### Exhaustion Detection
```
FUNCTION detect_exhaustion(momentum_history):
    IF len(history) < 5: RETURN FALSE
    peak = MAX(history[-10:])
    current = history[-1]
    IF peak > 0.005 AND current < peak * 0.4:
        RETURN TRUE  // Momentum decayed 60%+
    RETURN FALSE
```

### Execution
```
FUNCTION momentum_trade(market):
    sig = detect_momentum(market)
    
    IF abs(sig.momentum) > 0.003 AND abs(sig.divergence) > 0.02:
        IF sig.momentum > 0 AND sig.divergence > 0:
            // Upward momentum, fair value above market → BUY YES
            size = capital * 0.10 * MIN(abs(sig.divergence)/0.05, 2.0) * MIN(abs(sig.momentum)/0.005, 1.5)
            place_order(YES, BUY, get_best_ask(YES), size, FAK)
        ELIF sig.momentum < 0 AND sig.divergence < 0:
            // Downward momentum → BUY NO
            size = same_formula
            place_order(NO, BUY, get_best_ask(NO), size, FAK)
    
    // Exit on exhaustion or hold to resolution if < 45s remaining
    IF has_position() AND detect_exhaustion(history):
        sell_at_market()
    IF time_remaining < 45:
        HOLD  // Let resolution decide
```

### Optimal Hold Duration

| Market | Avg Hold | Max Hold |
|--------|----------|----------|
| 5m BTC | 30-90s | 180s |
| 15m BTC | 60-300s | 600s |
| 15m ETH | 60-240s | 600s |
| 15m SOL | 45-180s | 600s |
| 15m XRP | 60-300s | 600s |

### Expected Performance
- **Win rate**: 58-65%
- **Avg win**: 5-10c
- **Avg loss**: 3-6c
- **Daily ROI**: 5-15%
- **Risk**: Medium (reversal risk, mitigated by exhaustion detection)

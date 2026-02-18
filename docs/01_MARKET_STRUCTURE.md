# SATTEBAAZ — Market Structure Analysis

## 1. Polymarket Short-Duration Market Mechanics

Binary outcome markets: YES (price UP from open) or NO (price DOWN from open).
- Pays $1 if correct, $0 if wrong
- Settlement via Chainlink oracle, automated, on Polygon (chain_id=137)
- Collateral: USDC
- Fees: **0% maker, 0% taker** currently

### Slug Format
```
{asset}-updown-{duration}-{unix_timestamp}
btc-updown-5m-1770933900
eth-updown-15m-1768502700
```
Timestamp = interval start time (Unix seconds). New markets every 5 or 15 minutes.

### Market Lifecycle (5-minute)
```
T+0-5s    ALPHA WINDOW. Book nearly empty. Biggest mispricings.
T+5-30s   Thin liquidity. Spreads 8-15c. Early arb opportunities.
T+30-120s Books building. Spreads 4-8c. Prime trading zone.
T+120-240s Peak volume. Spreads 2-4c. Lag exploit + MM.
T+240-270s Convergence. Spreads 1-2c. Pre-resolution.
T+270-300s Lockout zone. Hold to resolution. No new orders.
T+300s    Resolution. Chainlink settles. Capital freed.
```

### CLOB Order Types
- **GTC**: Good-Til-Cancelled — standard limit order
- **GTD**: Good-Til-Date — auto-expires at timestamp
- **FOK**: Fill-Or-Kill — all or nothing market order
- **FAK**: Fill-And-Kill — partial fills OK, rest cancelled
- **Post-Only**: Ensures maker execution, rejects if would cross spread
- **Batch**: Submit multiple orders in single API call

### Key Structural Properties
1. Binary payout: $1 or $0, nothing in between
2. YES + NO must sum to ≤ $1 (often < $1 = arb opportunity)
3. Thin early books: first 30s depth often < $1,000
4. Oracle lag: Chainlink heartbeat 20-60s vs Binance 100ms updates
5. No short selling: can only buy YES or NO tokens

## 2. When Mispricing Occurs

| Condition | Frequency | Magnitude | Duration |
|-----------|-----------|-----------|----------|
| First 30s after open | VERY HIGH | 5-15c | 10-30s |
| Volatility spike (ATR>2x) | HIGH | 3-10c | 30-90s |
| Liquidation cascade | VERY HIGH | 8-20c | 15-60s |
| Funding rate spike | MEDIUM | 2-5c | 60-180s |
| VWAP deviation >0.3% | MEDIUM | 3-8c | 30-120s |
| Breakout from compression | HIGH | 5-12c | 20-60s |
| Low-vol compression | LOW | 1-2c | persistent |

### The Opening Window (60% of daily arb profit)
First 5-30 seconds of every market:
- MMs haven't posted quotes yet
- Book essentially empty
- YES+NO sum frequently < $0.92 (8%+ arb)
- Fast bot posting in first 5 seconds captures outsized edge

## 3. Volatility Regime Classification (1-minute ATR)

| Regime | BTC ATR(1m) | ETH | SOL | XRP | Best Strategy |
|--------|------------|-----|-----|-----|---------------|
| DEAD | <$15 | <$1 | <$0.05 | <$0.001 | MM only |
| LOW | $15-50 | $1-3 | $0.05-0.15 | $0.001-0.003 | Straddle+MM |
| MEDIUM | $50-150 | $3-10 | $0.15-0.50 | $0.003-0.008 | Lag+Straddle |
| HIGH | $150-300 | $10-20 | $0.50-1.00 | $0.008-0.015 | Arb+Lag |
| EXTREME | >$300 | >$20 | >$1.00 | >$0.015 | Arb only |

## 4. Quantitative Inefficiencies

### YES+NO Summation Gap
```
Efficient market: P(YES) + P(NO) = 1.00
Reality:
  Average:    0.94 - 0.98
  First 30s:  0.85 - 0.95  → 5-15% risk-free edge
  High vol:   0.80 - 0.92  → 8-20% risk-free edge
  Low vol:    0.96 - 1.00  → minimal edge
```

### Oracle Lag (30-90 second advantage)
```
T+0ms:     Binance perp trade executes
T+100ms:   Binance WebSocket broadcasts
T+150ms:   Our bot computes new implied probability
T+200ms:   Our bot submits to Polymarket
T+500ms:   Order confirmed
---
T+5000ms:  Average Polymarket bot detects change
T+30000ms: Manual traders notice
T+60000ms: Book fully reflects new info
```

### Probability Stickiness
BTC moves 0.3% in 10s → true P(UP) shifts 50%→72%
Polymarket YES token: sits at 0.52-0.55 for 30-60s more.
Caused by stale GTC orders, slow bots, retail anchoring.

### Spread Structure
```
First 30s:   8-15c wide
30s-2min:    4-8c wide
2min-4min:   2-4c wide
Last minute: 1-2c wide
```

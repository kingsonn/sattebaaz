# SATTEBAAZ — Polymarket Short-Duration Trading Bot

Elite prediction market trading bot targeting 5-minute and 15-minute BTC/ETH/SOL/XRP updown markets on Polymarket.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        main.rs                              │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────────┐  │
│  │ Binance  │  │Polymarket│  │ Strategy │  │   Risk     │  │
│  │   Feed   │  │   Feed   │  │Orchestrat│  │  Watchdog  │  │
│  │  (WS)    │  │ (WS+REST)│  │   or     │  │  (500ms)   │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └─────┬──────┘  │
│       │              │             │              │          │
│  price ticks    order books    OrderIntents   risk checks   │
│       │              │             │              │          │
│       ▼              ▼             ▼              ▼          │
│  ┌──────────────────────────────────────────────────────┐   │
│  │              Event Loop (tokio async)                │   │
│  │  price_rx → vol_tracker → orchestrator → risk_check  │   │
│  │              → batch_submitter → fill_tracker         │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

## Strategies

| Strategy | Description | Vol Regime | Expected Edge |
|---|---|---|---|
| **Straddle + Bias** | Buy YES+NO when combined < $0.97, amplify on directional signal | All | 3-5% per trade |
| **Pure Arbitrage** | Lock in risk-free profit when YES+NO < $1.00 | All | 1-3% guaranteed |
| **Lag Exploit** | Buy underpriced side when Polymarket lags Binance by >3¢ | Med-High | 2-6% per trade |
| **Market Making** | Two-sided quotes around fair value, capture spread | Dead-Med | 0.5-2% per round |
| **Momentum Capture** | Enter in direction of probability acceleration | Med-High | 3-8% per trade |

## Project Structure

```
src/
├── main.rs                    # Entry point, async event loop
├── config.rs                  # Typed configuration with defaults
├── models/
│   ├── market.rs              # Asset, Duration, Market, OrderBook
│   ├── order.rs               # OrderIntent, OrderResult, Fill
│   ├── signal.rs              # VolRegime, BiasSignal, ArbSignal, MomentumSignal
│   ├── position.rs            # Position, StraddlePosition, Portfolio
│   └── candle.rs              # Candle struct, IndicatorEngine (ATR, EMA, BBW)
├── feeds/
│   ├── binance.rs             # Binance futures WebSocket (aggTrade, forceOrder)
│   ├── polymarket.rs          # Polymarket REST + WebSocket (books, market discovery)
│   └── market_discovery.rs    # Slug generation, interval timing
├── signals/
│   ├── probability.rs         # Black-Scholes-like fair prob model + Kelly sizing
│   ├── arb_scanner.rs         # YES+NO < $1 arbitrage detection
│   ├── volatility.rs          # ATR-based volatility classification
│   ├── realtime_vol.rs        # Live ATR computation from price ticks
│   ├── bias.rs                # Multi-factor bias detector
│   ├── momentum.rs            # Probability acceleration + exhaustion
│   └── compression.rs         # BBW compression + breakout detection
├── strategies/
│   ├── straddle_bias.rs       # Core straddle-first bias amplification
│   ├── pure_arb.rs            # YES+NO risk-free arbitrage
│   ├── lag_exploit.rs         # Cross-exchange lag capture
│   ├── market_maker.rs        # Micro market-making with adverse selection
│   ├── momentum_capture.rs    # Probability momentum trading
│   └── orchestrator.rs        # Strategy priority + capital allocation
├── execution/
│   ├── order_builder.rs       # EIP-712 signed order construction
│   ├── clob_auth.rs           # L1 (EIP-712) and L2 (HMAC API key) authentication
│   ├── clob_client.rs         # Authenticated REST client for CLOB API
│   ├── batch_submitter.rs     # Batch order submission pipeline
│   └── fill_tracker.rs        # Real-time fill monitoring
├── risk/
│   ├── position_manager.rs    # Portfolio tracking, resolution settlement
│   ├── risk_manager.rs        # Kill switch, exposure limits, drawdown protection
│   └── sizing.rs              # Kelly criterion, capital-tier sizing
└── telemetry/
    ├── pnl.rs                 # Per-strategy P&L tracking
    ├── latency.rs             # Percentile latency histograms
    └── alerts.rs              # Telegram/Discord alert manager
```

## Setup

### Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs))
- Polygon wallet with USDC balance
- Polymarket account with CLOB access

### Configuration

1. Copy the example env file:
   ```bash
   cp .env.example .env
   ```

2. Set your private key:
   ```env
   POLYMARKET_PRIVATE_KEY=your_hex_private_key
   STARTING_CAPITAL=5
   ```

3. (Optional) Configure alerts:
   ```env
   TELEGRAM_BOT_TOKEN=your_bot_token
   TELEGRAM_CHAT_ID=your_chat_id
   ```

### Build & Run

```bash
# Build
cargo build --release

# Run (connects to Binance + Polymarket, starts trading)
cargo run --release

# Run with debug logging
RUST_LOG=debug cargo run --release

# Run tests
cargo test
```

## Risk Management

| Parameter | Default | Description |
|---|---|---|
| Max exposure | 50% | Maximum capital in open positions |
| Daily loss limit | 10% | Pause trading after this daily drawdown |
| Loss streak threshold | 5 | Reduce size after N consecutive losses |
| Size reduction | 50% | Position size multiplier during loss streak |
| Lockout | 30s | Stop trading before market resolution |

## Market Lifecycle (5-minute)

```
T+0-5s     ALPHA WINDOW — Book nearly empty, biggest mispricings
T+5-30s    EARLY ARBS — Thin liquidity, spreads 8-15¢
T+30-120s  PRIME ZONE — Books building, spreads 4-8¢
T+120-240s MATURE — Peak volume, spreads 2-4¢
T+240-270s PRE-RESOLUTION — Convergence, spreads 1-2¢
T+270-300s LOCKOUT — Hold to resolution, no new orders
T+300s     RESOLUTION — Chainlink settles, capital freed
```

## Capital Growth Path

| Phase | Capital | Daily Target | Strategies |
|---|---|---|---|
| 1 | $5–$50 | 15–25% | Full Kelly, all-in arb + straddle |
| 2 | $50–$500 | 10–15% | Half Kelly, add lag exploit |
| 3 | $500–$5K | 5–10% | Quarter Kelly, add market making |
| 4 | $5K–$50K | 3–5% | Conservative Kelly, full diversification |
| 5 | $50K+ | 1–3% | Ultra-conservative, depth-limited |

## Documentation

Detailed strategy research and design documents in `docs/`:

- `01_MARKET_STRUCTURE.md` — Market mechanics, lifecycle, inefficiencies
- `02_CORE_STRATEGY.md` — Straddle-first bias engine concept
- `03_STRATEGIES.md` — All 5 strategy implementations in detail
- `04_RISK_AND_EXECUTION.md` — Probability model, volatility thresholds, risk
- `05_ARCHITECTURE.md` — Rust architecture, concurrency model, latency budget

## License

Private — not for redistribution.

# SATTEBAAZ — Rust Execution Architecture

---

## System Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                        SATTEBAAZ ENGINE                          │
│                                                                  │
│  ┌─────────────┐  ┌─────────────┐  ┌──────────────────────────┐ │
│  │  BINANCE WS  │  │ POLYMARKET  │  │    CHAINLINK ORACLE      │ │
│  │  FEED (10ms) │  │  CLOB WS    │  │    MONITOR               │ │
│  └──────┬───────┘  └──────┬──────┘  └───────────┬──────────────┘ │
│         │                 │                      │               │
│         ▼                 ▼                      ▼               │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                  DATA INGESTION LAYER                      │  │
│  │  - Binance price/trade/liquidation streams                 │  │
│  │  - Polymarket orderbook snapshots + trade feed             │  │
│  │  - Market discovery (slug generation + active scanning)    │  │
│  │  - Normalized tick-by-tick storage (ring buffer)           │  │
│  └────────────────────────┬───────────────────────────────────┘  │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                  SIGNAL ENGINE                             │  │
│  │  - Volatility regime classifier (ATR-based)                │  │
│  │  - Probability model (Black-Scholes-like Φ(d))             │  │
│  │  - Bias detector (momentum + flow + funding + liquidations)│  │
│  │  - Arb scanner (YES+NO sum checker)                        │  │
│  │  - Momentum detector (velocity + acceleration)             │  │
│  │  - Compression/breakout detector (BBW)                     │  │
│  └────────────────────────┬───────────────────────────────────┘  │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                  STRATEGY ORCHESTRATOR                     │  │
│  │  - Straddle+Bias engine (primary)                          │  │
│  │  - Pure Arb engine                                         │  │
│  │  - Lag Exploit engine                                      │  │
│  │  - Market-Making engine                                    │  │
│  │  - Momentum engine                                         │  │
│  │  - Capital allocator (by tier + vol regime)                │  │
│  │  - Market lifecycle state machine (per active market)      │  │
│  └────────────────────────┬───────────────────────────────────┘  │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                  EXECUTION ENGINE                          │  │
│  │  - Order builder (sign + serialize EIP-712)                │  │
│  │  - Batch order submitter                                   │  │
│  │  - Fill tracker (WebSocket user channel)                   │  │
│  │  - Position manager                                        │  │
│  │  - Risk manager + kill switch                              │  │
│  └────────────────────────┬───────────────────────────────────┘  │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                  POLYMARKET CLOB REST API                  │  │
│  │  POST /order  |  POST /orders  |  DELETE /orders           │  │
│  │  GET /book    |  GET /price    |  GET /midpoint            │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │              MONITORING & TELEMETRY                        │  │
│  │  - P&L tracker (real-time per strategy)                    │  │
│  │  - Latency histogram (per API call)                        │  │
│  │  - Fill rate tracker                                       │  │
│  │  - Strategy performance dashboard                          │  │
│  │  - Alert system (Telegram/Discord webhook)                 │  │
│  └────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────┘
```

---

## Module Layout (Rust Crate Structure)

```
sattebaaz/
├── Cargo.toml
├── src/
│   ├── main.rs                  // Entry point, runtime setup
│   ├── config.rs                // Configuration (API keys, params, thresholds)
│   │
│   ├── feeds/                   // Data ingestion layer
│   │   ├── mod.rs
│   │   ├── binance.rs           // Binance WebSocket: price, trades, liquidations
│   │   ├── polymarket.rs        // Polymarket CLOB WebSocket: book, trades, user
│   │   └── market_discovery.rs  // Slug generation, active market scanning
│   │
│   ├── models/                  // Core data structures
│   │   ├── mod.rs
│   │   ├── market.rs            // Market, Token, OrderBook, Side
│   │   ├── order.rs             // Order, OrderType, Fill
│   │   ├── signal.rs            // Signal, BiasSignal, ArbSignal, MomentumSignal
│   │   ├── position.rs          // Position, Portfolio
│   │   └── candle.rs            // OHLCV, ATR, indicators
│   │
│   ├── signals/                 // Signal engine
│   │   ├── mod.rs
│   │   ├── volatility.rs        // ATR calculation, vol regime classifier
│   │   ├── probability.rs       // Price-to-probability model (Φ(d))
│   │   ├── bias.rs              // Bias detector (momentum+flow+funding+liqs)
│   │   ├── arb_scanner.rs       // YES+NO sum arbitrage detection
│   │   ├── momentum.rs          // Probability momentum + exhaustion
│   │   └── compression.rs       // BBW compression/breakout detector
│   │
│   ├── strategies/              // Strategy engines
│   │   ├── mod.rs
│   │   ├── straddle_bias.rs     // Core strategy: straddle + bias amplification
│   │   ├── pure_arb.rs          // Pure YES+NO arbitrage
│   │   ├── lag_exploit.rs       // Cross-exchange lag capture
│   │   ├── market_maker.rs      // Micro market-making
│   │   ├── momentum_capture.rs  // Probability momentum trading
│   │   └── orchestrator.rs      // Capital allocation, strategy prioritization
│   │
│   ├── execution/               // Order execution
│   │   ├── mod.rs
│   │   ├── order_builder.rs     // EIP-712 signing, order construction
│   │   ├── clob_client.rs       // REST client for Polymarket CLOB
│   │   ├── batch_submitter.rs   // Batch order submission
│   │   └── fill_tracker.rs      // WebSocket fill monitoring
│   │
│   ├── risk/                    // Risk management
│   │   ├── mod.rs
│   │   ├── position_manager.rs  // Track all positions across markets
│   │   ├── risk_manager.rs      // Kill switch, exposure limits, drawdown
│   │   └── sizing.rs            // Kelly criterion, tier-based sizing
│   │
│   └── telemetry/               // Monitoring
│       ├── mod.rs
│       ├── pnl.rs               // Real-time P&L tracking
│       ├── latency.rs           // Latency histograms
│       └── alerts.rs            // Telegram/Discord notifications
│
├── docs/
│   ├── 01_MARKET_STRUCTURE.md
│   ├── 02_CORE_STRATEGY.md
│   ├── 03_STRATEGIES.md
│   ├── 04_RISK_AND_EXECUTION.md
│   └── 05_ARCHITECTURE.md
│
└── .env.example                 // API keys template
```

---

## Concurrency Model

```
Tokio async runtime with dedicated task groups:

[Task Group 1: Data Feeds]  — 3 persistent WebSocket connections
  - binance_ws_task:     Binance price/trade/liquidation stream
  - polymarket_ws_task:  Polymarket book/trade updates  
  - discovery_task:      Polls for new markets every 5s

[Task Group 2: Signal Computation]  — triggered on every data tick
  - Runs in a tight loop on mpsc channel from feeds
  - Computes: vol regime, fair probability, bias, arb signals
  - Publishes signals to strategy channels
  - Target: < 1ms signal computation latency

[Task Group 3: Strategy Engines]  — one task per active market
  - Each active market gets its own tokio::spawn
  - Strategy orchestrator selects which sub-strategies run
  - Lifecycle state machine governs timing rules
  - Publishes OrderIntent to execution channel

[Task Group 4: Execution]  — single serialized executor
  - Receives OrderIntents from strategy tasks
  - Deduplicates, validates against risk limits
  - Batches compatible orders
  - Submits to Polymarket CLOB
  - Tracks fills via WebSocket
  - MUST be single-threaded to prevent conflicting orders

[Task Group 5: Risk Monitor]  — independent watchdog
  - Polls positions every 500ms
  - Checks exposure limits, drawdown
  - Can trigger cancel_all (kill switch)
  - Independent of other tasks — runs even if strategy hangs
```

### Channel Architecture
```
binance_ws  ──→ [mpsc] ──→ signal_engine ──→ [broadcast] ──→ strategy_tasks
polymarket_ws ──→ [mpsc] ──↗                                      │
                                                                   ▼
                                              [mpsc] ──→ execution_engine
                                                                   │
                                                                   ▼
                                              [mpsc] ──→ risk_monitor
```

---

## Latency Budget

| Component | Target | Notes |
|-----------|--------|-------|
| Binance WS → local | <10ms | Direct TCP, no proxy |
| Signal computation | <1ms | Pure math, no I/O |
| Strategy decision | <2ms | Simple logic |
| Order construction + signing | <5ms | EIP-712 in-memory |
| HTTP POST to Polymarket | <50ms | Polygon RPC, pre-warmed conn |
| **Total tick-to-order** | **<70ms** | vs competitors at 1-5 seconds |
| Polymarket WS → local | <30ms | For fill confirmation |
| Risk check cycle | <5ms | Every 500ms |

### Latency Optimizations
1. **Pre-warmed HTTP connections**: Keep-alive to clob.polymarket.com
2. **Pre-computed signatures**: Pre-sign order templates, fill in price/size at send time
3. **Zero-copy deserialization**: Use `serde` with borrowed data where possible
4. **Ring buffers**: Fixed-size arrays for price history (no heap allocation in hot path)
5. **SIMD for math**: Use `packed_simd` or `std::simd` for batch probability calculations
6. **Batched orders**: Always prefer `POST /orders` over multiple `POST /order`

---

## Arb Scanner Frequency

The arb scanner must run on every Polymarket book update:
- **Polymarket book updates**: ~10-50 per second per market
- **Scanner frequency**: Every book update tick = ~20-100ms
- **Scanner cost**: O(1) — just check best ask on both sides, sum them
- **Decision latency**: <1ms from book update to arb signal

For cross-market scanning (all 5 market types):
- 5 markets × ~30 updates/sec = ~150 checks/sec
- Each check: 2 price lookups + 1 addition + 1 comparison = ~100ns
- Total: 150 × 100ns = 15μs/sec — **negligible CPU cost**

---

## Safe Execution Logic

```rust
// Pseudocode for safe order submission

async fn safe_submit(order: OrderIntent) -> Result<OrderResult> {
    // 1. Pre-flight checks
    risk_manager.check_exposure(&order)?;
    risk_manager.check_daily_loss()?;
    risk_manager.check_position_limit(&order)?;
    
    // 2. Validate price sanity
    let mid = book.midpoint(order.token);
    if (order.price - mid).abs() > MAX_PRICE_DEVIATION {
        return Err(PriceSanityError);
    }
    
    // 3. Check balance
    let required = order.price * order.size;
    if required > wallet.available_balance() {
        return Err(InsufficientBalance);
    }
    
    // 4. Sign and submit
    let signed = signer.sign_order(&order)?;
    let result = clob_client.post_order(signed).await?;
    
    // 5. Track
    position_manager.record_order(&result);
    pnl_tracker.record_order(&result);
    
    // 6. Monitor fill
    fill_tracker.watch(result.order_id);
    
    Ok(result)
}
```

---

## Key Dependencies (Cargo.toml)

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = "0.24"          # WebSocket client
reqwest = { version = "0.12", features = ["json"] }  # HTTP client
serde = { version = "1", features = ["derive"] }
serde_json = "1"
ethers = "2"                         # Ethereum signing (EIP-712)
chrono = "0.4"
tracing = "0.1"                      # Structured logging
tracing-subscriber = "0.3"
dashmap = "6"                        # Concurrent hashmap
crossbeam = "0.8"                    # Lock-free channels
statrs = "0.17"                      # Normal CDF (Φ function)
rust_decimal = "1"                   # Precise decimal arithmetic
dotenv = "0.15"
anyhow = "1"
thiserror = "2"
```

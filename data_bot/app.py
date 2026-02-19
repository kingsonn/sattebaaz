"""
BTC 5-min Polymarket Data Collector + Live Dashboard.

Run:  python app.py
Opens dashboard at http://localhost:8050
"""

import asyncio
import sqlite3
import logging
import sys
import os
from pathlib import Path

from contextlib import asynccontextmanager

from fastapi import FastAPI, Query, Request
from fastapi.responses import HTMLResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
import uvicorn

from collector import PriceCollector

# ── Logging ─────────────────────────────────────────────────────────

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(name)-10s] %(levelname)-5s  %(message)s",
    datefmt="%H:%M:%S",
    handlers=[
        logging.StreamHandler(sys.stdout),
        logging.FileHandler("data_collector.log", mode="a"),
    ],
)
# Suppress noisy httpx request logging
logging.getLogger("httpx").setLevel(logging.WARNING)
logging.getLogger("httpcore").setLevel(logging.WARNING)
logger = logging.getLogger("app")

# ── Config ──────────────────────────────────────────────────────────

DB_PATH = os.environ.get("DB_PATH", "btc_5m_data.db")
PORT = int(os.environ.get("PORT", "8050"))

# ── Collector instance (shared with web) ────────────────────────────

collector = PriceCollector(db_path=DB_PATH)

# ── FastAPI ─────────────────────────────────────────────────────────

@asynccontextmanager
async def lifespan(app):
    logger.info(f"Dashboard at http://localhost:{PORT}")
    logger.info(f"SQLite DB: {DB_PATH}")
    asyncio.create_task(collector.run())
    yield

app = FastAPI(title="BTC 5m Data Collector", lifespan=lifespan)

TEMPLATE_DIR = Path(__file__).parent / "templates"


def _db():
    conn = sqlite3.connect(DB_PATH)
    conn.row_factory = sqlite3.Row
    return conn


# ── API routes ──────────────────────────────────────────────────────

@app.get("/", response_class=HTMLResponse)
async def dashboard():
    html_path = TEMPLATE_DIR / "dashboard.html"
    return HTMLResponse(html_path.read_text(encoding="utf-8"))


@app.get("/api/stats")
async def api_stats(market_type: str = Query("5m", pattern="^(5m|15m)$")):
    stats = collector.get_stats(market_type)
    # Add currently active market info
    active = []
    import time
    for slug, m in list(collector.markets.items()):
        if m.get("market_type", "5m") != market_type:
            continue
        remaining = m["close_ts"] - int(time.time())
        yes_bid, yes_ask = collector._best_prices(m["yes_token_id"])
        no_bid, no_ask = collector._best_prices(m["no_token_id"])
        active.append({
            "slug": slug,
            "remaining_secs": max(0, remaining),
            "yes_bid": yes_bid,
            "yes_ask": yes_ask,
            "no_bid": no_bid,
            "no_ask": no_ask,
            "yes_mid": round((yes_bid + yes_ask) / 2, 4) if yes_bid and yes_ask else None,
            "no_mid": round((no_bid + no_ask) / 2, 4) if no_bid and no_ask else None,
        })
    stats["active_details"] = active
    return stats


@app.get("/api/markets")
async def api_markets(market_type: str = Query("5m", pattern="^(5m|15m)$")):
    conn = _db()
    rows = conn.execute(
        "SELECT *, (SELECT COUNT(*) FROM price_ticks WHERE market_slug=m.slug) as tick_count "
        "FROM markets m WHERE m.market_type=? ORDER BY open_timestamp DESC",
        (market_type,),
    ).fetchall()
    markets = []
    for row in rows:
        m = dict(row)
        m["winner"] = None
        m["yes_open"] = None
        m["no_open"] = None
        m["yes_close"] = None
        m["no_close"] = None
        m["yes_min"] = None
        m["no_min"] = None
        m["strategy1"] = None
        m["strategy2"] = None
        m["strategy3"] = None
        m["strategy4"] = None
        m["strategy5"] = None

        if m["resolved"] == 1:
            slug = m["slug"]
            ticks = conn.execute(
                "SELECT yes_mid, no_mid FROM price_ticks "
                "WHERE market_slug=? AND yes_mid IS NOT NULL AND no_mid IS NOT NULL "
                "ORDER BY epoch_ms",
                (slug,),
            ).fetchall()

            if ticks:
                yes_series = [t["yes_mid"] for t in ticks if t["yes_mid"] is not None]
                no_series = [t["no_mid"] for t in ticks if t["no_mid"] is not None]

                yes_open = yes_series[0] if yes_series else None
                no_open = no_series[0] if no_series else None
                yes_close = yes_series[-1] if yes_series else None
                no_close = no_series[-1] if no_series else None
                yes_min = min(yes_series) if yes_series else None
                no_min = min(no_series) if no_series else None
                yes_max = max(yes_series) if yes_series else None
                no_max = max(no_series) if no_series else None

                winner = "yes" if (yes_close or 0) >= (no_close or 0) else "no"

                m["winner"] = winner
                m["yes_open"] = yes_open
                m["no_open"] = no_open
                m["yes_close"] = yes_close
                m["no_close"] = no_close
                m["yes_min"] = yes_min
                m["no_min"] = no_min

                # Strategy 1:
                # - Default: lower open-price side should win.
                # - If any side opens >= 0.53, use that side as the winning expectation.
                threshold_side = None
                if yes_open is not None and yes_open >= 0.53:
                    threshold_side = "yes"
                if no_open is not None and no_open >= 0.53:
                    if threshold_side is None or (yes_open is not None and no_open > yes_open):
                        threshold_side = "no"

                if threshold_side is not None:
                    m["strategy1"] = f"{'won' if winner == threshold_side else 'lost'}-1"
                else:
                    lower_open_side = "yes" if (yes_open or 1) <= (no_open or 1) else "no"
                    m["strategy1"] = "won" if winner == lower_open_side else "lost"

                # Strategy 2:
                # If a side touches <= 0.05 and that side wins -> win, else lost.
                yes_touched = yes_min is not None and yes_min <= 0.05
                no_touched = no_min is not None and no_min <= 0.05
                if yes_touched or no_touched:
                    winner_touched = (winner == "yes" and yes_touched) or (winner == "no" and no_touched)
                    m["strategy2"] = "won" if winner_touched else "lost"

                # Strategy 3:
                # If no open side is >= 0.53, both YES and NO must touch <= 0.48 at least once.
                if (yes_open is not None and no_open is not None
                        and yes_open < 0.53 and no_open < 0.53):
                    yes_touched_048 = yes_min is not None and yes_min <= 0.48
                    no_touched_048 = no_min is not None and no_min <= 0.48
                    m["strategy3"] = "won" if (yes_touched_048 and no_touched_048) else "lost"

                # Strategy 4:
                # Any option that touches <=0.35 and then later reaches >=0.70 => won.
                # If touches <=0.35 but never reaches >=0.70 after that => lost.
                # If no valid <=0.35-first pattern exists => blank.
                def _strategy4_side(series: list[float]) -> str | None:
                    if not series:
                        return None
                    first_035 = next((i for i, v in enumerate(series) if v <= 0.35), None)
                    first_070 = next((i for i, v in enumerate(series) if v >= 0.70), None)

                    # Ignore reversed order cases: 0.70 appears before any 0.35 touch.
                    if first_070 is not None and (first_035 is None or first_070 < first_035):
                        return None
                    if first_035 is None:
                        return None

                    has_070_after_035 = any(v >= 0.70 for v in series[first_035 + 1:])
                    return "won" if has_070_after_035 else "lost"

                yes_s4 = _strategy4_side(yes_series)
                no_s4 = _strategy4_side(no_series)
                if yes_s4 == "won" or no_s4 == "won":
                    m["strategy4"] = "won"
                elif yes_s4 == "lost" or no_s4 == "lost":
                    m["strategy4"] = "lost"

                # Strategy 5 (15m only):
                # From 700s onwards, whichever side first reaches >= 0.66 is the "signal side".
                # If that side resolves as winner -> won, else -> lost. Blank if neither reaches 0.66.
                if m.get("market_type", "5m") == "15m":
                    ticks_with_elapsed = conn.execute(
                        "SELECT yes_mid, no_mid, seconds_elapsed FROM price_ticks "
                        "WHERE market_slug=? AND seconds_elapsed >= 700 "
                        "AND (yes_mid IS NOT NULL OR no_mid IS NOT NULL) "
                        "ORDER BY epoch_ms",
                        (slug,),
                    ).fetchall()
                    signal_side = None
                    signal_value = None
                    for tick in ticks_with_elapsed:
                        y = tick["yes_mid"]
                        n = tick["no_mid"]
                        if signal_side is None:
                            if y is not None and y >= 0.66:
                                signal_side = "yes"
                                signal_value = y
                            elif n is not None and n >= 0.66:
                                signal_side = "no"
                                signal_value = n
                    if signal_side is not None:
                        m["strategy5"] = "won" if winner == signal_side else "lost"
                        if winner == signal_side:
                            m["strategy5_signal_value"] = max(signal_value, 0.66)

        markets.append(m)

    conn.close()
    return markets


@app.delete("/api/market/{slug}")
async def api_delete_market(slug: str):
    conn = _db()
    conn.execute("DELETE FROM price_ticks WHERE market_slug=?", (slug,))
    conn.execute("DELETE FROM markets WHERE slug=?", (slug,))
    conn.commit()
    conn.close()
    return {"deleted": slug}


@app.get("/api/market/{slug}/ticks")
async def api_market_ticks(slug: str):
    conn = _db()
    rows = conn.execute(
        "SELECT seconds_elapsed, yes_best_bid, yes_best_ask, no_best_bid, no_best_ask, "
        "yes_mid, no_mid, timestamp, epoch_ms "
        "FROM price_ticks WHERE market_slug=? ORDER BY epoch_ms",
        (slug,),
    ).fetchall()
    conn.close()
    return [dict(r) for r in rows]


@app.get("/api/market/{slug}/summary")
async def api_market_summary(slug: str):
    """Min/max/open/close prices for a single market."""
    conn = _db()
    row = conn.execute("""
        SELECT
            MIN(yes_mid) as yes_min, MAX(yes_mid) as yes_max,
            MIN(no_mid)  as no_min,  MAX(no_mid)  as no_max,
            COUNT(*)     as tick_count
        FROM price_ticks WHERE market_slug=? AND yes_mid IS NOT NULL
    """, (slug,)).fetchone()

    first = conn.execute(
        "SELECT yes_mid, no_mid FROM price_ticks "
        "WHERE market_slug=? AND yes_mid IS NOT NULL ORDER BY epoch_ms LIMIT 1",
        (slug,),
    ).fetchone()
    last = conn.execute(
        "SELECT yes_mid, no_mid FROM price_ticks "
        "WHERE market_slug=? AND yes_mid IS NOT NULL ORDER BY epoch_ms DESC LIMIT 1",
        (slug,),
    ).fetchone()
    conn.close()

    return {
        "slug": slug,
        "yes_min": row["yes_min"], "yes_max": row["yes_max"],
        "no_min": row["no_min"], "no_max": row["no_max"],
        "yes_open": first["yes_mid"] if first else None,
        "yes_close": last["yes_mid"] if last else None,
        "no_open": first["no_mid"] if first else None,
        "no_close": last["no_mid"] if last else None,
        "tick_count": row["tick_count"],
    }


@app.get("/api/query")
async def api_price_query(
    side: str = Query("yes", pattern="^(yes|no)$"),
    op: str = Query("gte", pattern="^(lte|gte|eq)$"),
    value: float = Query(0.5),
    price_type: str = Query("mid", pattern="^(mid|bid|ask)$"),
    market_type: str = Query("5m", pattern="^(5m|15m)$"),
):
    """
    Query: In how many RESOLVED markets did the YES/NO price reach a threshold?

    Example: /api/query?side=yes&op=gte&value=0.60
    → "In how many markets did yes_mid ever reach >= 0.60?"
    """
    col_map = {
        ("yes", "mid"): "yes_mid",
        ("yes", "bid"): "yes_best_bid",
        ("yes", "ask"): "yes_best_ask",
        ("no", "mid"): "no_mid",
        ("no", "bid"): "no_best_bid",
        ("no", "ask"): "no_best_ask",
    }
    col = col_map.get((side, price_type), "yes_mid")
    op_sql = {"lte": "<=", "gte": ">=", "eq": "="}[op]

    conn = _db()
    # Total resolved markets
    total = conn.execute(
        "SELECT COUNT(*) as c FROM markets WHERE resolved=1 AND market_type=?", (market_type,)
    ).fetchone()["c"]

    # Markets where the price condition was met at least once
    matching = conn.execute(f"""
        SELECT COUNT(DISTINCT pt.market_slug) as c
        FROM price_ticks pt
        JOIN markets m ON m.slug = pt.market_slug
        WHERE m.resolved = 1 AND m.market_type = ? AND pt.{col} {op_sql} ?
    """, (market_type, value)).fetchone()["c"]

    # Also get the list of matching slugs
    slugs = conn.execute(f"""
        SELECT DISTINCT pt.market_slug
        FROM price_ticks pt
        JOIN markets m ON m.slug = pt.market_slug
        WHERE m.resolved = 1 AND m.market_type = ? AND pt.{col} {op_sql} ?
        ORDER BY m.open_timestamp DESC
    """, (market_type, value)).fetchall()
    conn.close()

    return {
        "query": f"{side}_{price_type} {op_sql} {value}",
        "matching_markets": matching,
        "total_resolved": total,
        "percentage": round(matching / total * 100, 1) if total > 0 else 0,
        "matching_slugs": [r["market_slug"] for r in slugs],
    }


@app.get("/api/price_distribution")
async def api_price_distribution(
    side: str = Query("yes", pattern="^(yes|no)$"),
    buckets: int = Query(20),
    market_type: str = Query("5m", pattern="^(5m|15m)$"),
):
    """
    Distribution of min/max prices across all resolved markets.
    For each market: what was the min and max YES/NO mid price?
    """
    col = "yes_mid" if side == "yes" else "no_mid"
    conn = _db()
    rows = conn.execute(f"""
        SELECT pt.market_slug,
               MIN(pt.{col}) as price_min,
               MAX(pt.{col}) as price_max
        FROM price_ticks pt
        JOIN markets m ON m.slug = pt.market_slug
        WHERE m.resolved = 1 AND m.market_type = ? AND pt.{col} IS NOT NULL
        GROUP BY pt.market_slug
    """, (market_type,)).fetchall()
    conn.close()

    markets_data = [{"slug": r["market_slug"], "min": r["price_min"], "max": r["price_max"]}
                    for r in rows]

    # Build histogram buckets
    step = 1.0 / buckets
    histogram = []
    for i in range(buckets):
        low = round(i * step, 4)
        high = round((i + 1) * step, 4)
        # Count markets where the min price fell in this bucket
        count_min = sum(1 for m in markets_data if low <= (m["min"] or 0) < high)
        count_max = sum(1 for m in markets_data if low <= (m["max"] or 0) < high)
        histogram.append({
            "range": f"{low:.2f}-{high:.2f}",
            "low": low, "high": high,
            "count_reached_min": count_min,
            "count_reached_max": count_max,
        })

    return {"side": side, "total_markets": len(markets_data),
            "histogram": histogram, "markets": markets_data}


@app.get("/api/live_ticks")
async def api_live_ticks(since_ms: int = Query(0)):
    """Get ticks since a given epoch_ms (for live polling)."""
    conn = _db()
    rows = conn.execute(
        "SELECT market_slug, seconds_elapsed, yes_mid, no_mid, epoch_ms, timestamp "
        "FROM price_ticks WHERE epoch_ms > ? ORDER BY epoch_ms LIMIT 500",
        (since_ms,),
    ).fetchall()
    conn.close()
    return [dict(r) for r in rows]


# ── Main ────────────────────────────────────────────────────────────

if __name__ == "__main__":
    uvicorn.run("app:app", host="0.0.0.0", port=PORT, log_level="warning")

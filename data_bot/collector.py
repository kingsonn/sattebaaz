"""
Polymarket BTC 5-min and 15-min market data collector.

Connects to Polymarket REST + WebSocket, discovers 5-min and 15-min BTC up/down markets,
and saves all YES/NO price updates to SQLite.

Primary data source: REST polling every 1s (reliable).
Secondary data source: WebSocket for real-time deltas (bonus granularity).
"""

import asyncio
import time
import json
import logging
import sqlite3
from datetime import datetime, timezone

import httpx
import websockets

GAMMA_API = "https://gamma-api.polymarket.com"
CLOB_API = "https://clob.polymarket.com"
WS_URL = "wss://ws-subscriptions-clob.polymarket.com/ws/market"

logger = logging.getLogger("collector")


class PriceCollector:
    def __init__(self, db_path: str = "btc_5m_data.db"):
        self.db_path = db_path
        self.markets: dict = {}          # slug -> market info dict
        self.books: dict = {}            # token_id -> {bids: {price: size}, asks: {price: size}}
        self.token_to_slug: dict = {}    # token_id -> slug
        self.token_to_side: dict = {}    # token_id -> "yes" | "no"
        self.first_slug_seen: str | None = None
        self._running = False
        self._http: httpx.AsyncClient | None = None
        # Track last saved prices to avoid duplicate identical ticks
        self._last_saved: dict = {}      # slug -> (yes_bid, yes_ask, no_bid, no_ask)
        self._ws_msg_count = 0
        self._ws_tick_count = 0
        self._rest_tick_count = 0
        self._init_db()

    # ── Database setup ──────────────────────────────────────────────

    def _init_db(self):
        conn = sqlite3.connect(self.db_path)
        conn.execute("PRAGMA journal_mode=WAL")
        c = conn.cursor()
        c.execute("""CREATE TABLE IF NOT EXISTS markets (
            slug            TEXT PRIMARY KEY,
            yes_token_id    TEXT,
            no_token_id     TEXT,
            open_timestamp  INTEGER,
            close_timestamp INTEGER,
            resolved        INTEGER DEFAULT 0,
            market_type     TEXT DEFAULT '5m'
        )""")
        # Add market_type column if upgrading from old schema
        try:
            c.execute("ALTER TABLE markets ADD COLUMN market_type TEXT DEFAULT '5m'")
        except sqlite3.OperationalError:
            pass  # column already exists
        # Backfill market_type from slug pattern (idempotent)
        c.execute("UPDATE markets SET market_type='5m' WHERE slug LIKE 'btc-updown-5m-%'")
        c.execute("UPDATE markets SET market_type='15m' WHERE slug LIKE 'btc-updown-15m-%'")
        c.execute("""CREATE TABLE IF NOT EXISTS price_ticks (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            market_slug     TEXT NOT NULL,
            timestamp       TEXT NOT NULL,
            epoch_ms        INTEGER NOT NULL,
            seconds_elapsed REAL,
            yes_best_bid    REAL,
            yes_best_ask    REAL,
            no_best_bid     REAL,
            no_best_ask     REAL,
            yes_mid         REAL,
            no_mid          REAL,
            source          TEXT DEFAULT 'rest',
            FOREIGN KEY (market_slug) REFERENCES markets(slug)
        )""")
        c.execute("CREATE INDEX IF NOT EXISTS idx_ticks_slug  ON price_ticks(market_slug)")
        c.execute("CREATE INDEX IF NOT EXISTS idx_ticks_epoch ON price_ticks(epoch_ms)")
        # Add source column if upgrading from old schema
        try:
            c.execute("ALTER TABLE price_ticks ADD COLUMN source TEXT DEFAULT 'rest'")
        except sqlite3.OperationalError:
            pass  # column already exists
        conn.commit()
        conn.close()

    # ── Slug helpers ────────────────────────────────────────────────

    @staticmethod
    def _slug_for_ts(ts: int, market_type: str = "5m") -> tuple[str, int]:
        interval = 300 if market_type == "5m" else 900
        interval_start = (ts // interval) * interval
        return f"btc-updown-{market_type}-{interval_start}", interval_start

    def _current_slug(self, market_type: str = "5m") -> tuple[str, int]:
        return self._slug_for_ts(int(time.time()), market_type)

    # ── Gamma API ───────────────────────────────────────────────────

    async def resolve_market(self, slug: str) -> dict | None:
        """Resolve a market slug to YES/NO token IDs via Gamma API."""
        try:
            resp = await self._http.get(f"{GAMMA_API}/markets?slug={slug}")
            if resp.status_code != 200:
                return None
            data = resp.json()
            if not data:
                return None
            info = data[0]

            yes_id, no_id = None, None

            # Method 1: tokens array
            for t in info.get("tokens", []):
                outcome = t.get("outcome", "")
                if outcome in ("Yes", "Up"):
                    yes_id = t.get("token_id")
                elif outcome in ("No", "Down"):
                    no_id = t.get("token_id")

            # Method 2: clobTokenIds + outcomes (JSON-encoded strings)
            if not yes_id or not no_id:
                raw_ids = info.get("clobTokenIds", "[]")
                raw_out = info.get("outcomes", "[]")
                clob_ids = json.loads(raw_ids) if isinstance(raw_ids, str) else raw_ids
                outcomes = json.loads(raw_out) if isinstance(raw_out, str) else raw_out
                if len(clob_ids) >= 2 and len(outcomes) >= 2:
                    for i, o in enumerate(outcomes):
                        if o in ("Up", "Yes"):
                            yes_id = clob_ids[i]
                        elif o in ("Down", "No"):
                            no_id = clob_ids[i]

            if not yes_id or not no_id:
                return None
            return {"yes_token_id": yes_id, "no_token_id": no_id}
        except Exception as e:
            logger.debug(f"resolve_market({slug}): {e}")
            return None

    async def _fetch_book_rest(self, token_id: str) -> dict | None:
        """Fetch full order-book snapshot via REST and replace in-memory book."""
        try:
            resp = await self._http.get(f"{CLOB_API}/book?token_id={token_id}")
            if resp.status_code != 200:
                return None
            data = resp.json()
            # Full replacement — REST gives the complete book, not a delta
            new_bids = {}
            for level in data.get("bids", []):
                p, s = float(level["price"]), float(level["size"])
                if s > 0:
                    new_bids[p] = s
            new_asks = {}
            for level in data.get("asks", []):
                p, s = float(level["price"]), float(level["size"])
                if s > 0:
                    new_asks[p] = s
            self.books[token_id] = {"bids": new_bids, "asks": new_asks}
            return data
        except Exception as e:
            logger.debug(f"fetch_book error: {e}")
            return None

    # ── Book management ─────────────────────────────────────────────

    def _apply_book_delta(self, token_id: str, bids: list, asks: list):
        """Apply WS delta update to in-memory book."""
        if token_id not in self.books:
            self.books[token_id] = {"bids": {}, "asks": {}}
        book = self.books[token_id]
        for level in (bids or []):
            p, s = float(level["price"]), float(level["size"])
            if s == 0:
                book["bids"].pop(p, None)
            else:
                book["bids"][p] = s
        for level in (asks or []):
            p, s = float(level["price"]), float(level["size"])
            if s == 0:
                book["asks"].pop(p, None)
            else:
                book["asks"][p] = s

    def _best_prices(self, token_id: str) -> tuple[float | None, float | None]:
        book = self.books.get(token_id, {"bids": {}, "asks": {}})
        best_bid = max(book["bids"].keys()) if book["bids"] else None
        best_ask = min(book["asks"].keys()) if book["asks"] else None
        return best_bid, best_ask

    # ── Persistence ─────────────────────────────────────────────────

    def _save_market(self, slug, yes_id, no_id, open_ts, close_ts, market_type="5m"):
        conn = sqlite3.connect(self.db_path)
        conn.execute(
            "INSERT OR IGNORE INTO markets (slug, yes_token_id, no_token_id, open_timestamp, close_timestamp, resolved, market_type) VALUES (?,?,?,?,?,0,?)",
            (slug, yes_id, no_id, open_ts, close_ts, market_type),
        )
        conn.commit()
        conn.close()

    def _save_tick(self, slug: str, source: str = "rest", force: bool = False):
        """Save current best prices for a market. Skips if prices unchanged (unless force)."""
        market = self.markets.get(slug)
        if not market:
            return False
        yes_bid, yes_ask = self._best_prices(market["yes_token_id"])
        no_bid, no_ask = self._best_prices(market["no_token_id"])
        if yes_bid is None and yes_ask is None and no_bid is None and no_ask is None:
            return False

        # Dedup: skip if prices are identical to last save (unless forced)
        key = (yes_bid, yes_ask, no_bid, no_ask)
        if not force and self._last_saved.get(slug) == key:
            return False
        self._last_saved[slug] = key

        now = datetime.now(timezone.utc)
        epoch_ms = int(now.timestamp() * 1000)
        elapsed = time.time() - market["open_ts"]

        yes_mid = round((yes_bid + yes_ask) / 2, 6) if yes_bid is not None and yes_ask is not None else None
        no_mid = round((no_bid + no_ask) / 2, 6) if no_bid is not None and no_ask is not None else None

        conn = sqlite3.connect(self.db_path)
        conn.execute(
            """INSERT INTO price_ticks
               (market_slug, timestamp, epoch_ms, seconds_elapsed,
                yes_best_bid, yes_best_ask, no_best_bid, no_best_ask, yes_mid, no_mid, source)
               VALUES (?,?,?,?,?,?,?,?,?,?,?)""",
            (slug, now.isoformat(), epoch_ms, round(elapsed, 2),
             yes_bid, yes_ask, no_bid, no_ask, yes_mid, no_mid, source),
        )
        conn.commit()
        conn.close()
        return True

    def _mark_resolved(self, slug: str):
        conn = sqlite3.connect(self.db_path)
        conn.execute("UPDATE markets SET resolved = 1 WHERE slug = ?", (slug,))
        conn.commit()
        conn.close()

    # ── Public stats ────────────────────────────────────────────────

    def get_stats(self, market_type: str = "5m") -> dict:
        conn = sqlite3.connect(self.db_path)
        c = conn.cursor()
        total = c.execute("SELECT COUNT(*) FROM markets WHERE market_type=?", (market_type,)).fetchone()[0]
        resolved = c.execute("SELECT COUNT(*) FROM markets WHERE resolved=1 AND market_type=?", (market_type,)).fetchone()[0]
        ticks = c.execute(
            "SELECT COUNT(*) FROM price_ticks pt JOIN markets m ON m.slug=pt.market_slug WHERE m.market_type=?",
            (market_type,)
        ).fetchone()[0]
        conn.close()
        return {
            "total_markets": total,
            "resolved_markets": resolved,
            "active_markets": total - resolved,
            "total_ticks": ticks,
        }

    # ── Main loop ───────────────────────────────────────────────────

    async def run(self):
        self._running = True
        self._http = httpx.AsyncClient(
            timeout=10,
            limits=httpx.Limits(max_connections=20, max_keepalive_connections=10),
        )
        for mtype in ("5m", "15m"):
            current_slug, current_start = self._current_slug(mtype)
            interval = 300 if mtype == "5m" else 900
            remaining = (current_start + interval) - int(time.time())
            logger.info(f"[{mtype}] Current market: {current_slug}  (skipping — already in progress)")
            logger.info(f"[{mtype}] Next market starts in ~{remaining}s — will begin recording then")
        self.first_slug_5m = self._current_slug("5m")[0]
        self.first_slug_15m = self._current_slug("15m")[0]

        await asyncio.gather(
            self._discovery_loop("5m"),
            self._discovery_loop("15m"),
            self._rest_poll_loop(),
            self._ws_loop(),
        )

    # ── Discovery loop ──────────────────────────────────────────────

    async def _discovery_loop(self, market_type: str = "5m"):
        interval = 300 if market_type == "5m" else 900
        first_slug = self.first_slug_5m if market_type == "5m" else self.first_slug_15m

        while self._running:
            try:
                slug, interval_start = self._current_slug(market_type)

                # Skip the market that was running on startup
                if slug == first_slug:
                    await asyncio.sleep(3)
                    continue

                if slug not in self.markets:
                    info = await self.resolve_market(slug)
                    if info:
                        close_ts = interval_start + interval
                        self.markets[slug] = {
                            "yes_token_id": info["yes_token_id"],
                            "no_token_id": info["no_token_id"],
                            "open_ts": interval_start,
                            "close_ts": close_ts,
                            "market_type": market_type,
                        }
                        self.token_to_slug[info["yes_token_id"]] = slug
                        self.token_to_slug[info["no_token_id"]] = slug
                        self.token_to_side[info["yes_token_id"]] = "yes"
                        self.token_to_side[info["no_token_id"]] = "no"
                        self._save_market(slug, info["yes_token_id"], info["no_token_id"],
                                          interval_start, close_ts, market_type)

                        # Fetch initial books via REST
                        for tid in [info["yes_token_id"], info["no_token_id"]]:
                            await self._fetch_book_rest(tid)

                        # Save first tick immediately
                        self._save_tick(slug, source="rest", force=True)

                        logger.info(f"=== NEW {market_type.upper()} MARKET: {slug} ===")
                        logger.info(f"    YES token: {info['yes_token_id'][:20]}...")
                        logger.info(f"    NO  token: {info['no_token_id'][:20]}...")
                        yes_bid, yes_ask = self._best_prices(info["yes_token_id"])
                        no_bid, no_ask = self._best_prices(info["no_token_id"])
                        logger.info(f"    YES: bid={yes_bid} ask={yes_ask}")
                        logger.info(f"    NO:  bid={no_bid} ask={no_ask}")

                # Expire old markets of this type (30s grace after close)
                now_ts = int(time.time())
                expired = [s for s, m in list(self.markets.items())
                           if m.get("market_type", "5m") == market_type
                           and now_ts > m["close_ts"] + 30]
                for s in expired:
                    m = self.markets.pop(s)
                    for tid in [m["yes_token_id"], m["no_token_id"]]:
                        self.token_to_slug.pop(tid, None)
                        self.token_to_side.pop(tid, None)
                        self.books.pop(tid, None)
                    self._last_saved.pop(s, None)
                    self._mark_resolved(s)
                    logger.info(f"=== RESOLVED [{market_type}]: {s} ===")

            except Exception as e:
                logger.error(f"Discovery error [{market_type}]: {e}")

            await asyncio.sleep(3)

    # ── REST polling loop (PRIMARY data source) ─────────────────────

    async def _rest_poll_loop(self):
        """Poll order books via REST every 1s for all active markets."""
        while self._running:
            try:
                for slug, market in list(self.markets.items()):
                    # Fetch both books concurrently
                    yes_task = self._fetch_book_rest(market["yes_token_id"])
                    no_task = self._fetch_book_rest(market["no_token_id"])
                    await asyncio.gather(yes_task, no_task)

                    # Save tick (dedup handles identical prices)
                    if self._save_tick(slug, source="rest"):
                        self._rest_tick_count += 1

            except Exception as e:
                logger.error(f"REST poll error: {e}")

            await asyncio.sleep(1)

    # ── WebSocket loop (SECONDARY — bonus granularity) ──────────────

    async def _ws_loop(self):
        subscribed: set[str] = set()

        while self._running:
            try:
                logger.info("Connecting to Polymarket WS …")
                async with websockets.connect(WS_URL, ping_interval=30,
                                               close_timeout=5) as ws:
                    logger.info("Polymarket WS connected")
                    msg_count_at_connect = 0

                    while self._running:
                        # Subscribe to any new tokens
                        current_tids = set()
                        for m in self.markets.values():
                            current_tids.add(m["yes_token_id"])
                            current_tids.add(m["no_token_id"])

                        for tid in current_tids - subscribed:
                            sub_msg = json.dumps({
                                "auth": {},
                                "type": "subscribe",
                                "channel": "market",
                                "assets_ids": [tid],
                            })
                            await ws.send(sub_msg)
                            subscribed.add(tid)
                            side = self.token_to_side.get(tid, "?")
                            logger.info(f"  WS subscribed: {side.upper()} {tid[:20]}…")

                        # Drop stale subscriptions from tracking
                        subscribed &= current_tids

                        try:
                            raw = await asyncio.wait_for(ws.recv(), timeout=1.0)
                            # websockets v16 can return str or bytes
                            text = raw if isinstance(raw, str) else raw.decode("utf-8", errors="replace")
                            self._ws_msg_count += 1
                            msg_count_at_connect += 1

                            # Debug: log first few messages and every 1000th
                            if msg_count_at_connect <= 3 or self._ws_msg_count % 1000 == 0:
                                snippet = text[:200] if len(text) > 200 else text
                                logger.debug(f"WS msg #{self._ws_msg_count}: {snippet}")

                            self._handle_ws_msg(text)

                        except asyncio.TimeoutError:
                            continue
                        except websockets.ConnectionClosed:
                            logger.warning("WS connection closed")
                            break

            except Exception as e:
                logger.error(f"WS error: {e}")

            subscribed.clear()
            logger.info("WS reconnecting in 3s …")
            await asyncio.sleep(3)

    def _handle_ws_msg(self, text: str):
        try:
            data = json.loads(text)
        except json.JSONDecodeError:
            return

        updates = data if isinstance(data, list) else [data]
        for upd in updates:
            if not isinstance(upd, dict):
                continue
            asset_id = upd.get("asset_id")
            if not asset_id:
                continue
            slug = self.token_to_slug.get(asset_id)
            if not slug:
                continue
            bids = upd.get("bids", [])
            asks = upd.get("asks", [])
            if bids or asks:
                self._apply_book_delta(asset_id, bids, asks)
                if self._save_tick(slug, source="ws"):
                    self._ws_tick_count += 1

    

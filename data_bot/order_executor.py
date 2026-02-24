"""
Order Execution Layer for Polymarket BTC markets.

State machine:
  IDLE → WAITING_FOR_MARKET → ORDERS_PLACED → POSITION_HELD → RESOLVING → REDEEMING → IDLE

Uses py-clob-client for CLOB operations and py-builder-relayer-client for gasless redemption.
"""

import asyncio
import logging
import os
import time
from enum import Enum
from dataclasses import dataclass, field
from typing import Optional

from dotenv import load_dotenv
from py_clob_client.client import ClobClient
from py_clob_client.clob_types import OrderArgs, PartialCreateOrderOptions
from py_builder_relayer_client.client import RelayClient
from py_builder_signing_sdk.config import BuilderConfig, BuilderApiKeyCreds
from poly_web3 import PolyWeb3Service, RELAYER_URL as POLY_RELAYER_URL

load_dotenv()

logger = logging.getLogger("executor")

# ── Constants ────────────────────────────────────────────────────────

CLOB_HOST = "https://clob.polymarket.com"
GAMMA_API = "https://gamma-api.polymarket.com"
CHAIN_ID = 137

ORDER_PRICE = 0.95
TICK_SIZE = "0.01"


class BotState(str, Enum):
    IDLE = "idle"
    WAITING_FOR_MARKET = "waiting_for_market"
    MONITORING_PRICES = "monitoring_prices"
    ORDER_PLACED = "order_placed"
    POSITION_HELD = "position_held"
    RESOLVING = "resolving"
    REDEEMING = "redeeming"


@dataclass
class CycleInfo:
    """Tracks state for one market cycle."""
    market_slug: str = ""
    yes_token_id: str = ""
    no_token_id: str = ""
    condition_id: str = ""
    open_ts: int = 0
    close_ts: int = 0
    neg_risk: bool = False

    # Order tracking (single order — placed only when ask >= target price)
    order_id: Optional[str] = None
    order_side: Optional[str] = None  # "yes" or "no" — which side was ordered
    filled_side: Optional[str] = None  # "yes" or "no"
    shares: float = 0.0

    # Timing
    order_placed_at: float = 0.0


class OrderExecutor:
    """
    Manages the full lifecycle of order execution per market cycle.
    Controlled by the frontend via start/stop.
    """

    def __init__(self, collector):
        self.collector = collector
        self.state = BotState.IDLE
        self.market_type = "5m"
        self.share_size = 5.0
        self._running = False
        self._stop_requested = False
        self._task: Optional[asyncio.Task] = None
        self.cycle: Optional[CycleInfo] = None
        self.logs: list[str] = []
        self._max_logs = 100

        # CLOB client
        self._clob: Optional[ClobClient] = None
        # Relayer client for gasless redemption
        self._relay: Optional[RelayClient] = None

        # Track which slug was active when Start was pressed (to skip it)
        self._skip_slug: Optional[str] = None

    # ── Logging ────────────────────────────────────────────────────

    def _log(self, msg: str):
        ts = time.strftime("%H:%M:%S")
        entry = f"[{ts}] {msg}"
        logger.info(msg)
        self.logs.append(entry)
        if len(self.logs) > self._max_logs:
            self.logs = self.logs[-self._max_logs:]

    # ── Public control methods ─────────────────────────────────────

    def start(self, market_type: str, share_size: float):
        if self._running:
            return {"error": "Bot is already running"}
        self.market_type = market_type
        self.share_size = max(5.0, share_size)
        self._running = True
        self._stop_requested = False
        self.state = BotState.WAITING_FOR_MARKET
        self.cycle = None
        self.logs = []

        # Record current market slug to skip it
        current_slug, _ = self.collector._current_slug(market_type)
        self._skip_slug = current_slug

        self._log(f"Bot started: {market_type}, {self.share_size} shares, skip={current_slug}")

        # Initialize CLOB client
        self._init_clob()
        self._init_relay()

        # Launch background task
        self._task = asyncio.create_task(self._run_loop())
        return {"status": "started"}

    def stop(self):
        if not self._running:
            return {"error": "Bot is not running"}
        self._stop_requested = True
        self._log("Stop requested -- will finish current cycle then stop")
        return {"status": "stopping"}

    def get_status(self) -> dict:
        return {
            "running": self._running,
            "state": self.state.value,
            "market_type": self.market_type,
            "share_size": self.share_size,
            "stop_requested": self._stop_requested,
            "cycle": {
                "slug": self.cycle.market_slug if self.cycle else None,
                "order_side": self.cycle.order_side if self.cycle else None,
                "order_id": self.cycle.order_id if self.cycle else None,
                "filled_side": self.cycle.filled_side if self.cycle else None,
            } if self.cycle else None,
            "logs": self.logs[-20:],
        }

    # ── CLOB initialization ────────────────────────────────────────

    def _init_clob(self):
        private_key = os.environ.get("POLYMARKET_PRIVATE_KEY", "")
        funder = os.environ.get("POLYMARKET_FUNDER_ADDRESS", "")
        sig_type = int(os.environ.get("POLYMARKET_SIGNATURE_TYPE", "1"))

        self._clob = ClobClient(
            host=CLOB_HOST,
            chain_id=CHAIN_ID,
            key=private_key,
            signature_type=sig_type,
            funder=funder,
        )
        # Derive or create API credentials (synchronous — called once at init)
        creds = self._clob.create_or_derive_api_creds()
        self._clob.set_api_creds(creds)
        self._log(f"CLOB authenticated: funder={funder[:10]}...")

    # ── Thread-safe wrappers for synchronous CLOB calls ────────────

    async def _clob_create_and_post_order(self, args, opts):
        return await asyncio.to_thread(self._clob.create_and_post_order, args, opts)

    async def _clob_get_order(self, order_id):
        return await asyncio.to_thread(self._clob.get_order, order_id)

    async def _clob_cancel(self, order_id):
        return await asyncio.to_thread(self._clob.cancel, order_id)

    async def _clob_cancel_all(self):
        return await asyncio.to_thread(self._clob.cancel_all)

    async def _clob_get_fee_rate(self, token_id):
        return await asyncio.to_thread(self._clob.get_fee_rate_bps, token_id)

    def _init_relay(self):
        private_key = os.environ.get("POLYMARKET_PRIVATE_KEY", "")
        funder = os.environ.get("POLYMARKET_FUNDER_ADDRESS", "")
        sig_type = int(os.environ.get("POLYMARKET_SIGNATURE_TYPE", "1"))
        builder_key = os.environ.get("builder_api_key", "")
        builder_secret = os.environ.get("builder_secret", "")
        builder_passphrase = os.environ.get("builder_passphrase", "")

        if not builder_key:
            self._log("WARNING: No builder credentials -- redemption will not work")
            return

        relay_client = RelayClient(
            POLY_RELAYER_URL,
            CHAIN_ID,
            private_key,
            BuilderConfig(local_builder_creds=BuilderApiKeyCreds(
                key=builder_key,
                secret=builder_secret,
                passphrase=builder_passphrase,
            )),
        )
        self._relay = PolyWeb3Service(
            clob_client=self._clob,
            relayer_client=relay_client,
            rpc_url="https://1rpc.io/matic",
        )
        self._log("Relayer client initialized (proxy wallet mode)")

    # ── Main execution loop ────────────────────────────────────────

    async def _run_loop(self):
        # Launch background redemption sweep alongside the trading loop
        sweep_task = asyncio.create_task(self._redemption_sweep_loop())
        try:
            await self._trading_loop()
        finally:
            sweep_task.cancel()
            try:
                await sweep_task
            except asyncio.CancelledError:
                pass

    async def _trading_loop(self):
        try:
            while self._running:
                if self.state == BotState.WAITING_FOR_MARKET:
                    await self._wait_for_new_market()

                elif self.state == BotState.MONITORING_PRICES:
                    await self._monitor_prices()

                elif self.state == BotState.ORDER_PLACED:
                    await self._monitor_order()

                elif self.state == BotState.POSITION_HELD:
                    await self._wait_for_resolution()

                elif self.state == BotState.RESOLVING:
                    await self._handle_resolution()

                elif self.state == BotState.REDEEMING:
                    await self._handle_redemption()

                elif self.state == BotState.IDLE:
                    break

                await asyncio.sleep(2)

        except Exception as e:
            self._log(f"ERROR in run loop: {e}")
            logger.exception("Executor run loop error")
        finally:
            self._running = False
            self.state = BotState.IDLE
            self._log("Bot stopped")

    # ── State: WAITING_FOR_MARKET ──────────────────────────────────

    async def _wait_for_new_market(self):
        """Wait until a fresh market appears that we haven't seen."""
        for slug, m in list(self.collector.markets.items()):
            if m.get("market_type", "5m") != self.market_type:
                continue
            if slug == self._skip_slug:
                continue

            # Found a new market!
            self._log(f"New market detected: {slug}")

            # Get condition_id and neg_risk from Gamma API
            condition_id, neg_risk = await self._get_market_metadata(slug)

            self.cycle = CycleInfo(
                market_slug=slug,
                yes_token_id=m["yes_token_id"],
                no_token_id=m["no_token_id"],
                condition_id=condition_id,
                open_ts=m["open_ts"],
                close_ts=m["close_ts"],
                neg_risk=neg_risk,
                shares=self.share_size,
            )
            self._skip_slug = slug
            self.state = BotState.MONITORING_PRICES
            self._log(f"Monitoring prices -- waiting for YES or NO ask >= {ORDER_PRICE}")
            return

    async def _get_market_metadata(self, slug: str) -> tuple[str, bool]:
        """Fetch condition_id and neg_risk from Gamma API."""
        try:
            import httpx
            async with httpx.AsyncClient(timeout=10) as client:
                resp = await client.get(f"{GAMMA_API}/markets?slug={slug}")
                if resp.status_code == 200:
                    data = resp.json()
                    if data:
                        info = data[0]
                        condition_id = info.get("conditionId", "")
                        neg_risk = info.get("negRisk", False)
                        if isinstance(neg_risk, str):
                            neg_risk = neg_risk.lower() == "true"
                        return condition_id, neg_risk
        except Exception as e:
            self._log(f"Error fetching market metadata: {e}")
        return "", False

    # ── Periodic redemption sweep ──────────────────────────────────

    async def _redemption_sweep_loop(self):
        """Every 60 seconds, query the positions API and redeem anything redeemable."""
        SWEEP_INTERVAL = 60  # seconds
        DATA_API = "https://data-api.polymarket.com"

        await asyncio.sleep(30)  # brief initial delay so init settles
        while True:
            try:
                await self._sweep_redeemable_positions(DATA_API)
            except asyncio.CancelledError:
                raise
            except Exception as e:
                logger.debug(f"Redemption sweep error: {e}")
            await asyncio.sleep(SWEEP_INTERVAL)

    async def _sweep_redeemable_positions(self, _data_api: str):
        """Use poly-web3 redeem_all() to claim any winning resolved positions."""
        if not self._relay:
            return
        try:
            results = await asyncio.to_thread(self._relay.redeem_all, 10)
            if results:
                self._log(f"Sweep: redeemed {len(results)} position(s)")
        except Exception as e:
            logger.debug(f"Redemption sweep error: {e}")

    async def _redeem_condition(self, condition_id: str, neg_risk: bool, label: str = ""):
        """Redeem a single condition via poly-web3."""
        try:
            results = await asyncio.to_thread(self._relay.redeem, condition_id)
            if results:
                self._log(f"Redeem confirmed: {label or condition_id[:16]}")
            else:
                self._log(f"Redeem returned empty result for {label or condition_id[:16]} -- may need retry")
        except Exception as e:
            self._log(f"Redeem error ({label}): {e}")

    # ── State: MONITORING_PRICES — watch asks, place when >= target ─

    async def _monitor_prices(self):
        """Check YES and NO best ask prices. Place a single BUY order
        for the first side whose ask reaches >= ORDER_PRICE."""
        if not self.cycle:
            return

        now = int(time.time())

        # Read best asks from the collector's in-memory order book
        _, yes_ask = self.collector._best_prices(self.cycle.yes_token_id)
        _, no_ask = self.collector._best_prices(self.cycle.no_token_id)

        # Check if either side's ask has reached the target price
        target_side = None
        target_token = None
        if yes_ask is not None and yes_ask >= ORDER_PRICE:
            target_side = "yes"
            target_token = self.cycle.yes_token_id
            self._log(f"YES ask = {yes_ask} >= {ORDER_PRICE} -- placing BUY YES")
        elif no_ask is not None and no_ask >= ORDER_PRICE:
            target_side = "no"
            target_token = self.cycle.no_token_id
            self._log(f"NO ask = {no_ask} >= {ORDER_PRICE} -- placing BUY NO")

        if target_side is None:
            return  # Neither side reached target yet; keep monitoring

        # Place a single GTC BUY limit order at ORDER_PRICE
        success = await self._place_single_order(target_side, target_token)
        if success:
            self.state = BotState.ORDER_PLACED
            self.cycle.order_placed_at = time.time()
        else:
            self._log("Order placement failed, will keep monitoring")

    # ── Place a single GTC limit order ────────────────────────────

    async def _place_single_order(self, side: str, token_id: str) -> bool:
        """Place one GTC BUY limit order at ORDER_PRICE for the given side."""
        if not self.cycle or not self._clob:
            return False

        try:
            neg_risk = self.cycle.neg_risk

            # Get fee rate
            fee_rate = 0
            try:
                fee_resp = await self._clob_get_fee_rate(token_id)
                if isinstance(fee_resp, dict):
                    fee_rate = fee_resp.get("fee_rate_bps", 0)
                    if isinstance(fee_rate, str):
                        fee_rate = int(fee_rate)
                elif isinstance(fee_resp, (int, float)):
                    fee_rate = int(fee_resp)
            except Exception:
                fee_rate = 0

            self._log(f"Placing GTC BUY {side.upper()} @ {ORDER_PRICE}: "
                       f"{self.cycle.shares} shares, fee={fee_rate}bps, neg_risk={neg_risk}")

            args = OrderArgs(
                token_id=token_id,
                price=ORDER_PRICE,
                size=self.cycle.shares,
                side="BUY",
                fee_rate_bps=fee_rate,
            )
            opts = PartialCreateOrderOptions(
                tick_size=TICK_SIZE,
                neg_risk=neg_risk,
            )

            resp = await self._clob_create_and_post_order(args, opts)
            oid = (resp.get("orderID") or resp.get("order_id") or resp.get("id")) if resp else None
            if oid:
                self.cycle.order_id = oid
                self.cycle.order_side = side
                self._log(f"{side.upper()} order placed: {oid[:16]}... status={resp.get('status')}")
                return True
            else:
                self._log(f"{side.upper()} order FAILED: {resp}")
                return False

        except Exception as e:
            self._log(f"Error placing {side} order: {e}")
            logger.exception("place_single_order error")
            return False

    # ── State: ORDER_PLACED — monitor for fill ────────────────────

    async def _monitor_order(self):
        """Check if the single order got filled."""
        if not self.cycle or not self._clob or not self.cycle.order_id:
            return

        try:
            order_data = await self._check_order_status(self.cycle.order_id)
            if order_data:
                status = str(order_data.get("status", "")).upper()
                if status == "MATCHED":
                    matched = order_data.get("size_matched", self.cycle.shares)
                    side = self.cycle.order_side.upper()
                    self._log(f"{side} order FILLED ({matched} shares @ {ORDER_PRICE})")
                    self.cycle.filled_side = self.cycle.order_side
                    self.state = BotState.POSITION_HELD
                    self._log(f"Holding {side} position until resolution")
                    return

            # Check if market is closing soon — cancel unfilled order
            now = int(time.time())
            if now > self.cycle.close_ts - 10:
                self._log("Market closing -- cancelling unfilled order")
                await self._cancel_order()
                self.cycle = None
                if self._stop_requested:
                    self.state = BotState.IDLE
                else:
                    self.state = BotState.WAITING_FOR_MARKET
                return

        except Exception as e:
            self._log(f"Error monitoring order: {e}")

    async def _check_order_status(self, order_id: str) -> Optional[dict]:
        """Get order status from CLOB."""
        try:
            result = await self._clob_get_order(order_id)
            if isinstance(result, dict):
                return result
            return None
        except Exception as e:
            self._log(f"Error checking order {order_id[:12]}...: {e}")
            return None

    async def _cancel_order(self):
        """Cancel the current order if any."""
        if not self.cycle or not self.cycle.order_id:
            return
        try:
            await self._clob_cancel(self.cycle.order_id)
            self._log(f"Cancelled {self.cycle.order_side.upper()} order: {self.cycle.order_id[:16]}...")
            self.cycle.order_id = None
        except Exception as e:
            self._log(f"Error cancelling order: {e}")

    # ── State: POSITION_HELD — wait for resolution ─────────────────

    async def _wait_for_resolution(self):
        """Wait until the market resolves (collector marks it resolved)."""
        if not self.cycle:
            return

        slug = self.cycle.market_slug
        now = int(time.time())

        # Check if the market has been removed from active markets (= resolved)
        if slug not in self.collector.markets:
            self._log(f"Market {slug} resolved -- moving to redemption")
            self.state = BotState.RESOLVING
            return

        # Also check if we're well past close time (grace period)
        if now > self.cycle.close_ts + 60:
            self._log(f"Market {slug} past close time -- checking resolution")
            self.state = BotState.RESOLVING
            return

    # ── State: RESOLVING — detect outcome ──────────────────────────

    async def _handle_resolution(self):
        """Market resolved. Move to redemption."""
        if not self.cycle:
            self.state = BotState.IDLE
            return

        self._log(f"Market {self.cycle.market_slug} resolved. Filled side: {self.cycle.filled_side}")
        self.state = BotState.REDEEMING

    # ── State: REDEEMING — reclaim tokens ──────────────────────────

    async def _handle_redemption(self):
        """Redeem winning tokens back to USDC using gasless relayer."""
        if not self.cycle:
            self.state = BotState.IDLE
            return

        if not self._relay:
            self._log("No relayer client -- skipping redemption (manual redeem needed)")
            await self._finish_cycle()
            return

        if not self.cycle.condition_id:
            self._log("No condition_id -- skipping redemption (manual redeem needed)")
            await self._finish_cycle()
            return

        self._log(f"Redeeming positions for {self.cycle.market_slug}...")
        await self._redeem_condition(
            self.cycle.condition_id,
            self.cycle.neg_risk,
            self.cycle.market_slug,
        )
        await self._finish_cycle()

    async def _finish_cycle(self):
        """Clean up after a cycle completes."""
        if self.cycle:
            self._log(f"Cycle complete: {self.cycle.market_slug}")
        self.cycle = None

        if self._stop_requested:
            self.state = BotState.IDLE
            self._running = False
            self._log("Bot stopped (stop was requested)")
        else:
            self.state = BotState.WAITING_FOR_MARKET
            self._log("Waiting for next market...")

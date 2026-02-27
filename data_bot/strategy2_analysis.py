"""
Strategy 2 — Best/Worst IST Time-of-Day Analysis
==================================================
Strategy 2 rule:
  If any side (YES or NO) touches <= 0.05 during the market:
    - If the side that touched <= 0.05 resolves as the WINNER  -> WIN
    - Otherwise                                                 -> LOSS
  Markets where neither side ever touches <= 0.05              -> excluded (no signal)

Runs against the local SQLite DB for 5m BTC markets.
Outputs win-rate by IST hour-of-day, 30-min slot, and 1-hour range blocks,
sorted best -> worst.

Usage:
    python strategy2_analysis.py [--db PATH] [--min-markets N]
"""

import argparse
import sqlite3
from collections import defaultdict

IST_OFFSET_HOURS = 5.5  # UTC + 5:30


def epoch_to_ist_hour_minute(epoch: int) -> tuple[int, int]:
    """Return (hour, minute) in IST for a Unix timestamp."""
    total_minutes = int(epoch // 60) + int(IST_OFFSET_HOURS * 60)
    hour = (total_minutes // 60) % 24
    minute = total_minutes % 60
    return hour, minute


def fmt_hm(hour: int, minute: int = 0) -> str:
    return f"{hour:02d}:{minute:02d}"


def win_rate_str(wins: int, total: int) -> str:
    if total == 0:
        return "  N/A  "
    pct = wins / total * 100
    return f"{pct:5.1f}%"


def analyse(db_path: str, min_markets: int):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row

    # Fetch all resolved 5m markets
    markets = conn.execute(
        "SELECT slug, open_timestamp, close_timestamp "
        "FROM markets WHERE resolved=1 AND market_type='5m' "
        "ORDER BY open_timestamp"
    ).fetchall()

    print(f"Total resolved 5m markets found: {len(markets)}\n")

    # ── Per-market strategy2 result ─────────────────────────────────
    results = []  # (open_epoch, result)  result: 'win'|'loss'|None

    for m in markets:
        slug = m["slug"]
        open_ts = m["open_timestamp"]

        ticks = conn.execute(
            "SELECT yes_mid, no_mid FROM price_ticks "
            "WHERE market_slug=? AND yes_mid IS NOT NULL AND no_mid IS NOT NULL "
            "ORDER BY epoch_ms",
            (slug,),
        ).fetchall()

        if not ticks:
            continue

        yes_series = [t["yes_mid"] for t in ticks]
        no_series  = [t["no_mid"]  for t in ticks]

        yes_close = yes_series[-1]
        no_close  = no_series[-1]
        winner = "yes" if yes_close >= no_close else "no"

        yes_min = min(yes_series)
        no_min  = min(no_series)

        yes_touched = yes_min <= 0.05
        no_touched  = no_min  <= 0.05

        if not yes_touched and not no_touched:
            results.append((open_ts, None))  # no signal
            continue

        winner_touched = (winner == "yes" and yes_touched) or (winner == "no" and no_touched)
        results.append((open_ts, "win" if winner_touched else "loss"))

    conn.close()

    signal_results = [(ep, r) for ep, r in results if r is not None]
    total_signal = len(signal_results)
    total_wins   = sum(1 for _, r in signal_results if r == "win")
    print(f"Markets with strategy2 signal : {total_signal}")
    print(f"Overall win / loss             : {total_wins} / {total_signal - total_wins}  "
          f"({win_rate_str(total_wins, total_signal).strip()})\n")

    # ── Bucket by IST hour ──────────────────────────────────────────
    hour_stats: dict[int, dict] = defaultdict(lambda: {"wins": 0, "total": 0})

    for ep, result in signal_results:
        h, _ = epoch_to_ist_hour_minute(ep)
        hour_stats[h]["total"] += 1
        if result == "win":
            hour_stats[h]["wins"] += 1

    # ── Bucket by 30-min slot ───────────────────────────────────────
    slot_stats: dict[str, dict] = defaultdict(lambda: {"wins": 0, "total": 0})

    for ep, result in signal_results:
        h, mn = epoch_to_ist_hour_minute(ep)
        slot_mn = 0 if mn < 30 else 30
        key = fmt_hm(h, slot_mn)
        slot_stats[key]["total"] += 1
        if result == "win":
            slot_stats[key]["wins"] += 1

    # ── Bucket by 2-hour block ──────────────────────────────────────
    block_stats: dict[str, dict] = defaultdict(lambda: {"wins": 0, "total": 0})

    for ep, result in signal_results:
        h, _ = epoch_to_ist_hour_minute(ep)
        block_start = (h // 2) * 2
        block_end   = (block_start + 2) % 24
        key = f"{fmt_hm(block_start)}–{fmt_hm(block_end)}"
        block_stats[key]["total"] += 1
        if result == "win":
            block_stats[key]["wins"] += 1

    # ── Helpers ─────────────────────────────────────────────────────
    def sorted_by_winrate(stats: dict, min_n: int):
        rows = []
        for label, d in stats.items():
            if d["total"] >= min_n:
                wr = d["wins"] / d["total"]
                rows.append((label, d["wins"], d["total"], wr))
        rows.sort(key=lambda x: (-x[3], -x[2]))
        return rows

    def print_table(title: str, rows: list, col_label: str = "Slot"):
        print(f"{'─'*54}")
        print(f"  {title}")
        print(f"{'─'*54}")
        print(f"  {'#':<4}  {col_label:<14}  {'Wins':>5}  {'Total':>6}  {'Win Rate':>8}")
        print(f"  {'─'*4}  {'─'*14}  {'─'*5}  {'─'*6}  {'─'*8}")
        for i, (label, wins, total, wr) in enumerate(rows, 1):
            marker = "  ★" if i == 1 else ("  ✗" if i == len(rows) else "   ")
            print(f"  {i:<4}  {label:<14}  {wins:>5}  {total:>6}  {win_rate_str(wins, total):>8}{marker}")
        if not rows:
            print(f"  (no slots with >= {min_markets} markets)")
        print()

    # ── Hour-of-day table ───────────────────────────────────────────
    hour_rows = []
    for h in range(24):
        d = hour_stats.get(h, {"wins": 0, "total": 0})
        if d["total"] >= min_markets:
            wr = d["wins"] / d["total"]
            hour_rows.append((fmt_hm(h), d["wins"], d["total"], wr))
    hour_rows.sort(key=lambda x: (-x[3], -x[2]))

    print_table(
        f"BY IST HOUR  (min {min_markets} markets each)",
        hour_rows,
        col_label="IST Hour",
    )

    # ── 30-min slot table ───────────────────────────────────────────
    slot_rows = sorted_by_winrate(slot_stats, min_markets)
    print_table(
        f"BY 30-MIN SLOT  (min {min_markets} markets each)",
        slot_rows,
        col_label="IST Slot",
    )

    # ── 2-hour block table ──────────────────────────────────────────
    block_rows = sorted_by_winrate(block_stats, min_markets)
    print_table(
        f"BY 2-HOUR BLOCK  (min {min_markets} markets each)",
        block_rows,
        col_label="IST Block",
    )

    # ── Summary: top 3 & bottom 3 slots ────────────────────────────
    print("━" * 54)
    print("  BEST 3 HALF-HOUR SLOTS (strategy 2, 5m BTC)")
    print("━" * 54)
    for label, wins, total, wr in slot_rows[:3]:
        print(f"  {label}  →  {wins}/{total}  ({wr*100:.1f}%)")

    print()
    print("━" * 54)
    print("  WORST 3 HALF-HOUR SLOTS (strategy 2, 5m BTC)")
    print("━" * 54)
    for label, wins, total, wr in slot_rows[-3:]:
        print(f"  {label}  →  {wins}/{total}  ({wr*100:.1f}%)")
    print()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Strategy 2 IST time analysis")
    parser.add_argument("--db",          default="btc_5m_data.db", help="Path to SQLite DB")
    parser.add_argument("--min-markets", type=int, default=5,
                        help="Minimum markets per slot to include in ranking (default: 5)")
    args = parser.parse_args()
    analyse(args.db, args.min_markets)

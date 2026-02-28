#!/usr/bin/env python3
"""
Verify market outcomes against Polymarket API
=============================================
- Adds 'verified' column to markets table
- Fetches each market by slug via Polymarket API
- Compares DB outcome with API outcomePrices
- Updates DB if API outcome differs
- Marks verified markets with a timestamp
- Respects rate limits (1000+ markets)

Usage:
    python verify_markets.py [--db PATH] [--dry-run]
"""

import argparse
import sqlite3
import time
import httpx
from datetime import datetime
from pathlib import Path

POLY_API = "https://gamma-api.polymarket.com"
REQUEST_DELAY = 0.2  # 200ms between requests = 5 req/s (well under rate limits)


def add_verified_column(conn: sqlite3.Connection):
    """Add verified column if it doesn't exist."""
    try:
        conn.execute("ALTER TABLE markets ADD COLUMN verified TEXT")
        conn.commit()
        print("Added 'verified' column to markets table")
    except sqlite3.OperationalError as e:
        if "duplicate column name" in str(e):
            pass  # Column already exists
        else:
            raise


def get_api_outcome(slug: str) -> tuple[str | None, str | None]:
    """Fetch market from API and return (winner, outcome_prices_str).
    
    Returns:
        - winner: 'yes'/'no' based on outcomePrices ["1","0"] or ["0","1"]
        - outcome_prices_str: raw outcomePrices string
    """
    try:
        resp = httpx.get(f"{POLY_API}/markets?slug={slug}", timeout=10)
        if resp.status_code != 200:
            return None, None
        
        data = resp.json()
        if not data:
            return None, None
        
        market = data[0]
        outcome_prices = market.get("outcomePrices", "")
        
        # Skip if not resolved (outcomePrices not exactly ["1","0"] or ["0","1"])
        if outcome_prices not in ('["1", "0"]', '["0", "1"]'):
            return None, outcome_prices
        
        # Determine winner from outcomePrices
        if outcome_prices == '["1", "0"]':
            winner = "yes"
        elif outcome_prices == '["0", "1"]':
            winner = "no"
        else:
            winner = None
        
        return winner, outcome_prices
    
    except Exception as e:
        print(f"  API error for {slug}: {e}")
        return None, None


def get_db_outcome(conn: sqlite3.Connection, slug: str) -> str | None:
    """Get winner from DB based on last price tick."""
    row = conn.execute(
        "SELECT yes_mid, no_mid FROM price_ticks "
        "WHERE market_slug=? AND yes_mid IS NOT NULL AND no_mid IS NOT NULL "
        "ORDER BY epoch_ms DESC LIMIT 1",
        (slug,),
    ).fetchone()
    
    if row:
        return "yes" if row["yes_mid"] >= row["no_mid"] else "no"
    return None


def update_db_outcome(conn: sqlite3.Connection, slug: str, winner: str):
    """Update the winner in price_ticks table - strategies will be recalculated dynamically."""
    # Get the most recent tick with price data and update it with the correct outcome
    row = conn.execute(
        "SELECT epoch_ms FROM price_ticks "
        "WHERE market_slug=? AND yes_mid IS NOT NULL AND no_mid IS NOT NULL "
        "ORDER BY epoch_ms DESC LIMIT 1",
        (slug,),
    ).fetchone()
    
    if row:
        # Update the last tick to have the correct outcome prices
        if winner == "yes":
            yes_mid, no_mid = 1.0, 0.0
        else:
            yes_mid, no_mid = 0.0, 1.0
        
        conn.execute(
            "UPDATE price_ticks SET yes_mid=?, no_mid=? "
            "WHERE market_slug=? AND epoch_ms=?",
            (yes_mid, no_mid, slug, row["epoch_ms"])
        )
        print(f"  Updated DB outcome for {slug}: {winner}")




def verify_markets(db_path: str, dry_run: bool = False):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    
    # Add verified column if needed
    add_verified_column(conn)
    
    # Get only unverified markets
    markets = conn.execute(
        "SELECT slug, verified FROM markets WHERE verified IS NULL OR verified = '' ORDER BY open_timestamp"
    ).fetchall()
    
    total = len(markets)
    verified_count = 0
    updated_count = 0
    skipped_count = 0
    
    print(f"Found {total} markets to verify")
    print("-" * 60)
    
    for i, market in enumerate(markets, 1):
        slug = market["slug"]
        
        print(f"[{i:3d}/{total}] {slug}", end=" ... ")
        
        # We only fetch unverified markets, so no need to check
        
        # Rate limiting
        if i > 1:
            time.sleep(REQUEST_DELAY)
        
        # Get API outcome
        api_winner, outcome_prices = get_api_outcome(slug)
        
        if api_winner is None:
            if outcome_prices:
                print(f"not resolved yet ({outcome_prices})")
                skipped_count += 1
            else:
                print("API error")
                skipped_count += 1
            continue
        
        # Get DB outcome
        db_winner = get_db_outcome(conn, slug)
        
        if db_winner is None:
            print("no price data in DB")
            skipped_count += 1
            continue
        
        # Compare outcomes
        if db_winner == api_winner:
            print(f"verified ✓ ({api_winner})")
            status = "✓"
        else:
            print(f"MISMATCH! DB={db_winner}, API={api_winner}")
            if not dry_run:
                update_db_outcome(conn, slug, api_winner)
                updated_count += 1
            status = f"✓ (fixed {db_winner}→{api_winner})"
        
        # Mark as verified
        if not dry_run:
            conn.execute(
                "UPDATE markets SET verified=? WHERE slug=?",
                (status, slug)
            )
            conn.commit()
        
        verified_count += 1
    
    conn.close()
    
    print("-" * 60)
    print(f"Verification complete!")
    print(f"  Total markets: {total}")
    print(f"  Verified: {verified_count}")
    print(f"  Updated (DB mismatch): {updated_count}")
    print(f"  Skipped (unresolved/no data): {skipped_count}")
    
    if dry_run:
        print("\n[DRY RUN] No changes made to database")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Verify market outcomes against Polymarket API")
    parser.add_argument("--db", default="btc_5m_data.db", help="Path to SQLite DB")
    parser.add_argument("--dry-run", action="store_true", 
                       help="Show what would be done without making changes")
    args = parser.parse_args()
    
    verify_markets(args.db, args.dry_run)

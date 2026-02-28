#!/usr/bin/env python3
"""
Remove all active markets except the latest one
============================================
- Finds all markets with resolved=0 (active)
- Keeps the most recent active market (by open_timestamp)
- Deletes all other active markets and their price_ticks
- Useful for cleaning up stale active markets

Usage:
    python cleanup_active_markets.py [--db PATH] [--dry-run]
"""

import argparse
import sqlite3
from datetime import datetime


def cleanup_active_markets(db_path: str, dry_run: bool = False):
    conn = sqlite3.connect(db_path)
    conn.row_factory = sqlite3.Row
    
    # Get active markets by type
    active_5m = conn.execute(
        "SELECT slug, open_timestamp, market_type FROM markets "
        "WHERE resolved = 0 AND market_type = '5m' ORDER BY open_timestamp DESC"
    ).fetchall()
    
    active_15m = conn.execute(
        "SELECT slug, open_timestamp, market_type FROM markets "
        "WHERE resolved = 0 AND market_type = '15m' ORDER BY open_timestamp DESC"
    ).fetchall()
    
    total_active = len(active_5m) + len(active_15m)
    
    if total_active == 0:
        print("No active markets found.")
        conn.close()
        return
    
    # Determine which markets to keep and delete
    keep_markets = []
    delete_markets = []
    
    # Keep latest 5m market if any exist
    if active_5m:
        keep_markets.append(active_5m[0])
        delete_markets.extend(active_5m[1:])
    
    # Keep latest 15m market if any exist
    if active_15m:
        keep_markets.append(active_15m[0])
        delete_markets.extend(active_15m[1:])
    
    if not delete_markets:
        print("No stale active markets to delete.")
        if keep_markets:
            print("Currently active markets:")
            for m in keep_markets:
                print(f"  {m['slug']} ({m['market_type']}) - opened at {datetime.fromtimestamp(m['open_timestamp'])}")
        conn.close()
        return
    
    print(f"Found {total_active} active markets")
    print("Markets to keep:")
    for m in keep_markets:
        print(f"  {m['slug']} ({m['market_type']}) - opened at {datetime.fromtimestamp(m['open_timestamp'])}")
    print(f"Will delete {len(delete_markets)} stale active markets:")
    print("-" * 60)
    
    deleted_count = 0
    for market in delete_markets:
        slug = market['slug']
        open_time = datetime.fromtimestamp(market['open_timestamp'])
        print(f"  {slug} ({market['market_type']}) - opened at {open_time}")
        
        if not dry_run:
            # Delete price_ticks first (foreign key constraint)
            conn.execute("DELETE FROM price_ticks WHERE market_slug=?", (slug,))
            # Delete the market
            conn.execute("DELETE FROM markets WHERE slug=?", (slug,))
            deleted_count += 1
    
    if not dry_run:
        conn.commit()
        print("-" * 60)
        print(f"✓ Deleted {deleted_count} stale active markets")
        print(f"✓ Kept {len(keep_markets)} active markets:")
        for m in keep_markets:
            print(f"    {m['slug']} ({m['market_type']})")
    else:
        print("-" * 60)
        print("[DRY RUN] No changes made")
        print(f"Would delete {len(delete_markets)} markets")
    
    conn.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Remove stale active markets")
    parser.add_argument("--db", default="btc_5m_data.db", help="Path to SQLite DB")
    parser.add_argument("--dry-run", action="store_true", 
                       help="Show what would be deleted without making changes")
    args = parser.parse_args()
    
    cleanup_active_markets(args.db, args.dry_run)

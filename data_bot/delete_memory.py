#!/usr/bin/env python3
"""
Script to delete memory/data from the database.
Can delete markets, price ticks, or clean up old data.
"""

import argparse
import sqlite3
from datetime import datetime, timedelta


def delete_markets_by_slug(conn, slugs):
    """Delete specific markets by slug."""
    cursor = conn.cursor()
    for slug in slugs:
        # Delete price ticks first (foreign key constraint)
        cursor.execute("DELETE FROM price_ticks WHERE market_slug = ?", (slug,))
        # Then delete the market
        cursor.execute("DELETE FROM markets WHERE slug = ?", (slug,))
        print(f"Deleted market: {slug}")
    conn.commit()


def delete_markets_by_timerange(conn, days_old, market_type=None):
    """Delete markets older than specified days."""
    cursor = conn.cursor()
    cutoff_date = datetime.now() - timedelta(days=days_old)
    cutoff_timestamp = int(cutoff_date.timestamp() * 1000)
    
    where_clause = "open_timestamp < ?"
    params = [cutoff_timestamp]
    
    if market_type:
        where_clause += " AND market_type = ?"
        params.append(market_type)
    
    # Get markets to be deleted
    markets = cursor.execute(
        f"SELECT slug FROM markets WHERE {where_clause}",
        params
    ).fetchall()
    
    if not markets:
        print(f"No markets found older than {days_old} days")
        return
    
    print(f"Deleting {len(markets)} markets older than {days_old} days...")
    
    # Delete price ticks first
    for (slug,) in markets:
        cursor.execute("DELETE FROM price_ticks WHERE market_slug = ?", (slug,))
    
    # Then delete markets
    cursor.execute(f"DELETE FROM markets WHERE {where_clause}", params)
    
    conn.commit()
    print(f"Deleted {len(markets)} markets and their price ticks")


def delete_all_markets(conn, market_type=None):
    """Delete all markets (and their price ticks)."""
    cursor = conn.cursor()
    
    where_clause = ""
    params = []
    
    if market_type:
        where_clause = "WHERE market_type = ?"
        params = [market_type]
    
    # Get count first
    count = cursor.execute(f"SELECT COUNT(*) FROM markets {where_clause}", params).fetchone()[0]
    
    if count == 0:
        print(f"No markets found" + (f" of type {market_type}" if market_type else ""))
        return
    
    print(f"Deleting ALL {count} markets" + (f" of type {market_type}" if market_type else "") + "...")
    
    # Delete price ticks first
    if market_type:
        cursor.execute("DELETE FROM price_ticks WHERE market_slug IN (SELECT slug FROM markets WHERE market_type = ?)", [market_type])
    else:
        cursor.execute("DELETE FROM price_ticks")
    
    # Then delete markets
    cursor.execute(f"DELETE FROM markets {where_clause}", params)
    
    conn.commit()
    print(f"Deleted {count} markets and all their price ticks")


def reset_verification_status(conn):
    """Reset verification status for all markets."""
    cursor = conn.cursor()
    cursor.execute("UPDATE markets SET verified = NULL")
    conn.commit()
    print("Reset verification status for all markets")


def main():
    parser = argparse.ArgumentParser(description="Delete data from the BTC market database")
    parser.add_argument("--db", default="btc_5m_data.db", help="Database file path")
    
    subparsers = parser.add_subparsers(dest="command", help="Available commands")
    
    # Delete by slug
    slug_parser = subparsers.add_parser("slug", help="Delete markets by slug")
    slug_parser.add_argument("slugs", nargs="+", help="Market slugs to delete")
    
    # Delete by time range
    time_parser = subparsers.add_parser("old", help="Delete markets older than N days")
    time_parser.add_argument("days", type=int, help="Delete markets older than this many days")
    time_parser.add_argument("--type", choices=["5m", "15m"], help="Market type filter")
    
    # Delete all
    all_parser = subparsers.add_parser("all", help="Delete all markets")
    all_parser.add_argument("--type", choices=["5m", "15m"], help="Market type filter")
    
    # Reset verification
    subparsers.add_parser("reset-verified", help="Reset verification status for all markets")
    
    # Dry run option
    parser.add_argument("--dry-run", action="store_true", help="Show what would be deleted without actually deleting")
    
    args = parser.parse_args()
    
    if not args.command:
        parser.print_help()
        return
    
    conn = sqlite3.connect(args.db)
    
    try:
        if args.command == "slug":
            if args.dry_run:
                print(f"DRY RUN: Would delete markets: {args.slugs}")
            else:
                delete_markets_by_slug(conn, args.slugs)
                
        elif args.command == "old":
            if args.dry_run:
                print(f"DRY RUN: Would delete markets older than {args.days} days" + 
                      (f" of type {args.type}" if args.type else ""))
            else:
                delete_markets_by_timerange(conn, args.days, args.type)
                
        elif args.command == "all":
            if args.dry_run:
                print(f"DRY RUN: Would delete ALL markets" + 
                      (f" of type {args.type}" if args.type else ""))
            else:
                delete_all_markets(conn, args.type)
                
        elif args.command == "reset-verified":
            if args.dry_run:
                print("DRY RUN: Would reset verification status for all markets")
            else:
                reset_verification_status(conn)
                
    finally:
        conn.close()


if __name__ == "__main__":
    main()

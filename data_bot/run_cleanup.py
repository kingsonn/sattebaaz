#!/usr/bin/env python3
"""
Quick script to clean up stale active markets
"""

import subprocess
import sys
import os

# Change to the script directory
os.chdir(os.path.dirname(os.path.abspath(__file__)))

print("Active Market Cleanup")
print("=" * 50)
print("This will delete all active markets except the latest one.")
print("Useful when markets get stuck in 'active' state.")
print()

# First show what would be deleted
print("Checking what markets would be deleted...")
print("-" * 50)
try:
    result = subprocess.run([sys.executable, "cleanup_active_markets.py", "--dry-run"], 
                          check=True, capture_output=True, text=True)
    print(result.stdout)
except subprocess.CalledProcessError as e:
    print(f"Error checking markets: {e}")
    sys.exit(1)

# Ask for confirmation
print("-" * 50)
response = input("Delete these markets? (y/N): ").strip().lower()
if response != 'y':
    print("Cancelled.")
    sys.exit(0)

# Run the actual cleanup
print("\nDeleting stale active markets...")
try:
    result = subprocess.run([sys.executable, "cleanup_active_markets.py"], 
                          check=True, capture_output=True, text=True)
    print(result.stdout)
    print("\n✓ Cleanup completed successfully!")
except subprocess.CalledProcessError as e:
    print(f"\n✗ Cleanup failed with exit code {e.returncode}")
    if e.stderr:
        print(f"Error: {e.stderr}")
    sys.exit(1)

#!/usr/bin/env python3
"""
Quick script to run market verification
"""

import subprocess
import sys
import os

# Change to the script directory
os.chdir(os.path.dirname(os.path.abspath(__file__)))

# Run the verification script
print("Starting market verification...")
print("This will check all markets against Polymarket API and update outcomes if needed.")
print("Rate limiting: 5 requests/second to respect API limits.\n")

# Ask for confirmation
response = input("Continue? (y/N): ").strip().lower()
if response != 'y':
    print("Cancelled.")
    sys.exit(0)

# Run verify_markets.py
try:
    result = subprocess.run([sys.executable, "verify_markets.py"], check=True)
    print("\n✓ Verification completed successfully!")
    print("Check the dashboard to see the ✓ marks in the Verified column.")
except subprocess.CalledProcessError as e:
    print(f"\n✗ Verification failed with exit code {e.returncode}")
    sys.exit(1)

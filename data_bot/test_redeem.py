"""
Standalone script to discover and redeem unclaimed resolved positions.
Supports Magic Link proxy wallets (signature_type=1) via poly-web3.

Usage:
    python test_redeem.py [--dry-run]

Set env vars in .env:
    POLYMARKET_PRIVATE_KEY       - EOA private key (signer)
    POLYMARKET_FUNDER_ADDRESS    - Proxy wallet address (holds tokens)
    POLYMARKET_SIGNATURE_TYPE    - 1 for Magic Link proxy, 2 for Safe (default 1)
    builder_api_key
    builder_secret
    builder_passphrase
"""

import os
import sys
import requests
from dotenv import load_dotenv

load_dotenv()

from py_clob_client.client import ClobClient
from py_builder_relayer_client.client import RelayClient
from py_builder_signing_sdk.config import BuilderConfig, BuilderApiKeyCreds
from poly_web3 import PolyWeb3Service, RELAYER_URL

CLOB_HOST = "https://clob.polymarket.com"
DATA_API  = "https://data-api.polymarket.com"
CHAIN_ID  = 137

DRY_RUN = "--dry-run" in sys.argv


def build_service() -> PolyWeb3Service:
    private_key    = os.environ["POLYMARKET_PRIVATE_KEY"]
    funder         = os.environ["POLYMARKET_FUNDER_ADDRESS"]
    sig_type       = int(os.environ.get("POLYMARKET_SIGNATURE_TYPE", "1"))
    builder_key    = os.environ["builder_api_key"]
    builder_secret = os.environ["builder_secret"]
    builder_pass   = os.environ["builder_passphrase"]

    clob = ClobClient(
        host=CLOB_HOST,
        chain_id=CHAIN_ID,
        key=private_key,
        signature_type=sig_type,
        funder=funder,
    )
    clob.set_api_creds(clob.create_or_derive_api_creds())

    relay = RelayClient(
        RELAYER_URL,
        CHAIN_ID,
        private_key,
        BuilderConfig(local_builder_creds=BuilderApiKeyCreds(
            key=builder_key,
            secret=builder_secret,
            passphrase=builder_pass,
        )),
    )

    return PolyWeb3Service(
        clob_client=clob,
        relayer_client=relay,
        rpc_url="https://1rpc.io/matic",
    )


def fetch_redeemable(wallet: str) -> list[dict]:
    r = requests.get(
        f"{DATA_API}/positions",
        params={"user": wallet, "sizeThreshold": "0.01", "redeemable": "true"},
        timeout=15,
    )
    r.raise_for_status()
    data = r.json()
    return data if isinstance(data, list) else data.get("data", data.get("results", []))


def main():
    private_key = os.environ.get("POLYMARKET_PRIVATE_KEY", "")
    wallet      = os.environ.get("POLYMARKET_FUNDER_ADDRESS", "")

    if not private_key or not wallet:
        print("ERROR: POLYMARKET_PRIVATE_KEY and POLYMARKET_FUNDER_ADDRESS must be set in .env")
        sys.exit(1)

    print(f"Wallet (proxy): {wallet}")
    print(f"Sig type: {os.environ.get('POLYMARKET_SIGNATURE_TYPE', '1')} (1=Magic Link proxy, 2=Safe)")
    print(f"Dry run: {DRY_RUN}\n")

    print("Fetching redeemable positions...")
    positions = fetch_redeemable(wallet)

    if not positions:
        print("No redeemable positions found. (API may lag 1-3 min after resolution)")
        return

    print(f"Found {len(positions)} redeemable position(s):")
    for p in positions:
        print(f"  {p.get('market', '?')}  size={p.get('size', '?')}  outcome={p.get('outcome', '?')}")

    # Deduplicate by conditionId
    seen: set[str] = set()
    condition_ids: list[str] = []
    for pos in positions:
        cid = pos.get("conditionId") or pos.get("condition_id", "")
        if not cid:
            continue
        key = cid.lower().replace("0x", "")
        if key not in seen:
            seen.add(key)
            condition_ids.append(cid)

    if not condition_ids:
        print("Could not extract conditionIds. Check API response format.")
        return

    print(f"\nUnique conditions to redeem: {len(condition_ids)}")
    for cid in condition_ids:
        print(f"  {cid}")

    if DRY_RUN:
        print("\n[DRY RUN] Skipping submission.")
        return

    print("\nInitializing poly-web3 service...")
    service = build_service()
    print("Ready.\n")

    print("Running redeem_all() (only redeems winning positions with pnl > 0)...")
    results = service.redeem_all(batch_size=10)
    print(f"\nResults: {results}")

    if not results:
        print("No winning positions to redeem (losing positions are worth $0 and don't need redemption).")
    elif any(r is None for r in results):
        print("WARNING: some redeems returned None -- retry may be needed (relayer congestion or API lag)")
    else:
        print(f"Success: {len(results)} redemption(s) confirmed.")


if __name__ == "__main__":
    main()

//! On-chain token merger for arb positions (neg risk markets).
//!
//! After buying both YES and NO tokens on the CLOB, merges them back to USDC
//! via ProxyWalletFactory.proxy() → NegRiskAdapter.mergePositions().
//!
//! For neg risk markets, CTF uses WrappedCollateral internally (not USDC).
//! The NegRiskAdapter handles unwrapping automatically, returning real USDC.
//!
//! Flow:
//! 1. EOA calls ProxyWalletFactory.proxy([
//!      {CALL, CTF, 0, setApprovalForAll(adapter, true)},  // approve adapter
//!      {CALL, NegRiskAdapter, 0, mergePositions(...)},     // merge + unwrap
//!    ])
//! 2. Factory routes to our PolyProxy wallet
//! 3. Proxy executes both calls atomically
//!
//! Requires: EOA has small amount of MATIC for gas (~0.01 MATIC ≈ $0.004)

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rlp::{Encodable, Header};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall};
use anyhow::{Result, bail, Context};
use serde::Deserialize;
use tracing::info;

// Polymarket contract addresses on Polygon
const CTF_ADDRESS: &str = "4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const NEG_RISK_ADAPTER: &str = "d91E80cF2E7be2e162c6513ceD06f1dD0dA35296";
const USDC_ADDRESS: &str = "2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const PROXY_FACTORY_ADDRESS: &str = "aB45c5A4B0c941a2F231C04C3f49182e1A254052";
const POLYGON_CHAIN_ID: u64 = 137;
const MERGE_GAS_LIMIT: u64 = 600_000; // Higher for 2-call proxy (approve + merge)

// ABI definitions via sol! macro
sol! {
    // CTF-compatible signature (NegRiskAdapter has overloaded version)
    function mergePositions(
        address collateralToken,
        bytes32 parentCollectionId,
        bytes32 conditionId,
        uint256[] partition,
        uint256 amount
    );

    // ERC1155 approval for NegRiskAdapter to transfer CTF tokens
    function setApprovalForAll(address operator, bool approved);

    // Matches ProxyWalletLib.ProxyCall struct
    // typeCode: 0=INVALID, 1=CALL, 2=DELEGATECALL
    struct ProxyCallItem {
        uint8 typeCode;
        address to;
        uint256 value;
        bytes data;
    }

    function proxy(ProxyCallItem[] calls);
}

pub struct PolygonMerger {
    rpc_url: String,
    http: reqwest::Client,
    wallet: PrivateKeySigner,
    ctf_address: Address,
    neg_risk_adapter: Address,
    usdc_address: Address,
    factory_address: Address,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct TxReceipt {
    status: Option<String>,
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "gasUsed")]
    gas_used: Option<String>,
}

impl PolygonMerger {
    pub fn new(rpc_url: &str, wallet: PrivateKeySigner) -> Result<Self> {
        Ok(Self {
            rpc_url: rpc_url.to_string(),
            http: reqwest::Client::new(),
            wallet,
            ctf_address: Address::from_slice(&hex::decode(CTF_ADDRESS)?),
            neg_risk_adapter: Address::from_slice(&hex::decode(NEG_RISK_ADAPTER)?),
            usdc_address: Address::from_slice(&hex::decode(USDC_ADDRESS)?),
            factory_address: Address::from_slice(&hex::decode(PROXY_FACTORY_ADDRESS)?),
        })
    }

    /// Check if EOA has enough MATIC for gas.
    pub async fn check_gas_balance(&self) -> Result<f64> {
        let eoa = self.wallet.address();
        let resp = self.rpc_call(
            "eth_getBalance",
            serde_json::json!([format!("{:?}", eoa), "latest"]),
        ).await?;
        let hex_bal = resp.as_str().unwrap_or("0x0");
        let bal = u128::from_str_radix(hex_bal.trim_start_matches("0x"), 16).unwrap_or(0);
        Ok(bal as f64 / 1e18) // MATIC has 18 decimals
    }

    /// Merge YES + NO tokens into USDC via on-chain transaction.
    /// `condition_id_hex` is the market's conditionId from Gamma API.
    /// `amount_tokens` is the number of token pairs to merge (float, e.g. 1.5).
    /// Returns the transaction hash on success.
    pub async fn merge_positions(
        &self,
        condition_id_hex: &str,
        amount_tokens: f64,
    ) -> Result<String> {
        // Convert condition_id from hex string to B256
        let cid_clean = condition_id_hex.trim_start_matches("0x");
        let cid_bytes = hex::decode(cid_clean)
            .context("invalid condition_id hex")?;
        if cid_bytes.len() != 32 {
            bail!("condition_id must be 32 bytes, got {}", cid_bytes.len());
        }
        let condition_id = B256::from_slice(&cid_bytes);

        // Convert token amount to raw units (6 decimals for USDC-backed tokens)
        let amount_raw = (amount_tokens * 1_000_000.0) as u64;
        if amount_raw == 0 {
            bail!("merge amount too small: {}", amount_tokens);
        }

        info!(
            "Merging {} tokens (raw={}) for condition {}",
            amount_tokens, amount_raw, condition_id_hex
        );

        // 1. Encode CTF.setApprovalForAll(negRiskAdapter, true)
        //    Idempotent — safe to call even if already approved.
        let approve_calldata = setApprovalForAllCall {
            operator: self.neg_risk_adapter,
            approved: true,
        }
        .abi_encode();

        // 2. Encode NegRiskAdapter.mergePositions() calldata
        //    Uses CTF-compatible overloaded signature. Adapter unwraps
        //    WrappedCollateral → USDC automatically.
        let merge_calldata = mergePositionsCall {
            collateralToken: self.usdc_address,
            parentCollectionId: B256::ZERO,
            conditionId: condition_id,
            partition: vec![U256::from(1), U256::from(2)],
            amount: U256::from(amount_raw),
        }
        .abi_encode();

        // 3. Wrap both in ProxyCalls for atomic execution
        let approve_call = ProxyCallItem {
            typeCode: 1, // CALL
            to: self.ctf_address,
            value: U256::ZERO,
            data: approve_calldata.into(),
        };
        let merge_call = ProxyCallItem {
            typeCode: 1, // CALL
            to: self.neg_risk_adapter,
            value: U256::ZERO,
            data: merge_calldata.into(),
        };

        let factory_calldata = proxyCall {
            calls: vec![approve_call, merge_call],
        }
        .abi_encode();

        // 4. Get nonce and gas price from Polygon RPC
        let nonce = self.get_nonce().await?;
        let gas_price = self.get_gas_price().await?;

        // 5. Build and sign legacy transaction
        let to = self.factory_address;
        let value: u128 = 0;

        // RLP encode for signing (EIP-155): [nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]
        let sign_rlp = rlp_encode_legacy_tx(
            nonce, gas_price, MERGE_GAS_LIMIT, to, value, &factory_calldata,
            Some(POLYGON_CHAIN_ID),
        );
        let tx_hash = keccak256(&sign_rlp);

        // Sign the hash
        let signature = self.wallet.sign_hash(&tx_hash).await
            .map_err(|e| anyhow::anyhow!("signing failed: {}", e))?;
        let sig_bytes = signature.as_bytes();
        let recovery_id = sig_bytes[64]; // 0 or 1
        let v = POLYGON_CHAIN_ID * 2 + 35 + recovery_id as u64;
        let r = U256::from_be_slice(&sig_bytes[0..32]);
        let s = U256::from_be_slice(&sig_bytes[32..64]);

        // RLP encode signed transaction: [nonce, gasPrice, gasLimit, to, value, data, v, r, s]
        let signed_rlp = rlp_encode_signed_legacy_tx(
            nonce, gas_price, MERGE_GAS_LIMIT, to, value, &factory_calldata, v, r, s,
        );

        // 6. Send raw transaction
        let raw_hex = format!("0x{}", hex::encode(&signed_rlp));
        let send_resp = self.rpc_call(
            "eth_sendRawTransaction",
            serde_json::json!([raw_hex]),
        ).await?;

        let tx_hash_str = send_resp.as_str()
            .ok_or_else(|| anyhow::anyhow!("no tx hash in response: {:?}", send_resp))?
            .to_string();

        info!("Merge tx sent: {}", tx_hash_str);

        // 6. Wait for confirmation (up to 30 seconds)
        let receipt = self.wait_for_receipt(&tx_hash_str, 30).await?;

        // Check status
        let status = receipt.status.as_deref().unwrap_or("0x0");
        if status == "0x1" {
            let gas_used = receipt.gas_used.as_deref().unwrap_or("?");
            info!("Merge confirmed! tx={} gas={}", tx_hash_str, gas_used);
            Ok(tx_hash_str)
        } else {
            bail!("Merge transaction reverted: tx={}", tx_hash_str);
        }
    }

    // ═══════════════════════════════════════════════════
    // JSON-RPC helpers
    // ═══════════════════════════════════════════════════

    async fn rpc_call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        });

        let resp: JsonRpcResponse = self.http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if let Some(err) = resp.error {
            bail!("RPC error in {}: {:?}", method, err);
        }

        resp.result.ok_or_else(|| anyhow::anyhow!("no result in {} response", method))
    }

    async fn get_nonce(&self) -> Result<u64> {
        let eoa = self.wallet.address();
        let resp = self.rpc_call(
            "eth_getTransactionCount",
            serde_json::json!([format!("{:?}", eoa), "pending"]),
        ).await?;
        let hex = resp.as_str().unwrap_or("0x0");
        Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0))
    }

    async fn get_gas_price(&self) -> Result<u128> {
        let resp = self.rpc_call("eth_gasPrice", serde_json::json!([])).await?;
        let hex = resp.as_str().unwrap_or("0x0");
        let price = u128::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(30_000_000_000);
        // Add 20% buffer for faster inclusion
        Ok(price * 120 / 100)
    }

    async fn wait_for_receipt(&self, tx_hash: &str, max_secs: u64) -> Result<TxReceipt> {
        let start = tokio::time::Instant::now();
        loop {
            if start.elapsed().as_secs() > max_secs {
                bail!("timeout waiting for tx receipt: {}", tx_hash);
            }

            let resp = self.rpc_call(
                "eth_getTransactionReceipt",
                serde_json::json!([tx_hash]),
            ).await;

            match resp {
                Ok(val) if !val.is_null() => {
                    let receipt: TxReceipt = serde_json::from_value(val)?;
                    return Ok(receipt);
                }
                _ => {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Legacy transaction RLP encoding
// ═══════════════════════════════════════════════════════════════════

/// RLP encode an unsigned legacy tx for signing (EIP-155).
/// Includes [nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]
fn rlp_encode_legacy_tx(
    nonce: u64,
    gas_price: u128,
    gas_limit: u64,
    to: Address,
    value: u128,
    data: &[u8],
    chain_id: Option<u64>,
) -> Vec<u8> {
    let mut payload = Vec::new();
    nonce.encode(&mut payload);
    gas_price.encode(&mut payload);
    gas_limit.encode(&mut payload);
    to.encode(&mut payload);
    value.encode(&mut payload);
    data.encode(&mut payload);
    if let Some(cid) = chain_id {
        cid.encode(&mut payload);
        0u8.encode(&mut payload);
        0u8.encode(&mut payload);
    }

    let mut out = Vec::new();
    Header { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    out
}

/// RLP encode a signed legacy tx: [nonce, gasPrice, gasLimit, to, value, data, v, r, s]
fn rlp_encode_signed_legacy_tx(
    nonce: u64,
    gas_price: u128,
    gas_limit: u64,
    to: Address,
    value: u128,
    data: &[u8],
    v: u64,
    r: U256,
    s: U256,
) -> Vec<u8> {
    let mut payload = Vec::new();
    nonce.encode(&mut payload);
    gas_price.encode(&mut payload);
    gas_limit.encode(&mut payload);
    to.encode(&mut payload);
    value.encode(&mut payload);
    data.encode(&mut payload);
    v.encode(&mut payload);
    r.encode(&mut payload);
    s.encode(&mut payload);

    let mut out = Vec::new();
    Header { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    out
}

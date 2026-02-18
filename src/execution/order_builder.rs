use crate::models::order::{OrderIntent, OrderSide, OrderType};
use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, Eip712Domain, SolStruct};
use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::debug;

// Use alloy's sol! macro to get the canonical EIP-712 hash computation
// IMPORTANT: struct must be named "Order" (not "Order") to match Polymarket's
// on-chain EIP-712 type hash: "Order(uint256 salt,address maker,...)"
sol! {
    #[derive(Debug)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        address taker;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint256 expiration;
        uint256 nonce;
        uint256 feeRateBps;
        uint8 side;
        uint8 signatureType;
    }
}

// --- Polymarket CTF Exchange EIP-712 constants ---

/// CTF Exchange contract on Polygon mainnet
const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
/// Neg Risk CTF Exchange (for markets with neg risk adapter)
const NEG_RISK_CTF_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

/// Polymarket Proxy Wallet Factory on Polygon mainnet
const PROXY_WALLET_FACTORY: &str = "0xaB45c5A4B0c941a2F231C04C3f49182e1A254052";
/// Init code hash for Polymarket EIP-1167 minimal proxy wallets
const PROXY_INIT_CODE_HASH: [u8; 32] = [
    0xd2, 0x1d, 0xf8, 0xdc, 0x65, 0x88, 0x0a, 0x86, 0x06, 0xf0, 0x9f, 0xe0, 0xce, 0x3d, 0xf9, 0xb8,
    0x86, 0x92, 0x87, 0xab, 0x0b, 0x05, 0x8b, 0xe0, 0x5a, 0xa9, 0xe8, 0xaf, 0x63, 0x30, 0xa0, 0x0b,
];

/// EIP-712 domain separator components
const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
const DOMAIN_VERSION: &str = "1";

/// EIP-712 type hashes (pre-computed keccak256 of type strings)
fn domain_type_hash() -> B256 {
    keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    )
}

fn order_type_hash() -> B256 {
    keccak256(
        "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)",
    )
}

/// Builds and signs orders for Polymarket CLOB submission.
///
/// Implements EIP-712 typed data signing per the CTF Exchange contract.
pub struct OrderBuilder {
    chain_id: u64,
    signer: PrivateKeySigner,
    maker_address: Address,
    funder_address: Option<Address>,
    signature_type: u8,
    use_neg_risk: bool,
    fee_rate_bps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedOrder {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    pub fee_rate_bps: String,
    pub side: String,
    pub signature_type: u8,
    pub signature: String,
}

/// Raw order struct for EIP-712 hashing
struct RawOrder {
    salt: U256,
    maker: Address,
    signer: Address,
    taker: Address,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: U256,
    nonce: U256,
    fee_rate_bps: U256,
    side: u8,
    signature_type: u8,
}

impl OrderBuilder {
    pub fn new(
        chain_id: u64,
        private_key: String,
        funder_address: Option<String>,
        signature_type: u8,
    ) -> Self {
        // Parse private key — if empty/invalid, create a random signer for dry-run
        let signer = if private_key.is_empty() {
            PrivateKeySigner::random()
        } else {
            let key_hex = private_key.strip_prefix("0x").unwrap_or(&private_key);
            key_hex
                .parse::<PrivateKeySigner>()
                .unwrap_or_else(|_| {
                    tracing::warn!("Invalid private key, using random signer (dry-run mode)");
                    PrivateKeySigner::random()
                })
        };

        let maker_address = signer.address();

        // For proxy wallets (signature_type=1), auto-derive the funder via CREATE2
        // matching the official rs-clob-client derive_proxy_wallet()
        let funder = if signature_type == 1 {
            // Try explicit funder first, fall back to CREATE2 derivation
            let explicit = funder_address
                .as_ref()
                .and_then(|f| f.parse::<Address>().ok());

            // CREATE2: salt = keccak256(eoa_address packed 20 bytes)
            let salt = keccak256(maker_address.as_slice());
            let factory = PROXY_WALLET_FACTORY.parse::<Address>().unwrap();
            let init_hash = B256::from(PROXY_INIT_CODE_HASH);

            // CREATE2 address = keccak256(0xff ++ factory ++ salt ++ init_code_hash)[12..]
            let mut create2_input = Vec::with_capacity(85);
            create2_input.push(0xff);
            create2_input.extend_from_slice(factory.as_slice());
            create2_input.extend_from_slice(salt.as_slice());
            create2_input.extend_from_slice(init_hash.as_slice());
            let derived_hash = keccak256(&create2_input);
            let derived = Address::from_slice(&derived_hash[12..]);

            if let Some(exp) = explicit {
                if exp != derived {
                    tracing::warn!(
                        "Funder mismatch! .env={:?} CREATE2-derived={:?} — using derived",
                        exp, derived
                    );
                }
            }
            tracing::info!("Proxy wallet (CREATE2): {:?}", derived);
            Some(derived)
        } else {
            funder_address
                .as_ref()
                .and_then(|f| f.parse::<Address>().ok())
        };

        Self {
            chain_id,
            signer,
            maker_address,
            funder_address: funder,
            signature_type,
            use_neg_risk: false,
            fee_rate_bps: 0,
        }
    }

    /// Set whether to use the neg risk CTF exchange address.
    pub fn set_neg_risk(&mut self, neg_risk: bool) {
        self.use_neg_risk = neg_risk;
    }

    /// Set the fee rate in basis points (fetch from CLOB API per token).
    /// 15-min crypto markets = 1000, fee-free = 0.
    pub fn set_fee_rate_bps(&mut self, bps: u32) {
        self.fee_rate_bps = bps;
    }

    /// Get the maker/signer address.
    pub fn address(&self) -> Address {
        self.maker_address
    }

    /// Build and sign an order from an OrderIntent.
    pub async fn build(&self, intent: &OrderIntent) -> Result<SignedOrder> {
        let price_f64 = intent.price.to_string().parse::<f64>().unwrap_or(0.0);
        let size_f64 = intent.size.to_string().parse::<f64>().unwrap_or(0.0);

        // Polymarket uses 6-decimal micro-units (1 USDC = 1_000_000).
        // Precision rules differ by order type:
        //   Market orders (FOK/FAK): maker ÷10000 (2 dec), taker ÷100 (4 dec)
        //   Limit orders  (GTC/GTD): maker ÷100   (4 dec), taker ÷10000 (2 dec)
        let is_market_order = matches!(intent.order_type, OrderType::FOK | OrderType::FAK);
        let is_sell = matches!(intent.order_side, OrderSide::Sell);
        // Polymarket rounding for tick_size 0.01: size=2dec, amount=4dec
        // BUY:  maker=USDC(amount,4dec), taker=shares(size,2dec)
        // SELL: maker=shares(size,2dec), taker=USDC(amount,4dec)
        // Market orders use the same rule but through build_market_order().
        let (maker_div, taker_div) = if is_market_order {
            (10000u64, 100u64) // market: maker 2 dec, taker 4 dec
        } else if is_sell {
            (10000u64, 100u64) // limit SELL: maker=shares(2dec), taker=USDC(4dec)
        } else {
            (100u64, 10000u64) // limit BUY: maker=USDC(4dec), taker=shares(2dec)
        };

        // Use .round() before as u64 to prevent IEEE 754 imprecision
        // (e.g., 4.35 * 1e6 = 4349999.999... → as u64 = 4349999 → misaligned)
        let size_trunc = (size_f64 * 100.0).floor() / 100.0;
        let (maker_amount, taker_amount) = match intent.order_side {
            OrderSide::Buy => {
                // maker = USDC (what we pay), taker = shares (what we get)
                let usdc_raw = (price_f64 * size_trunc * 1_000_000.0).round() as u64;
                let usdc = ((usdc_raw + maker_div - 1) / maker_div) * maker_div; // ceil
                let tokens_raw = (size_trunc * 1_000_000.0).round() as u64;
                let tokens = (tokens_raw / taker_div) * taker_div;               // floor
                (usdc, tokens)
            }
            OrderSide::Sell => {
                // maker = shares (what we provide), taker = USDC (what we get)
                let tokens_raw = (size_trunc * 1_000_000.0).round() as u64;
                let tokens = ((tokens_raw + maker_div - 1) / maker_div) * maker_div; // ceil
                let usdc_raw = (price_f64 * size_trunc * 1_000_000.0).round() as u64;
                let usdc = (usdc_raw / taker_div) * taker_div;                       // floor
                (tokens, usdc)
            }
        };

        let side: u8 = match intent.order_side {
            OrderSide::Buy => 0,
            OrderSide::Sell => 1,
        };

        // Generate random salt — must fit in IEEE 754 safe integer (≤ 2^53 - 1)
        let salt: u64 = rand::thread_rng().gen::<u64>() & ((1u64 << 53) - 1);

        // Polymarket token IDs are decimal strings; only treat as hex if 0x-prefixed
        let token_id = if intent.token_id.starts_with("0x") || intent.token_id.starts_with("0X") {
            U256::from_str_radix(&intent.token_id[2..], 16).unwrap_or(U256::ZERO)
        } else {
            U256::from_str_radix(&intent.token_id, 10).unwrap_or(U256::ZERO)
        };

        let raw = RawOrder {
            salt: U256::from(salt),
            maker: self.funder_address.unwrap_or(self.maker_address),
            signer: self.maker_address,
            taker: Address::ZERO, // Open taker
            token_id,
            maker_amount: U256::from(maker_amount),
            taker_amount: U256::from(taker_amount),
            expiration: U256::from(intent.expiration.unwrap_or(0)),
            nonce: U256::ZERO,
            fee_rate_bps: U256::from(self.fee_rate_bps),
            side,
            signature_type: self.signature_type,
        };

        // Use alloy's sol!-generated Order for canonical EIP-712 hash
        let exchange_addr = if self.use_neg_risk {
            NEG_RISK_CTF_EXCHANGE
        } else {
            CTF_EXCHANGE
        };
        let verifying_contract = exchange_addr.parse::<Address>().unwrap_or(Address::ZERO);

        let domain = Eip712Domain {
            name: Some(DOMAIN_NAME.into()),
            version: Some(DOMAIN_VERSION.into()),
            chain_id: Some(U256::from(self.chain_id)),
            verifying_contract: Some(verifying_contract),
            salt: None,
        };

        let sol_order = Order {
            salt: raw.salt,
            maker: raw.maker,
            signer: raw.signer,
            taker: raw.taker,
            tokenId: raw.token_id,
            makerAmount: raw.maker_amount,
            takerAmount: raw.taker_amount,
            expiration: raw.expiration,
            nonce: raw.nonce,
            feeRateBps: raw.fee_rate_bps,
            side: raw.side,
            signatureType: raw.signature_type,
        };

        let digest = sol_order.eip712_signing_hash(&domain);

        // Sign the digest
        let signature = self.signer.sign_hash(&digest).await?;
        // Official rs-clob-client uses signature.to_string() which produces v=0/1 (raw y_parity)
        // alloy 0.8 as_bytes() returns [r(32) || s(32) || v(1)] with v=0/1 — matches official client
        let sig_bytes = signature.as_bytes();
        let sig_hex = format!("0x{}", hex::encode(sig_bytes));

        let side_label = match intent.order_side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };

        debug!(
            "Signed order: token={} side={side_label} maker_amt={maker_amount} taker_amt={taker_amount}",
            intent.token_id
        );


        // CLOB API expects side as "BUY"/"SELL" string in JSON body
        let side_str = match intent.order_side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };

        Ok(SignedOrder {
            salt,
            maker: format!("{:?}", raw.maker),
            signer: format!("{:?}", raw.signer),
            taker: format!("{:?}", Address::ZERO),
            token_id: intent.token_id.clone(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: intent.expiration.unwrap_or(0).to_string(),
            nonce: "0".to_string(),
            fee_rate_bps: self.fee_rate_bps.to_string(),
            side: side_str.to_string(),
            signature_type: self.signature_type,
            signature: sig_hex,
        })
    }

    /// Build and sign a MARKET order (mirrors official py-clob-client create_market_order).
    ///
    /// For BUY:  `amount` = USDC to spend, `price` = worst price from book walk
    ///           maker = USDC (2 dec), taker = amount/price = shares (4 dec)
    /// For SELL: `amount` = shares to sell, `price` = worst price from book walk
    ///           maker = shares (2 dec), taker = amount*price = USDC (4 dec)
    ///
    /// Always posted as FOK. Expiration = 0.
    /// Returns (SignedOrder, actual_spend_or_shares, actual_taker).
    /// For BUY:  actual_spend = USDC spent (2 dec), actual_taker = shares received (4 dec)
    /// For SELL: actual_shares = shares sold (2 dec), actual_taker = USDC received (4 dec)
    pub async fn build_market_order(
        &self,
        token_id: &str,
        side: OrderSide,
        amount: f64,  // BUY: dollars, SELL: shares
        price: f64,   // worst acceptable price from book walk
    ) -> Result<(SignedOrder, f64, f64)> {
        // Market order rounding (tick_size 0.01): maker 2 dec, taker 4 dec
        // This matches official ROUNDING_CONFIG["0.01"] = RoundConfig(price=2, size=2, amount=4)
        // CRITICAL: Use integer arithmetic for micro-unit conversion.
        // f64 * 1_000_000.0 can lose precision (e.g., 3.13*1e6 = 3129999.99...)
        // which makes `as u64` produce values NOT aligned to the required divisor.
        // Fix: compute cents/bips as integers first, then multiply to micro-units.
        let price_rounded = (price * 100.0).round() / 100.0; // round price to 2 dec
        let (maker_amount, taker_amount, raw_maker_f, raw_taker_f) = match side {
            OrderSide::Buy => {
                // maker = USDC we spend (2 dec)
                let cents = (amount * 100.0).floor() as u64; // exact integer cents
                let maker = cents * 10_000; // 2 dec aligned in micro-units (cents * 10000)
                let raw_maker = cents as f64 / 100.0;
                // taker = shares we get (4 dec)
                let raw_taker = raw_maker / price_rounded;
                let bips = (raw_taker * 10_000.0).floor() as u64; // exact integer 4-dec units
                let taker = bips * 100; // 4 dec aligned in micro-units (bips * 100)
                let raw_taker = bips as f64 / 10_000.0;
                (maker, taker, raw_maker, raw_taker)
            }
            OrderSide::Sell => {
                // maker = shares we sell (2 dec)
                let cents = (amount * 100.0).floor() as u64;
                let maker = cents * 10_000;
                let raw_maker = cents as f64 / 100.0;
                // taker = USDC we get (4 dec)
                let raw_taker = raw_maker * price_rounded;
                let bips = (raw_taker * 10_000.0).floor() as u64;
                let taker = bips * 100;
                let raw_taker = bips as f64 / 10_000.0;
                (maker, taker, raw_maker, raw_taker)
            }
        };

        let side_u8: u8 = match side {
            OrderSide::Buy => 0,
            OrderSide::Sell => 1,
        };

        let salt: u64 = rand::thread_rng().gen::<u64>() & ((1u64 << 53) - 1);

        let token_id_u256 = if token_id.starts_with("0x") || token_id.starts_with("0X") {
            U256::from_str_radix(&token_id[2..], 16).unwrap_or(U256::ZERO)
        } else {
            U256::from_str_radix(token_id, 10).unwrap_or(U256::ZERO)
        };

        let exchange_addr = if self.use_neg_risk {
            NEG_RISK_CTF_EXCHANGE
        } else {
            CTF_EXCHANGE
        };
        let verifying_contract = exchange_addr.parse::<Address>().unwrap_or(Address::ZERO);

        let domain = Eip712Domain {
            name: Some(DOMAIN_NAME.into()),
            version: Some(DOMAIN_VERSION.into()),
            chain_id: Some(U256::from(self.chain_id)),
            verifying_contract: Some(verifying_contract),
            salt: None,
        };

        let sol_order = Order {
            salt: U256::from(salt),
            maker: self.funder_address.unwrap_or(self.maker_address),
            signer: self.maker_address,
            taker: Address::ZERO,
            tokenId: token_id_u256,
            makerAmount: U256::from(maker_amount),
            takerAmount: U256::from(taker_amount),
            expiration: U256::ZERO,  // market orders have no expiration
            nonce: U256::ZERO,
            feeRateBps: U256::from(self.fee_rate_bps),
            side: side_u8,
            signatureType: self.signature_type,
        };

        let digest = sol_order.eip712_signing_hash(&domain);
        let signature = self.signer.sign_hash(&digest).await?;
        let sig_hex = format!("0x{}", hex::encode(signature.as_bytes()));

        let side_str = match side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };

        debug!(
            "Market order: token={token_id} side={side_str} amount={amount} price={price} maker={maker_amount} taker={taker_amount}"
        );

        Ok((SignedOrder {
            salt,
            maker: format!("{:?}", self.funder_address.unwrap_or(self.maker_address)),
            signer: format!("{:?}", self.maker_address),
            taker: format!("{:?}", Address::ZERO),
            token_id: token_id.to_string(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: "0".to_string(),
            nonce: "0".to_string(),
            fee_rate_bps: self.fee_rate_bps.to_string(),
            side: side_str.to_string(),
            signature_type: self.signature_type,
            signature: sig_hex,
        }, raw_maker_f, raw_taker_f))
    }

    /// Build multiple signed orders.
    pub async fn build_batch(&self, intents: &[OrderIntent]) -> Result<Vec<SignedOrder>> {
        let mut results = Vec::with_capacity(intents.len());
        for intent in intents {
            results.push(self.build(intent).await?);
        }
        Ok(results)
    }

    /// Compute EIP-712 domain separator.
    fn domain_separator(&self) -> B256 {
        let exchange_addr = if self.use_neg_risk {
            NEG_RISK_CTF_EXCHANGE
        } else {
            CTF_EXCHANGE
        };
        let verifying_contract = exchange_addr.parse::<Address>().unwrap_or(Address::ZERO);

        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(domain_type_hash().as_slice());
        buf.extend_from_slice(keccak256(DOMAIN_NAME.as_bytes()).as_slice());
        buf.extend_from_slice(keccak256(DOMAIN_VERSION.as_bytes()).as_slice());
        buf.extend_from_slice(&U256::from(self.chain_id).to_be_bytes::<32>());
        // Address is 20 bytes, left-padded to 32 for ABI encoding
        let mut addr_padded = [0u8; 32];
        addr_padded[12..].copy_from_slice(verifying_contract.as_slice());
        buf.extend_from_slice(&addr_padded);

        keccak256(&buf)
    }

    /// Compute EIP-712 struct hash for an order.
    fn hash_order(&self, order: &RawOrder) -> B256 {
        let mut buf = Vec::with_capacity(13 * 32);

        // typeHash
        buf.extend_from_slice(order_type_hash().as_slice());
        // salt
        buf.extend_from_slice(&order.salt.to_be_bytes::<32>());
        // maker (address → left-padded 32 bytes)
        let mut maker_padded = [0u8; 32];
        maker_padded[12..].copy_from_slice(order.maker.as_slice());
        buf.extend_from_slice(&maker_padded);
        // signer
        let mut signer_padded = [0u8; 32];
        signer_padded[12..].copy_from_slice(order.signer.as_slice());
        buf.extend_from_slice(&signer_padded);
        // taker
        let mut taker_padded = [0u8; 32];
        taker_padded[12..].copy_from_slice(order.taker.as_slice());
        buf.extend_from_slice(&taker_padded);
        // tokenId
        buf.extend_from_slice(&order.token_id.to_be_bytes::<32>());
        // makerAmount
        buf.extend_from_slice(&order.maker_amount.to_be_bytes::<32>());
        // takerAmount
        buf.extend_from_slice(&order.taker_amount.to_be_bytes::<32>());
        // expiration
        buf.extend_from_slice(&order.expiration.to_be_bytes::<32>());
        // nonce
        buf.extend_from_slice(&order.nonce.to_be_bytes::<32>());
        // feeRateBps
        buf.extend_from_slice(&order.fee_rate_bps.to_be_bytes::<32>());
        // side (uint8 → left-padded 32 bytes)
        buf.extend_from_slice(&U256::from(order.side).to_be_bytes::<32>());
        // signatureType (uint8 → left-padded 32 bytes)
        buf.extend_from_slice(&U256::from(order.signature_type).to_be_bytes::<32>());

        keccak256(&buf)
    }
}

use alloy_primitives::{keccak256, Address, B256};
use alloy_signer::Signer;
use alloy_signer_local::PrivateKeySigner;
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

/// Polymarket CLOB API authentication.
///
/// The CLOB uses two levels of authentication:
///
/// **L1 Auth** (read-only + market orders):
///   - EIP-712 signature of a CLOB auth message
///   - Headers: POLY_ADDRESS, POLY_SIGNATURE, POLY_TIMESTAMP, POLY_NONCE
///
/// **L2 Auth** (derived API key for all operations):
///   - First: derive an API key via POST /auth/api-key
///   - Then: HMAC-SHA256 signed headers on every request
///   - Headers: POLY_ADDRESS, POLY_API_KEY, POLY_TIMESTAMP, POLY_SIGNATURE, POLY_PASSPHRASE
///
/// We use L2 (API key) auth for production since it's faster (no EIP-712 per request).
pub struct ClobAuth {
    signer: PrivateKeySigner,
    address: Address,
    api_creds: Option<ApiCredentials>,
    chain_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCredentials {
    #[serde(alias = "apiKey")]
    pub api_key: String,
    #[serde(alias = "secret")]
    pub api_secret: String,
    #[serde(alias = "passphrase")]
    pub api_passphrase: String,
}

/// Headers required for authenticated CLOB requests.
#[derive(Debug, Clone)]
pub struct AuthHeaders {
    pub address: String,
    pub signature: String,
    pub timestamp: String,
    pub nonce: String,
    // L2 fields (only present if using API key auth)
    pub api_key: Option<String>,
    pub passphrase: Option<String>,
}

// EIP-712 domain for CLOB auth
const CLOB_AUTH_DOMAIN_NAME: &str = "ClobAuthDomain";
const CLOB_AUTH_DOMAIN_VERSION: &str = "1";

impl ClobAuth {
    pub fn new(private_key: &str, chain_id: u64) -> Self {
        let signer = if private_key.is_empty() {
            PrivateKeySigner::random()
        } else {
            let key_hex = private_key.strip_prefix("0x").unwrap_or(private_key);
            key_hex
                .parse::<PrivateKeySigner>()
                .unwrap_or_else(|_| {
                    tracing::warn!("Invalid key for auth, using random (dry-run)");
                    PrivateKeySigner::random()
                })
        };

        let address = signer.address();

        Self {
            signer,
            address,
            api_creds: None,
            chain_id,
        }
    }

    /// Set API credentials (obtained from POST /auth/api-key).
    pub fn set_api_credentials(&mut self, creds: ApiCredentials) {
        info!("API credentials set for {}", self.address);
        self.api_creds = Some(creds);
    }

    /// Get the wallet address.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Get the API key string (for the "owner" field in order requests).
    pub fn api_key(&self) -> Option<String> {
        self.api_creds.as_ref().map(|c| c.api_key.clone())
    }

    /// Whether we have L2 (API key) auth configured.
    pub fn has_api_key(&self) -> bool {
        self.api_creds.is_some()
    }

    /// Generate L1 auth headers (EIP-712 signature-based).
    pub async fn l1_headers(&self) -> Result<AuthHeaders> {
        let timestamp = Utc::now().timestamp().to_string();
        let nonce = "0".to_string(); // Default nonce=0 per official Polymarket client

        // EIP-712 CLOB auth message:
        // ClobAuth(address address,string timestamp,uint256 nonce,string message)
        // Note: timestamp is 'string' type per official Polymarket spec
        let message = "This message attests that I control the given wallet";

        let type_hash = keccak256(
            "ClobAuth(address address,string timestamp,uint256 nonce,string message)",
        );

        // Encode struct
        let mut struct_data = Vec::with_capacity(5 * 32);
        struct_data.extend_from_slice(type_hash.as_slice());

        // address (left-padded)
        let mut addr_padded = [0u8; 32];
        addr_padded[12..].copy_from_slice(self.address.as_slice());
        struct_data.extend_from_slice(&addr_padded);

        // timestamp as string (EIP-712 encodes strings as keccak256 of bytes)
        struct_data.extend_from_slice(keccak256(timestamp.as_bytes()).as_slice());

        // nonce as uint256
        let n: u64 = nonce.parse().unwrap_or(0);
        let mut nonce_bytes = [0u8; 32];
        nonce_bytes[24..].copy_from_slice(&n.to_be_bytes());
        struct_data.extend_from_slice(&nonce_bytes);

        // message (keccak of string)
        struct_data.extend_from_slice(keccak256(message.as_bytes()).as_slice());

        let struct_hash = keccak256(&struct_data);

        // Domain separator
        let domain_sep = self.clob_domain_separator();

        // EIP-712 digest
        let mut digest_input = Vec::with_capacity(66);
        digest_input.push(0x19);
        digest_input.push(0x01);
        digest_input.extend_from_slice(domain_sep.as_slice());
        digest_input.extend_from_slice(struct_hash.as_slice());
        let digest = keccak256(&digest_input);

        let sig = self.signer.sign_hash(&digest).await?;
        let mut sig_bytes = sig.as_bytes();
        // alloy 0.8 as_bytes() returns recovery id (0/1) as last byte,
        // but Polymarket expects Ethereum-style v (27/28)
        if sig_bytes[64] < 27 {
            sig_bytes[64] += 27;
        }
        let sig_hex = format!("0x{}", hex::encode(sig_bytes));

        Ok(AuthHeaders {
            address: format!("{:?}", self.address),
            signature: sig_hex,
            timestamp,
            nonce,
            api_key: None,
            passphrase: None,
        })
    }

    /// Generate L2 auth headers (HMAC-based, requires API credentials).
    pub fn l2_headers(&self, method: &str, path: &str, body: &str) -> Result<AuthHeaders> {
        let creds = self
            .api_creds
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("API credentials not set — call derive_api_key first"))?;

        let timestamp = Utc::now().timestamp().to_string();

        // HMAC payload: timestamp + method + path_only + body
        // Official client uses url.path() which excludes query string
        let path_only = path.split('?').next().unwrap_or(path);
        let payload = format!("{}{}{}{}", timestamp, method.to_uppercase(), path_only, body);

        // Decode base64 secret
        let secret_bytes = base64_decode(&creds.api_secret)?;

        // HMAC-SHA256
        let signature = hmac_sha256(&secret_bytes, payload.as_bytes());
        let sig_b64 = base64_encode(&signature);

        Ok(AuthHeaders {
            address: format!("{:?}", self.address),
            signature: sig_b64,
            timestamp,
            nonce: String::new(),
            api_key: Some(creds.api_key.clone()),
            passphrase: Some(creds.api_passphrase.clone()),
        })
    }

    /// Create or derive API key from the CLOB server.
    /// Tries POST /auth/api-key (create) first, then GET /auth/derive-api-key (derive existing).
    /// Matches official client's createOrDeriveApiKey() pattern.
    pub async fn derive_api_key(&mut self, clob_host: &str) -> Result<ApiCredentials> {
        let http = reqwest::Client::new();

        // Try creating a new API key first
        let headers = self.l1_headers().await?;
        let create_url = format!("{}/auth/api-key", clob_host);

        let resp = http
            .post(&create_url)
            .header("POLY_ADDRESS", &headers.address)
            .header("POLY_SIGNATURE", &headers.signature)
            .header("POLY_TIMESTAMP", &headers.timestamp)
            .header("POLY_NONCE", &headers.nonce)
            .send()
            .await?;

        if resp.status().is_success() {
            if let Ok(creds) = resp.json::<ApiCredentials>().await {
                if !creds.api_key.is_empty() {
                    info!("API key created: {}", &creds.api_key[..8.min(creds.api_key.len())]);
                    self.api_creds = Some(creds.clone());
                    return Ok(creds);
                }
            }
        }

        // Fallback: derive existing API key
        let headers = self.l1_headers().await?;
        let derive_url = format!("{}/auth/derive-api-key", clob_host);

        let resp = http
            .get(&derive_url)
            .header("POLY_ADDRESS", &headers.address)
            .header("POLY_SIGNATURE", &headers.signature)
            .header("POLY_TIMESTAMP", &headers.timestamp)
            .header("POLY_NONCE", &headers.nonce)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("API key derivation failed: HTTP {status} — {body}");
        }

        let creds: ApiCredentials = resp.json().await?;
        info!("API key derived: {}", &creds.api_key[..8.min(creds.api_key.len())]);

        self.api_creds = Some(creds.clone());
        Ok(creds)
    }

    /// Compute CLOB auth EIP-712 domain separator.
    fn clob_domain_separator(&self) -> B256 {
        let domain_type = keccak256(
            "EIP712Domain(string name,string version,uint256 chainId)",
        );

        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(domain_type.as_slice());
        buf.extend_from_slice(keccak256(CLOB_AUTH_DOMAIN_NAME.as_bytes()).as_slice());
        buf.extend_from_slice(keccak256(CLOB_AUTH_DOMAIN_VERSION.as_bytes()).as_slice());

        let mut chain_bytes = [0u8; 32];
        chain_bytes[24..].copy_from_slice(&self.chain_id.to_be_bytes());
        buf.extend_from_slice(&chain_bytes);

        keccak256(&buf)
    }
}

impl AuthHeaders {
    /// Apply auth headers to a reqwest RequestBuilder.
    pub fn apply(self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut r = req
            .header("POLY_ADDRESS", &self.address)
            .header("POLY_SIGNATURE", &self.signature)
            .header("POLY_TIMESTAMP", &self.timestamp);

        if let Some(key) = &self.api_key {
            r = r.header("POLY_API_KEY", key);
        }
        if let Some(pass) = &self.passphrase {
            r = r.header("POLY_PASSPHRASE", pass);
        }
        if !self.nonce.is_empty() {
            r = r.header("POLY_NONCE", &self.nonce);
        }

        r
    }
}

// --- Crypto helpers (using sha2, hmac, base64 crates) ---

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    // Official client uses URL_SAFE base64 for both decode and encode
    base64::engine::general_purpose::URL_SAFE
        .decode(input)
        .map_err(|e| anyhow::anyhow!("base64 decode error: {e}"))
}

fn base64_encode(input: &[u8]) -> String {
    use base64::Engine;
    // Official client uses URL_SAFE base64
    base64::engine::general_purpose::URL_SAFE.encode(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_roundtrip() {
        let original = b"Hello, Polymarket!";
        let encoded = base64_encode(original);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_base64_known() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
    }

    #[tokio::test]
    async fn test_l1_headers_sign() {
        let auth = ClobAuth::new("", 137);
        let headers = auth.l1_headers().await.unwrap();
        assert!(!headers.signature.is_empty());
        assert!(headers.signature.starts_with("0x"));
        assert!(!headers.timestamp.is_empty());
    }
}

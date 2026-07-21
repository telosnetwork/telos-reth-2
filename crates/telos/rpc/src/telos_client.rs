//! Telos native chain client for forwarding transactions.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_primitives::{keccak256, Bytes, B256, U256};
use jsonrpsee::server::RpcModule;
use jsonrpsee_types::{ErrorObject, ErrorObjectOwned};
use reth_ethereum_primitives::TransactionSigned;
use reth_rpc_eth_types::EthApiError;
use secp256k1::SecretKey;
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use crate::antelope::{
    self, name_to_u64, now_plus, ref_block_num, ref_block_prefix, serialize_raw_action_data,
    sig_digest, sign_k1_canonical, wif_to_secret_key, PackedAction, PackedTransaction,
};

/// Default gas-price cache TTL (seconds) when `--telos.gas_cache_seconds` is not set.
/// 8 seconds chosen because the eosio.evm config table is updated by an on-chain action
/// at most once every few minutes; 8s gives sub-block freshness without hammering nodeos.
const DEFAULT_GAS_CACHE_SECONDS: u32 = 8;

/// `eth_maxPriorityFeePerGas` constant returned by the canonical Telos RPC.
/// 1 gwei = 0x3b9aca00. Telos has no priority-fee market — transactions pay only
/// `gas_price` from the eosio.evm config — but the canonical RPC returns 1 gwei
/// to satisfy EIP-1559 wallets. We mirror that for parity.
const TELOS_MAX_PRIORITY_FEE_PER_GAS_WEI: u64 = 1_000_000_000;

/// Maximum response body accepted from a native Telos endpoint.
const MAX_NODEOS_RESPONSE_BYTES: usize = 1024 * 1024;

/// Maximum untrusted nodeos response text retained in errors and logs.
const MAX_NODEOS_ERROR_BODY_BYTES: usize = 4096;

/// Maximum signer-key file size. A WIF is normally 51 or 52 bytes.
const MAX_SIGNER_KEY_FILE_BYTES: u64 = 256;

/// Maximum canonical EIP-2718 transaction size accepted by the forwarder.
const MAX_RAW_TRANSACTION_BYTES: usize = 256 * 1024;

/// Arguments for constructing a [`TelosClient`].
#[derive(Clone, Default)]
pub struct TelosClientArgs {
    /// Native Telos HTTP endpoint.
    pub telos_endpoint: Option<String>,
    /// Antelope signer account.
    pub signer_account: Option<String>,
    /// Antelope signer permission.
    pub signer_permission: Option<String>,
    /// Owner-only regular file containing the signer WIF.
    pub signer_key_file: Option<PathBuf>,
    /// Seconds to cache the gas-price reading from the `eosio.evm` config table.
    /// Defaults to 8 seconds when unset.
    pub gas_cache_seconds: Option<u32>,
}

impl fmt::Debug for TelosClientArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelosClientArgs")
            .field("telos_endpoint", &self.telos_endpoint)
            .field("signer_account", &self.signer_account)
            .field("signer_permission", &self.signer_permission)
            .field("signer_key_file", &self.signer_key_file.as_ref().map(|_| "<redacted>"))
            .field("gas_cache_seconds", &self.gas_cache_seconds)
            .finish()
    }
}

/// Invalid or unsafe transaction-forwarder configuration.
#[derive(Debug, thiserror::Error)]
pub enum TelosClientError {
    /// A required setting was omitted.
    #[error("missing required Telos forwarder option `{0}`")]
    Missing(&'static str),
    /// Endpoint URL is malformed or uses an unsupported scheme.
    #[error("invalid Telos endpoint: {0}")]
    InvalidEndpoint(String),
    /// A signer account, permission, or WIF is invalid.
    #[error(transparent)]
    Antelope(#[from] antelope::AntelopeError),
    /// The key file could not be inspected or read.
    #[error("failed to access signer key file `{path}`: {source}")]
    KeyFileIo {
        /// Key-file path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: io::Error,
    },
    /// Key-file type or permissions are unsafe.
    #[error("unsafe signer key file `{path}`: {reason}")]
    UnsafeKeyFile {
        /// Key-file path.
        path: PathBuf,
        /// Validation failure.
        reason: &'static str,
    },
    /// Key-file contents are not valid UTF-8.
    #[error("signer key file `{0}` is not valid UTF-8")]
    InvalidKeyEncoding(PathBuf),
    /// HTTP client construction failed.
    #[error("failed to build Telos HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

/// A client that forwards signed Ethereum transactions to the Telos native chain
/// by wrapping them in an `eosio.evm::raw` action and submitting a signed Antelope
/// transaction to `/v1/chain/push_transaction`.
#[derive(Clone)]
pub struct TelosClient {
    inner: Arc<TelosClientInner>,
}

struct TelosClientInner {
    endpoint: String,
    signer_actor: u64,
    signer_permission: u64,
    ram_payer: u64,
    contract_account: u64,
    action_name: u64,
    secret_key: SecretKey,
    http_client: reqwest::Client,
    gas_cache_seconds: u32,
    gas_price_cache: Mutex<Option<(Instant, U256)>>,
}

impl Drop for TelosClientInner {
    fn drop(&mut self) {
        self.secret_key.non_secure_erase();
    }
}

impl fmt::Debug for TelosClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelosClient")
            .field("endpoint", &self.inner.endpoint)
            .field("signer_actor", &self.inner.signer_actor)
            .field("signer_permission", &self.inner.signer_permission)
            .field("secret_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
struct GetInfoResponse {
    chain_id: String,
    last_irreversible_block_num: u32,
    last_irreversible_block_id: String,
}

#[derive(Debug, Deserialize)]
struct GetTableRowsResponse {
    rows: Vec<EvmConfigRow>,
}

/// Subset of the `eosio.evm` config-table row we care about. The contract
/// stores `gas_price` as a hex-encoded `uint256` string (e.g. `"4c68cd444de"`)
/// representing wei.
#[derive(Debug, Deserialize)]
struct EvmConfigRow {
    gas_price: String,
}

fn validate_endpoint(endpoint: &str) -> Result<String, TelosClientError> {
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|error| TelosClientError::InvalidEndpoint(error.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(TelosClientError::InvalidEndpoint("scheme must be http or https".to_string()))
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(TelosClientError::InvalidEndpoint(
            "embedded credentials are not allowed".to_string(),
        ))
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(TelosClientError::InvalidEndpoint(
            "query strings and fragments are not allowed".to_string(),
        ))
    }
    Ok(endpoint.trim_end_matches('/').to_string())
}

fn read_signer_key(path: &Path) -> Result<String, TelosClientError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|source| TelosClientError::KeyFileIo { path: path.to_path_buf(), source })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TelosClientError::UnsafeKeyFile {
            path: path.to_path_buf(),
            reason: "must be a regular file, not a symlink",
        })
    }
    if metadata.len() == 0 || metadata.len() > MAX_SIGNER_KEY_FILE_BYTES {
        return Err(TelosClientError::UnsafeKeyFile {
            path: path.to_path_buf(),
            reason: "size must be between 1 and 256 bytes",
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 || mode & 0o400 == 0 {
            return Err(TelosClientError::UnsafeKeyFile {
                path: path.to_path_buf(),
                reason: "permissions must grant owner read access and no group/other access",
            })
        }
    }

    let mut contents = std::fs::read(path)
        .map_err(|source| TelosClientError::KeyFileIo { path: path.to_path_buf(), source })?;
    while matches!(contents.last(), Some(b'\r' | b'\n')) {
        contents.pop();
    }
    let key = String::from_utf8(contents)
        .map_err(|_| TelosClientError::InvalidKeyEncoding(path.to_path_buf()))?;
    if key.is_empty() || key.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(TelosClientError::UnsafeKeyFile {
            path: path.to_path_buf(),
            reason: "key must be a single non-empty WIF without whitespace",
        })
    }
    Ok(key)
}

impl TelosClient {
    /// Creates a client after validating all endpoint, identity, and key-file settings.
    pub fn new(args: TelosClientArgs) -> Result<Self, TelosClientError> {
        let endpoint = args.telos_endpoint.ok_or(TelosClientError::Missing("telos_endpoint"))?;
        let endpoint = validate_endpoint(&endpoint)?;
        let signer_account_str =
            args.signer_account.ok_or(TelosClientError::Missing("signer_account"))?;
        let signer_permission_str =
            args.signer_permission.ok_or(TelosClientError::Missing("signer_permission"))?;
        let signer_key_file =
            args.signer_key_file.ok_or(TelosClientError::Missing("signer_key_file"))?;
        let gas_cache_seconds = args.gas_cache_seconds.unwrap_or(DEFAULT_GAS_CACHE_SECONDS);

        let signer_actor = name_to_u64(&signer_account_str)?;
        let signer_permission_u64 = name_to_u64(&signer_permission_str)?;
        let ram_payer = name_to_u64("eosio.evm")?;
        let contract_account = ram_payer;
        let action_name = name_to_u64("raw")?;
        let signer_key = read_signer_key(&signer_key_file)?;
        let secret_key = wif_to_secret_key(&signer_key)?;
        drop(signer_key);

        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            inner: Arc::new(TelosClientInner {
                endpoint,
                signer_actor,
                signer_permission: signer_permission_u64,
                ram_payer,
                contract_account,
                action_name,
                secret_key,
                http_client,
                gas_cache_seconds,
                gas_price_cache: Mutex::new(None),
            }),
        })
    }

    /// Returns the configured native Telos endpoint.
    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    /// Sign + submit a raw EVM transaction through `eosio.evm::raw`.
    ///
    /// 1. Fetch `get_info` for `chain_id` and a LIB block for TAPOS.
    /// 2. Build the action + packed transaction.
    /// 3. `sha256(chain_id || packed_trx || zero_cfa_hash)` → digest.
    /// 4. K1 canonical sign.
    /// 5. POST to `/v1/chain/push_transaction`.
    pub async fn send_to_telos(&self, tx: &[u8]) -> Result<(), EthApiError> {
        let max_retries = 6;
        let mut backoff_ms = 50u64;

        for attempt in 0..max_retries {
            match self.submit_once(tx).await {
                Ok(()) => {
                    debug!(attempt, "forwarded tx to Telos native");
                    return Ok(());
                }
                Err(err) => {
                    if attempt == max_retries - 1 {
                        error!(error = %err, "giving up forwarding tx to Telos native");
                        return Err(EthApiError::EvmCustom(format!("Telos forward error: {err}")));
                    }
                    warn!(
                        attempt,
                        error = %err,
                        "forward failed, retrying"
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(2000);
        }
        Err(EthApiError::EvmCustom("Telos forward retry loop exhausted".to_string()))
    }

    async fn submit_once(&self, tx: &[u8]) -> Result<(), antelope::AntelopeError> {
        let info = self.get_info().await?;

        // Parse chain_id / block_id as 32-byte digests.
        let chain_id_bytes = hex::decode(&info.chain_id)?;
        if chain_id_bytes.len() != 32 {
            return Err(antelope::AntelopeError::BadBlockId);
        }
        let mut chain_id = [0u8; 32];
        chain_id.copy_from_slice(&chain_id_bytes);
        let chain_id = B256::from(chain_id);

        let block_id_bytes = hex::decode(&info.last_irreversible_block_id)?;
        if block_id_bytes.len() != 32 {
            return Err(antelope::AntelopeError::BadBlockId);
        }
        let mut block_id_arr = [0u8; 32];
        block_id_arr.copy_from_slice(&block_id_bytes);
        let block_id = B256::from(block_id_arr);

        // Build action data.
        let action_data = serialize_raw_action_data(self.inner.ram_payer, tx, false, None);

        let action = PackedAction {
            account: self.inner.contract_account,
            name: self.inner.action_name,
            authorization: vec![(self.inner.signer_actor, self.inner.signer_permission)],
            data: action_data,
        };

        let packed = PackedTransaction {
            expiration: now_plus(60),
            ref_block_num: ref_block_num(info.last_irreversible_block_num),
            ref_block_prefix: ref_block_prefix(&block_id),
            max_net_usage_words: 0,
            max_cpu_usage_ms: 0,
            delay_sec: 0,
            actions: vec![action],
        };
        let packed_bytes = packed.serialize();

        let digest = sig_digest(&chain_id, &packed_bytes);
        let signature = sign_k1_canonical(&self.inner.secret_key, &digest)?;

        let payload = serde_json::json!({
            "signatures": [signature],
            "compression": "none",
            "packed_context_free_data": "",
            "packed_trx": hex::encode(&packed_bytes),
        });

        let url = format!("{}/v1/chain/push_transaction", self.inner.endpoint);
        let response = self.inner.http_client.post(&url).json(&payload).send().await?;
        let (status, body) = read_nodeos_response(response).await?;
        let status_code = status.as_u16();
        if !status.is_success() {
            return Err(antelope::AntelopeError::Nodeos {
                status: status_code,
                body: bounded_error_text(&body),
            })
        }
        let value: serde_json::Value = serde_json::from_slice(&body)?;
        if let Some(error) = transaction_response_error(&value) {
            return Err(antelope::AntelopeError::Nodeos {
                status: status_code,
                body: truncate_error_text(&error),
            })
        }
        Ok(())
    }

    /// Build a jsonrpsee RPC module that overrides:
    ///
    /// - `eth_sendRawTransaction` — forwards the raw transaction to Telos native via
    ///   [`Self::send_to_telos`]. The handler decodes the raw bytes, computes the EVM transaction
    ///   hash, and returns it synchronously after the native submission succeeds. It does NOT
    ///   insert the transaction into reth's local pool — blocks produced by nodeos flow back
    ///   through the consensus client and will land the tx naturally.
    /// - `eth_gasPrice` — returns the canonical gas price from the `eosio.evm` config table
    ///   on-chain (cached for `gas_cache_seconds`). Without this override, the default reth oracle
    ///   samples recent block transactions and returns 0 on Telos because empty 0.5s blocks
    ///   dominate the sample window. Wallets and SDKs depend on a non-zero value to construct
    ///   legacy txs.
    /// - `eth_maxPriorityFeePerGas` — returns 1 gwei to mirror canonical RPC. Telos has no
    ///   priority-fee market; transactions pay only `gas_price` from the config table. EIP-1559
    ///   wallets nonetheless query this method and a 0 reply makes them refuse to broadcast.
    pub fn build_forwarder_module(&self) -> Result<RpcModule<()>, ErrorObjectOwned> {
        let mut module = RpcModule::new(());

        // eth_sendRawTransaction — forward to Telos native.
        let forward_client = self.clone();
        module
            .register_async_method("eth_sendRawTransaction", move |params, _ctx, _ext| {
                let client = forward_client.clone();
                async move {
                    let (bytes,): (Bytes,) = params.parse().map_err(|e| {
                        ErrorObject::owned(
                            -32602,
                            format!("invalid params: {e}"),
                            None::<()>,
                        )
                    })?;
                    validate_raw_transaction(&bytes).map_err(EthApiError::into_rpc_err)?;
                    let hash: B256 = keccak256(&bytes);
                    info!(target: "telos::forward", tx_hash = %hash, bytes = bytes.len(), "forwarding tx to Telos native");
                    if let Err(err) = client.send_to_telos(&bytes).await {
                        error!(target: "telos::forward", error = %err, tx_hash = %hash, "forward failed");
                        return Err(err.into_rpc_err());
                    }
                    Ok::<B256, ErrorObject<'static>>(hash)
                }
            })
            .map_err(|e| {
                ErrorObject::owned(-32603, format!("register method: {e}"), None::<()>)
            })?;

        // eth_gasPrice — read from eosio.evm config table on-chain (cached).
        let gas_client = self.clone();
        module
            .register_async_method("eth_gasPrice", move |_params, _ctx, _ext| {
                let client = gas_client.clone();
                async move {
                    match client.get_gas_price().await {
                        Ok(price) => Ok::<U256, ErrorObject<'static>>(price),
                        Err(err) => {
                            warn!(target: "telos::gas", error = %err, "eth_gasPrice query failed");
                            Err(ErrorObject::owned(
                                -32603,
                                format!("Telos gas price unavailable: {err}"),
                                None::<()>,
                            ))
                        }
                    }
                }
            })
            .map_err(|e| ErrorObject::owned(-32603, format!("register method: {e}"), None::<()>))?;

        // eth_maxPriorityFeePerGas — Telos has no priority-fee market; mirror canonical RPC.
        module
            .register_async_method("eth_maxPriorityFeePerGas", |_params, _ctx, _ext| async move {
                Ok::<U256, ErrorObject<'static>>(U256::from(TELOS_MAX_PRIORITY_FEE_PER_GAS_WEI))
            })
            .map_err(|e| ErrorObject::owned(-32603, format!("register method: {e}"), None::<()>))?;

        Ok(module)
    }

    /// Returns the current Telos gas price in wei, sourced from the on-chain
    /// `eosio.evm` config singleton table. Cached per the `gas_cache_seconds` arg.
    ///
    /// On a cache miss (first call, or TTL expired), POSTs `/v1/chain/get_table_rows`
    /// with `code=eosio.evm scope=eosio.evm table=config json=true limit=1`. The
    /// contract stores `gas_price` as a hex string (e.g. `"4c68cd444de"`) in wei.
    pub async fn get_gas_price(&self) -> Result<U256, antelope::AntelopeError> {
        // Fast path — return cached value if still fresh.
        if let Some((fetched_at, price)) = *self
            .inner
            .gas_price_cache
            .lock()
            .map_err(|_| antelope::AntelopeError::CacheUnavailable)? &&
            fetched_at.elapsed() < Duration::from_secs(self.inner.gas_cache_seconds as u64)
        {
            return Ok(price);
        }

        // Cache miss — fetch from nodeos.
        let url = format!("{}/v1/chain/get_table_rows", self.inner.endpoint);
        let body = serde_json::json!({
            "code": "eosio.evm",
            "scope": "eosio.evm",
            "table": "config",
            "json": true,
            "limit": 1,
        });
        let response = self.inner.http_client.post(&url).json(&body).send().await?;
        let (status, response_body) = read_nodeos_response(response).await?;
        if !status.is_success() {
            return Err(antelope::AntelopeError::Nodeos {
                status: status.as_u16(),
                body: bounded_error_text(&response_body),
            })
        }
        let parsed: GetTableRowsResponse = serde_json::from_slice(&response_body)?;
        let row = parsed.rows.first().ok_or_else(|| antelope::AntelopeError::Nodeos {
            status: 200,
            body: "eosio.evm config table returned no rows".to_string(),
        })?;

        let price =
            parse_evm_gas_price(&row.gas_price).ok_or_else(|| antelope::AntelopeError::Nodeos {
                status: 200,
                body: format!("malformed gas_price hex: {:?}", row.gas_price),
            })?;

        // Update the cache. Multiple writers racing to insert the same value is fine.
        *self
            .inner
            .gas_price_cache
            .lock()
            .map_err(|_| antelope::AntelopeError::CacheUnavailable)? =
            Some((Instant::now(), price));

        debug!(target: "telos::gas", price = %price, "refreshed eosio.evm gas_price");
        Ok(price)
    }

    async fn get_info(&self) -> Result<GetInfoResponse, antelope::AntelopeError> {
        let url = format!("{}/v1/chain/get_info", self.inner.endpoint);
        let response = self.inner.http_client.post(&url).send().await?;
        let (status, body) = read_nodeos_response(response).await?;
        if !status.is_success() {
            return Err(antelope::AntelopeError::Nodeos {
                status: status.as_u16(),
                body: bounded_error_text(&body),
            })
        }
        let info: GetInfoResponse = serde_json::from_slice(&body)?;
        Ok(info)
    }
}

fn validate_raw_transaction(raw: &[u8]) -> Result<(), EthApiError> {
    if raw.len() > MAX_RAW_TRANSACTION_BYTES {
        return Err(EthApiError::EvmCustom(format!(
            "raw transaction exceeds {MAX_RAW_TRANSACTION_BYTES} bytes"
        )))
    }
    reth_rpc_eth_types::utils::recover_raw_transaction::<TransactionSigned>(raw)?;
    Ok(())
}

async fn read_nodeos_response(
    mut response: reqwest::Response,
) -> Result<(reqwest::StatusCode, Vec<u8>), antelope::AntelopeError> {
    if response.content_length().is_some_and(|length| length > MAX_NODEOS_RESPONSE_BYTES as u64) {
        return Err(antelope::AntelopeError::ResponseTooLarge { limit: MAX_NODEOS_RESPONSE_BYTES })
    }

    let status = response.status();
    let mut body = Vec::with_capacity(
        response.content_length().unwrap_or_default().min(MAX_NODEOS_RESPONSE_BYTES as u64)
            as usize,
    );
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_NODEOS_RESPONSE_BYTES {
            return Err(antelope::AntelopeError::ResponseTooLarge {
                limit: MAX_NODEOS_RESPONSE_BYTES,
            })
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

fn bounded_error_text(body: &[u8]) -> String {
    truncate_error_text(&String::from_utf8_lossy(body))
}

fn truncate_error_text(text: &str) -> String {
    if text.len() <= MAX_NODEOS_ERROR_BODY_BYTES {
        return text.to_string()
    }

    const SUFFIX: &str = "… [truncated]";
    let mut end = MAX_NODEOS_ERROR_BODY_BYTES.saturating_sub(SUFFIX.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{SUFFIX}", &text[..end])
}

/// Inspects a `/v1/chain/push_transaction` JSON response and returns
/// `Some(error)` if nodeos reports the transaction failed, or `None` if it
/// executed cleanly. The push endpoint returns HTTP 200 even for transactions
/// that revert or run into resource exhaustion, so we have to look inside the
/// response body to surface real failures rather than phantom-success hashes.
fn transaction_response_error(value: &serde_json::Value) -> Option<String> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Some(format!("nodeos response error: {error}"));
    }
    let processed = value.get("processed")?;
    if let Some(status) = processed
        .pointer("/receipt/status")
        .and_then(serde_json::Value::as_str)
        .filter(|status| *status != "executed")
    {
        return Some(format!("nodeos transaction status {status}"));
    }
    if let Some(exception) = processed.get("except").filter(|exception| !exception.is_null()) {
        return Some(format!("nodeos transaction exception: {exception}"));
    }
    if let Some(exception) = processed.get("except_ptr").filter(|exception| !exception.is_null()) {
        return Some(format!("nodeos transaction exception: {exception}"));
    }
    None
}

/// Parses the `gas_price` field from an `eosio.evm` config row.
///
/// The field is a hex-encoded uint256 (with or without `0x` prefix), e.g.
/// `"4c68cd444de"` for 5,250,812,757,214 wei. Empty string and non-hex
/// inputs are treated as malformed and return None.
fn parse_evm_gas_price(raw: &str) -> Option<U256> {
    let trimmed = raw.trim_start_matches("0x");
    if trimmed.is_empty() {
        return None;
    }
    U256::from_str_radix(trimmed, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{b256, bytes, hex, Signature, TxKind};

    fn canonical_transaction() -> Vec<u8> {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0x18,
            gas_price: 0xfa56ea00,
            gas_limit: 119_902,
            to: TxKind::Call(hex!("06012c8cf97bead5deae237070f9587f8e7a266d").into()),
            value: U256::from(0x1c6bf526340000u64),
            input: bytes!("f7d8c88300000000000000000000000000000000000000000000000000000000000cee6100000000000000000000000000000000000000000000000000000000000ac3e1"),
        };
        let signature = Signature::from_scalars_and_parity(
            b256!("2a378831cf81d99a3f06a18ae1b6ca366817ab4d88a70053c41d7a8f0368e031"),
            b256!("450d831a05b6e418724436c05c155e0a1b7b921015d0fbc2f667aed709ac4fb5"),
            false,
        );
        let transaction: TransactionSigned = tx.into_signed(signature).into();
        transaction.encoded_2718()
    }

    #[test]
    fn parses_canonical_mainnet_gas_price() {
        // Live value observed on rpc.telos.net 2026-05-04 (`eth_gasPrice` = 0x4c68cd444de).
        let parsed = parse_evm_gas_price("4c68cd444de").expect("parses");
        assert_eq!(parsed, U256::from(5_250_812_757_214u64));
    }

    #[test]
    fn parses_zero_padded_nodeos_format() {
        // Actual format returned by `/v1/chain/get_table_rows` for eosio.evm.config:
        // a 64-char zero-padded hex string. Verified against mainnet.telos.net 2026-05-04.
        let raw = "000000000000000000000000000000000000000000000000000004c68cd444de";
        let parsed = parse_evm_gas_price(raw).expect("parses");
        assert_eq!(parsed, U256::from(5_250_812_757_214u64));
    }

    #[test]
    fn parses_with_0x_prefix() {
        let parsed = parse_evm_gas_price("0x4c68cd444de").expect("parses");
        assert_eq!(parsed, U256::from(5_250_812_757_214u64));
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_evm_gas_price("").is_none());
        assert!(parse_evm_gas_price("0x").is_none());
    }

    #[test]
    fn rejects_non_hex() {
        assert!(parse_evm_gas_price("zzz").is_none());
    }

    #[test]
    fn detects_failed_receipt_status() {
        let value = serde_json::json!({
            "transaction_id": "abc",
            "processed": { "receipt": { "status": "hard_fail" } }
        });
        assert!(transaction_response_error(&value).unwrap().contains("hard_fail"));
    }

    #[test]
    fn detects_top_level_error() {
        let value = serde_json::json!({
            "error": { "code": 3050003, "message": "incorrect nonce" }
        });
        assert!(transaction_response_error(&value).unwrap().contains("incorrect nonce"));
    }

    #[test]
    fn passes_clean_executed_response() {
        let value = serde_json::json!({
            "transaction_id": "abc",
            "processed": { "receipt": { "status": "executed" } }
        });
        assert!(transaction_response_error(&value).is_none());
    }

    #[test]
    fn accepts_canonical_recoverable_raw_transaction() {
        assert!(validate_raw_transaction(&canonical_transaction()).is_ok());
    }

    #[test]
    fn rejects_trailing_raw_transaction_bytes() {
        let mut transaction = canonical_transaction();
        transaction.push(0);
        assert!(validate_raw_transaction(&transaction).is_err());
    }

    #[test]
    fn rejects_oversized_raw_transaction() {
        let transaction = vec![0; MAX_RAW_TRANSACTION_BYTES + 1];
        let error = validate_raw_transaction(&transaction).unwrap_err().to_string();
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn truncates_untrusted_error_text() {
        let input = "x".repeat(MAX_NODEOS_ERROR_BODY_BYTES + 10);
        let output = truncate_error_text(&input);
        assert_eq!(output.len(), MAX_NODEOS_ERROR_BODY_BYTES);
        assert!(output.ends_with("[truncated]"));
    }
}

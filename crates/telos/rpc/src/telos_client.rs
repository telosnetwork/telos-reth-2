//! Telos native chain client for forwarding transactions.

use std::{
    fmt,
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use alloy_consensus::{transaction::SignerRecoverable, Transaction};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{b256, keccak256, Bytes, B256, U256};
use jsonrpsee::server::RpcModule;
use jsonrpsee_types::{ErrorObject, ErrorObjectOwned};
use reth_ethereum_primitives::TransactionSigned;
use reth_rpc_eth_types::EthApiError;
use secp256k1::SecretKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};
use zeroize::Zeroizing;

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

/// Maximum attempts for a prepared native-chain submission, including the first attempt.
const MAX_FORWARD_ATTEMPTS: usize = 6;

/// Maximum attempts to obtain a current native-chain reference block before signing.
const MAX_PREPARATION_ATTEMPTS: usize = 3;

/// Bounds each ambiguous push attempt so every retry finishes before the 60-second expiration.
const SUBMISSION_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);

/// Canonical Telos Zero mainnet chain ID paired with EVM chain ID 40.
const TELOS_MAINNET_NATIVE_CHAIN_ID: B256 =
    b256!("4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11");

/// Canonical Telos Zero testnet chain ID paired with EVM chain ID 41.
const TELOS_TESTNET_NATIVE_CHAIN_ID: B256 =
    b256!("1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f");

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
    /// The configured EVM chain is not a supported Telos network.
    #[error("Telos transaction forwarding only supports EVM chain IDs 40 and 41, got {0}")]
    UnsupportedEvmChainId(u64),
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
    expected_evm_chain_id: u64,
    expected_native_chain_id: B256,
    gas_cache_seconds: u32,
    gas_price_cache: Mutex<Option<(Instant, U256)>>,
}

struct PreparedSubmission {
    payload: serde_json::Value,
    transaction_id: [u8; 32],
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
    if parsed.path() != "/" {
        return Err(TelosClientError::InvalidEndpoint(
            "endpoint must not contain a path".to_string(),
        ))
    }
    if parsed.scheme() == "http" {
        let host = parsed.host_str().unwrap_or_default();
        let ip_literal =
            host.strip_prefix('[').and_then(|host| host.strip_suffix(']')).unwrap_or(host);
        let is_loopback = host.eq_ignore_ascii_case("localhost") ||
            ip_literal.parse::<std::net::IpAddr>().is_ok_and(|address| address.is_loopback());
        if !is_loopback {
            return Err(TelosClientError::InvalidEndpoint(
                "plaintext HTTP is allowed only for an explicit loopback endpoint".to_string(),
            ))
        }
    }
    Ok(endpoint.trim_end_matches('/').to_string())
}

fn read_signer_key(path: &Path) -> Result<Zeroizing<Vec<u8>>, TelosClientError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|source| {
        #[cfg(unix)]
        if source.raw_os_error() == Some(libc::ELOOP) {
            return TelosClientError::UnsafeKeyFile {
                path: path.to_path_buf(),
                reason: "must be a regular file, not a symlink",
            }
        }
        TelosClientError::KeyFileIo { path: path.to_path_buf(), source }
    })?;
    // Validate and read the same open descriptor. This prevents a path swap between checking the
    // file and consuming the WIF.
    let metadata = file
        .metadata()
        .map_err(|source| TelosClientError::KeyFileIo { path: path.to_path_buf(), source })?;
    if !metadata.is_file() {
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

    let mut contents = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.by_ref()
        .take(MAX_SIGNER_KEY_FILE_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|source| TelosClientError::KeyFileIo { path: path.to_path_buf(), source })?;
    if contents.is_empty() || contents.len() as u64 > MAX_SIGNER_KEY_FILE_BYTES {
        return Err(TelosClientError::UnsafeKeyFile {
            path: path.to_path_buf(),
            reason: "size must be between 1 and 256 bytes",
        })
    }
    while matches!(contents.last(), Some(b'\r' | b'\n')) {
        contents.pop();
    }
    std::str::from_utf8(&contents)
        .map_err(|_| TelosClientError::InvalidKeyEncoding(path.to_path_buf()))?;
    if contents.is_empty() || contents.iter().any(|byte| byte.is_ascii_whitespace()) {
        return Err(TelosClientError::UnsafeKeyFile {
            path: path.to_path_buf(),
            reason: "key must be a single non-empty WIF without whitespace",
        })
    }
    Ok(contents)
}

impl TelosClient {
    /// Creates a client after validating all endpoint, identity, and key-file settings.
    pub fn new(
        args: TelosClientArgs,
        expected_evm_chain_id: u64,
    ) -> Result<Self, TelosClientError> {
        let expected_native_chain_id = match expected_evm_chain_id {
            40 => TELOS_MAINNET_NATIVE_CHAIN_ID,
            41 => TELOS_TESTNET_NATIVE_CHAIN_ID,
            _ => return Err(TelosClientError::UnsupportedEvmChainId(expected_evm_chain_id)),
        };
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
        let signer_key = std::str::from_utf8(&signer_key)
            .map_err(|_| TelosClientError::InvalidKeyEncoding(signer_key_file.clone()))?;
        let secret_key = wif_to_secret_key(signer_key)?;

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
                expected_evm_chain_id,
                expected_native_chain_id,
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
        validate_raw_transaction(tx, self.inner.expected_evm_chain_id)?;
        let submission = self.prepare_submission_with_retry(tx).await.map_err(|err| {
            error!(error = %err, "failed to prepare transaction for Telos native");
            EthApiError::EvmCustom(format!("Telos forward error: {err}"))
        })?;
        let mut backoff_ms = 50u64;

        for attempt in 0..MAX_FORWARD_ATTEMPTS {
            let result =
                tokio::time::timeout(SUBMISSION_ATTEMPT_TIMEOUT, self.submit_prepared(&submission))
                    .await
                    .unwrap_or(Err(antelope::AntelopeError::SubmissionTimeout));
            match result {
                Ok(()) => {
                    debug!(attempt, "forwarded tx to Telos native");
                    return Ok(());
                }
                Err(err) => {
                    let retryable = is_retryable_submission_error(&err);
                    if !retryable || attempt == MAX_FORWARD_ATTEMPTS - 1 {
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

    async fn prepare_submission_with_retry(
        &self,
        tx: &[u8],
    ) -> Result<PreparedSubmission, antelope::AntelopeError> {
        let mut backoff_ms = 50u64;
        for attempt in 0..MAX_PREPARATION_ATTEMPTS {
            match self.prepare_submission(tx).await {
                Ok(submission) => return Ok(submission),
                Err(error)
                    if is_retryable_submission_error(&error) &&
                        attempt < MAX_PREPARATION_ATTEMPTS - 1 =>
                {
                    warn!(attempt, error = %error, "native transaction preparation failed, retrying");
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(2000);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("preparation loop returns on its final attempt")
    }

    async fn prepare_submission(
        &self,
        tx: &[u8],
    ) -> Result<PreparedSubmission, antelope::AntelopeError> {
        let info = self.get_validated_info().await?;
        let chain_id = self.inner.expected_native_chain_id;

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
        let transaction_id = Sha256::digest(&packed_bytes).into();

        let payload = serde_json::json!({
            "signatures": [signature],
            "compression": "none",
            "packed_context_free_data": "",
            "packed_trx": hex::encode(&packed_bytes),
        });

        Ok(PreparedSubmission { payload, transaction_id })
    }

    async fn submit_prepared(
        &self,
        submission: &PreparedSubmission,
    ) -> Result<(), antelope::AntelopeError> {
        let url = format!("{}/v1/chain/push_transaction", self.inner.endpoint);
        let response = self.inner.http_client.post(&url).json(&submission.payload).send().await?;
        let (status, body) = read_nodeos_response(response).await?;
        let status_code = status.as_u16();
        if !status.is_success() {
            // A timed-out request can still have executed. The exact prepared transaction is
            // retried, so a structured duplicate response naming its transaction ID is success.
            if is_matching_duplicate_response(&body, &submission.transaction_id) {
                return Ok(())
            }
            return Err(antelope::AntelopeError::Nodeos {
                status: status_code,
                body: bounded_error_text(&body),
            })
        }
        let value: serde_json::Value = serde_json::from_slice(&body)?;
        if let Err(error) = validate_transaction_response(&value, &submission.transaction_id) {
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
        // Validate the endpoint identity before trusting network-specific contract configuration.
        self.get_validated_info().await?;
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

    async fn get_validated_info(&self) -> Result<GetInfoResponse, antelope::AntelopeError> {
        let info = self.get_info().await?;
        validate_native_chain_id(&info.chain_id, self.inner.expected_native_chain_id)?;
        Ok(info)
    }
}

fn validate_raw_transaction(raw: &[u8], expected_chain_id: u64) -> Result<(), EthApiError> {
    if raw.len() > MAX_RAW_TRANSACTION_BYTES {
        return Err(EthApiError::EvmCustom(format!(
            "raw transaction exceeds {MAX_RAW_TRANSACTION_BYTES} bytes"
        )))
    }
    if raw.is_empty() {
        return Err(EthApiError::EmptyRawTransactionData)
    }

    let transaction = TransactionSigned::decode_2718_exact(raw)
        .map_err(|_| EthApiError::FailedToDecodeSignedTransaction)?;
    if !matches!(&transaction, TransactionSigned::Legacy(_)) {
        return Err(EthApiError::EvmCustom(
            "unsupported Telos transaction type; only legacy type 0 is accepted".to_string(),
        ))
    }
    let chain_id = transaction.chain_id();
    if chain_id == Some(3) {
        return Err(EthApiError::EvmCustom(
            "legacy chain-ID-3 transactions are reserved for native Telos block ingestion"
                .to_string(),
        ))
    }
    let chain_id = chain_id.ok_or_else(|| {
        EthApiError::EvmCustom(format!(
            "unprotected transactions are not accepted; expected Telos EVM chain ID {expected_chain_id}"
        ))
    })?;
    if chain_id != expected_chain_id {
        return Err(EthApiError::EvmCustom(format!(
            "transaction chain ID {chain_id} does not match configured Telos EVM chain ID {expected_chain_id}"
        )))
    }
    transaction.try_into_recovered().map_err(|_| EthApiError::InvalidTransactionSignature)?;
    Ok(())
}

fn validate_native_chain_id(raw: &str, expected: B256) -> Result<B256, antelope::AntelopeError> {
    let bytes: [u8; 32] =
        hex::decode(raw)?.try_into().map_err(|_| antelope::AntelopeError::BadChainId)?;
    let actual = B256::from(bytes);
    if actual != expected {
        return Err(antelope::AntelopeError::NativeChainIdMismatch { expected, actual })
    }
    Ok(actual)
}

fn is_retryable_submission_error(error: &antelope::AntelopeError) -> bool {
    match error {
        antelope::AntelopeError::Http(error) => {
            error.is_connect() || error.is_timeout() || error.is_body()
        }
        // Nodeos reports permanent contract and validation exceptions as HTTP 500. Only retry
        // statuses that identify timeout, throttling, or an unavailable proxy/service.
        antelope::AntelopeError::Nodeos { status, .. } => {
            matches!(*status, 408 | 425 | 429 | 502..=504)
        }
        antelope::AntelopeError::SubmissionTimeout => true,
        _ => false,
    }
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

/// Validates that nodeos executed the exact packed transaction that was submitted.
fn validate_transaction_response(
    value: &serde_json::Value,
    expected_transaction_id: &[u8; 32],
) -> Result<(), String> {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(format!("nodeos response error: {error}"))
    }

    let transaction_id = value
        .get("transaction_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "nodeos response is missing a string transaction_id".to_string())?;
    let transaction_id = hex::decode(transaction_id)
        .map_err(|_| "nodeos response contains a malformed transaction_id".to_string())?;
    if transaction_id.as_slice() != expected_transaction_id {
        return Err(format!(
            "nodeos transaction_id does not match submitted transaction {}",
            hex::encode(expected_transaction_id)
        ))
    }

    let processed = value
        .get("processed")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "nodeos response is missing a processed object".to_string())?;
    if let Some(exception) = processed.get("except").filter(|exception| !exception.is_null()) {
        return Err(format!("nodeos transaction exception: {exception}"))
    }
    if let Some(exception) = processed.get("except_ptr").filter(|exception| !exception.is_null()) {
        return Err(format!("nodeos transaction exception: {exception}"))
    }
    let status = processed
        .get("receipt")
        .and_then(serde_json::Value::as_object)
        .and_then(|receipt| receipt.get("status"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            "nodeos response is missing processed.receipt.status as a string".to_string()
        })?;
    if status != "executed" {
        return Err(format!("nodeos transaction status {status}"))
    }
    Ok(())
}

fn is_matching_duplicate_response(body: &[u8], expected_transaction_id: &[u8; 32]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else { return false };
    let Some(error) = value.get("error").and_then(serde_json::Value::as_object) else {
        return false
    };
    if error.get("code").and_then(serde_json::Value::as_u64) != Some(3_040_008) ||
        error.get("name").and_then(serde_json::Value::as_str) != Some("tx_duplicate")
    {
        return false
    }
    json_contains_string(
        &serde_json::Value::Object(error.clone()),
        &hex::encode(expected_transaction_id),
    )
}

fn json_contains_string(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(needle),
        serde_json::Value::Array(values) => {
            values.iter().any(|value| json_contains_string(value, needle))
        }
        serde_json::Value::Object(values) => {
            values.values().any(|value| json_contains_string(value, needle))
        }
        _ => false,
    }
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
    use alloy_consensus::{SignableTransaction, TxEip2930, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{b256, bytes, hex, Signature, TxKind};
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn canonical_transaction(chain_id: Option<u64>) -> Vec<u8> {
        let tx = TxLegacy {
            chain_id,
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
        let expected_transaction_id = [0xabu8; 32];
        let value = serde_json::json!({
            "transaction_id": hex::encode(expected_transaction_id),
            "processed": { "receipt": { "status": "hard_fail" } }
        });
        assert!(validate_transaction_response(&value, &expected_transaction_id)
            .unwrap_err()
            .contains("hard_fail"));
    }

    #[test]
    fn detects_top_level_error() {
        let expected_transaction_id = [0xabu8; 32];
        let value = serde_json::json!({
            "error": { "code": 3050003, "message": "incorrect nonce" }
        });
        assert!(validate_transaction_response(&value, &expected_transaction_id)
            .unwrap_err()
            .contains("incorrect nonce"));
    }

    #[test]
    fn passes_clean_executed_response() {
        let expected_transaction_id = [0xabu8; 32];
        let value = serde_json::json!({
            "transaction_id": hex::encode(expected_transaction_id),
            "processed": { "receipt": { "status": "executed" } }
        });
        assert!(validate_transaction_response(&value, &expected_transaction_id).is_ok());
    }

    #[test]
    fn rejects_incomplete_success_response() {
        let expected_transaction_id = [0xabu8; 32];
        for value in [
            serde_json::json!({}),
            serde_json::json!({ "transaction_id": hex::encode(expected_transaction_id) }),
            serde_json::json!({
                "transaction_id": hex::encode(expected_transaction_id),
                "processed": {}
            }),
            serde_json::json!({
                "transaction_id": hex::encode(expected_transaction_id),
                "processed": { "receipt": {} }
            }),
        ] {
            assert!(validate_transaction_response(&value, &expected_transaction_id).is_err());
        }
    }

    #[test]
    fn rejects_mismatched_transaction_identity() {
        let expected_transaction_id = [0xabu8; 32];
        let value = serde_json::json!({
            "transaction_id": hex::encode([0xcdu8; 32]),
            "processed": { "receipt": { "status": "executed" } }
        });
        assert!(validate_transaction_response(&value, &expected_transaction_id)
            .unwrap_err()
            .contains("does not match"));
    }

    #[test]
    fn accepts_only_matching_structured_duplicate_response() {
        let expected_transaction_id = [0xabu8; 32];
        let expected_hex = hex::encode(expected_transaction_id);
        let matching = serde_json::json!({
            "code": 500,
            "error": {
                "code": 3040008,
                "name": "tx_duplicate",
                "what": "Duplicate transaction",
                "details": [{ "message": format!("duplicate transaction {expected_hex}") }]
            }
        });
        assert!(is_matching_duplicate_response(
            &serde_json::to_vec(&matching).unwrap(),
            &expected_transaction_id,
        ));

        for non_matching in [
            serde_json::json!({
                "error": {
                    "code": 3040008,
                    "name": "tx_duplicate",
                    "details": [{ "message": hex::encode([0xcdu8; 32]) }]
                }
            }),
            serde_json::json!({
                "error": {
                    "code": 3040008,
                    "name": "different_error",
                    "details": [{ "message": expected_hex }]
                }
            }),
        ] {
            assert!(!is_matching_duplicate_response(
                &serde_json::to_vec(&non_matching).unwrap(),
                &expected_transaction_id,
            ));
        }
    }

    #[test]
    fn accepts_canonical_recoverable_raw_transaction() {
        assert!(validate_raw_transaction(&canonical_transaction(Some(40)), 40).is_ok());
    }

    #[test]
    fn native_endpoint_requires_https_off_host() {
        assert_eq!(validate_endpoint("http://127.0.0.1:8888/").unwrap(), "http://127.0.0.1:8888");
        assert_eq!(validate_endpoint("http://[::1]:8888").unwrap(), "http://[::1]:8888");
        assert_eq!(
            validate_endpoint("https://mainnet.telos.net").unwrap(),
            "https://mainnet.telos.net"
        );
        for endpoint in
            ["http://mainnet.telos.net", "http://192.0.2.1:8888", "https://mainnet.telos.net/base"]
        {
            assert!(validate_endpoint(endpoint).is_err(), "accepted unsafe endpoint {endpoint}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn signer_key_is_read_from_one_owner_only_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("signer.wif");
        std::fs::write(&key_path, b"test-wif\n").unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o400)).unwrap();

        let key = read_signer_key(&key_path).unwrap();
        assert_eq!(key.as_slice(), b"test-wif");
    }

    #[cfg(unix)]
    #[test]
    fn signer_key_rejects_symlinks_and_broad_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("signer.wif");
        let link_path = directory.path().join("signer-link.wif");
        std::fs::write(&key_path, b"test-wif").unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o400)).unwrap();
        symlink(&key_path, &link_path).unwrap();

        assert!(matches!(read_signer_key(&link_path), Err(TelosClientError::UnsafeKeyFile { .. })));

        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o440)).unwrap();
        assert!(matches!(read_signer_key(&key_path), Err(TelosClientError::UnsafeKeyFile { .. })));
    }

    #[test]
    fn rejects_typed_transaction_even_for_the_expected_chain() {
        let transaction: TransactionSigned = TxEip2930 { chain_id: 40, ..Default::default() }
            .into_signed(Signature::new(U256::MAX, U256::ZERO, false))
            .into();
        let error =
            validate_raw_transaction(&transaction.encoded_2718(), 40).unwrap_err().to_string();
        assert!(error.contains("only legacy type 0"));
    }

    #[test]
    fn rejects_transaction_for_another_evm_chain() {
        let error =
            validate_raw_transaction(&canonical_transaction(Some(41)), 40).unwrap_err().to_string();
        assert!(error.contains("does not match"));
    }

    #[test]
    fn rejects_legacy_chain_id_three_before_sender_recovery() {
        let transaction: TransactionSigned = TxLegacy { chain_id: Some(3), ..Default::default() }
            .into_signed(Signature::new(U256::MAX, U256::ZERO, false))
            .into();
        let error =
            validate_raw_transaction(&transaction.encoded_2718(), 40).unwrap_err().to_string();
        assert!(error.contains("chain-ID-3"));
    }

    #[test]
    fn rejects_unprotected_legacy_transaction() {
        let error =
            validate_raw_transaction(&canonical_transaction(None), 40).unwrap_err().to_string();
        assert!(error.contains("unprotected"));
    }

    #[test]
    fn rejects_unsupported_expected_evm_chain_at_construction() {
        let error = TelosClient::new(TelosClientArgs::default(), 1).unwrap_err();
        assert!(matches!(error, TelosClientError::UnsupportedEvmChainId(1)));
    }

    #[test]
    fn pins_canonical_native_chain_ids() {
        assert_eq!(
            validate_native_chain_id(
                "4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11",
                TELOS_MAINNET_NATIVE_CHAIN_ID,
            )
            .unwrap(),
            TELOS_MAINNET_NATIVE_CHAIN_ID
        );
        assert_eq!(
            validate_native_chain_id(
                "1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f",
                TELOS_TESTNET_NATIVE_CHAIN_ID,
            )
            .unwrap(),
            TELOS_TESTNET_NATIVE_CHAIN_ID
        );
        assert!(matches!(
            validate_native_chain_id(
                "1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f",
                TELOS_MAINNET_NATIVE_CHAIN_ID,
            ),
            Err(antelope::AntelopeError::NativeChainIdMismatch { .. })
        ));
        assert!(matches!(
            validate_native_chain_id("00", TELOS_MAINNET_NATIVE_CHAIN_ID),
            Err(antelope::AntelopeError::BadChainId)
        ));
    }

    #[test]
    fn rejects_trailing_raw_transaction_bytes() {
        let mut transaction = canonical_transaction(Some(40));
        transaction.push(0);
        assert!(validate_raw_transaction(&transaction, 40).is_err());
    }

    #[test]
    fn rejects_oversized_raw_transaction() {
        let transaction = vec![0; MAX_RAW_TRANSACTION_BYTES + 1];
        let error = validate_raw_transaction(&transaction, 40).unwrap_err().to_string();
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn retries_only_transient_nodeos_statuses() {
        for status in [408, 425, 429, 502, 503, 504] {
            assert!(is_retryable_submission_error(&antelope::AntelopeError::Nodeos {
                status,
                body: String::new(),
            }));
        }
        for status in [200, 400, 401, 403, 404, 422, 500, 501, 505, 599] {
            assert!(!is_retryable_submission_error(&antelope::AntelopeError::Nodeos {
                status,
                body: String::new(),
            }));
        }
        assert!(is_retryable_submission_error(&antelope::AntelopeError::SubmissionTimeout));
    }

    #[test]
    fn truncates_untrusted_error_text() {
        let input = "x".repeat(MAX_NODEOS_ERROR_BODY_BYTES + 10);
        let output = truncate_error_text(&input);
        assert_eq!(output.len(), MAX_NODEOS_ERROR_BODY_BYTES);
        assert!(output.ends_with("[truncated]"));
    }
}

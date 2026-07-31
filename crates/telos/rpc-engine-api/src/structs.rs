use alloy_primitives::{Address, Bytes, Log, B256, U256};
use serde::{Deserialize, Serialize};

/// Maximum accepted serialized size of the Telos execution extension.
pub const MAX_EXTRA_FIELDS_BYTES: usize = 16 * 1024 * 1024;

/// Current block-bound execution metadata protocol.
pub const TELOS_EXECUTION_METADATA_VERSION: u8 = 3;

/// One native execution value change at a zero-based transaction boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosExecutionChange<T> {
    /// Boundary at which the new value becomes effective.
    pub boundary: u64,
    /// Value effective from this boundary onward.
    pub value: T,
}

/// Self-contained execution context for one exact payload.
///
/// A boundary of zero applies before transaction zero. A boundary equal to
/// `transaction_count` applies after the last transaction and becomes the child block's starting
/// context. The payload hashes prevent a valid sidecar for one fork from being replayed on another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosExecutionMetadataV3 {
    /// Schema version. Must equal [`TELOS_EXECUTION_METADATA_VERSION`].
    pub version: u8,
    /// Exact execution payload hash this metadata describes.
    pub block_hash: B256,
    /// Exact parent hash from the execution payload.
    pub parent_hash: B256,
    /// Number of transactions committed by the payload.
    pub transaction_count: u64,
    /// Base fee carried by the Engine payload but omitted from the canonical Telos header.
    pub execution_base_fee: U256,
    /// Gas price effective at boundary zero.
    pub starting_gas_price: U256,
    /// Native EVM revision effective at boundary zero.
    pub starting_revision: u64,
    /// Ordered gas-price changes after the starting value.
    pub gas_price_changes: Vec<TelosExecutionChange<U256>>,
    /// Ordered revision changes after the starting value.
    pub revision_changes: Vec<TelosExecutionChange<u64>>,
}

/// Telos EVM account-table row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosAccountTableRow {
    /// Whether this account was removed.
    pub removed: bool,
    /// EVM address.
    pub address: Address,
    /// Native account name associated with the address.
    pub account: String,
    /// Account nonce after this block.
    pub nonce: u64,
    /// Account bytecode after this block.
    pub code: Bytes,
    /// Account balance after this block.
    pub balance: U256,
}

/// Telos EVM account-state table row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosAccountStateTableRow {
    /// Whether this storage slot was removed.
    pub removed: bool,
    /// EVM address.
    pub address: Address,
    /// Storage key.
    pub key: U256,
    /// Storage value after this block.
    pub value: U256,
}

/// Transaction type representation accepted from existing Telos consensus clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TelosReceiptType {
    /// Named variant, such as `Legacy` or `Eip1559`.
    Name(String),
    /// EIP-2718 type byte.
    Number(u8),
}

impl Default for TelosReceiptType {
    fn default() -> Self {
        Self::Name("Legacy".to_string())
    }
}

/// Receipt produced by the Telos EVM contract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosExtraFieldReceipt {
    /// EIP-2718 transaction type.
    #[serde(alias = "txType")]
    pub tx_type: TelosReceiptType,
    /// Whether execution succeeded.
    pub success: bool,
    /// Cumulative gas used through this transaction.
    #[serde(alias = "cumulativeGasUsed")]
    pub cumulative_gas_used: u64,
    /// Logs emitted by the transaction.
    pub logs: Vec<Log>,
}

/// Versioned Telos extension sent as the second `engine_newPayloadV1` parameter.
///
/// The extension is bound to the execution payload by the authenticated JSON-RPC request that
/// carries both values. All collection fields required to reconstruct state are intentionally
/// optional at the serde boundary for compatibility, then required by validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosEngineApiExtraFields {
    /// Account table changes.
    pub statediffs_account: Option<Vec<TelosAccountTableRow>>,
    /// Storage table changes.
    pub statediffs_accountstate: Option<Vec<TelosAccountStateTableRow>>,
    /// Transaction index and new EVM revision.
    pub revision_changes: Option<(u64, u64)>,
    /// Transaction index and new gas price.
    pub gasprice_changes: Option<(u64, U256)>,
    /// Versioned, payload-bound execution context used by the production protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<TelosExecutionMetadataV3>,
    /// Addresses allocated by `create`, keyed by an inclusive native event boundary.
    ///
    /// Boundary `transaction_count` records a terminal event after the last EVM transaction and
    /// is applied during post-execution reconciliation.
    pub new_addresses_using_create: Option<Vec<(u64, U256)>>,
    /// Addresses allocated by `openwallet`, keyed by an inclusive native event boundary.
    pub new_addresses_using_openwallet: Option<Vec<(u64, U256)>>,
    /// Canonical receipts emitted by the Telos EVM contract.
    pub receipts: Option<Vec<TelosExtraFieldReceipt>>,
}

/// Backwards-compatible spelling used by the existing companion client.
pub type TelosEngineAPIExtraFields = TelosEngineApiExtraFields;

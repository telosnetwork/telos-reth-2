use alloy_primitives::{Address, Bytes, Log, U256};
use serde::{Deserialize, Serialize};

/// Maximum accepted serialized size of the Telos execution extension.
pub const MAX_EXTRA_FIELDS_BYTES: usize = 16 * 1024 * 1024;

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

/// Version-one Telos extension sent as the second `engine_newPayloadV1` parameter.
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
    /// Addresses allocated by `create`, keyed by transaction index.
    pub new_addresses_using_create: Option<Vec<(u64, U256)>>,
    /// Addresses allocated by `openwallet`, keyed by transaction index.
    pub new_addresses_using_openwallet: Option<Vec<(u64, U256)>>,
    /// Canonical receipts emitted by the Telos EVM contract.
    pub receipts: Option<Vec<TelosExtraFieldReceipt>>,
}

/// Backwards-compatible spelling used by the existing companion client.
pub type TelosEngineAPIExtraFields = TelosEngineApiExtraFields;

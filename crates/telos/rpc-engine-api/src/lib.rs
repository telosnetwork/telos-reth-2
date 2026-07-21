//! Telos Engine API execution extensions and state reconciliation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Authoritative state-diff reconciliation.
pub mod compare;
/// Engine payload wrapper that keeps Telos metadata attached in queues and reorgs.
pub mod payload;
/// Version-one Telos Engine API extension schema.
pub mod structs;

use alloy_consensus::TxType;
use alloy_primitives::{Address, U256};
use reth_ethereum_primitives::Receipt;
use std::collections::HashSet;
use structs::{
    TelosEngineApiExtraFields, TelosExtraFieldReceipt, TelosReceiptType, MAX_EXTRA_FIELDS_BYTES,
};
use thiserror::Error;

/// Errors that make a Telos execution extension unsafe to apply.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExtraFieldsError {
    /// A required collection is absent.
    #[error("missing required extra-fields collection `{0}`")]
    MissingField(&'static str),
    /// Receipt count does not match the payload transaction count.
    #[error("receipt count mismatch: expected {expected}, got {actual}")]
    ReceiptCount {
        /// Payload transaction count.
        expected: usize,
        /// Extension receipt count.
        actual: usize,
    },
    /// Receipt cumulative gas is not monotonic.
    #[error("receipt {index} has decreasing cumulative gas: previous {previous}, got {actual}")]
    NonMonotonicGas {
        /// Receipt index.
        index: usize,
        /// Previous cumulative gas.
        previous: u64,
        /// Current cumulative gas.
        actual: u64,
    },
    /// Final receipt gas does not match the payload.
    #[error("receipt gas mismatch: payload uses {expected}, receipts use {actual}")]
    GasUsed {
        /// Payload gas used.
        expected: u64,
        /// Last receipt cumulative gas.
        actual: u64,
    },
    /// A state-diff key appears more than once.
    #[error("duplicate {kind} row for account {address} key {key:?}")]
    DuplicateRow {
        /// Row kind.
        kind: &'static str,
        /// Account address.
        address: Address,
        /// Storage key, when applicable.
        key: Option<U256>,
    },
    /// A per-transaction event points outside this block.
    #[error("{kind} event index {index} is outside transaction count {transaction_count}")]
    InvalidEventIndex {
        /// Event kind.
        kind: &'static str,
        /// Event transaction index.
        index: u64,
        /// Payload transaction count.
        transaction_count: usize,
    },
    /// Events must be ordered so they can be applied deterministically.
    #[error("{0} event indexes are not sorted")]
    UnsortedEvents(&'static str),
    /// Extension exceeds the configured hard size limit.
    #[error("extra fields are too large: {actual} bytes exceeds {maximum}")]
    TooLarge {
        /// Encoded size.
        actual: usize,
        /// Maximum encoded size.
        maximum: usize,
    },
    /// Receipt type is unknown.
    #[error("unsupported receipt transaction type `{0}`")]
    UnsupportedReceiptType(String),
    /// The upstream execution backend cannot safely apply a Telos execution-context field.
    #[error(
        "Telos execution field `{0}` requires a verified revm backend with Telos transaction context"
    )]
    UnsupportedExecutionField(&'static str),
    /// Encoding the extension for a size check failed.
    #[error("failed to encode extra fields: {0}")]
    Encoding(String),
}

/// Validates the implicit version-one Telos extension schema against its payload.
pub fn validate_extra_fields(
    fields: &TelosEngineApiExtraFields,
    transaction_count: usize,
    gas_used: u64,
) -> Result<(), ExtraFieldsError> {
    let encoded =
        serde_json::to_vec(fields).map_err(|err| ExtraFieldsError::Encoding(err.to_string()))?;
    if encoded.len() > MAX_EXTRA_FIELDS_BYTES {
        return Err(ExtraFieldsError::TooLarge {
            actual: encoded.len(),
            maximum: MAX_EXTRA_FIELDS_BYTES,
        });
    }
    if fields.revision_changes.is_some() {
        return Err(ExtraFieldsError::UnsupportedExecutionField("revision_changes"))
    }
    if fields.gasprice_changes.is_some() {
        return Err(ExtraFieldsError::UnsupportedExecutionField("gasprice_changes"))
    }

    let accounts = fields
        .statediffs_account
        .as_ref()
        .ok_or(ExtraFieldsError::MissingField("statediffs_account"))?;
    let storage = fields
        .statediffs_accountstate
        .as_ref()
        .ok_or(ExtraFieldsError::MissingField("statediffs_accountstate"))?;
    let creates = fields
        .new_addresses_using_create
        .as_ref()
        .ok_or(ExtraFieldsError::MissingField("new_addresses_using_create"))?;
    let wallets = fields
        .new_addresses_using_openwallet
        .as_ref()
        .ok_or(ExtraFieldsError::MissingField("new_addresses_using_openwallet"))?;
    let receipts = fields.receipts.as_ref().ok_or(ExtraFieldsError::MissingField("receipts"))?;

    if receipts.len() != transaction_count {
        return Err(ExtraFieldsError::ReceiptCount {
            expected: transaction_count,
            actual: receipts.len(),
        });
    }

    let mut previous_gas = 0;
    for (index, receipt) in receipts.iter().enumerate() {
        receipt_type(&receipt.tx_type)?;
        if receipt.cumulative_gas_used < previous_gas {
            return Err(ExtraFieldsError::NonMonotonicGas {
                index,
                previous: previous_gas,
                actual: receipt.cumulative_gas_used,
            });
        }
        previous_gas = receipt.cumulative_gas_used;
    }
    if previous_gas != gas_used {
        return Err(ExtraFieldsError::GasUsed { expected: gas_used, actual: previous_gas });
    }

    let mut account_keys = HashSet::with_capacity(accounts.len());
    for row in accounts {
        if !account_keys.insert(row.address) {
            return Err(ExtraFieldsError::DuplicateRow {
                kind: "account",
                address: row.address,
                key: None,
            });
        }
    }

    let mut storage_keys = HashSet::with_capacity(storage.len());
    for row in storage {
        if !storage_keys.insert((row.address, row.key)) {
            return Err(ExtraFieldsError::DuplicateRow {
                kind: "storage",
                address: row.address,
                key: Some(row.key),
            });
        }
    }

    validate_events("create", creates, transaction_count)?;
    validate_events("openwallet", wallets, transaction_count)?;
    Ok(())
}

/// Converts validated Telos receipts to Reth receipts.
pub fn convert_receipts(
    receipts: &[TelosExtraFieldReceipt],
) -> Result<Vec<Receipt>, ExtraFieldsError> {
    receipts
        .iter()
        .map(|receipt| {
            Ok(Receipt {
                tx_type: receipt_type(&receipt.tx_type)?,
                success: receipt.success,
                cumulative_gas_used: receipt.cumulative_gas_used,
                logs: receipt.logs.clone(),
            })
        })
        .collect()
}

fn validate_events<T>(
    kind: &'static str,
    events: &[(u64, T)],
    transaction_count: usize,
) -> Result<(), ExtraFieldsError> {
    let mut previous = None;
    for (index, _) in events {
        let index = *index;
        if index > transaction_count as u64 {
            return Err(ExtraFieldsError::InvalidEventIndex { kind, index, transaction_count });
        }
        if previous.is_some_and(|previous| index < previous) {
            return Err(ExtraFieldsError::UnsortedEvents(kind));
        }
        previous = Some(index);
    }
    Ok(())
}

fn receipt_type(receipt_type: &TelosReceiptType) -> Result<TxType, ExtraFieldsError> {
    let value = match receipt_type {
        TelosReceiptType::Number(value) => *value,
        TelosReceiptType::Name(name) => match name.as_str() {
            "Legacy" | "legacy" | "0x0" | "0" => 0,
            "Eip2930" | "eip2930" | "0x1" | "1" => 1,
            "Eip1559" | "eip1559" | "0x2" | "2" => 2,
            "Eip4844" | "eip4844" | "0x3" | "3" => 3,
            "Eip7702" | "eip7702" | "0x4" | "4" => 4,
            _ => return Err(ExtraFieldsError::UnsupportedReceiptType(name.clone())),
        },
    };
    match value {
        0 => Ok(TxType::Legacy),
        1 => Ok(TxType::Eip2930),
        2 => Ok(TxType::Eip1559),
        3 => Ok(TxType::Eip4844),
        4 => Ok(TxType::Eip7702),
        _ => Err(ExtraFieldsError::UnsupportedReceiptType(value.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(receipts: Vec<TelosExtraFieldReceipt>) -> TelosEngineApiExtraFields {
        TelosEngineApiExtraFields {
            statediffs_account: Some(Vec::new()),
            statediffs_accountstate: Some(Vec::new()),
            new_addresses_using_create: Some(Vec::new()),
            new_addresses_using_openwallet: Some(Vec::new()),
            receipts: Some(receipts),
            ..Default::default()
        }
    }

    #[test]
    fn requires_complete_receipts() {
        let error = validate_extra_fields(&fields(Vec::new()), 1, 21_000).unwrap_err();
        assert_eq!(error, ExtraFieldsError::ReceiptCount { expected: 1, actual: 0 });
    }

    #[test]
    fn requires_final_gas_to_match_payload() {
        let receipt = TelosExtraFieldReceipt { cumulative_gas_used: 20_999, ..Default::default() };
        let error = validate_extra_fields(&fields(vec![receipt]), 1, 21_000).unwrap_err();
        assert_eq!(error, ExtraFieldsError::GasUsed { expected: 21_000, actual: 20_999 });
    }

    #[test]
    fn rejects_unknown_receipt_types() {
        let receipt = TelosExtraFieldReceipt {
            tx_type: TelosReceiptType::Name("future-type".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            validate_extra_fields(&fields(vec![receipt]), 1, 0),
            Err(ExtraFieldsError::UnsupportedReceiptType(_))
        ));
    }

    #[test]
    fn rejects_unimplemented_telos_execution_context() {
        let mut fields = fields(Vec::new());
        fields.revision_changes = Some((0, 1));
        assert_eq!(
            validate_extra_fields(&fields, 0, 0),
            Err(ExtraFieldsError::UnsupportedExecutionField("revision_changes"))
        );
    }

    #[test]
    fn rejects_duplicate_state_rows() {
        let mut fields = fields(Vec::new());
        fields.statediffs_account = Some(vec![Default::default(), Default::default()]);
        assert!(matches!(
            validate_extra_fields(&fields, 0, 0),
            Err(ExtraFieldsError::DuplicateRow { kind: "account", .. })
        ));
    }

    #[test]
    fn accepts_post_block_event_at_transaction_count() {
        let mut fields = fields(vec![Default::default()]);
        fields.new_addresses_using_create = Some(vec![(1, U256::ZERO)]);
        assert!(validate_extra_fields(&fields, 1, 0).is_ok());
    }

    #[test]
    fn rejects_event_above_transaction_count() {
        let mut fields = fields(vec![Default::default()]);
        fields.new_addresses_using_create = Some(vec![(2, U256::ZERO)]);
        assert_eq!(
            validate_extra_fields(&fields, 1, 0),
            Err(ExtraFieldsError::InvalidEventIndex {
                kind: "create",
                index: 2,
                transaction_count: 1,
            })
        );
    }
}

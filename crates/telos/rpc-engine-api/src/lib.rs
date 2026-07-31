//! Telos Engine API execution extensions and state reconciliation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

/// Authoritative state-diff reconciliation.
pub mod compare;
/// Engine payload wrapper that keeps Telos metadata attached in queues and reorgs.
pub mod payload;
/// Version-one Telos Engine API extension schema.
pub mod structs;

use alloy_consensus::TxType;
use alloy_primitives::{Address, B256, U256};
use reth_ethereum_primitives::Receipt;
use std::collections::HashSet;
use structs::{
    TelosEngineApiExtraFields, TelosExecutionChange, TelosExecutionMetadataV3,
    TelosExtraFieldReceipt, TelosReceiptType, MAX_EXTRA_FIELDS_BYTES,
    TELOS_EXECUTION_METADATA_VERSION,
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
    /// The receipt type does not match Telos's canonical legacy receipt encoding.
    #[error(
        "receipt transaction type `{0}` is incompatible with canonical Telos receipt encoding"
    )]
    NonLegacyReceiptType(String),
    /// An ambiguous legacy scalar was supplied instead of the versioned execution metadata.
    #[error("legacy Telos execution field `{0}` is unsupported; use payload-bound V3 metadata")]
    UnsupportedExecutionField(&'static str),
    /// A versioned execution sidecar used an unknown schema.
    #[error("unsupported Telos execution metadata version {actual}; expected {expected}")]
    UnsupportedExecutionVersion {
        /// Required version.
        expected: u8,
        /// Received version.
        actual: u8,
    },
    /// The sidecar does not describe the payload transported with it.
    #[error("execution metadata {kind} mismatch: expected {expected}, got {actual}")]
    PayloadBinding {
        /// Bound payload field.
        kind: &'static str,
        /// Value from the payload.
        expected: B256,
        /// Value from the sidecar.
        actual: B256,
    },
    /// The metadata transaction count does not match the payload.
    #[error("execution metadata transaction count mismatch: expected {expected}, got {actual}")]
    ExecutionTransactionCount {
        /// Payload transaction count.
        expected: usize,
        /// Sidecar transaction count.
        actual: u64,
    },
    /// The payload-only execution base fee is not bound by the versioned metadata.
    #[error("execution metadata base fee mismatch: expected {expected}, got {actual}")]
    ExecutionBaseFee {
        /// Base fee carried by the payload.
        expected: U256,
        /// Base fee committed by the metadata.
        actual: U256,
    },
    /// The payload-only execution base fee cannot be represented by revm.
    #[error("execution base fee exceeds 64 bits")]
    ExecutionBaseFeeOverflow,
    /// A change list contains a duplicate or decreasing boundary.
    #[error("{0} change boundaries must be strictly increasing")]
    InvalidChangeOrder(&'static str),
    /// A gas price cannot be represented by revm's consensus transaction environment.
    #[error("{kind} gas price exceeds 128 bits")]
    GasPriceOverflow {
        /// Gas-price field or change list.
        kind: &'static str,
    },
    /// Encoding the extension for a size check failed.
    #[error("failed to encode extra fields: {0}")]
    Encoding(String),
}

/// Validates Telos extension collections and any included execution metadata.
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
    if let Some(execution) = &fields.execution {
        validate_execution_metadata(execution, transaction_count)?;
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
        legacy_receipt_type(&receipt.tx_type)?;
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

/// Validates all extension fields and requires metadata bound to this exact payload.
pub fn validate_extra_fields_for_payload(
    fields: &TelosEngineApiExtraFields,
    transaction_count: usize,
    gas_used: u64,
    execution_base_fee: U256,
    block_hash: B256,
    parent_hash: B256,
) -> Result<(), ExtraFieldsError> {
    validate_extra_fields(fields, transaction_count, gas_used)?;
    let execution = fields.execution.as_ref().ok_or(ExtraFieldsError::MissingField("execution"))?;
    if execution.block_hash != block_hash {
        return Err(ExtraFieldsError::PayloadBinding {
            kind: "block hash",
            expected: block_hash,
            actual: execution.block_hash,
        })
    }
    if execution.parent_hash != parent_hash {
        return Err(ExtraFieldsError::PayloadBinding {
            kind: "parent hash",
            expected: parent_hash,
            actual: execution.parent_hash,
        })
    }
    if execution.execution_base_fee != execution_base_fee {
        return Err(ExtraFieldsError::ExecutionBaseFee {
            expected: execution_base_fee,
            actual: execution.execution_base_fee,
        })
    }
    Ok(())
}

fn validate_execution_metadata(
    execution: &TelosExecutionMetadataV3,
    transaction_count: usize,
) -> Result<(), ExtraFieldsError> {
    if execution.version != TELOS_EXECUTION_METADATA_VERSION {
        return Err(ExtraFieldsError::UnsupportedExecutionVersion {
            expected: TELOS_EXECUTION_METADATA_VERSION,
            actual: execution.version,
        })
    }
    if execution.transaction_count != transaction_count as u64 {
        return Err(ExtraFieldsError::ExecutionTransactionCount {
            expected: transaction_count,
            actual: execution.transaction_count,
        })
    }
    if execution.execution_base_fee.bit_len() > u64::BITS as usize {
        return Err(ExtraFieldsError::ExecutionBaseFeeOverflow)
    }
    if execution.starting_gas_price.bit_len() > u128::BITS as usize {
        return Err(ExtraFieldsError::GasPriceOverflow { kind: "starting" })
    }
    validate_execution_changes("gas price", &execution.gas_price_changes, transaction_count)?;
    if execution.gas_price_changes.iter().any(|change| change.value.bit_len() > u128::BITS as usize)
    {
        return Err(ExtraFieldsError::GasPriceOverflow { kind: "changed" })
    }
    validate_execution_changes("revision", &execution.revision_changes, transaction_count)
}

fn validate_execution_changes<T>(
    kind: &'static str,
    changes: &[TelosExecutionChange<T>],
    transaction_count: usize,
) -> Result<(), ExtraFieldsError> {
    let mut previous = None;
    for change in changes {
        if change.boundary > transaction_count as u64 {
            return Err(ExtraFieldsError::InvalidEventIndex {
                kind,
                index: change.boundary,
                transaction_count,
            })
        }
        if previous.is_some_and(|previous| change.boundary <= previous) {
            return Err(ExtraFieldsError::InvalidChangeOrder(kind))
        }
        previous = Some(change.boundary);
    }
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
                tx_type: legacy_receipt_type(&receipt.tx_type)?,
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

fn legacy_receipt_type(encoded_type: &TelosReceiptType) -> Result<TxType, ExtraFieldsError> {
    let tx_type = receipt_type(encoded_type)?;
    if tx_type != TxType::Legacy {
        return Err(ExtraFieldsError::NonLegacyReceiptType(format!("{tx_type:?}")))
    }
    Ok(tx_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use structs::{
        TelosExecutionChange, TelosExecutionMetadataV3, TELOS_EXECUTION_METADATA_VERSION,
    };

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

    fn execution(
        block_hash: B256,
        parent_hash: B256,
        transaction_count: u64,
    ) -> TelosExecutionMetadataV3 {
        TelosExecutionMetadataV3 {
            version: TELOS_EXECUTION_METADATA_VERSION,
            block_hash,
            parent_hash,
            transaction_count,
            execution_base_fee: U256::from(7),
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
    fn rejects_typed_receipts_for_legacy_only_payloads() {
        for tx_type in [TelosReceiptType::Number(1), TelosReceiptType::Name("Eip1559".to_string())]
        {
            let receipt = TelosExtraFieldReceipt { tx_type, ..Default::default() };
            assert!(matches!(
                validate_extra_fields(&fields(vec![receipt]), 1, 0),
                Err(ExtraFieldsError::NonLegacyReceiptType(_))
            ));
        }
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

    #[test]
    fn payload_validation_requires_bound_execution_metadata() {
        let block_hash = B256::repeat_byte(0x11);
        let parent_hash = B256::repeat_byte(0x22);
        let mut fields = fields(Vec::new());
        assert_eq!(
            validate_extra_fields_for_payload(
                &fields,
                0,
                0,
                U256::from(7),
                block_hash,
                parent_hash,
            ),
            Err(ExtraFieldsError::MissingField("execution"))
        );

        fields.execution = Some(execution(B256::ZERO, parent_hash, 0));
        assert_eq!(
            validate_extra_fields_for_payload(
                &fields,
                0,
                0,
                U256::from(7),
                block_hash,
                parent_hash,
            ),
            Err(ExtraFieldsError::PayloadBinding {
                kind: "block hash",
                expected: block_hash,
                actual: B256::ZERO,
            })
        );

        fields.execution.as_mut().unwrap().block_hash = block_hash;
        fields.execution.as_mut().unwrap().execution_base_fee = U256::from(8);
        assert_eq!(
            validate_extra_fields_for_payload(
                &fields,
                0,
                0,
                U256::from(7),
                block_hash,
                parent_hash,
            ),
            Err(ExtraFieldsError::ExecutionBaseFee {
                expected: U256::from(7),
                actual: U256::from(8),
            })
        );
    }

    #[test]
    fn rejects_execution_base_fee_that_revm_would_saturate() {
        let block_hash = B256::repeat_byte(0x11);
        let parent_hash = B256::repeat_byte(0x22);
        let mut fields = fields(Vec::new());
        let mut metadata = execution(block_hash, parent_hash, 0);
        metadata.execution_base_fee = U256::from(u64::MAX) + U256::from(1);
        fields.execution = Some(metadata);

        assert_eq!(
            validate_extra_fields_for_payload(
                &fields,
                0,
                0,
                U256::from(u64::MAX) + U256::from(1),
                block_hash,
                parent_hash,
            ),
            Err(ExtraFieldsError::ExecutionBaseFeeOverflow)
        );
    }

    #[test]
    fn execution_changes_use_strict_zero_based_boundaries() {
        let block_hash = B256::repeat_byte(0x11);
        let parent_hash = B256::repeat_byte(0x22);
        let mut fields = fields(vec![Default::default()]);
        let mut metadata = execution(block_hash, parent_hash, 1);
        metadata.revision_changes = vec![
            TelosExecutionChange { boundary: 0, value: 1 },
            TelosExecutionChange { boundary: 1, value: 2 },
        ];
        fields.execution = Some(metadata);
        assert!(validate_extra_fields_for_payload(
            &fields,
            1,
            0,
            U256::from(7),
            block_hash,
            parent_hash,
        )
        .is_ok());

        fields.execution.as_mut().unwrap().revision_changes[1].boundary = 0;
        assert_eq!(
            validate_extra_fields_for_payload(
                &fields,
                1,
                0,
                U256::from(7),
                block_hash,
                parent_hash,
            ),
            Err(ExtraFieldsError::InvalidChangeOrder("revision"))
        );
    }
}

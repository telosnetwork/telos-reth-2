//! Telos-aware RPC receipt conversion.

use crate::{
    execution::{TelosBlockExecutionSchedule, TelosScheduleError},
    sidecar::{
        validate_accepted_sidecar_continuity, TelosExecutionAnchor, TelosSidecarError,
        TelosSidecarStore,
    },
};
use alloy_consensus::{ReceiptEnvelope, Transaction, TxType};
use alloy_eips::Typed2718;
use alloy_primitives::B256;
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use reth_chainspec::EthChainSpec;
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_primitives_traits::SealedBlock;
use reth_rpc_convert::transaction::{ConvertReceiptInput, ReceiptConverter};
use reth_rpc_eth_types::{receipt::EthReceiptConverter, EthApiError};
use std::sync::Arc;

/// RPC receipt converter that derives Telos's charged gas price from authenticated sidecars.
pub struct TelosReceiptConverter<ChainSpec> {
    inner: EthReceiptConverter<ChainSpec>,
    sidecar_store: Arc<dyn TelosSidecarStore>,
    execution_anchor: TelosExecutionAnchor,
}

impl<ChainSpec> Clone for TelosReceiptConverter<ChainSpec> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            sidecar_store: self.sidecar_store.clone(),
            execution_anchor: self.execution_anchor,
        }
    }
}

impl<ChainSpec> std::fmt::Debug for TelosReceiptConverter<ChainSpec> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelosReceiptConverter")
            .field("execution_anchor", &self.execution_anchor)
            .finish_non_exhaustive()
    }
}

impl<ChainSpec> TelosReceiptConverter<ChainSpec> {
    /// Creates a receipt converter bound to one chain's durable execution sidecars.
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        sidecar_store: Arc<dyn TelosSidecarStore>,
        execution_anchor: TelosExecutionAnchor,
    ) -> Self {
        Self { inner: EthReceiptConverter::new(chain_spec), sidecar_store, execution_anchor }
    }

    fn execution_context(
        &self,
        binding: BlockBinding,
    ) -> Result<TelosReceiptExecutionContext, TelosReceiptConversionError> {
        self.execution_anchor.validate_for_chain(self.sidecar_store.chain_identity())?;
        if binding.number <= self.execution_anchor.parent_block_number {
            return Err(TelosReceiptConversionError::PreAnchorBlock {
                block_number: binding.number,
                anchor_number: self.execution_anchor.parent_block_number,
            })
        }

        let sidecar = self
            .sidecar_store
            .get_accepted_by_hash(binding.hash)?
            .ok_or(TelosReceiptConversionError::MissingSidecar(binding.hash))?;
        let envelope = sidecar.envelope();
        if envelope.block_hash != binding.hash {
            return Err(TelosReceiptConversionError::BlockHashMismatch {
                expected: binding.hash,
                actual: envelope.block_hash,
            })
        }
        if envelope.block_number != binding.number {
            return Err(TelosReceiptConversionError::BlockNumberMismatch {
                block_hash: binding.hash,
                expected: binding.number,
                actual: envelope.block_number,
            })
        }
        if let Some(transaction_count) = binding.transaction_count {
            let actual = usize::try_from(envelope.transaction_count).map_err(|_| {
                TelosReceiptConversionError::TransactionCountOverflow(envelope.transaction_count)
            })?;
            if actual != transaction_count {
                return Err(TelosReceiptConversionError::TransactionCountMismatch {
                    block_hash: binding.hash,
                    expected: transaction_count,
                    actual,
                })
            }
        }

        validate_accepted_sidecar_continuity(
            self.sidecar_store.as_ref(),
            &self.execution_anchor,
            &sidecar,
        )?;
        let execution = envelope
            .extra_fields
            .execution
            .as_ref()
            .ok_or(TelosReceiptConversionError::MissingExecutionMetadata(binding.hash))?;
        Ok(TelosReceiptExecutionContext {
            schedule: TelosBlockExecutionSchedule::from_metadata(execution)?,
        })
    }

    fn convert_bound(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, EthPrimitives>>,
        block: Option<BlockBinding>,
    ) -> Result<Vec<TransactionReceipt<ReceiptEnvelope<Log>>>, EthApiError>
    where
        ChainSpec: EthChainSpec + 'static,
    {
        if let Some(expected) = block.and_then(|binding| binding.transaction_count) {
            if inputs.len() != expected {
                return Err(rpc_error(TelosReceiptConversionError::ReceiptInputCountMismatch {
                    expected,
                    actual: inputs.len(),
                }))
            }
            if expected == 0 {
                return Ok(Vec::new())
            }
        } else if inputs.is_empty() {
            return Ok(Vec::new())
        }

        let binding = block.unwrap_or_else(|| {
            let meta = inputs[0].meta;
            BlockBinding {
                hash: meta.block_hash,
                number: meta.block_number,
                transaction_count: None,
            }
        });
        let execution = self.execution_context(binding).map_err(rpc_error)?;
        let mut effective_gas_prices = Vec::with_capacity(inputs.len());
        let mut transaction_types = Vec::with_capacity(inputs.len());

        for input in &inputs {
            if input.meta.block_hash != binding.hash || input.meta.block_number != binding.number {
                return Err(rpc_error(TelosReceiptConversionError::MixedBlockInputs {
                    expected_hash: binding.hash,
                    expected_number: binding.number,
                    actual_hash: input.meta.block_hash,
                    actual_number: input.meta.block_number,
                }))
            }
            let tx_type = match input.tx.ty() {
                0 => TxType::Legacy,
                tx_type => {
                    return Err(rpc_error(TelosReceiptConversionError::UnsupportedTransactionType(
                        tx_type,
                    )))
                }
            };
            if !input.receipt.tx_type.is_legacy() {
                return Err(rpc_error(TelosReceiptConversionError::NonLegacyStoredReceipt {
                    index: input.meta.index,
                    tx_type: input.receipt.tx_type,
                }))
            }
            let transaction_index = usize::try_from(input.meta.index).map_err(|_| {
                rpc_error(TelosReceiptConversionError::TransactionIndexOverflow(input.meta.index))
            })?;
            let context = execution
                .schedule
                .context_for_transaction(transaction_index)
                .map_err(TelosReceiptConversionError::from)
                .map_err(rpc_error)?;
            let effective_gas_price = input
                .tx
                .gas_price()
                .ok_or_else(|| {
                    rpc_error(TelosReceiptConversionError::MissingLegacyGasPrice(input.meta.index))
                })?
                .min(context.fixed_gas_price);
            effective_gas_prices.push(effective_gas_price);
            transaction_types.push(tx_type);
        }

        let receipts = self.inner.convert_receipts(inputs)?;
        if receipts.len() != effective_gas_prices.len() {
            return Err(rpc_error(TelosReceiptConversionError::ReceiptCountMismatch {
                expected: effective_gas_prices.len(),
                actual: receipts.len(),
            }))
        }
        let mut converted = Vec::with_capacity(receipts.len());
        for ((mut receipt, effective_gas_price), tx_type) in
            receipts.into_iter().zip(effective_gas_prices).zip(transaction_types)
        {
            let canonical = match receipt.inner {
                ReceiptEnvelope::Legacy(receipt) => receipt,
                receipt => {
                    return Err(rpc_error(TelosReceiptConversionError::NonLegacyStoredReceipt {
                        index: u64::try_from(converted.len()).unwrap_or(u64::MAX),
                        tx_type: receipt.tx_type(),
                    }))
                }
            };
            receipt.inner = ReceiptEnvelope::from_typed(tx_type, canonical);
            receipt.effective_gas_price = effective_gas_price;
            converted.push(receipt);
        }
        Ok(converted)
    }
}

impl<ChainSpec> ReceiptConverter<EthPrimitives> for TelosReceiptConverter<ChainSpec>
where
    ChainSpec: EthChainSpec + 'static,
{
    type RpcReceipt = TransactionReceipt<ReceiptEnvelope<Log>>;
    type Error = EthApiError;

    fn convert_receipts(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, EthPrimitives>>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        self.convert_bound(inputs, None)
    }

    fn convert_receipts_with_block(
        &self,
        inputs: Vec<ConvertReceiptInput<'_, EthPrimitives>>,
        block: &SealedBlock<Block>,
    ) -> Result<Vec<Self::RpcReceipt>, Self::Error> {
        self.convert_bound(
            inputs,
            Some(BlockBinding {
                hash: block.hash(),
                number: block.header().number,
                transaction_count: Some(block.transaction_count()),
            }),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct BlockBinding {
    hash: B256,
    number: u64,
    transaction_count: Option<usize>,
}

#[derive(Clone, Debug)]
struct TelosReceiptExecutionContext {
    schedule: TelosBlockExecutionSchedule,
}

/// A canonical receipt could not be represented truthfully through Ethereum RPC.
#[derive(Debug, thiserror::Error)]
enum TelosReceiptConversionError {
    #[error("block {block_number} is not after Telos execution anchor {anchor_number}")]
    PreAnchorBlock { block_number: u64, anchor_number: u64 },
    #[error("missing durable Telos execution sidecar for receipt block {0}")]
    MissingSidecar(B256),
    #[error("Telos receipt sidecar hash mismatch: expected {expected}, got {actual}")]
    BlockHashMismatch { expected: B256, actual: B256 },
    #[error(
        "Telos receipt sidecar block-number mismatch for {block_hash}: expected {expected}, got {actual}"
    )]
    BlockNumberMismatch { block_hash: B256, expected: u64, actual: u64 },
    #[error(
        "Telos receipt sidecar transaction-count mismatch for {block_hash}: expected {expected}, got {actual}"
    )]
    TransactionCountMismatch { block_hash: B256, expected: usize, actual: usize },
    #[error("Telos receipt sidecar transaction count {0} cannot be represented")]
    TransactionCountOverflow(u64),
    #[error("missing Telos execution metadata for receipt block {0}")]
    MissingExecutionMetadata(B256),
    #[error(
        "mixed receipt block metadata: expected {expected_number} ({expected_hash}), got {actual_number} ({actual_hash})"
    )]
    MixedBlockInputs {
        expected_hash: B256,
        expected_number: u64,
        actual_hash: B256,
        actual_number: u64,
    },
    #[error("Telos RPC receipt conversion rejects unsupported transaction type {0}")]
    UnsupportedTransactionType(u8),
    #[error(
        "stored Telos receipt {index} uses {tx_type:?}; canonical execution receipts must be legacy encoded"
    )]
    NonLegacyStoredReceipt { index: u64, tx_type: TxType },
    #[error("receipt transaction index {0} cannot be represented")]
    TransactionIndexOverflow(u64),
    #[error("legacy receipt transaction {0} has no signed gas price")]
    MissingLegacyGasPrice(u64),
    #[error("receipt converter returned {actual} receipts for {expected} inputs")]
    ReceiptCountMismatch { expected: usize, actual: usize },
    #[error(
        "receipt conversion received {actual} inputs for a block with {expected} transactions"
    )]
    ReceiptInputCountMismatch { expected: usize, actual: usize },
    #[error(transparent)]
    Sidecar(#[from] TelosSidecarError),
    #[error(transparent)]
    Schedule(#[from] TelosScheduleError),
}

fn rpc_error(error: TelosReceiptConversionError) -> EthApiError {
    EthApiError::EvmCustom(format!("Telos receipt conversion failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::{
        InMemoryTelosSidecarStore, TelosChainIdentity, TelosExecutionSidecar,
        TELOS_EXECUTION_ANCHOR_VERSION,
    };
    use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy, TxType};
    use alloy_genesis::Genesis;
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use reth_chainspec::{Chain, ChainSpec};
    use reth_ethereum_primitives::{Receipt, TransactionSigned};
    use reth_primitives_traits::{Recovered, TransactionMeta};
    use reth_telos_rpc_engine_api::structs::{
        TelosEngineApiExtraFields, TelosExecutionChange, TelosExecutionMetadataV3,
        TelosExtraFieldReceipt, TelosReceiptType, TELOS_EXECUTION_METADATA_VERSION,
    };

    fn legacy_transaction(gas_price: u128) -> TransactionSigned {
        TxLegacy {
            chain_id: Some(3),
            gas_price,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x44)),
            ..Default::default()
        }
        .into_signed(Signature::new(U256::from(1), U256::from(2), false))
        .into()
    }

    fn eip1559_transaction(
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> TransactionSigned {
        TxEip1559 {
            chain_id: 40,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x44)),
            ..Default::default()
        }
        .into_signed(Signature::test_signature())
        .into()
    }

    fn receipt(cumulative_gas_used: u64) -> Receipt {
        Receipt { tx_type: TxType::Legacy, success: true, cumulative_gas_used, logs: Vec::new() }
    }

    fn setup(
        anchor_number: u64,
    ) -> (TelosReceiptConverter<ChainSpec>, Arc<InMemoryTelosSidecarStore>, TelosExecutionAnchor)
    {
        let chain_spec = Arc::new(
            ChainSpec::builder().chain(Chain::from_id(40)).genesis(Genesis::default()).build(),
        );
        let chain = TelosChainIdentity { chain_id: 40, genesis_hash: chain_spec.genesis_hash() };
        let store = Arc::new(InMemoryTelosSidecarStore::new(chain));
        let anchor = TelosExecutionAnchor {
            version: TELOS_EXECUTION_ANCHOR_VERSION,
            chain,
            parent_block_number: anchor_number,
            parent_block_hash: B256::repeat_byte(0x10),
            starting_gas_price: U256::from(7),
            starting_revision: 0,
        };
        let converter = TelosReceiptConverter::new(chain_spec, store.clone(), anchor);
        (converter, store, anchor)
    }

    fn insert_two_transaction_sidecar(
        store: &InMemoryTelosSidecarStore,
        anchor: TelosExecutionAnchor,
        block_hash: B256,
        execution_base_fee: U256,
    ) {
        let fields = TelosEngineApiExtraFields {
            statediffs_account: Some(Vec::new()),
            statediffs_accountstate: Some(Vec::new()),
            execution: Some(TelosExecutionMetadataV3 {
                version: TELOS_EXECUTION_METADATA_VERSION,
                block_hash,
                parent_hash: anchor.parent_block_hash,
                transaction_count: 2,
                execution_base_fee,
                starting_gas_price: anchor.starting_gas_price,
                starting_revision: 0,
                gas_price_changes: vec![TelosExecutionChange {
                    boundary: 1,
                    value: U256::from(50),
                }],
                revision_changes: Vec::new(),
            }),
            new_addresses_using_create: Some(Vec::new()),
            new_addresses_using_openwallet: Some(Vec::new()),
            receipts: Some(vec![
                TelosExtraFieldReceipt {
                    tx_type: TelosReceiptType::Number(0),
                    success: true,
                    cumulative_gas_used: 21_000,
                    logs: Vec::new(),
                },
                TelosExtraFieldReceipt {
                    tx_type: TelosReceiptType::Number(0),
                    success: true,
                    cumulative_gas_used: 42_000,
                    logs: Vec::new(),
                },
            ]),
            ..Default::default()
        };
        let sidecar = TelosExecutionSidecar::new(
            anchor.chain,
            anchor.parent_block_number + 1,
            block_hash,
            anchor.parent_block_hash,
            2,
            42_000,
            fields,
        )
        .unwrap();
        store.put_pending(&sidecar).unwrap();
        store.mark_dispatched(block_hash, sidecar.digest()).unwrap();
        store.mark_accepted(block_hash, sidecar.digest()).unwrap();
    }

    #[test]
    fn effective_gas_price_uses_each_transaction_boundary_and_signed_cap() {
        let (converter, store, anchor) = setup(0);
        let block_hash = B256::repeat_byte(0x22);
        insert_two_transaction_sidecar(store.as_ref(), anchor, block_hash, U256::ZERO);
        let first = legacy_transaction(100);
        let second = legacy_transaction(30);
        let inputs = vec![
            ConvertReceiptInput {
                receipt: receipt(21_000),
                tx: Recovered::new_unchecked(&first, Address::repeat_byte(0x11)),
                gas_used: 21_000,
                next_log_index: 0,
                meta: TransactionMeta {
                    tx_hash: B256::repeat_byte(0x31),
                    index: 0,
                    block_hash,
                    block_number: 1,
                    ..Default::default()
                },
            },
            ConvertReceiptInput {
                receipt: receipt(42_000),
                tx: Recovered::new_unchecked(&second, Address::repeat_byte(0x12)),
                gas_used: 21_000,
                next_log_index: 0,
                meta: TransactionMeta {
                    tx_hash: B256::repeat_byte(0x32),
                    index: 1,
                    block_hash,
                    block_number: 1,
                    ..Default::default()
                },
            },
        ];

        let receipts = converter.convert_receipts(inputs).unwrap();
        assert_eq!(receipts[0].effective_gas_price, 7);
        assert_eq!(receipts[1].effective_gas_price, 30);
    }

    #[test]
    fn eip1559_rpc_receipt_is_rejected_until_qualified_activation() {
        // The only reviewed type-2 vector is the undeployed companion test fixture
        // `translator/tests/common/testcontainer-actions-v1.5.json`. Production history contains
        // no activation evidence, so receipt conversion fails closed instead of interpreting it.
        let (converter, store, anchor) = setup(0);
        let block_hash = B256::repeat_byte(0x22);
        insert_two_transaction_sidecar(store.as_ref(), anchor, block_hash, U256::from(10));
        let transaction = eip1559_transaction(113_378_400_388, 0);
        let error = converter
            .convert_receipts(vec![ConvertReceiptInput {
                receipt: receipt(21_000),
                tx: Recovered::new_unchecked(&transaction, Address::repeat_byte(0x11)),
                gas_used: 21_000,
                next_log_index: 0,
                meta: TransactionMeta {
                    tx_hash: B256::repeat_byte(0x31),
                    index: 0,
                    block_hash,
                    block_number: 1,
                    ..Default::default()
                },
            }])
            .unwrap_err();

        assert!(error.to_string().contains("unsupported transaction type 2"));
    }

    #[test]
    fn rpc_conversion_rejects_nonlegacy_canonical_receipt_storage() {
        let (converter, store, anchor) = setup(0);
        let block_hash = B256::repeat_byte(0x22);
        insert_two_transaction_sidecar(store.as_ref(), anchor, block_hash, U256::from(10));
        let transaction = legacy_transaction(100);
        let error = converter
            .convert_receipts(vec![ConvertReceiptInput {
                receipt: Receipt {
                    tx_type: TxType::Eip1559,
                    success: true,
                    cumulative_gas_used: 21_000,
                    logs: Vec::new(),
                },
                tx: Recovered::new_unchecked(&transaction, Address::repeat_byte(0x11)),
                gas_used: 21_000,
                next_log_index: 0,
                meta: TransactionMeta {
                    tx_hash: B256::repeat_byte(0x31),
                    index: 0,
                    block_hash,
                    block_number: 1,
                    ..Default::default()
                },
            }])
            .unwrap_err();

        assert!(error.to_string().contains("canonical execution receipts must be legacy"));
    }

    #[test]
    fn missing_sidecar_fails_closed() {
        let (converter, _, _) = setup(0);
        let transaction = legacy_transaction(100);
        let error = converter
            .convert_receipts(vec![ConvertReceiptInput {
                receipt: receipt(21_000),
                tx: Recovered::new_unchecked(&transaction, Address::repeat_byte(0x11)),
                gas_used: 21_000,
                next_log_index: 0,
                meta: TransactionMeta {
                    index: 0,
                    block_hash: B256::repeat_byte(0x22),
                    block_number: 1,
                    ..Default::default()
                },
            }])
            .unwrap_err();

        assert!(error.to_string().contains("missing durable Telos execution sidecar"));
    }

    #[test]
    fn pre_anchor_receipt_fails_closed_before_sidecar_lookup() {
        let (converter, _, _) = setup(10);
        let transaction = legacy_transaction(100);
        let error = converter
            .convert_receipts(vec![ConvertReceiptInput {
                receipt: receipt(21_000),
                tx: Recovered::new_unchecked(&transaction, Address::repeat_byte(0x11)),
                gas_used: 21_000,
                next_log_index: 0,
                meta: TransactionMeta {
                    index: 0,
                    block_hash: B256::repeat_byte(0x22),
                    block_number: 10,
                    ..Default::default()
                },
            }])
            .unwrap_err();

        assert!(error.to_string().contains("not after Telos execution anchor 10"));
    }

    #[test]
    fn empty_anchor_block_has_an_empty_receipt_list_without_a_sidecar() {
        let (converter, _, anchor) = setup(10);
        let receipts = converter
            .convert_bound(
                Vec::new(),
                Some(BlockBinding {
                    hash: anchor.parent_block_hash,
                    number: anchor.parent_block_number,
                    transaction_count: Some(0),
                }),
            )
            .unwrap();
        assert!(receipts.is_empty());
    }

    #[test]
    fn block_receipt_conversion_rejects_missing_inputs() {
        let (converter, _, anchor) = setup(10);
        let error = converter
            .convert_bound(
                Vec::new(),
                Some(BlockBinding {
                    hash: B256::repeat_byte(0x22),
                    number: anchor.parent_block_number + 1,
                    transaction_count: Some(1),
                }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("received 0 inputs for a block with 1 transactions"));
    }
}

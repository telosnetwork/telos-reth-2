//! Telos EVM configuration and authenticated post-execution reconciliation.

use crate::{
    block::{TelosBlockAssembler, TelosBlockExecutionCtx, TelosBlockExecutorFactory},
    engine::recover_telos_sender,
    execution::{TelosBlockExecutionSchedule, TelosEvmFactory, TelosScheduleError, TelosTxEnv},
    sidecar::{
        validate_accepted_sidecar_continuity, validate_sidecar_continuity,
        ProviderTelosSidecarStore, TelosChainIdentity, TelosExecutionAnchor, TelosExecutionSidecar,
        TelosSidecarError, TelosSidecarStore,
    },
};

use alloy_consensus::{Header, EMPTY_ROOT_HASH};
use alloy_eips::Decodable2718;
use alloy_evm::{block::BlockExecutionResult, eth::EthBlockExecutionCtx, FromRecoveredTx};
use alloy_primitives::{Bytes, B256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt};
use reth_evm::{
    eth::spec::EthExecutorSpec, execute::WithTxEnv, ConfigureEngineEvm, ConfigureEvm, EvmEnvFor,
    ExecutableTxIterator, ExecutionCtxFor, ExecutionReconciliation, NextBlockEnvAttributes,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};
use reth_primitives_traits::{Recovered, SealedBlock, SealedHeader, SignedTransaction, TxTy};
use reth_storage_errors::any::AnyError;
use reth_telos_rpc_engine_api::{
    compare::reconcile_state_diffs, convert_receipts, payload::TelosExecutionData,
    structs::TelosExecutionMetadataV3, validate_extra_fields_for_payload,
};
use revm::{database::State, Database};
use std::{borrow::Cow, convert::Infallible, sync::Arc};

/// Telos EVM configuration failed before execution could start.
#[derive(Debug, thiserror::Error)]
pub enum TelosEvmConfigError {
    /// Stock Ethereum environment construction is infallible for this configuration.
    #[error("unexpected infallible Ethereum EVM configuration error")]
    Infallible(#[from] Infallible),
    /// Payload validation did not attach versioned execution metadata.
    #[error("missing validated Telos execution metadata")]
    MissingExecutionMetadata,
    /// Native execution boundaries cannot be represented deterministically.
    #[error(transparent)]
    Schedule(#[from] TelosScheduleError),
    /// Durable sidecar storage or integrity validation failed.
    #[error(transparent)]
    Sidecar(#[from] TelosSidecarError),
    /// Engine metadata does not bind the exact payload execution environment.
    #[error(transparent)]
    ExtraFields(#[from] reth_telos_rpc_engine_api::ExtraFieldsError),
    /// Engine execution attempted to bypass precommitted sidecar ingress.
    #[error("missing durable Telos execution sidecar for payload {0}")]
    MissingDurableSidecar(B256),
    /// Durable bytes for this payload differ from the authenticated Engine request.
    #[error(
        "durable Telos execution sidecar mismatch for payload {block_hash}: stored {stored}, requested {requested}"
    )]
    DurableSidecarMismatch {
        /// Payload hash.
        block_hash: B256,
        /// Previously committed canonical digest.
        stored: B256,
        /// Digest of this Engine request.
        requested: B256,
    },
    /// A stored block cannot be replayed because it is outside the sidecar-covered range.
    #[error(
        "Telos block {block_number} ({block_hash}) is not after execution anchor {anchor_number}"
    )]
    BlockPrecedesExecutionAnchor {
        /// Candidate block number.
        block_number: u64,
        /// Candidate block hash.
        block_hash: B256,
        /// Snapshot anchor block number.
        anchor_number: u64,
    },
    /// Durable sidecar identity disagrees with the stored block.
    #[error(
        "durable Telos sidecar binding mismatch for block {block_hash} field {field}: expected {expected}, got {actual}"
    )]
    StoredBlockBindingMismatch {
        /// Stored block hash.
        block_hash: B256,
        /// Mismatching field.
        field: &'static str,
        /// Value derived from the stored block.
        expected: String,
        /// Value read from the sidecar.
        actual: String,
    },
    /// Telos cannot synthesize blocks without native consensus metadata.
    #[error("Telos payload building is disabled; blocks require authenticated native sidecars")]
    PayloadBuildingDisabled,
    /// Speculative block-access-list execution cannot preserve Telos transaction ordering.
    #[error(
        "Telos rejects EIP-7928 block access lists; native sidecars require sequential execution"
    )]
    BlockAccessListUnsupported,
    /// Telos cannot authenticate native execution context for an uncommitted pending state.
    #[error(
        "Telos RPC simulation against an uncommitted pending block is unsupported; use latest or an exact canonical block"
    )]
    PendingRpcSimulationUnsupported,
    /// Locally generated receipts differ from the authenticated Telos receipt record.
    #[error("local receipts do not match the authenticated receipts for block {0}")]
    ReceiptMismatch(B256),
    /// Locally consumed gas differs from the authenticated block result.
    #[error(
        "local gas used does not match the authenticated result for block {block_hash}: expected {expected}, got {actual}"
    )]
    ExecutionGasUsedMismatch {
        /// Mismatching block.
        block_hash: B256,
        /// Authenticated gas used.
        expected: u64,
        /// Locally executed gas used.
        actual: u64,
    },
}

/// Telos wrapper around the stock Ethereum EVM configuration.
#[derive(Clone)]
pub struct TelosEvmConfig<C> {
    inner: EthEvmConfig<C, TelosEvmFactory>,
    executor_factory: TelosBlockExecutorFactory<C>,
    block_assembler: TelosBlockAssembler,
    sidecar_store: Arc<dyn TelosSidecarStore>,
    execution_anchor: TelosExecutionAnchor,
}

impl<C> std::fmt::Debug for TelosEvmConfig<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelosEvmConfig")
            .field("execution_anchor", &self.execution_anchor)
            .finish_non_exhaustive()
    }
}

impl<C> TelosEvmConfig<C> {
    /// Creates a Telos EVM configuration.
    pub fn new(
        chain_spec: Arc<C>,
        sidecar_store: Arc<dyn TelosSidecarStore>,
        execution_anchor: TelosExecutionAnchor,
    ) -> Self {
        Self {
            inner: EthEvmConfig::new_with_evm_factory(chain_spec.clone(), TelosEvmFactory),
            executor_factory: TelosBlockExecutorFactory::new(chain_spec),
            block_assembler: TelosBlockAssembler,
            sidecar_store,
            execution_anchor,
        }
    }

    fn durable_sidecar_for_payload(
        &self,
        payload: &TelosExecutionData,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        self.validate_payload_metadata(payload)?;
        let requested = TelosExecutionSidecar::new(
            self.sidecar_store.chain_identity(),
            payload.inner.payload.as_v1().block_number,
            payload.inner.payload.block_hash(),
            payload.inner.parent_hash(),
            u64::try_from(payload.inner.payload.transactions().len())
                .map_err(|_| TelosSidecarError::TransactionCountOverflow(u64::MAX))?,
            payload.inner.payload.as_v1().gas_used,
            payload.extra_fields.clone(),
        )?;
        let block_hash = requested.envelope().block_hash;
        let stored = self
            .sidecar_store
            .get_engine_by_hash(block_hash)?
            .ok_or(TelosEvmConfigError::MissingDurableSidecar(block_hash))?;
        if stored.digest() != requested.digest() ||
            stored.canonical_bytes() != requested.canonical_bytes()
        {
            return Err(TelosEvmConfigError::DurableSidecarMismatch {
                block_hash,
                stored: stored.digest(),
                requested: requested.digest(),
            })
        }
        validate_sidecar_continuity(self.sidecar_store.as_ref(), &self.execution_anchor, &stored)?;
        Ok(stored)
    }

    fn validate_payload_metadata<'a>(
        &self,
        payload: &'a TelosExecutionData,
    ) -> Result<&'a TelosExecutionMetadataV3, TelosEvmConfigError> {
        validate_extra_fields_for_payload(
            &payload.extra_fields,
            payload.inner.payload.transactions().len(),
            payload.inner.payload.as_v1().gas_used,
            payload.inner.payload.as_v1().base_fee_per_gas,
            payload.inner.payload.block_hash(),
            payload.inner.parent_hash(),
        )?;
        payload.extra_fields.execution.as_ref().ok_or(TelosEvmConfigError::MissingExecutionMetadata)
    }

    fn execution_base_fee(
        execution: &TelosExecutionMetadataV3,
    ) -> Result<u64, TelosEvmConfigError> {
        u64::try_from(execution.execution_base_fee)
            .map_err(|_| TelosScheduleError::ExecutionBaseFeeOverflow.into())
    }

    fn validate_execution_result(
        block_hash: B256,
        expected_gas_used: u64,
        expected_receipts: &[Receipt],
        result: &BlockExecutionResult<Receipt>,
    ) -> Result<(), TelosEvmConfigError> {
        if result.receipts != expected_receipts {
            return Err(TelosEvmConfigError::ReceiptMismatch(block_hash))
        }
        if result.gas_used != expected_gas_used {
            return Err(TelosEvmConfigError::ExecutionGasUsedMismatch {
                block_hash,
                expected: expected_gas_used,
                actual: result.gas_used,
            })
        }
        Ok(())
    }

    fn durable_sidecar_for_header(
        &self,
        header: &Header,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        self.durable_sidecar_for_header_with_policy(header, false)
    }

    fn durable_sidecar_for_engine_header(
        &self,
        header: &Header,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        self.durable_sidecar_for_header_with_policy(header, true)
    }

    fn durable_sidecar_for_header_with_policy(
        &self,
        header: &Header,
        engine_buffered: bool,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        let block_hash = header.hash_slow();
        let sidecar = if engine_buffered {
            self.sidecar_store.get_engine_by_hash(block_hash)?
        } else {
            self.sidecar_store.get_accepted_by_hash(block_hash)?
        }
        .ok_or(TelosEvmConfigError::MissingDurableSidecar(block_hash))?;
        if engine_buffered {
            validate_sidecar_continuity(
                self.sidecar_store.as_ref(),
                &self.execution_anchor,
                &sidecar,
            )?;
        } else {
            validate_accepted_sidecar_continuity(
                self.sidecar_store.as_ref(),
                &self.execution_anchor,
                &sidecar,
            )?;
        }
        let envelope = sidecar.envelope();
        let bindings = [
            ("block_number", header.number.to_string(), envelope.block_number.to_string()),
            ("block_hash", block_hash.to_string(), envelope.block_hash.to_string()),
            ("parent_hash", header.parent_hash.to_string(), envelope.parent_hash.to_string()),
            ("gas_used", header.gas_used.to_string(), envelope.gas_used.to_string()),
        ];
        for (field, expected, actual) in bindings {
            if expected != actual {
                return Err(TelosEvmConfigError::StoredBlockBindingMismatch {
                    block_hash,
                    field,
                    expected,
                    actual,
                })
            }
        }
        Ok(sidecar)
    }

    fn durable_sidecar_for_block(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        self.durable_sidecar_for_block_with_policy(block, false)
    }

    fn durable_sidecar_for_engine_block(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        self.durable_sidecar_for_block_with_policy(block, true)
    }

    fn durable_sidecar_for_block_with_policy(
        &self,
        block: &SealedBlock<Block>,
        engine_buffered: bool,
    ) -> Result<TelosExecutionSidecar, TelosEvmConfigError> {
        let block_number = block.header().number;
        let block_hash = block.hash();
        if block.header().block_access_list_hash.is_some() {
            return Err(TelosEvmConfigError::BlockAccessListUnsupported)
        }
        if block_number <= self.execution_anchor.parent_block_number {
            return Err(TelosEvmConfigError::BlockPrecedesExecutionAnchor {
                block_number,
                block_hash,
                anchor_number: self.execution_anchor.parent_block_number,
            })
        }
        let sidecar = if engine_buffered {
            self.sidecar_store.get_engine_by_hash(block_hash)?
        } else {
            self.sidecar_store.get_accepted_by_hash(block_hash)?
        }
        .ok_or(TelosEvmConfigError::MissingDurableSidecar(block_hash))?;
        if engine_buffered {
            validate_sidecar_continuity(
                self.sidecar_store.as_ref(),
                &self.execution_anchor,
                &sidecar,
            )?;
        } else {
            validate_accepted_sidecar_continuity(
                self.sidecar_store.as_ref(),
                &self.execution_anchor,
                &sidecar,
            )?;
        }
        let envelope = sidecar.envelope();
        let bindings = [
            ("block_number", block_number.to_string(), envelope.block_number.to_string()),
            ("block_hash", block_hash.to_string(), envelope.block_hash.to_string()),
            (
                "parent_hash",
                block.header().parent_hash.to_string(),
                envelope.parent_hash.to_string(),
            ),
            (
                "transaction_count",
                block.body().transactions.len().to_string(),
                envelope.transaction_count.to_string(),
            ),
            ("gas_used", block.header().gas_used.to_string(), envelope.gas_used.to_string()),
        ];
        for (field, expected, actual) in bindings {
            if expected != actual {
                return Err(TelosEvmConfigError::StoredBlockBindingMismatch {
                    block_hash,
                    field,
                    expected,
                    actual,
                })
            }
        }
        Ok(sidecar)
    }

    fn block_execution_context<'a>(
        &self,
        ethereum: EthBlockExecutionCtx<'a>,
        sidecar: TelosExecutionSidecar,
    ) -> Result<TelosBlockExecutionCtx<'a>, TelosEvmConfigError> {
        let execution = sidecar
            .envelope()
            .extra_fields
            .execution
            .as_ref()
            .ok_or(TelosEvmConfigError::MissingExecutionMetadata)?;
        let schedule = Arc::new(TelosBlockExecutionSchedule::from_metadata(execution)?);
        Ok(TelosBlockExecutionCtx { ethereum, schedule, sidecar: Arc::new(sidecar) })
    }
}

impl<C> ConfigureEvm for TelosEvmConfig<C>
where
    C: EthExecutorSpec + EthChainSpec<Header = Header> + Hardforks + 'static,
{
    type Primitives = EthPrimitives;
    type Error = TelosEvmConfigError;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = TelosBlockExecutorFactory<C>;
    type BlockAssembler = TelosBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        let mut env = self.inner.evm_env(header)?;
        if header.number > self.execution_anchor.parent_block_number {
            let sidecar = self.durable_sidecar_for_header(header)?;
            let execution = sidecar
                .envelope()
                .extra_fields
                .execution
                .as_ref()
                .ok_or(TelosEvmConfigError::MissingExecutionMetadata)?;
            env.block_env.basefee = Self::execution_base_fee(execution)?;
        }
        Ok(env)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        Ok(self.inner.next_evm_env(parent, attributes)?)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let sidecar = self.durable_sidecar_for_block(block)?;
        self.block_execution_context(self.inner.context_for_block(block)?, sidecar)
    }

    fn context_for_next_block(
        &self,
        _parent: &SealedHeader<Header>,
        _attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        Err(TelosEvmConfigError::PayloadBuildingDisabled)
    }

    fn recover_block_transaction(
        &self,
        transaction: reth_ethereum_primitives::TransactionSigned,
    ) -> Result<
        Recovered<reth_ethereum_primitives::TransactionSigned>,
        reth_execution_errors::BlockExecutionError,
    > {
        let signer = recover_telos_sender(&transaction)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        Ok(Recovered::new_unchecked(transaction, signer))
    }

    fn reconcile_block_execution<DB>(
        &self,
        block: &SealedBlock<Block>,
        state: &mut State<DB>,
        result: &mut BlockExecutionResult<Receipt>,
    ) -> Result<ExecutionReconciliation, reth_execution_errors::BlockExecutionError>
    where
        DB: revm::Database,
    {
        let sidecar = self
            .durable_sidecar_for_block(block)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let extra_fields = &sidecar.envelope().extra_fields;
        let receipts = extra_fields.receipts.as_deref().ok_or_else(|| {
            reth_execution_errors::BlockExecutionError::msg("missing validated receipts")
        })?;
        let receipts = convert_receipts(receipts)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        Self::validate_execution_result(
            block.hash(),
            sidecar.envelope().gas_used,
            &receipts,
            result,
        )
        .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let report = reconcile_state_diffs(state, extra_fields)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        Ok(if report.is_empty() {
            ExecutionReconciliation::Unchanged
        } else {
            ExecutionReconciliation::Reconciled
        })
    }

    fn requires_rpc_transaction_context(&self) -> bool {
        true
    }

    fn apply_rpc_transaction_context(
        &self,
        block: &SealedBlock<Block>,
        transaction_boundary: usize,
        pending: bool,
        tx_env: &mut TelosTxEnv,
    ) -> Result<(), Self::Error> {
        if pending {
            return Err(TelosEvmConfigError::PendingRpcSimulationUnsupported)
        }

        if block.header().number == self.execution_anchor.parent_block_number {
            if block.hash() != self.execution_anchor.parent_block_hash {
                return Err(TelosEvmConfigError::StoredBlockBindingMismatch {
                    block_hash: block.hash(),
                    field: "anchor_hash",
                    expected: self.execution_anchor.parent_block_hash.to_string(),
                    actual: block.hash().to_string(),
                })
            }
            let transaction_count = block.body().transactions.len();
            if transaction_boundary != transaction_count {
                return Err(TelosScheduleError::TransactionIndex {
                    index: transaction_boundary,
                    transaction_count,
                }
                .into())
            }
            let fixed_gas_price = u128::try_from(self.execution_anchor.starting_gas_price)
                .map_err(|_| TelosScheduleError::GasPriceOverflow { boundary: 0 })?;
            tx_env.context = Some(crate::execution::TelosExecutionContext {
                fixed_gas_price,
                revision: self.execution_anchor.starting_revision,
                first_new_address: None,
            });
            return Ok(())
        }

        let sidecar = self.durable_sidecar_for_block(block)?;
        let execution = sidecar
            .envelope()
            .extra_fields
            .execution
            .as_ref()
            .ok_or(TelosEvmConfigError::MissingExecutionMetadata)?;
        let schedule = TelosBlockExecutionSchedule::from_metadata(execution)?;
        tx_env.context = Some(schedule.context_for_boundary(transaction_boundary)?);
        Ok(())
    }
}

impl<C> ConfigureEngineEvm<TelosExecutionData> for TelosEvmConfig<C>
where
    C: EthExecutorSpec + EthChainSpec<Header = Header> + Hardforks + 'static,
{
    fn evm_env_for_payload(
        &self,
        payload: &TelosExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        let execution = self.validate_payload_metadata(payload)?;
        let mut env = self.inner.evm_env_for_payload(&payload.inner)?;
        env.block_env.basefee = Self::execution_base_fee(execution)?;
        Ok(env)
    }

    fn evm_env_for_engine_block(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        let mut env = self.inner.evm_env(header)?;
        if header.number > self.execution_anchor.parent_block_number {
            let sidecar = self.durable_sidecar_for_engine_header(header)?;
            let execution = sidecar
                .envelope()
                .extra_fields
                .execution
                .as_ref()
                .ok_or(TelosEvmConfigError::MissingExecutionMetadata)?;
            env.block_env.basefee = Self::execution_base_fee(execution)?;
        }
        Ok(env)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a TelosExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let sidecar = self.durable_sidecar_for_payload(payload)?;
        let ethereum = EthBlockExecutionCtx {
            tx_count_hint: Some(payload.inner.payload.transactions().len()),
            parent_hash: payload.inner.parent_hash(),
            parent_beacon_block_root: payload.inner.sidecar.parent_beacon_block_root(),
            ommers: &[],
            withdrawals: payload
                .inner
                .payload
                .withdrawals()
                .map(|withdrawals| Cow::Borrowed(withdrawals.as_slice())),
            extra_data: payload.inner.payload.as_v1().extra_data.clone(),
            slot_number: payload.inner.payload.as_v4().map(|v4| v4.slot_number),
        };
        self.block_execution_context(ethereum, sidecar)
    }

    fn context_for_engine_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let sidecar = self.durable_sidecar_for_engine_block(block)?;
        self.block_execution_context(self.inner.context_for_block(block)?, sidecar)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &TelosExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let sidecar = self.durable_sidecar_for_payload(payload)?;
        let execution = sidecar
            .envelope()
            .extra_fields
            .execution
            .as_ref()
            .ok_or(TelosEvmConfigError::MissingExecutionMetadata)?;
        let schedule = Arc::new(TelosBlockExecutionSchedule::from_metadata(execution)?);
        let txs =
            payload.inner.payload.transactions().iter().cloned().enumerate().collect::<Vec<_>>();
        let convert = move |(transaction_index, encoded): (usize, Bytes)| {
            let tx = TxTy::<Self::Primitives>::decode_2718_exact(encoded.as_ref())
                .map_err(AnyError::new)?;
            let signer = recover_telos_sender(&tx).map_err(AnyError::new)?;
            let context =
                schedule.context_for_transaction(transaction_index).map_err(AnyError::new)?;
            let tx_env = TelosTxEnv::from_recovered_tx(&tx, signer).with_telos_context(context);
            Ok::<_, AnyError>(WithTxEnv::new((tx_env, tx.with_signer(signer))))
        };
        Ok((txs, convert))
    }

    fn reconcile_payload_execution<DB>(
        &self,
        payload: &TelosExecutionData,
        state: &mut State<DB>,
        result: &mut BlockExecutionResult<Receipt>,
    ) -> Result<ExecutionReconciliation, reth_execution_errors::BlockExecutionError>
    where
        DB: Database,
    {
        validate_extra_fields_for_payload(
            &payload.extra_fields,
            payload.inner.payload.transactions().len(),
            payload.inner.payload.as_v1().gas_used,
            payload.inner.payload.as_v1().base_fee_per_gas,
            payload.inner.payload.block_hash(),
            payload.inner.parent_hash(),
        )
        .map_err(reth_execution_errors::BlockExecutionError::other)?;

        let receipts = payload.extra_fields.receipts.as_deref().ok_or_else(|| {
            reth_execution_errors::BlockExecutionError::msg("missing validated receipts")
        })?;
        let receipts = convert_receipts(receipts)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        Self::validate_execution_result(
            payload.inner.payload.block_hash(),
            payload.inner.payload.as_v1().gas_used,
            &receipts,
            result,
        )
        .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let report = reconcile_state_diffs(state, &payload.extra_fields)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;

        Ok(if report.is_empty() {
            ExecutionReconciliation::Unchanged
        } else {
            ExecutionReconciliation::Reconciled
        })
    }

    fn reconcile_engine_block_execution<DB>(
        &self,
        block: &SealedBlock<Block>,
        state: &mut State<DB>,
        result: &mut BlockExecutionResult<Receipt>,
    ) -> Result<ExecutionReconciliation, reth_execution_errors::BlockExecutionError>
    where
        DB: Database,
    {
        let sidecar = self
            .durable_sidecar_for_engine_block(block)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let extra_fields = &sidecar.envelope().extra_fields;
        let receipts = extra_fields.receipts.as_deref().ok_or_else(|| {
            reth_execution_errors::BlockExecutionError::msg("missing validated receipts")
        })?;
        let receipts = convert_receipts(receipts)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        Self::validate_execution_result(
            block.hash(),
            sidecar.envelope().gas_used,
            &receipts,
            result,
        )
        .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let report = reconcile_state_diffs(state, extra_fields)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        Ok(if report.is_empty() {
            ExecutionReconciliation::Unchanged
        } else {
            ExecutionReconciliation::Reconciled
        })
    }

    fn state_root_matches(
        &self,
        _payload: Option<&TelosExecutionData>,
        _computed: B256,
        expected: B256,
    ) -> bool {
        // Both Engine payloads and sidecar-gated stored blocks carry Telos's historical placeholder
        // root. Stored execution cannot reach this check without an exact durable sidecar.
        expected == EMPTY_ROOT_HASH
    }
}

/// Telos EVM/executor builder.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TelosExecutorBuilder {
    execution_anchor: TelosExecutionAnchor,
}

impl TelosExecutorBuilder {
    /// Creates an executor builder bound to a trusted snapshot anchor.
    pub const fn new(execution_anchor: TelosExecutionAnchor) -> Self {
        Self { execution_anchor }
    }
}

impl<Types, Node> ExecutorBuilder<Node> for TelosExecutorBuilder
where
    Types: NodeTypes<
        ChainSpec: Hardforks + EthExecutorSpec + EthereumHardforks + EthChainSpec,
        Primitives = EthPrimitives,
    >,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = TelosEvmConfig<Types::ChainSpec>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let chain_spec = ctx.chain_spec();
        let chain = TelosChainIdentity {
            chain_id: chain_spec.chain().id(),
            genesis_hash: chain_spec.genesis_hash(),
        };
        self.execution_anchor.validate_for_chain(chain)?;
        let store: Arc<dyn TelosSidecarStore> =
            Arc::new(ProviderTelosSidecarStore::new(ctx.provider().clone(), chain));
        Ok(TelosEvmConfig::new(chain_spec, store, self.execution_anchor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::{InMemoryTelosSidecarStore, TELOS_EXECUTION_ANCHOR_VERSION};
    use alloy_consensus::{BlockBody, SignableTransaction, TxLegacy, TxType};
    use alloy_genesis::Genesis;
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use reth_chainspec::{Chain, ChainSpec};
    use reth_evm::block::BlockExecutor;
    use reth_primitives_traits::{Block as _, RecoveredBlock};
    use reth_telos_rpc_engine_api::structs::{
        TelosEngineApiExtraFields, TelosExecutionChange, TelosExecutionMetadataV3,
        TelosExtraFieldReceipt, TelosReceiptType, TELOS_EXECUTION_METADATA_VERSION,
    };
    use revm::{
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    fn empty_payload() -> ExecutionPayloadV1 {
        ExecutionPayloadV1 {
            parent_hash: Default::default(),
            fee_recipient: Default::default(),
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom: Default::default(),
            prev_randao: Default::default(),
            block_number: 0,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: Default::default(),
            base_fee_per_gas: Default::default(),
            block_hash: Default::default(),
            transactions: Vec::new(),
        }
    }

    fn chain_three_transaction(
        sender: Address,
        nonce: u64,
    ) -> reth_ethereum_primitives::TransactionSigned {
        let mut s = [0u8; 32];
        s[..20].copy_from_slice(sender.as_slice());
        TxLegacy {
            chain_id: Some(3),
            nonce,
            gas_price: 100,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x44)),
            ..Default::default()
        }
        .into_signed(Signature::new(U256::MAX, U256::from_be_bytes(s), false))
        .into()
    }

    fn receipt(success: bool, cumulative_gas_used: u64) -> Receipt {
        Receipt { tx_type: TxType::Legacy, success, cumulative_gas_used, logs: Vec::new() }
    }

    #[test]
    fn rejects_authenticated_receipts_that_differ_from_local_execution() {
        let block_hash = B256::repeat_byte(0x44);
        let expected = vec![receipt(true, 21_000)];
        let result = BlockExecutionResult {
            receipts: vec![receipt(false, 21_000)],
            gas_used: 21_000,
            ..Default::default()
        };

        let err = TelosEvmConfig::<ChainSpec>::validate_execution_result(
            block_hash, 21_000, &expected, &result,
        )
        .unwrap_err();

        assert!(matches!(err, TelosEvmConfigError::ReceiptMismatch(hash) if hash == block_hash));
        assert!(!result.receipts[0].success);
    }

    #[test]
    fn rejects_authenticated_gas_that_differs_from_local_execution() {
        let block_hash = B256::repeat_byte(0x55);
        let receipts = vec![receipt(true, 21_000)];
        let result = BlockExecutionResult {
            receipts: receipts.clone(),
            gas_used: 20_999,
            ..Default::default()
        };

        let err = TelosEvmConfig::<ChainSpec>::validate_execution_result(
            block_hash, 21_000, &receipts, &result,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            TelosEvmConfigError::ExecutionGasUsedMismatch {
                block_hash: hash,
                expected: 21_000,
                actual: 20_999,
            } if hash == block_hash
        ));
    }

    #[test]
    fn placeholder_state_root_is_bypassed_for_payload_and_sidecar_gated_replay() {
        let chain_spec = Arc::new(
            ChainSpec::builder().chain(Chain::from_id(40)).genesis(Genesis::default()).build(),
        );
        let chain = TelosChainIdentity { chain_id: 40, genesis_hash: chain_spec.genesis_hash() };
        let anchor = TelosExecutionAnchor {
            version: TELOS_EXECUTION_ANCHOR_VERSION,
            chain,
            parent_block_number: 0,
            parent_block_hash: B256::ZERO,
            starting_gas_price: Default::default(),
            starting_revision: 0,
        };
        let config = TelosEvmConfig::new(
            chain_spec,
            Arc::new(InMemoryTelosSidecarStore::new(chain)),
            anchor,
        );
        let computed = B256::repeat_byte(0x11);
        let payload = TelosExecutionData::from(alloy_rpc_types_engine::ExecutionData {
            payload: empty_payload().into(),
            sidecar: alloy_rpc_types_engine::ExecutionPayloadSidecar::none(),
        });
        assert!(ConfigureEngineEvm::<TelosExecutionData>::state_root_matches(
            &config,
            Some(&payload),
            computed,
            EMPTY_ROOT_HASH,
        ));
        assert!(ConfigureEngineEvm::<TelosExecutionData>::state_root_matches(
            &config,
            None,
            computed,
            EMPTY_ROOT_HASH,
        ));
        assert!(!ConfigureEngineEvm::<TelosExecutionData>::state_root_matches(
            &config,
            None,
            computed,
            B256::repeat_byte(0x22),
        ));
    }

    #[test]
    fn stored_block_replay_uses_embedded_senders_and_zero_based_sidecar_schedule() {
        let chain_spec = Arc::new(
            ChainSpec::builder().chain(Chain::from_id(40)).genesis(Genesis::default()).build(),
        );
        let chain = TelosChainIdentity { chain_id: 40, genesis_hash: chain_spec.genesis_hash() };
        let parent_hash = B256::repeat_byte(0x10);
        let anchor = TelosExecutionAnchor {
            version: TELOS_EXECUTION_ANCHOR_VERSION,
            chain,
            parent_block_number: 0,
            parent_block_hash: parent_hash,
            starting_gas_price: U256::from(2),
            starting_revision: 1,
        };
        let first_sender = Address::repeat_byte(0x11);
        let second_sender = Address::repeat_byte(0x22);
        let transactions = vec![
            chain_three_transaction(first_sender, 0),
            chain_three_transaction(second_sender, 0),
        ];
        let block = Block {
            header: Header {
                parent_hash,
                number: 1,
                gas_limit: 1_000_000,
                gas_used: 42_000,
                ..Default::default()
            },
            body: BlockBody { transactions, ..Default::default() },
        }
        .seal_slow();
        let block_hash = block.hash();
        let fields = TelosEngineApiExtraFields {
            statediffs_account: Some(Vec::new()),
            statediffs_accountstate: Some(Vec::new()),
            execution: Some(TelosExecutionMetadataV3 {
                version: TELOS_EXECUTION_METADATA_VERSION,
                block_hash,
                parent_hash,
                transaction_count: 2,
                execution_base_fee: U256::from(1),
                starting_gas_price: U256::from(2),
                starting_revision: 1,
                gas_price_changes: vec![TelosExecutionChange { boundary: 2, value: U256::from(7) }],
                revision_changes: Vec::new(),
            }),
            new_addresses_using_create: Some(Vec::new()),
            new_addresses_using_openwallet: Some(Vec::new()),
            receipts: Some(vec![
                TelosExtraFieldReceipt {
                    tx_type: TelosReceiptType::Name("Legacy".to_string()),
                    success: true,
                    cumulative_gas_used: 21_000,
                    logs: Vec::new(),
                },
                TelosExtraFieldReceipt {
                    tx_type: TelosReceiptType::Name("Legacy".to_string()),
                    success: true,
                    cumulative_gas_used: 42_000,
                    logs: Vec::new(),
                },
            ]),
            ..Default::default()
        };
        let sidecar =
            TelosExecutionSidecar::new(chain, 1, block_hash, parent_hash, 2, 42_000, fields)
                .unwrap();
        let store = Arc::new(InMemoryTelosSidecarStore::new(chain));
        store.put_pending(&sidecar).unwrap();
        store.mark_dispatched(block_hash, sidecar.digest()).unwrap();
        let config = TelosEvmConfig::new(chain_spec, store.clone(), anchor);
        assert!(config.requires_rpc_transaction_context());

        assert!(matches!(
            config.evm_env(block.header()),
            Err(TelosEvmConfigError::MissingDurableSidecar(hash)) if hash == block_hash
        ));
        assert_eq!(
            <TelosEvmConfig<ChainSpec> as ConfigureEngineEvm<TelosExecutionData>>::
                evm_env_for_engine_block(&config, block.header())
                .unwrap()
                .block_env
                .basefee,
            1
        );
        <TelosEvmConfig<ChainSpec> as ConfigureEngineEvm<TelosExecutionData>>::
            context_for_engine_block(&config, &block)
            .unwrap();

        store.mark_accepted(block_hash, sidecar.digest()).unwrap();
        assert_eq!(config.evm_env(block.header()).unwrap().block_env.basefee, 1);

        let mut database = CacheDB::<EmptyDB>::default();
        for sender in [first_sender, second_sender] {
            database.insert_account_info(
                sender,
                AccountInfo { balance: U256::from(10_000_000), ..Default::default() },
            );
        }
        let mut state = State::builder().with_database(database).with_bundle_update().build();
        let recovered = RecoveredBlock::new_sealed(
            block,
            vec![Address::repeat_byte(0xaa), Address::repeat_byte(0xbb)],
        );

        let mut boundary_tx = TelosTxEnv::default();
        config
            .apply_rpc_transaction_context(recovered.sealed_block(), 1, false, &mut boundary_tx)
            .unwrap();
        assert_eq!(boundary_tx.fixed_gas_price(), Some(2));

        let mut post_block_tx = TelosTxEnv::default();
        config
            .apply_rpc_transaction_context(recovered.sealed_block(), 2, false, &mut post_block_tx)
            .unwrap();
        assert_eq!(post_block_tx.fixed_gas_price(), Some(7));

        let mut invalid_tx = TelosTxEnv::default();
        assert!(matches!(
            config.apply_rpc_transaction_context(
                recovered.sealed_block(),
                3,
                false,
                &mut invalid_tx
            ),
            Err(TelosEvmConfigError::Schedule(TelosScheduleError::ExecutionBoundary {
                boundary: 3,
                transaction_count: 2
            }))
        ));
        assert!(invalid_tx.context.is_none());

        assert!(matches!(
            config.apply_rpc_transaction_context(
                recovered.sealed_block(),
                2,
                true,
                &mut invalid_tx
            ),
            Err(TelosEvmConfigError::PendingRpcSimulationUnsupported)
        ));

        let missing_block = Block {
            header: Header {
                parent_hash: block_hash,
                number: 2,
                gas_limit: 1_000_000,
                ..Default::default()
            },
            body: BlockBody::default(),
        }
        .seal_slow();
        assert!(matches!(
            config.apply_rpc_transaction_context(&missing_block, 0, false, &mut invalid_tx),
            Err(TelosEvmConfigError::MissingDurableSidecar(hash)) if hash == missing_block.hash()
        ));

        let wrong_anchor_block = Block {
            header: Header { number: 0, ..Default::default() },
            body: BlockBody::default(),
        }
        .seal_slow();
        assert!(matches!(
            config.apply_rpc_transaction_context(&wrong_anchor_block, 0, false, &mut invalid_tx),
            Err(TelosEvmConfigError::StoredBlockBindingMismatch { field: "anchor_hash", .. })
        ));

        let mut executor = config.executor_for_block(&mut state, recovered.sealed_block()).unwrap();
        executor.apply_pre_execution_changes().unwrap();
        for transaction in recovered.transactions_recovered() {
            executor.execute_transaction(transaction).unwrap();
        }
        let (evm, result) = executor.finish().unwrap();
        drop(evm);

        assert_eq!(result.gas_used, 42_000);
        let expected_balance = U256::from(10_000_000 - 21_000 * 2);
        assert_eq!(state.basic(first_sender).unwrap().unwrap().balance, expected_balance);
        assert_eq!(state.basic(second_sender).unwrap().unwrap().balance, expected_balance);
        assert!(state.basic(Address::repeat_byte(0xaa)).unwrap().is_none());
        assert!(state.basic(Address::repeat_byte(0xbb)).unwrap().is_none());
    }
}

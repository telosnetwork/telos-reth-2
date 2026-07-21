//! Telos EVM configuration and authenticated post-execution reconciliation.

use crate::engine::recover_telos_sender;

use alloy_consensus::{Header, EMPTY_ROOT_HASH};
use alloy_eips::Decodable2718;
use alloy_evm::{
    block::BlockExecutionResult,
    eth::{EthBlockExecutionCtx, EthBlockExecutorFactory},
};
use alloy_primitives::{Address, Bytes, B256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::{EthPrimitives, Receipt};
use reth_evm::{
    eth::spec::EthExecutorSpec, ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator,
    ExecutionCtxFor, ExecutionReconciliation, NextBlockEnvAttributes,
};
use reth_evm_ethereum::{
    factory::RethEvmFactory, EthBlockAssembler, EthEvmConfig, RethReceiptBuilder,
};
use reth_node_api::{FullNodeTypes, NodeTypes};
use reth_node_builder::{components::ExecutorBuilder, BuilderContext};
use reth_primitives_traits::{SealedBlock, SealedHeader, SignedTransaction, TxTy};
use reth_storage_errors::any::AnyError;
use reth_telos_rpc_engine_api::{
    compare::{prepare_created_account, reconcile_state_diffs},
    convert_receipts,
    payload::TelosExecutionData,
    validate_extra_fields,
};
use revm::{database::State, Database};
use std::{borrow::Cow, convert::Infallible, sync::Arc};

/// Telos wrapper around the stock Ethereum EVM configuration.
#[derive(Debug, Clone)]
pub struct TelosEvmConfig<C> {
    inner: EthEvmConfig<C, RethEvmFactory>,
}

impl<C> TelosEvmConfig<C> {
    /// Creates a Telos EVM configuration.
    pub fn new(chain_spec: Arc<C>) -> Self {
        Self { inner: EthEvmConfig::new_with_evm_factory(chain_spec, RethEvmFactory::default()) }
    }
}

impl<C> ConfigureEvm for TelosEvmConfig<C>
where
    C: EthExecutorSpec + EthChainSpec<Header = Header> + Hardforks + 'static,
{
    type Primitives = EthPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = EthBlockExecutorFactory<RethReceiptBuilder, Arc<C>, RethEvmFactory>;
    type BlockAssembler = EthBlockAssembler<C>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self.inner.block_executor_factory()
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<reth_ethereum_primitives::Block>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<Header>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
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
        self.inner.evm_env_for_payload(&payload.inner)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a TelosExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(EthBlockExecutionCtx {
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
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &TelosExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.inner.payload.transactions().clone();
        let convert = |tx: Bytes| {
            let tx =
                TxTy::<Self::Primitives>::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = recover_telos_sender(&tx).map_err(AnyError::new)?;
            Ok::<_, AnyError>(tx.with_signer(signer))
        };
        Ok((txs, convert))
    }

    fn prepare_payload_transaction<DB>(
        &self,
        payload: &TelosExecutionData,
        transaction_index: usize,
        state: &mut State<DB>,
    ) -> Result<(), reth_execution_errors::BlockExecutionError>
    where
        DB: Database,
    {
        let creates =
            payload.extra_fields.new_addresses_using_create.as_ref().ok_or_else(|| {
                reth_execution_errors::BlockExecutionError::msg(
                    "missing new_addresses_using_create",
                )
            })?;
        for (_, value) in creates.iter().filter(|(index, _)| *index == transaction_index as u64) {
            prepare_created_account(state, Address::from_word(B256::from(*value)))
                .map_err(reth_execution_errors::BlockExecutionError::other)?;
        }
        Ok(())
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
        validate_extra_fields(
            &payload.extra_fields,
            payload.inner.payload.transactions().len(),
            payload.inner.payload.as_v1().gas_used,
        )
        .map_err(reth_execution_errors::BlockExecutionError::other)?;

        let report = reconcile_state_diffs(state, &payload.extra_fields)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let receipts = payload.extra_fields.receipts.as_deref().ok_or_else(|| {
            reth_execution_errors::BlockExecutionError::msg("missing validated receipts")
        })?;
        let receipts = convert_receipts(receipts)
            .map_err(reth_execution_errors::BlockExecutionError::other)?;
        let receipts_changed = result.receipts != receipts;
        result.receipts = receipts;
        result.gas_used = payload.inner.payload.as_v1().gas_used;

        Ok(if report.is_empty() && !receipts_changed {
            ExecutionReconciliation::Unchanged
        } else {
            ExecutionReconciliation::Reconciled
        })
    }

    fn state_root_matches(
        &self,
        payload: Option<&TelosExecutionData>,
        computed: B256,
        expected: B256,
    ) -> bool {
        computed == expected || (payload.is_some() && expected == EMPTY_ROOT_HASH)
    }
}

/// Telos EVM/executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct TelosExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for TelosExecutorBuilder
where
    Types: NodeTypes<
        ChainSpec: Hardforks + EthExecutorSpec + EthereumHardforks,
        Primitives = EthPrimitives,
    >,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = TelosEvmConfig<Types::ChainSpec>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(TelosEvmConfig::new(ctx.chain_spec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::Genesis;
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use reth_chainspec::{Chain, ChainSpec};

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

    #[test]
    fn only_placeholder_state_root_is_bypassed() {
        let chain_spec = Arc::new(
            ChainSpec::builder().chain(Chain::from_id(40)).genesis(Genesis::default()).build(),
        );
        let config = TelosEvmConfig::new(chain_spec);
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
        assert!(!ConfigureEngineEvm::<TelosExecutionData>::state_root_matches(
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
}

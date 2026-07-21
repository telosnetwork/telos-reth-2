//! Telos-aware stored block execution.

use crate::{
    engine::recover_telos_sender,
    execution::{TelosBlockExecutionSchedule, TelosEvmFactory, TelosTxEnv},
    sidecar::TelosExecutionSidecar,
};
use alloy_consensus::transaction::Recovered;
use alloy_evm::{
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockExecutorFactory,
        ExecutableTx, GasOutput, StateDB,
    },
    eth::{
        receipt_builder::ReceiptBuilder, spec::EthExecutorSpec, EthBlockExecutionCtx,
        EthBlockExecutor, EthBlockExecutorFactory,
    },
    Evm, EvmFactory, RecoveredTx,
};
use alloy_primitives::{Address, B256};
use reth_ethereum_primitives::{Block, Receipt, TransactionSigned};
use reth_evm::execute::{BlockAssembler, BlockAssemblerInput};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_telos_rpc_engine_api::compare::prepare_created_account;
use revm::Inspector;
use std::sync::Arc;

/// Block execution context bound to a durable, validated native sidecar.
#[derive(Clone, Debug)]
pub struct TelosBlockExecutionCtx<'a> {
    /// Standard Ethereum block context used for system calls and block-level accounting.
    pub ethereum: EthBlockExecutionCtx<'a>,
    /// Per-transaction native execution schedule.
    pub schedule: Arc<TelosBlockExecutionSchedule>,
    /// Exact sidecar whose state and receipt records validate this block.
    pub sidecar: Arc<TelosExecutionSidecar>,
}

/// Factory for Telos-aware block executors.
#[derive(Clone, Debug)]
pub struct TelosBlockExecutorFactory<C> {
    inner: EthBlockExecutorFactory<RethReceiptBuilder, Arc<C>, TelosEvmFactory>,
}

impl<C> TelosBlockExecutorFactory<C> {
    /// Creates a factory for the given chain specification.
    pub fn new(chain_spec: Arc<C>) -> Self {
        Self {
            inner: EthBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                TelosEvmFactory,
            ),
        }
    }
}

impl<C> BlockExecutorFactory for TelosBlockExecutorFactory<C>
where
    C: EthExecutorSpec + 'static,
{
    type EvmFactory = TelosEvmFactory;
    type TxExecutionResult = <EthBlockExecutorFactory<
        RethReceiptBuilder,
        Arc<C>,
        TelosEvmFactory,
    > as BlockExecutorFactory>::TxExecutionResult;
    type ExecutionCtx<'a> = TelosBlockExecutionCtx<'a>;
    type Transaction = TransactionSigned;
    type Receipt = Receipt;
    type Executor<'a, DB: StateDB, I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>> =
        TelosBlockExecutor<
            'a,
            <Self::EvmFactory as EvmFactory>::Evm<DB, I>,
            &'a Arc<C>,
            &'a RethReceiptBuilder,
        >;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.inner.evm_factory()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<DB>>,
    {
        let inner = EthBlockExecutor::new(
            evm,
            ctx.ethereum,
            self.inner.spec(),
            self.inner.receipt_builder(),
        );
        TelosBlockExecutor { inner, schedule: ctx.schedule, sidecar: ctx.sidecar }
    }
}

/// Executor wrapper that attaches authenticated Telos context before every stored transaction.
#[expect(missing_debug_implementations)]
pub struct TelosBlockExecutor<'a, EvmT, Spec, R>
where
    R: ReceiptBuilder,
{
    inner: EthBlockExecutor<'a, EvmT, Spec, R>,
    schedule: Arc<TelosBlockExecutionSchedule>,
    sidecar: Arc<TelosExecutionSidecar>,
}

impl<'a, EvmT, Spec, R> BlockExecutor for TelosBlockExecutor<'a, EvmT, Spec, R>
where
    R: ReceiptBuilder,
    EvmT: Evm<Tx = TelosTxEnv>,
    EvmT::DB: StateDB,
    EthBlockExecutor<'a, EvmT, Spec, R>:
        BlockExecutor<Transaction = TransactionSigned, Receipt = Receipt, Evm = EvmT>,
{
    type Transaction = TransactionSigned;
    type Receipt = Receipt;
    type Evm = EvmT;
    type Result = <EthBlockExecutor<'a, EvmT, Spec, R> as BlockExecutor>::Result;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let transaction_index = self.inner.receipts().len();
        let transaction_boundary = u64::try_from(transaction_index)
            .map_err(|_| BlockExecutionError::msg("Telos transaction boundary exceeds u64"))?;
        let creates = self
            .sidecar
            .envelope()
            .extra_fields
            .new_addresses_using_create
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("missing new_addresses_using_create"))?
            .iter()
            .filter(|(boundary, _)| *boundary == transaction_boundary)
            .map(|(_, value)| Address::from_word(B256::from(*value)))
            .collect::<Vec<_>>();
        for address in creates {
            prepare_created_account(self.inner.evm_mut().db_mut(), address)
                .map_err(BlockExecutionError::other)?;
        }

        let (mut tx_env, recovered) = tx.into_parts();
        let signer = recover_telos_sender(recovered.tx()).map_err(BlockExecutionError::other)?;
        tx_env.inner.caller = signer;
        tx_env.context = Some(
            self.schedule
                .context_for_transaction(transaction_index)
                .map_err(BlockExecutionError::other)?,
        );

        let recovered = Recovered::new_unchecked(recovered.tx(), signer);
        self.inner.execute_transaction_without_commit((tx_env, recovered))
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.inner.commit_transaction(output)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}

/// Payload building is unsupported because Telos blocks require authoritative native sidecars.
#[derive(Clone, Copy, Debug, Default)]
pub struct TelosBlockAssembler;

impl<F> BlockAssembler<F> for TelosBlockAssembler
where
    F: BlockExecutorFactory<Transaction = TransactionSigned, Receipt = Receipt>,
{
    type Block = Block;

    fn assemble_block(
        &self,
        _input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<Self::Block, BlockExecutionError> {
        Err(BlockExecutionError::msg(
            "Telos payload building is disabled; blocks require authenticated native sidecars",
        ))
    }
}

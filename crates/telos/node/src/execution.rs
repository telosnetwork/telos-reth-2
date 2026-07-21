//! Telos transaction execution context for revm 41.

use crate::{frame::TelosEvmInner, handler::TelosHandler, instructions::telos_instructions};

use alloy_evm::{
    precompiles::PrecompilesMap, Database, Evm, EvmEnv, EvmFactory, FromRecoveredTx,
    FromTxWithEncoded, IntoTxEnv, TransactionEnvMut,
};
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_rpc_types_eth::TransactionRequest;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{ConfigureEvm, EvmEnvFor, EvmFactoryFor};
use reth_rpc_convert::{transaction::TxEnvConverter, EthTxEnvError, TryIntoTxEnv};
use reth_telos_rpc_engine_api::structs::TelosExecutionMetadataV3;
use revm::{
    context::{BlockEnv, CfgEnv, Context, DBErrorMarker, FrameStack, TxEnv},
    context_interface::{
        either::Either,
        journaled_state::JournalTr,
        result::{EVMError, HaltReason, InvalidTransaction, ResultAndState},
        transaction::{
            AccessList, AccessListItem, RecoveredAuthorization, SignedAuthorization, Transaction,
        },
    },
    database_interface::EmptyDB,
    handler::{
        instructions::EthInstructions, EthPrecompiles, Handler, PrecompileProvider, SystemCallTx,
    },
    inspector::{InspectorHandler, NoOpInspector},
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
    precompile::{PrecompileSpecId, Precompiles},
    primitives::hardfork::SpecId,
    Inspector,
};
use std::ops::{Deref, DerefMut};

/// Transaction environment carrying the native Telos execution schedule.
///
/// The context is intentionally optional at construction time because Reth also constructs
/// transaction environments for RPC simulation and system calls. Consensus execution validates
/// that a context is present before executing a Telos transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelosTxEnv {
    /// Standard revm transaction fields.
    pub inner: TxEnv,
    /// Native execution values effective at this transaction boundary.
    pub context: Option<TelosExecutionContext>,
}

impl TelosTxEnv {
    /// Wraps a standard transaction without claiming a native execution context.
    pub const fn new(inner: TxEnv) -> Self {
        Self { inner, context: None }
    }

    /// Attaches the native execution values effective for this transaction.
    pub const fn with_telos_context(mut self, context: TelosExecutionContext) -> Self {
        self.context = Some(context);
        self
    }

    /// Returns the fixed native gas price when authenticated context is available.
    pub const fn fixed_gas_price(&self) -> Option<u128> {
        match self.context {
            Some(context) => Some(context.fixed_gas_price),
            None => None,
        }
    }

    /// Returns the native EVM revision when authenticated context is available.
    pub const fn revision(&self) -> Option<u64> {
        match self.context {
            Some(context) => Some(context.revision),
            None => None,
        }
    }

    /// Returns the mutable first-new-address slot used by legacy revision zero.
    pub fn first_new_address_mut(&mut self) -> Option<&mut Option<Address>> {
        self.context.as_mut().map(|context| &mut context.first_new_address)
    }

    fn capped_gas_price(&self) -> u128 {
        self.fixed_gas_price().map_or(self.inner.gas_price, |fixed| self.inner.gas_price.min(fixed))
    }
}

impl Default for TelosTxEnv {
    fn default() -> Self {
        Self::new(TxEnv::default())
    }
}

impl IntoTxEnv<Self> for TelosTxEnv {
    fn into_tx_env(self) -> Self {
        self
    }
}

impl SystemCallTx for TelosTxEnv {
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self {
        Self::new(TxEnv::new_system_tx_with_caller(caller, system_contract_address, data))
    }
}

impl Transaction for TelosTxEnv {
    type AccessListItem<'a> = &'a AccessListItem;
    type Authorization<'a> = &'a Either<SignedAuthorization, RecoveredAuthorization>;

    fn tx_type(&self) -> u8 {
        self.inner.tx_type()
    }

    fn caller(&self) -> Address {
        self.inner.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn value(&self) -> U256 {
        self.inner.value()
    }

    fn input(&self) -> &Bytes {
        self.inner.input()
    }

    fn nonce(&self) -> u64 {
        self.inner.nonce()
    }

    fn kind(&self) -> TxKind {
        self.inner.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.inner.chain_id()
    }

    fn gas_price(&self) -> u128 {
        self.capped_gas_price()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.inner.access_list()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.inner.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.inner.max_fee_per_blob_gas()
    }

    fn authorization_list_len(&self) -> usize {
        self.inner.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.inner.authorization_list()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.inner.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.inner.max_priority_fee_per_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.context
            .map_or_else(|| self.inner.effective_gas_price(base_fee), |_| self.capped_gas_price())
    }

    fn max_balance_spending(&self) -> Result<U256, InvalidTransaction> {
        let mut spending = (self.gas_limit() as u128)
            .checked_mul(self.capped_gas_price())
            .and_then(|gas| U256::from(gas).checked_add(self.value()))
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;

        let data_fee = self.calc_max_data_fee();
        spending = spending
            .checked_add(data_fee)
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
        Ok(spending)
    }
}

impl TransactionEnvMut for TelosTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.inner.set_gas_limit(gas_limit);
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.inner.set_nonce(nonce);
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.inner.set_access_list(access_list);
    }
}

impl FromRecoveredTx<TransactionSigned> for TelosTxEnv {
    fn from_recovered_tx(tx: &TransactionSigned, sender: Address) -> Self {
        Self::new(TxEnv::from_recovered_tx(tx, sender))
    }
}

impl FromTxWithEncoded<TransactionSigned> for TelosTxEnv {
    fn from_encoded_tx(tx: &TransactionSigned, sender: Address, encoded: Bytes) -> Self {
        Self::new(TxEnv::from_encoded_tx(tx, sender, encoded))
    }
}

/// Converts Ethereum RPC requests into the transaction environment used by Telos execution.
///
/// RPC requests do not carry authenticated native execution metadata. The standard request fields
/// are preserved and the context remains absent; callers that need historical Telos semantics must
/// attach context obtained from the block-bound native execution sidecar.
#[derive(Clone, Copy, Debug, Default)]
pub struct TelosTxEnvConverter;

impl<EvmConfig> TxEnvConverter<TransactionRequest, EvmConfig> for TelosTxEnvConverter
where
    EvmConfig: ConfigureEvm,
    EvmFactoryFor<EvmConfig>: EvmFactory<Tx = TelosTxEnv, Spec = SpecId, BlockEnv = BlockEnv>,
{
    type Error = EthTxEnvError;

    fn convert_tx_env(
        &self,
        request: TransactionRequest,
        evm_env: &EvmEnvFor<EvmConfig>,
    ) -> Result<TelosTxEnv, Self::Error> {
        let inner = <TransactionRequest as TryIntoTxEnv<TxEnv, SpecId, BlockEnv>>::try_into_tx_env(
            request, evm_env,
        )?;
        Ok(TelosTxEnv::new(inner))
    }
}

/// Native execution values effective at the start of one transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TelosExecutionContext {
    /// Gas price supplied by the native Telos EVM contract.
    pub fixed_gas_price: u128,
    /// Native EVM revision number.
    pub revision: u64,
    /// First previously-empty address observed in legacy revision zero.
    pub first_new_address: Option<Address>,
}

/// Per-block native execution schedule with explicit zero-based transaction boundaries.
///
/// A change at boundary `n` is effective immediately before transaction `n`. Boundary
/// `transaction_count` is the post-block boundary and establishes the starting context for the
/// child block. This representation deliberately does not accept ambiguous one-based indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelosBlockExecutionSchedule {
    transaction_count: usize,
    starting_context: TelosExecutionContext,
    gas_price_changes: Vec<TelosExecutionChange<u128>>,
    revision_changes: Vec<TelosExecutionChange<u64>>,
}

impl TelosBlockExecutionSchedule {
    /// Creates and validates a block execution schedule.
    pub fn new(
        transaction_count: usize,
        starting_context: TelosExecutionContext,
        gas_price_changes: Vec<TelosExecutionChange<u128>>,
        revision_changes: Vec<TelosExecutionChange<u64>>,
    ) -> Result<Self, TelosScheduleError> {
        validate_changes("gas price", transaction_count, &gas_price_changes)?;
        validate_changes("revision", transaction_count, &revision_changes)?;
        Ok(Self { transaction_count, starting_context, gas_price_changes, revision_changes })
    }

    /// Converts validated wire metadata into the revm execution schedule.
    pub fn from_metadata(metadata: &TelosExecutionMetadataV3) -> Result<Self, TelosScheduleError> {
        let transaction_count = usize::try_from(metadata.transaction_count).map_err(|_| {
            TelosScheduleError::TransactionCountOverflow(metadata.transaction_count)
        })?;
        let starting_gas_price = u128::try_from(metadata.starting_gas_price)
            .map_err(|_| TelosScheduleError::GasPriceOverflow { boundary: 0 })?;
        let gas_price_changes = metadata
            .gas_price_changes
            .iter()
            .map(|change| {
                Ok(TelosExecutionChange {
                    boundary: usize::try_from(change.boundary).map_err(|_| {
                        TelosScheduleError::ChangeBoundary {
                            kind: "gas price",
                            boundary: usize::MAX,
                            transaction_count,
                        }
                    })?,
                    value: u128::try_from(change.value).map_err(|_| {
                        TelosScheduleError::GasPriceOverflow { boundary: change.boundary }
                    })?,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let revision_changes = metadata
            .revision_changes
            .iter()
            .map(|change| {
                Ok(TelosExecutionChange {
                    boundary: usize::try_from(change.boundary).map_err(|_| {
                        TelosScheduleError::ChangeBoundary {
                            kind: "revision",
                            boundary: usize::MAX,
                            transaction_count,
                        }
                    })?,
                    value: change.value,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(
            transaction_count,
            TelosExecutionContext {
                fixed_gas_price: starting_gas_price,
                revision: metadata.starting_revision,
                first_new_address: None,
            },
            gas_price_changes,
            revision_changes,
        )
    }

    /// Returns the context effective for a zero-based transaction index.
    pub fn context_for_transaction(
        &self,
        transaction_index: usize,
    ) -> Result<TelosExecutionContext, TelosScheduleError> {
        if transaction_index >= self.transaction_count {
            return Err(TelosScheduleError::TransactionIndex {
                index: transaction_index,
                transaction_count: self.transaction_count,
            })
        }
        Ok(self.context_at_boundary(transaction_index))
    }

    /// Returns the context effective at an inclusive transaction boundary.
    ///
    /// Boundary `transaction_count` is the post-block context used by calls against the block's
    /// resulting state.
    pub fn context_for_boundary(
        &self,
        boundary: usize,
    ) -> Result<TelosExecutionContext, TelosScheduleError> {
        if boundary > self.transaction_count {
            return Err(TelosScheduleError::ExecutionBoundary {
                boundary,
                transaction_count: self.transaction_count,
            })
        }
        Ok(self.context_at_boundary(boundary))
    }

    /// Returns the post-block context that must be inherited by a child of this exact block.
    pub fn child_context(&self) -> TelosExecutionContext {
        self.context_at_boundary(self.transaction_count)
    }

    fn context_at_boundary(&self, boundary: usize) -> TelosExecutionContext {
        let mut context = self.starting_context;
        context.first_new_address = None;
        for change in self.gas_price_changes.iter().take_while(|change| change.boundary <= boundary)
        {
            context.fixed_gas_price = change.value;
        }
        for change in self.revision_changes.iter().take_while(|change| change.boundary <= boundary)
        {
            context.revision = change.value;
        }
        context
    }
}

/// One native value change effective at a zero-based transaction boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelosExecutionChange<T> {
    /// Boundary at which the value becomes effective.
    pub boundary: usize,
    /// New value.
    pub value: T,
}

/// revm context used by Telos execution and RPC inspection.
pub type TelosEvmContext<DB> = Context<BlockEnv, TelosTxEnv, CfgEnv, DB>;

/// revm EVM wrapper using the Telos transaction environment.
#[expect(missing_debug_implementations)]
pub struct TelosEvm<DB: Database, I, PRECOMPILE = EthPrecompiles> {
    inner: TelosEvmInner<DB, I, EthInstructions<EthInterpreter, TelosEvmContext<DB>>, PRECOMPILE>,
    inspect: bool,
}

impl<DB: Database, I, PRECOMPILE> TelosEvm<DB, I, PRECOMPILE> {
    /// Creates a Telos EVM wrapper.
    pub const fn new(
        inner: TelosEvmInner<
            DB,
            I,
            EthInstructions<EthInterpreter, TelosEvmContext<DB>>,
            PRECOMPILE,
        >,
        inspect: bool,
    ) -> Self {
        Self { inner, inspect }
    }
}

impl<DB: Database, I, PRECOMPILE> Deref for TelosEvm<DB, I, PRECOMPILE> {
    type Target = TelosEvmContext<DB>;

    fn deref(&self) -> &Self::Target {
        &self.inner.ctx
    }
}

impl<DB: Database, I, PRECOMPILE> DerefMut for TelosEvm<DB, I, PRECOMPILE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.ctx
    }
}

impl<DB, I, PRECOMPILE> Evm for TelosEvm<DB, I, PRECOMPILE>
where
    DB: Database,
    I: Inspector<TelosEvmContext<DB>>,
    PRECOMPILE: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
{
    type DB = DB;
    type Tx = TelosTxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PRECOMPILE;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        &self.block
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        &self.cfg
    }

    fn chain_id(&self) -> u64 {
        self.cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.ctx.tx = tx;
        let output = if self.inspect {
            TelosHandler::<DB, I, PRECOMPILE>::default().inspect_run(&mut self.inner)
        } else {
            TelosHandler::<DB, I, PRECOMPILE>::default().run(&mut self.inner)
        };
        let state = self.inner.ctx.journaled_state.finalize();
        Ok(ResultAndState::new(output?, state))
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.ctx.tx = TelosTxEnv::new_system_tx_with_caller(caller, contract, data);
        let output = TelosHandler::<DB, I, PRECOMPILE>::default().run_system_call(&mut self.inner);
        let state = self.inner.ctx.journaled_state.finalize();
        Ok(ResultAndState::new(output?, state))
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>) {
        let Context { block: block_env, cfg: cfg_env, journaled_state, .. } = self.inner.ctx;
        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (&self.inner.ctx.journaled_state.database, &self.inner.inspector, &self.inner.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
    }
}

/// Factory producing revm instances with a [`TelosTxEnv`].
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct TelosEvmFactory;

impl TelosEvmFactory {
    fn create<DB: Database, I: Inspector<TelosEvmContext<DB>>>(
        db: DB,
        input: EvmEnv,
        inspector: I,
        inspect: bool,
    ) -> TelosEvm<DB, I, PrecompilesMap> {
        let spec = input.cfg_env.spec;
        let context = Context::<BlockEnv, TelosTxEnv, CfgEnv, EmptyDB>::new(
            EmptyDB::new(),
            SpecId::default(),
        )
        .with_block(input.block_env)
        .with_cfg(input.cfg_env)
        .with_db(db);
        let inner = TelosEvmInner {
            ctx: context,
            inspector,
            instruction: telos_instructions(spec),
            precompiles: PrecompilesMap::from_static(Precompiles::new(
                PrecompileSpecId::from_spec_id(spec),
            )),
            frame_stack: FrameStack::new_prealloc(8),
        };
        TelosEvm::new(inner, inspect)
    }
}

impl EvmFactory for TelosEvmFactory {
    type Evm<DB: Database, I: Inspector<TelosEvmContext<DB>>> = TelosEvm<DB, I, Self::Precompiles>;
    type Context<DB: Database> = TelosEvmContext<DB>;
    type Tx = TelosTxEnv;
    type Error<DBError: DBErrorMarker> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
        Self::create(db, input, NoOpInspector {}, false)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        Self::create(db, input, inspector, true)
    }
}

/// A native execution schedule cannot be applied deterministically.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum TelosScheduleError {
    /// The declared transaction count cannot be represented by this platform.
    #[error("transaction count {0} cannot be represented by this platform")]
    TransactionCountOverflow(u64),
    /// A native gas price cannot be represented by revm.
    #[error("gas price at boundary {boundary} exceeds 128 bits")]
    GasPriceOverflow {
        /// Boundary carrying the unrepresentable value.
        boundary: u64,
    },
    /// The Engine-only block base fee cannot be represented by revm.
    #[error("execution base fee exceeds 64 bits")]
    ExecutionBaseFeeOverflow,
    /// A requested transaction does not exist in this block.
    #[error("transaction index {index} is outside transaction count {transaction_count}")]
    TransactionIndex {
        /// Invalid zero-based index.
        index: usize,
        /// Number of transactions in the block.
        transaction_count: usize,
    },
    /// A requested execution boundary is outside the inclusive block-boundary range.
    #[error("execution boundary {boundary} is outside transaction count {transaction_count}")]
    ExecutionBoundary {
        /// Invalid zero-based boundary.
        boundary: usize,
        /// Number of transactions in the block.
        transaction_count: usize,
    },
    /// A change is outside the inclusive block-boundary range.
    #[error("{kind} change boundary {boundary} is outside transaction count {transaction_count}")]
    ChangeBoundary {
        /// Schedule kind.
        kind: &'static str,
        /// Invalid boundary.
        boundary: usize,
        /// Number of transactions in the block.
        transaction_count: usize,
    },
    /// Two changes of one kind target the same or decreasing boundary.
    #[error("{kind} change boundaries must be strictly increasing")]
    ChangeOrder {
        /// Schedule kind.
        kind: &'static str,
    },
}

fn validate_changes<T>(
    kind: &'static str,
    transaction_count: usize,
    changes: &[TelosExecutionChange<T>],
) -> Result<(), TelosScheduleError> {
    let mut previous = None;
    for change in changes {
        if change.boundary > transaction_count {
            return Err(TelosScheduleError::ChangeBoundary {
                kind,
                boundary: change.boundary,
                transaction_count,
            })
        }
        if previous.is_some_and(|previous| change.boundary <= previous) {
            return Err(TelosScheduleError::ChangeOrder { kind })
        }
        previous = Some(change.boundary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_telos_rpc_engine_api::structs::{
        TelosExecutionChange as WireExecutionChange, TelosExecutionMetadataV3,
        TELOS_EXECUTION_METADATA_VERSION,
    };

    #[test]
    fn fixed_gas_price_caps_legacy_execution_and_balance_checks() {
        let inner =
            TxEnv { gas_limit: 21_000, gas_price: 100, value: U256::from(7), ..Default::default() };
        let tx = TelosTxEnv::new(inner).with_telos_context(TelosExecutionContext {
            fixed_gas_price: 40,
            revision: 1,
            first_new_address: None,
        });

        // Generic RPC affordability checks call `gas_price()` directly. It must observe the same
        // native cap as execution or `eth_estimateGas` can reject an affordable transaction.
        assert_eq!(tx.gas_price(), 40);
        assert_eq!(tx.effective_gas_price(0), 40);
        assert_eq!(tx.max_fee_per_gas(), 100);
        assert_eq!(tx.max_balance_spending().unwrap(), U256::from(21_000 * 40 + 7));
    }

    #[test]
    fn unauthenticated_dynamic_fee_retains_standard_ethereum_pricing() {
        let tx = TelosTxEnv::new(TxEnv {
            tx_type: 2,
            gas_limit: 21_000,
            gas_price: 100,
            gas_priority_fee: Some(60),
            ..Default::default()
        });

        assert_eq!(tx.max_priority_fee_per_gas(), Some(60));
        assert_eq!(tx.effective_gas_price(10), 70);
    }

    #[test]
    fn missing_context_does_not_silently_zero_rpc_simulation_gas() {
        let inner = TxEnv { gas_price: 100, ..Default::default() };
        let tx = TelosTxEnv::new(inner);

        assert_eq!(tx.effective_gas_price(0), 100);
        assert_eq!(tx.max_fee_per_gas(), 100);
    }

    #[test]
    fn first_new_address_is_transaction_local() {
        let address = Address::repeat_byte(0x11);
        let mut tx = TelosTxEnv::default().with_telos_context(TelosExecutionContext::default());
        *tx.first_new_address_mut().unwrap() = Some(address);

        assert_eq!(tx.context.unwrap().first_new_address, Some(address));
        assert!(TelosTxEnv::default().context.is_none());
    }

    #[test]
    fn schedule_applies_changes_at_explicit_boundaries() {
        let schedule = TelosBlockExecutionSchedule::new(
            3,
            TelosExecutionContext {
                fixed_gas_price: 10,
                revision: 0,
                first_new_address: Some(Address::repeat_byte(0xff)),
            },
            vec![
                TelosExecutionChange { boundary: 1, value: 20 },
                TelosExecutionChange { boundary: 3, value: 30 },
            ],
            vec![TelosExecutionChange { boundary: 2, value: 1 }],
        )
        .unwrap();

        assert_eq!(
            schedule.context_for_transaction(0).unwrap(),
            TelosExecutionContext { fixed_gas_price: 10, revision: 0, first_new_address: None }
        );
        assert_eq!(schedule.context_for_transaction(1).unwrap().fixed_gas_price, 20);
        assert_eq!(schedule.context_for_transaction(2).unwrap().revision, 1);
        assert_eq!(schedule.child_context().fixed_gas_price, 30);
        assert_eq!(schedule.context_for_boundary(3).unwrap(), schedule.child_context());
        assert_eq!(
            schedule.context_for_boundary(4),
            Err(TelosScheduleError::ExecutionBoundary { boundary: 4, transaction_count: 3 })
        );
    }

    #[test]
    fn schedule_rejects_ambiguous_or_out_of_range_changes() {
        let duplicate = vec![
            TelosExecutionChange { boundary: 1, value: 20 },
            TelosExecutionChange { boundary: 1, value: 30 },
        ];
        assert_eq!(
            TelosBlockExecutionSchedule::new(
                2,
                TelosExecutionContext::default(),
                duplicate,
                Vec::new(),
            ),
            Err(TelosScheduleError::ChangeOrder { kind: "gas price" })
        );

        let outside = vec![TelosExecutionChange { boundary: 3, value: 1 }];
        assert_eq!(
            TelosBlockExecutionSchedule::new(
                2,
                TelosExecutionContext::default(),
                Vec::new(),
                outside,
            ),
            Err(TelosScheduleError::ChangeBoundary {
                kind: "revision",
                boundary: 3,
                transaction_count: 2,
            })
        );
    }

    #[test]
    fn wire_metadata_preserves_boundary_semantics() {
        let metadata = TelosExecutionMetadataV3 {
            version: TELOS_EXECUTION_METADATA_VERSION,
            transaction_count: 2,
            execution_base_fee: U256::from(7),
            starting_gas_price: U256::from(10),
            starting_revision: 0,
            gas_price_changes: vec![WireExecutionChange { boundary: 1, value: U256::from(20) }],
            revision_changes: vec![WireExecutionChange { boundary: 2, value: 1 }],
            ..Default::default()
        };

        let schedule = TelosBlockExecutionSchedule::from_metadata(&metadata).unwrap();
        assert_eq!(schedule.context_for_transaction(0).unwrap().fixed_gas_price, 10);
        assert_eq!(schedule.context_for_transaction(1).unwrap().fixed_gas_price, 20);
        assert_eq!(schedule.context_for_transaction(1).unwrap().revision, 0);
        assert_eq!(schedule.child_context().revision, 1);
    }
}

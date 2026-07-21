//! Telos transaction validation and fee handling for revm 41.

use crate::{execution::TelosEvmContext, frame::TelosEvmInner};
use alloy_primitives::{TxKind, U256};
use revm::{
    context::{result::EVMError, BlockEnv, CfgEnv},
    context_interface::{
        journaled_state::{account::JournaledAccountTr, JournalTr},
        result::{HaltReason, InvalidTransaction},
        Block, Cfg, ContextTr, Database, Transaction,
    },
    handler::{
        instructions::EthInstructions, post_execution, pre_execution, validation, EvmTr, FrameTr,
        Handler, PrecompileProvider,
    },
    inspector::{Inspector, InspectorEvmTr, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, InterpreterResult},
};
use std::marker::PhantomData;

type InnerEvm<DB, INSP, PRECOMPILE> =
    TelosEvmInner<DB, INSP, EthInstructions<EthInterpreter, TelosEvmContext<DB>>, PRECOMPILE>;

/// Mainnet-compatible handler with Telos's historical transaction exceptions.
#[derive(Debug, Clone)]
pub struct TelosHandler<DB, INSP, PRECOMPILE>(PhantomData<(DB, INSP, PRECOMPILE)>);

impl<DB, INSP, PRECOMPILE> Default for TelosHandler<DB, INSP, PRECOMPILE> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<DB, INSP, PRECOMPILE> Handler for TelosHandler<DB, INSP, PRECOMPILE>
where
    DB: Database,
    PRECOMPILE: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
{
    type Evm = InnerEvm<DB, INSP, PRECOMPILE>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        let authenticated_tx_type = evm.ctx.tx.context.is_some().then(|| evm.ctx.tx.tx_type());
        if let Some(tx_type) = authenticated_tx_type {
            match tx_type {
                0 => {}
                1 => return Err(InvalidTransaction::Eip2930NotSupported.into()),
                2 => return Err(InvalidTransaction::Eip1559NotSupported.into()),
                3 => return Err(InvalidTransaction::Eip4844NotSupported.into()),
                4 => return Err(InvalidTransaction::Eip7702NotSupported.into()),
                _ => {
                    return Err(InvalidTransaction::Str(
                        format!("Telos legacy runtime does not support transaction type {tx_type}")
                            .into(),
                    )
                    .into())
                }
            }
        }
        // Telos deposit/withdraw transactions historically use chain ID 3. Keep every other chain
        // ID check intact and restore the configuration immediately after this validation call.
        let accepts_embedded_sender =
            evm.ctx.tx.context.is_some() && evm.ctx.tx.chain_id() == Some(3);
        let check_chain_id = evm.ctx.cfg.tx_chain_id_check;
        let disable_base_fee = evm.ctx.cfg.disable_base_fee;
        if accepts_embedded_sender {
            evm.ctx.cfg.tx_chain_id_check = false;
        }
        if authenticated_tx_type.is_some() {
            // Telos has no fee market. The nonzero Engine payload field is transport metadata, not
            // a native transaction-admission floor.
            evm.ctx.cfg.disable_base_fee = true;
        }
        let result = validation::validate_env(&mut evm.ctx);
        evm.ctx.cfg.tx_chain_id_check = check_chain_id;
        evm.ctx.cfg.disable_base_fee = disable_base_fee;
        result
    }

    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        _init_and_floor_gas: &mut revm::interpreter::InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        if evm.ctx.tx.context.is_none() {
            return pre_execution::validate_against_state_and_deduct_caller(&mut evm.ctx)
        }
        validate_and_deduct(&mut evm.ctx)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if evm.ctx.tx.context.is_some() && matches!(evm.ctx.cfg.chain_id, 40 | 41) {
            return Ok(())
        }
        post_execution::reward_beneficiary(&mut evm.ctx, exec_result.gas()).map_err(From::from)
    }
}

impl<DB, INSP, PRECOMPILE> InspectorHandler for TelosHandler<DB, INSP, PRECOMPILE>
where
    DB: Database,
    INSP: Inspector<<InnerEvm<DB, INSP, PRECOMPILE> as EvmTr>::Context, EthInterpreter>,
    PRECOMPILE: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
    InnerEvm<DB, INSP, PRECOMPILE>: InspectorEvmTr<Inspector = INSP>,
{
    type IT = EthInterpreter;
}

fn validate_and_deduct<DB: Database>(
    context: &mut TelosEvmContext<DB>,
) -> Result<(), EVMError<DB::Error>> {
    let (block, tx, cfg, journal, _, _) = context.all_mut();
    let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

    // Preserve EIP-3607 while applying the two nonce exceptions used by native-created and
    // chain-ID-3 transactions.
    pre_execution::validate_account_nonce_and_code(
        &caller.account().info,
        caller.nonce(),
        cfg.is_eip3607_disabled(),
        true,
    )?;
    if !cfg.is_nonce_check_disabled() {
        let tx_nonce = tx.nonce();
        let state_nonce = caller.nonce();
        if tx_nonce == u64::MAX && state_nonce == u64::MAX {
            return Err(InvalidTransaction::NonceOverflowInTransaction.into())
        }
        if tx_nonce > state_nonce && !(tx_nonce == 1 && state_nonce == 0) {
            return Err(InvalidTransaction::NonceTooHigh { tx: tx_nonce, state: state_nonce }.into())
        }
        if tx_nonce < state_nonce && !(tx_nonce == 0 && tx.chain_id() == Some(3)) {
            return Err(InvalidTransaction::NonceTooLow { tx: tx_nonce, state: state_nonce }.into())
        }
    }

    let mut balance = *caller.balance();
    let spending = tx.max_balance_spending()?;
    if tx.caller().is_zero() {
        // The legacy runtime funds the zero sender with the complete maximum transaction spend
        // before checking or deducting fees, even when the account already has enough balance.
        balance = balance
            .checked_add(spending)
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
    }
    if spending > balance {
        if cfg.is_balance_check_disabled() {
            balance = spending;
        } else {
            return Err(InvalidTransaction::LackOfFundForMaxFee {
                fee: Box::new(spending),
                balance: Box::new(balance),
            }
            .into())
        }
    }
    let new_balance = calculate_telos_caller_fee(balance, tx, block, cfg)?;
    caller.set_balance(new_balance);

    if tx.kind().is_call() {
        caller.bump_nonce();
    }
    if matches!(tx.kind(), TxKind::Call(address) if address.is_zero()) &&
        tx.chain_id() == Some(3) &&
        tx.nonce() == 0 &&
        caller.nonce() > 1
    {
        caller.set_nonce(caller.nonce() - 1);
    }
    if tx.caller().is_zero() {
        caller.set_nonce(0);
    }
    if tx.nonce() == 1 && caller.nonce() == 1 {
        caller.bump_nonce();
    }
    Ok(())
}

/// Deducts the authenticated native transaction fee using U256 arithmetic.
///
/// The Telos contract caps a canonical legacy transaction's signed gas price at the authenticated
/// native fixed price.
fn calculate_telos_caller_fee(
    balance: U256,
    tx: &crate::execution::TelosTxEnv,
    block: &BlockEnv,
    cfg: &CfgEnv,
) -> Result<U256, InvalidTransaction> {
    if cfg.is_fee_charge_disabled() {
        return Ok(balance)
    }

    let gas_fee = U256::from(tx.gas_limit())
        .checked_mul(U256::from(tx.effective_gas_price(block.basefee as u128)))
        .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
    let blob_fee = U256::from(tx.total_blob_gas())
        .checked_mul(U256::from(block.blob_gasprice().unwrap_or_default()))
        .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
    let total_fee =
        gas_fee.checked_add(blob_fee).ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
    Ok(balance.saturating_sub(total_fee))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{TelosEvmFactory, TelosExecutionContext, TelosTxEnv};
    use alloy_evm::{Evm, EvmEnv, EvmFactory};
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, TxEnv},
        database::InMemoryDB,
        primitives::hardfork::SpecId,
        state::AccountInfo,
    };

    fn evm_with_caller(
        caller: Address,
        caller_nonce: u64,
    ) -> crate::execution::TelosEvm<
        InMemoryDB,
        revm::inspector::NoOpInspector,
        alloy_evm::precompiles::PrecompilesMap,
    > {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(10_000_000),
                nonce: caller_nonce,
                ..Default::default()
            },
        );
        let target = Address::repeat_byte(0x22);
        db.insert_account_info(
            target,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[0x00]))),
        );
        let env = EvmEnv {
            cfg_env: CfgEnv::new_with_spec(SpecId::BERLIN).with_chain_id(40),
            block_env: BlockEnv { gas_limit: 30_000_000, ..Default::default() },
        };
        TelosEvmFactory.create_evm(db, env)
    }

    fn transaction(caller: Address, nonce: u64, native_context: bool) -> TelosTxEnv {
        let tx = TelosTxEnv::new(TxEnv {
            caller,
            chain_id: Some(3),
            nonce,
            gas_limit: 30_000,
            gas_price: 1,
            kind: TxKind::Call(Address::repeat_byte(0x22)),
            ..Default::default()
        });
        if native_context {
            tx.with_telos_context(TelosExecutionContext {
                fixed_gas_price: 1,
                revision: 1,
                first_new_address: None,
            })
        } else {
            tx
        }
    }

    #[test]
    fn authenticated_chain_three_transaction_is_accepted() {
        let caller = Address::repeat_byte(0x11);
        let result = evm_with_caller(caller, 0).transact_raw(transaction(caller, 0, true));
        assert!(result.is_ok());
    }

    #[test]
    fn missing_native_context_retains_chain_id_validation() {
        let caller = Address::repeat_byte(0x11);
        let error =
            evm_with_caller(caller, 0).transact_raw(transaction(caller, 0, false)).unwrap_err();
        assert!(matches!(error, EVMError::Transaction(InvalidTransaction::InvalidChainId)));
    }

    #[test]
    fn authenticated_telos_execution_rejects_unsupported_typed_transactions() {
        let caller = Address::repeat_byte(0x11);
        for (tx_type, expected) in [
            (1, InvalidTransaction::Eip2930NotSupported),
            (3, InvalidTransaction::Eip4844NotSupported),
            (4, InvalidTransaction::Eip7702NotSupported),
        ] {
            let mut tx = transaction(caller, 0, true);
            tx.inner.tx_type = tx_type;
            let error = evm_with_caller(caller, 0).transact_raw(tx).unwrap_err();
            assert!(matches!(error, EVMError::Transaction(actual) if actual == expected));
        }
    }

    #[test]
    fn authenticated_eip1559_is_rejected_until_qualified_activation() {
        let caller = Address::repeat_byte(0x11);
        let mut tx = transaction(caller, 0, true);
        tx.inner.tx_type = 2;
        tx.inner.gas_price = 100;
        tx.inner.gas_priority_fee = Some(0);
        tx.context.as_mut().unwrap().revision = 2;

        let error = evm_with_caller(caller, 0).transact_raw(tx).unwrap_err();
        assert!(matches!(error, EVMError::Transaction(InvalidTransaction::Eip1559NotSupported)));
    }

    #[test]
    fn native_first_nonce_advances_from_zero_to_two() {
        let caller = Address::repeat_byte(0x11);
        let output = evm_with_caller(caller, 0).transact_raw(transaction(caller, 1, true)).unwrap();
        assert_eq!(output.state.get(&caller).unwrap().info.nonce, 2);
    }

    #[test]
    fn telos_does_not_reward_block_beneficiary() {
        let caller = Address::repeat_byte(0x11);
        let beneficiary = Address::repeat_byte(0x44);
        let mut evm = evm_with_caller(caller, 0);
        evm.block.beneficiary = beneficiary;
        let output = evm.transact_raw(transaction(caller, 0, true)).unwrap();
        assert_eq!(output.state.get(&beneficiary).map(|account| account.info.balance), None);
    }

    #[test]
    fn legacy_transaction_gas_price_is_capped_by_native_fixed_price() {
        let caller = Address::repeat_byte(0x11);
        let mut tx = transaction(caller, 0, true);
        tx.inner.gas_price = 100;
        tx.context.as_mut().unwrap().fixed_gas_price = 7;

        let mut evm = evm_with_caller(caller, 0);
        evm.block.basefee = 1_000;
        let output = evm.transact_raw(tx).unwrap();
        let gas_used = output.result.gas().tx_gas_used();
        let expected_balance = U256::from(10_000_000 - gas_used * 7);
        assert_eq!(output.state.get(&caller).unwrap().info.balance, expected_balance);
    }

    #[test]
    fn native_nonce_overflow_is_rejected() {
        let caller = Address::repeat_byte(0x11);
        let error = evm_with_caller(caller, u64::MAX)
            .transact_raw(transaction(caller, u64::MAX, true))
            .unwrap_err();
        assert!(matches!(
            error,
            EVMError::Transaction(InvalidTransaction::NonceOverflowInTransaction)
        ));
    }

    #[test]
    fn funded_zero_caller_is_unconditionally_topped_up_before_fee_deduction() {
        let caller = Address::ZERO;
        let output = evm_with_caller(caller, 0).transact_raw(transaction(caller, 0, true)).unwrap();
        let gas_used = output.result.gas().tx_gas_used();
        let expected_balance = U256::from(10_000_000 + 30_000 - gas_used);
        assert_eq!(output.state.get(&caller).unwrap().info.balance, expected_balance);
        assert_eq!(output.state.get(&caller).unwrap().info.nonce, 0);
    }

    #[test]
    fn chain_three_deposit_to_zero_burns_value() {
        let caller = Address::repeat_byte(0x11);
        let mut evm = evm_with_caller(caller, 0);
        evm.components_mut().0.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(7), ..Default::default() },
        );
        let mut tx = transaction(caller, 0, true);
        tx.inner.kind = TxKind::Call(Address::ZERO);
        tx.inner.value = U256::from(25);

        let output = evm.transact_raw(tx).unwrap();
        let gas_used = output.result.gas().tx_gas_used();
        assert_eq!(output.state[&caller].info.balance, U256::from(10_000_000 - gas_used - 25));
        assert_eq!(output.state[&Address::ZERO].info.balance, U256::from(7));
    }

    #[test]
    fn zero_caller_transfer_stops_before_target_bytecode() {
        let caller = Address::ZERO;
        let target = Address::repeat_byte(0x22);
        let mut evm = evm_with_caller(caller, 0);
        evm.components_mut().0.insert_account_info(
            target,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[
                0x60, 0x01, // PUSH1 1
                0x60, 0x00, // PUSH1 0
                0x55, // SSTORE
                0x00, // STOP
            ]))),
        );
        let mut tx = transaction(caller, 0, true);
        tx.inner.value = U256::from(25);

        let output = evm.transact_raw(tx).unwrap();
        assert_eq!(output.state[&target].info.balance, U256::from(25));
        assert!(output.state[&target].storage.is_empty());
    }

    #[test]
    fn zero_caller_to_zero_is_a_balance_neutral_self_transfer_before_stop() {
        let caller = Address::ZERO;
        let mut tx = transaction(caller, 0, true);
        tx.inner.kind = TxKind::Call(Address::ZERO);
        tx.inner.value = U256::from(25);

        let output = evm_with_caller(caller, 0).transact_raw(tx).unwrap();
        let gas_used = output.result.gas().tx_gas_used();
        assert_eq!(
            output.state[&caller].info.balance,
            U256::from(10_000_000 + 30_000 + 25 - gas_used)
        );
    }

    #[test]
    fn chain_three_stale_nonce_to_zero_preserves_existing_nonce() {
        let caller = Address::repeat_byte(0x11);
        let mut tx = transaction(caller, 0, true);
        tx.inner.kind = TxKind::Call(Address::ZERO);

        let output = evm_with_caller(caller, 5).transact_raw(tx).unwrap();

        assert_eq!(output.state[&caller].info.nonce, 5);
    }

    #[test]
    fn chain_three_stale_nonce_to_nonzero_advances_existing_nonce() {
        let caller = Address::repeat_byte(0x11);
        let output = evm_with_caller(caller, 5).transact_raw(transaction(caller, 0, true)).unwrap();

        assert_eq!(output.state[&caller].info.nonce, 6);
    }

    #[test]
    fn fixed_fee_deduction_uses_authenticated_native_price() {
        let tx = TelosTxEnv::new(TxEnv {
            gas_limit: u64::MAX,
            gas_price: u128::MAX,
            ..Default::default()
        })
        .with_telos_context(TelosExecutionContext {
            fixed_gas_price: 1,
            revision: 1,
            first_new_address: None,
        });
        let block = BlockEnv { basefee: u64::MAX, ..Default::default() };
        let balance = U256::MAX;
        let expected_fee = U256::from(u64::MAX);

        assert_eq!(
            calculate_telos_caller_fee(balance, &tx, &block, &CfgEnv::default()).unwrap(),
            balance - expected_fee
        );
    }

    #[test]
    fn missing_context_credits_zero_with_stock_ethereum_transfer() {
        let caller = Address::repeat_byte(0x11);
        let mut evm = evm_with_caller(caller, 0);
        evm.components_mut().0.insert_account_info(
            Address::ZERO,
            AccountInfo { balance: U256::from(7), ..Default::default() },
        );
        evm.block.beneficiary = Address::repeat_byte(0x44);
        let mut tx = transaction(caller, 0, false);
        tx.inner.chain_id = Some(40);
        tx.inner.kind = TxKind::Call(Address::ZERO);
        tx.inner.value = U256::from(25);

        let output = evm.transact_raw(tx).unwrap();
        assert_eq!(output.state[&Address::ZERO].info.balance, U256::from(32));
    }

    #[test]
    fn inspector_path_executes_telos_frame() {
        let caller = Address::repeat_byte(0x11);
        let mut evm = evm_with_caller(caller, 0);
        evm.set_inspector_enabled(true);

        assert!(evm.transact_raw(transaction(caller, 0, true)).is_ok());
    }

    #[test]
    fn system_call_without_native_context_uses_stock_frame() {
        let caller = Address::repeat_byte(0x11);
        let target = Address::repeat_byte(0x22);
        let mut evm = evm_with_caller(caller, 0);
        evm.components_mut().0.insert_account_info(
            target,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[
                0x60, 0x01, // PUSH1 1
                0x60, 0x00, // PUSH1 0
                0x55, // SSTORE
                0x00, // STOP
            ]))),
        );

        let output = evm.transact_system_call(caller, target, Bytes::new()).unwrap();
        assert!(!output.state[&target].storage.is_empty());
    }
}

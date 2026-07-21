//! Telos-specific EVM instruction overrides.
//!
//! The native Telos EVM revision is transaction-scoped rather than an Ethereum hardfork. These
//! overrides therefore preserve the upstream instruction table and only change behavior when an
//! authenticated [`TelosTxEnv`](crate::execution::TelosTxEnv) context is present. RPC simulations
//! without native context retain upstream Ethereum behavior.
//!
//! This module intentionally covers instruction-table behavior only. Transaction validation,
//! caller deduction and nonce exceptions, zero-address transfer behavior, the revision-zero
//! `DELEGATECALL` balance check, and beneficiary suppression belong in a custom revm
//! [`Handler`](revm::handler::Handler) and frame implementation.

use crate::execution::TelosEvmContext;

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use revm::{
    bytecode::opcode::{
        BALANCE, BLOCKHASH, CALL, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, GASLIMIT, PUSH0,
        SELFDESTRUCT, STATICCALL,
    },
    context_interface::{host::LoadError, journaled_state::AccountInfoLoad},
    handler::instructions::EthInstructions,
    interpreter::{
        instructions::{
            contract::{get_memory_input_and_out_ranges, load_acc_and_calc_gas},
            host as ethereum_host,
            utility::{IntoAddress, IntoU256},
        },
        interpreter::EthInterpreter,
        interpreter_action::FrameInput,
        interpreter_types::{
            InputsTr, InterpreterTypes, LoopControl, MemoryTr, RuntimeFlag, StackTr,
        },
        CallInput, CallInputs, CallScheme, CallValue, Gas, Host, Instruction, InstructionContext,
        InstructionExecResult, InstructionResult, InterpreterAction,
    },
    primitives::hardfork::SpecId,
    Database,
};
use std::{boxed::Box, cmp::min};

/// Builds the Ethereum instruction set with Telos transaction-scoped overrides installed.
pub fn telos_instructions<DB: Database>(
    spec: SpecId,
) -> EthInstructions<EthInterpreter, TelosEvmContext<DB>> {
    let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
    install_telos_instructions(&mut instructions);
    instructions
}

/// Installs the Telos instruction overrides while retaining upstream static gas costs.
pub fn install_telos_instructions<DB: Database>(
    instructions: &mut EthInstructions<EthInterpreter, TelosEvmContext<DB>>,
) {
    let table = instructions.instruction_table_mut();
    table[BALANCE as usize] = Instruction::new(balance::<EthInterpreter, DB>);
    table[EXTCODESIZE as usize] = Instruction::new(extcodesize::<EthInterpreter, DB>);
    table[EXTCODEHASH as usize] = Instruction::new(extcodehash::<EthInterpreter, DB>);
    table[EXTCODECOPY as usize] = Instruction::new(extcodecopy::<EthInterpreter, DB>);
    table[BLOCKHASH as usize] = Instruction::new(blockhash::<EthInterpreter, DB>);
    table[GASLIMIT as usize] = Instruction::new(gaslimit::<EthInterpreter, DB>);
    table[PUSH0 as usize] = Instruction::new(push0::<EthInterpreter, DB>);
    table[CALL as usize] = Instruction::new(call::<CALL, EthInterpreter, DB>);
    table[STATICCALL as usize] = Instruction::new(call::<STATICCALL, EthInterpreter, DB>);
    table[SELFDESTRUCT as usize] = Instruction::new(selfdestruct::<EthInterpreter, DB>);
}

fn load_account<'a, DB: Database>(
    gas: &mut Gas,
    host: &'a mut TelosEvmContext<DB>,
    address: Address,
    load_code: bool,
) -> Result<AccountInfoLoad<'a>, LoadError> {
    let cold_load_gas = host.gas_params().cold_account_additional_cost();
    let skip_cold_load = gas.remaining() < cold_load_gas;
    let account = host.load_account_info_skip_cold_load(address, load_code, skip_cold_load)?;
    if account.is_cold && !gas.record_regular_cost(cold_load_gas) {
        return Err(LoadError::ColdLoadSkipped)
    }
    Ok(account)
}

/// Records the first account considered empty by revm's delegated-account loader.
///
/// The legacy implementation performed this second delegated load after the opcode's normal
/// account load. Keeping that ordering preserves both EIP-7702 delegated-account interpretation
/// and warm/cold state transitions.
fn observe_new_address<DB: Database>(
    host: &mut TelosEvmContext<DB>,
    address: Address,
) -> Result<bool, InstructionResult> {
    let account =
        host.load_account_delegated(address).ok_or(InstructionResult::FatalExternalError)?;
    let is_new = account.data.is_empty;
    if is_new {
        let first_new_address =
            host.tx.first_new_address_mut().ok_or(InstructionResult::FatalExternalError)?;
        if first_new_address.is_none() {
            *first_new_address = Some(address);
        }
    }
    Ok(is_new)
}

fn balance<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return ethereum_host::balance(context)
    }

    revm::interpreter::popn_top!([], top, context.interpreter);
    let address = top.into_address();
    let account = load_account(&mut context.interpreter.gas, context.host, address, false)?;
    let balance = account.balance;
    drop(account);

    let is_new = observe_new_address(context.host, address)?;
    *top = if context.host.tx.revision() == Some(0) && is_new { U256::ZERO } else { balance };
    Ok(())
}

fn extcodesize<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return ethereum_host::extcodesize(context)
    }

    revm::interpreter::popn_top!([], top, context.interpreter);
    let address = top.into_address();
    let account = load_account(&mut context.interpreter.gas, context.host, address, true)?;
    let code_len = account.code.as_ref().expect("code was requested").len();
    drop(account);

    let is_new = observe_new_address(context.host, address)?;
    *top = if context.host.tx.revision() == Some(0) && is_new {
        U256::ZERO
    } else {
        U256::from(code_len)
    };
    Ok(())
}

fn extcodehash<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return ethereum_host::extcodehash(context)
    }
    if !context.interpreter.runtime_flag.spec_id().is_enabled_in(SpecId::PETERSBURG) {
        return Err(InstructionResult::NotActivated)
    }

    revm::interpreter::popn_top!([], top, context.interpreter);
    let address = top.into_address();
    let account = load_account(&mut context.interpreter.gas, context.host, address, false)?;
    let code_hash = if account.is_empty { B256::ZERO } else { account.code_hash };
    drop(account);

    let is_new = observe_new_address(context.host, address)?;
    *top = if context.host.tx.revision() == Some(0) && is_new {
        U256::ZERO
    } else {
        code_hash.into_u256()
    };
    Ok(())
}

fn extcodecopy<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return ethereum_host::extcodecopy(context)
    }

    revm::interpreter::popn!([address, memory_offset, code_offset, len], context.interpreter);
    let address = address.into_address();
    let len = usize::try_from(len).map_err(|_| InstructionResult::InvalidOperandOOG)?;
    revm::interpreter::gas!(context.interpreter, context.host.gas_params().extcodecopy(len));

    let mut memory_offset_usize = 0;
    if len != 0 {
        memory_offset_usize =
            usize::try_from(memory_offset).map_err(|_| InstructionResult::InvalidOperandOOG)?;
        context.interpreter.resize_memory(context.host.gas_params(), memory_offset_usize, len)?;
    }

    let account = load_account(&mut context.interpreter.gas, context.host, address, true)?;
    let code = account.code.as_ref().expect("code was requested").original_bytes();
    drop(account);

    // The pinned legacy implementation returned before its second delegated-account load when
    // the requested length was zero, so this opcode must not claim the first-new-address slot.
    if len == 0 {
        return Ok(())
    }

    let is_new = observe_new_address(context.host, address)?;
    let empty_code = Bytes::new();
    let code = if context.host.tx.revision() == Some(0) && is_new { &empty_code } else { &code };
    let code_offset = min(usize::try_from(code_offset).unwrap_or(usize::MAX), code.len());
    context.interpreter.memory.set_data(memory_offset_usize, code_offset, len, code);
    Ok(())
}

fn blockhash<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return ethereum_host::blockhash(context)
    }

    revm::interpreter::popn_top!([], number, context.interpreter);
    let requested = *number;
    let Some(distance) = context.host.block_number().checked_sub(requested) else {
        *number = U256::ZERO;
        return Ok(())
    };
    if distance.is_zero() || distance > U256::from(256) {
        *number = U256::ZERO;
        return Ok(())
    }

    let requested = u64::try_from(requested).unwrap_or(u64::MAX);
    *number = U256::from_be_bytes(keccak256(requested.to_string().as_bytes()).0);
    Ok(())
}

fn gaslimit<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    match context.host.tx.revision() {
        Some(0 | 1) => {
            revm::interpreter::push!(context.interpreter, U256::from(10_000_000));
            Ok(())
        }
        _ => revm::interpreter::instructions::block_info::gaslimit(context),
    }
}

fn push0<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    match context.host.tx.revision() {
        Some(revision) if revision > 0 => {
            revm::interpreter::push!(context.interpreter, U256::ZERO);
            Ok(())
        }
        _ => revm::interpreter::instructions::stack::push0(context),
    }
}

fn call<const KIND: u8, IT: InterpreterTypes, DB: Database>(
    mut context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return revm::interpreter::instructions::contract::call::<KIND, IT, _>(context)
    }

    if KIND == STATICCALL &&
        !context.interpreter.runtime_flag.spec_id().is_enabled_in(SpecId::BYZANTIUM)
    {
        return Err(InstructionResult::NotActivated)
    }

    let [local_gas_limit, to, value] = if KIND == CALL {
        context.interpreter.stack.popn::<3>().ok_or(InstructionResult::StackUnderflow)?
    } else {
        let [local_gas_limit, to] =
            context.interpreter.stack.popn::<2>().ok_or(InstructionResult::StackUnderflow)?;
        [local_gas_limit, to, U256::ZERO]
    };
    let to = to.into_address();
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);
    let has_transfer = !value.is_zero();

    if KIND == CALL && context.interpreter.runtime_flag.is_static() && has_transfer {
        return Err(InstructionResult::CallNotAllowedInsideStatic)
    }

    let (input, return_memory_offset) =
        get_memory_input_and_out_ranges(context.interpreter, context.host.gas_params())?;
    let (gas_limit, bytecode, bytecode_hash, charged_new_account_state_gas) =
        load_acc_and_calc_gas(&mut context, to, has_transfer, KIND == CALL, local_gas_limit)?;

    let is_new = observe_new_address(context.host, to)?;
    let target_address = if context.host.tx.revision() == Some(0) && is_new {
        context.host.tx.context.and_then(|context| context.first_new_address).unwrap_or(to)
    } else {
        to
    };
    let caller = context.interpreter.input.target_address();
    let scheme = if KIND == CALL { CallScheme::Call } else { CallScheme::StaticCall };
    let is_static = context.interpreter.runtime_flag.is_static() || KIND == STATICCALL;

    context.interpreter.bytecode.set_action(InterpreterAction::NewFrame(FrameInput::Call(
        Box::new(CallInputs {
            input: CallInput::SharedBuffer(input),
            gas_limit,
            target_address,
            caller,
            bytecode_address: to,
            known_bytecode: (bytecode_hash, bytecode),
            value: CallValue::Transfer(value),
            scheme,
            is_static,
            return_memory_offset,
            reservoir: context.interpreter.gas.reservoir(),
            charged_new_account_state_gas,
        }),
    )));
    Err(InstructionResult::Suspend)
}

fn selfdestruct<IT: InterpreterTypes, DB: Database>(
    context: InstructionContext<'_, TelosEvmContext<DB>, IT>,
) -> InstructionExecResult {
    if context.host.tx.revision().is_none() {
        return ethereum_host::selfdestruct(context)
    }
    if context.interpreter.runtime_flag.is_static() {
        return Err(InstructionResult::StateChangeDuringStaticCall)
    }

    revm::interpreter::popn!([target], context.interpreter);
    let target = target.into_address();
    let spec = context.interpreter.runtime_flag.spec_id();
    let cold_load_gas = context.host.gas_params().selfdestruct_cold_cost();
    let skip_cold_load = context.interpreter.gas.remaining() < cold_load_gas;
    let result = context.host.selfdestruct(
        context.interpreter.input.target_address(),
        target,
        skip_cold_load,
    )?;

    let should_charge_topup = if spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) {
        result.had_value && !result.target_exists
    } else {
        !result.target_exists
    };
    revm::interpreter::gas!(
        context.interpreter,
        context.host.gas_params().selfdestruct_cost(should_charge_topup, result.is_cold)
    );
    if context.host.is_amsterdam_eip8037_enabled() && should_charge_topup {
        revm::interpreter::state_gas!(
            context.interpreter,
            context.host.gas_params().new_account_state_gas()
        );
    }
    if !result.previously_destroyed {
        context.interpreter.gas.record_refund(context.host.gas_params().selfdestruct_refund());
    }

    observe_new_address(context.host, target)?;
    Err(InstructionResult::SelfDestruct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{TelosExecutionContext, TelosTxEnv};
    use revm::{
        bytecode::{opcode, opcode::GASPRICE, Bytecode},
        context::{BlockEnv, CfgEnv, Context},
        database::InMemoryDB,
        database_interface::EmptyDB,
        interpreter::{
            interpreter::{ExtBytecode, InputsImpl},
            FrameInput, Interpreter, SharedMemory,
        },
        state::AccountInfo,
    };

    fn test_host(
        revision: Option<u64>,
        block_number: u64,
        gas_limit: u64,
    ) -> TelosEvmContext<InMemoryDB> {
        let tx = if let Some(revision) = revision {
            TelosTxEnv::default()
                .with_telos_context(TelosExecutionContext { revision, ..Default::default() })
        } else {
            TelosTxEnv::default()
        };
        let block = BlockEnv { number: U256::from(block_number), gas_limit, ..Default::default() };
        Context::<BlockEnv, TelosTxEnv, CfgEnv, EmptyDB>::new(EmptyDB::new(), SpecId::CANCUN)
            .with_tx(tx)
            .with_block(block)
            .with_db(InMemoryDB::default())
    }

    fn run(
        host: &mut TelosEvmContext<InMemoryDB>,
        spec: SpecId,
        code: &[u8],
    ) -> Interpreter<EthInterpreter> {
        let mut interpreter = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(Bytecode::new_raw(Bytes::copy_from_slice(code))),
            InputsImpl::default(),
            false,
            spec,
            u64::MAX,
        );
        let instructions = telos_instructions::<InMemoryDB>(spec);
        let action =
            interpreter.run_plain(instructions.instruction_table(), instructions.gas_table(), host);
        assert_eq!(action.instruction_result(), Some(InstructionResult::Stop));
        interpreter
    }

    #[test]
    fn synthetic_blockhash_uses_decimal_block_number() {
        let mut host = test_host(Some(1), 100, 30_000_000);
        let interpreter =
            run(&mut host, SpecId::CANCUN, &[opcode::PUSH1, 42, BLOCKHASH, opcode::STOP]);

        assert_eq!(interpreter.stack.data(), &[U256::from_be_bytes(keccak256(b"42").0)]);
    }

    #[test]
    fn synthetic_blockhash_returns_zero_outside_history_window() {
        let mut host = test_host(Some(1), 400, 30_000_000);
        let interpreter =
            run(&mut host, SpecId::CANCUN, &[opcode::PUSH1, 42, BLOCKHASH, opcode::STOP]);
        assert_eq!(interpreter.stack.data(), &[U256::ZERO]);

        let mut host = test_host(Some(1), 42, 30_000_000);
        let interpreter =
            run(&mut host, SpecId::CANCUN, &[opcode::PUSH1, 43, BLOCKHASH, opcode::STOP]);
        assert_eq!(interpreter.stack.data(), &[U256::ZERO]);
    }

    #[test]
    fn legacy_gaslimit_is_fixed_until_revision_two() {
        for (revision, expected) in [(0, 10_000_000), (1, 10_000_000), (2, 30_000_000)] {
            let mut host = test_host(Some(revision), 1, 30_000_000);
            let interpreter = run(&mut host, SpecId::CANCUN, &[GASLIMIT, opcode::STOP]);
            assert_eq!(interpreter.stack.data(), &[U256::from(expected)]);
        }
    }

    #[test]
    fn gasprice_opcode_uses_native_fixed_price_cap() {
        let mut host = test_host(Some(1), 1, 30_000_000);
        host.tx.inner.gas_price = 100;
        host.tx.context.as_mut().unwrap().fixed_gas_price = 7;

        let interpreter = run(&mut host, SpecId::CANCUN, &[GASPRICE, opcode::STOP]);

        assert_eq!(interpreter.stack.data(), &[U256::from(7)]);
    }

    #[test]
    fn revision_one_activates_push0_before_shanghai() {
        let mut revision_one = test_host(Some(1), 1, 30_000_000);
        let interpreter = run(&mut revision_one, SpecId::LONDON, &[PUSH0, opcode::STOP]);
        assert_eq!(interpreter.stack.data(), &[U256::ZERO]);

        let mut revision_zero = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::new(
            SharedMemory::new(),
            ExtBytecode::new(Bytecode::new_raw(Bytes::from_static(&[PUSH0, opcode::STOP]))),
            InputsImpl::default(),
            false,
            SpecId::LONDON,
            u64::MAX,
        );
        let instructions = telos_instructions::<InMemoryDB>(SpecId::LONDON);
        let action = interpreter.run_plain(
            instructions.instruction_table(),
            instructions.gas_table(),
            &mut revision_zero,
        );
        assert_eq!(action.instruction_result(), Some(InstructionResult::NotActivated));
    }

    #[test]
    fn first_empty_address_remaps_later_legacy_call_target() {
        let first = Address::repeat_byte(0x11);
        let second = Address::repeat_byte(0x22);
        let mut host = test_host(Some(0), 1, 30_000_000);

        let mut balance_interpreter = Interpreter::default();
        assert!(balance_interpreter.stack.push(first.into_word().into()));
        balance(InstructionContext { interpreter: &mut balance_interpreter, host: &mut host })
            .unwrap();
        assert_eq!(host.tx.context.unwrap().first_new_address, Some(first));

        let mut call_interpreter = Interpreter::default();
        for value in [
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            second.into_word().into(),
            U256::from(1_000_000),
        ] {
            assert!(call_interpreter.stack.push(value));
        }
        let result = call::<CALL, EthInterpreter, InMemoryDB>(InstructionContext {
            interpreter: &mut call_interpreter,
            host: &mut host,
        });
        assert_eq!(result, Err(InstructionResult::Suspend));
        let action = call_interpreter.take_next_action();
        let InterpreterAction::NewFrame(FrameInput::Call(inputs)) = action else {
            panic!("expected call frame")
        };
        assert_eq!(inputs.target_address, first);
        assert_eq!(inputs.bytecode_address, second);
    }

    #[test]
    fn first_empty_address_remaps_legacy_staticcall_target() {
        let first = Address::repeat_byte(0x44);
        let second = Address::repeat_byte(0x55);
        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut balance_interpreter = Interpreter::default();
        assert!(balance_interpreter.stack.push(first.into_word().into()));
        balance(InstructionContext { interpreter: &mut balance_interpreter, host: &mut host })
            .unwrap();

        let mut call_interpreter = Interpreter::default();
        for value in [
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            second.into_word().into(),
            U256::from(1_000_000),
        ] {
            assert!(call_interpreter.stack.push(value));
        }
        let result = call::<STATICCALL, EthInterpreter, InMemoryDB>(InstructionContext {
            interpreter: &mut call_interpreter,
            host: &mut host,
        });
        assert_eq!(result, Err(InstructionResult::Suspend));
        let action = call_interpreter.take_next_action();
        let InterpreterAction::NewFrame(FrameInput::Call(inputs)) = action else {
            panic!("expected call frame")
        };
        assert_eq!(inputs.target_address, first);
        assert_eq!(inputs.bytecode_address, second);
        assert_eq!(inputs.scheme, CallScheme::StaticCall);
        assert!(inputs.is_static);
    }

    #[test]
    fn legacy_account_introspection_opcodes_observe_empty_addresses() {
        let addresses = [
            Address::repeat_byte(0x61),
            Address::repeat_byte(0x62),
            Address::repeat_byte(0x63),
            Address::repeat_byte(0x64),
        ];

        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::default();
        assert!(interpreter.stack.push(addresses[0].into_word().into()));
        balance(InstructionContext { interpreter: &mut interpreter, host: &mut host }).unwrap();
        assert_eq!(host.tx.context.unwrap().first_new_address, Some(addresses[0]));

        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::default();
        assert!(interpreter.stack.push(addresses[1].into_word().into()));
        extcodesize(InstructionContext { interpreter: &mut interpreter, host: &mut host }).unwrap();
        assert_eq!(host.tx.context.unwrap().first_new_address, Some(addresses[1]));

        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::default();
        assert!(interpreter.stack.push(addresses[2].into_word().into()));
        extcodehash(InstructionContext { interpreter: &mut interpreter, host: &mut host }).unwrap();
        assert_eq!(host.tx.context.unwrap().first_new_address, Some(addresses[2]));

        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::default();
        for value in [U256::from(1), U256::ZERO, U256::ZERO, addresses[3].into_word().into()] {
            assert!(interpreter.stack.push(value));
        }
        extcodecopy(InstructionContext { interpreter: &mut interpreter, host: &mut host }).unwrap();
        assert_eq!(host.tx.context.unwrap().first_new_address, Some(addresses[3]));
    }

    #[test]
    fn zero_length_extcodecopy_does_not_observe_first_empty_address() {
        let address = Address::repeat_byte(0x65);
        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::default();
        for value in [U256::ZERO, U256::ZERO, U256::ZERO, address.into_word().into()] {
            assert!(interpreter.stack.push(value));
        }

        extcodecopy(InstructionContext { interpreter: &mut interpreter, host: &mut host }).unwrap();

        assert_eq!(host.tx.context.unwrap().first_new_address, None);
    }

    #[test]
    fn selfdestruct_observes_empty_beneficiary_after_state_transition() {
        let beneficiary = Address::repeat_byte(0x66);
        let mut host = test_host(Some(0), 1, 30_000_000);
        let mut interpreter = Interpreter::default();
        // A running frame has already loaded its target account into the journal.
        assert!(host.balance(interpreter.input.target_address()).is_some());
        assert!(interpreter.stack.push(beneficiary.into_word().into()));

        let result =
            selfdestruct(InstructionContext { interpreter: &mut interpreter, host: &mut host });

        assert_eq!(result, Err(InstructionResult::SelfDestruct));
        assert_eq!(host.tx.context.unwrap().first_new_address, Some(beneficiary));
    }

    #[test]
    fn extcodecopy_respects_nonzero_memory_offset() {
        let address = Address::repeat_byte(0x33);
        let mut host = test_host(Some(0), 1, 30_000_000);
        host.journaled_state.database.insert_account_info(
            address,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[0xaa, 0xbb]))),
        );

        let mut interpreter = Interpreter::default();
        for value in [U256::from(2), U256::ZERO, U256::from(32), address.into_word().into()] {
            assert!(interpreter.stack.push(value));
        }
        extcodecopy(InstructionContext { interpreter: &mut interpreter, host: &mut host }).unwrap();

        let copied = interpreter.memory.slice_len(32, 2);
        assert_eq!(&*copied, &[0xaa, 0xbb]);
    }

    #[test]
    fn missing_native_context_keeps_ethereum_gaslimit() {
        let mut host = test_host(None, 1, 30_000_000);
        let interpreter = run(&mut host, SpecId::CANCUN, &[GASLIMIT, opcode::STOP]);
        assert_eq!(interpreter.stack.data(), &[U256::from(30_000_000)]);
    }
}

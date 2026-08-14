//! Telos-specific EVM frame handling.
//!
//! The legacy Telos runtime changes a small set of operations that happen while a call or create
//! frame is initialized rather than while an opcode executes. This frame keeps the upstream
//! [`EthFrame`] implementation for interpreter execution and return handling, and limits the
//! native differences to authenticated [`TelosTxEnv`](crate::execution::TelosTxEnv) contexts.
//! Transactions without that context retain upstream Ethereum behavior.

use crate::execution::TelosEvmContext;
use alloy_primitives::{Address, Bytes};
use revm::{
    context::{result::FromStringError, FrameStack},
    context_interface::{
        context::ContextError,
        journaled_state::{account::JournaledAccountTr, JournalTr},
        local::{FrameToken, OutFrame},
        ContextTr, Database, Transaction,
    },
    handler::{
        instructions::InstructionProvider, CallFrame, CreateFrame, EthFrame, EvmTr, FrameData,
        FrameInitOrResult, FrameResult, FrameTr, ItemOrResult, PrecompileProvider,
    },
    inspector::{Inspector, InspectorEvmTr, InspectorFrame},
    interpreter::{
        interpreter::{EthInterpreter, ExtBytecode},
        interpreter_action::FrameInit,
        CallInput, CallInputs, CallOutcome, CallValue, CreateInputs, CreateOutcome, FrameInput,
        Gas, InputsImpl, InstructionResult, InterpreterResult, SharedMemory,
    },
    primitives::constants::CALL_STACK_LIMIT,
    state::Bytecode,
};
use std::{boxed::Box, vec::Vec};

/// Ethereum frame execution with Telos's historical frame-initialization rules.
#[derive(Debug, Default)]
pub struct TelosFrame(pub EthFrame<EthInterpreter>);

/// EVM components backed by [`TelosFrame`].
///
/// This local container is required because Rust's orphan rules prevent implementing revm's
/// [`EvmTr`] for revm's generic `Evm` solely by selecting a local frame type.
#[expect(missing_debug_implementations)]
pub struct TelosEvmInner<DB: Database, INSP, I, P> {
    /// Execution context and journal.
    pub ctx: TelosEvmContext<DB>,
    /// Optional execution inspector.
    pub inspector: INSP,
    /// Opcode and gas tables.
    pub instruction: I,
    /// Precompile provider.
    pub precompiles: P,
    /// Reusable frame stack.
    pub frame_stack: FrameStack<TelosFrame>,
}

impl FrameTr for TelosFrame {
    type FrameResult = FrameResult;
    type FrameInit = FrameInit;
}

impl InspectorFrame for TelosFrame {
    type IT = EthInterpreter;

    fn eth_frame(&mut self) -> Option<&mut EthFrame<EthInterpreter>> {
        Some(&mut self.0)
    }
}

impl TelosFrame {
    /// Initializes a call or create frame with the supplied Telos execution context.
    pub fn init_with_context<
        DB: Database,
        PRECOMPILES: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
    >(
        this: OutFrame<'_, Self>,
        context: &mut TelosEvmContext<DB>,
        precompiles: &mut PRECOMPILES,
        frame_init: FrameInit,
    ) -> Result<ItemOrResult<FrameToken, FrameResult>, ContextError<DB::Error>> {
        let FrameInit { depth, memory, frame_input } = frame_init;

        match frame_input {
            FrameInput::Call(inputs) => {
                Self::make_call_frame(this, context, precompiles, depth, memory, inputs)
            }
            FrameInput::Create(inputs) => {
                Self::make_create_frame(this, context, depth, memory, inputs)
            }
            FrameInput::Empty => unreachable!(),
        }
    }

    #[inline]
    fn make_call_frame<
        DB: Database,
        PRECOMPILES: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
        ERROR: From<DB::Error> + FromStringError,
    >(
        mut this: OutFrame<'_, Self>,
        context: &mut TelosEvmContext<DB>,
        precompiles: &mut PRECOMPILES,
        depth: usize,
        memory: SharedMemory,
        inputs: Box<CallInputs>,
    ) -> Result<ItemOrResult<FrameToken, FrameResult>, ERROR> {
        let reservoir_remaining_gas = inputs.reservoir;
        let charged_new_account_state_gas = inputs.charged_new_account_state_gas;
        let gas =
            Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, reservoir_remaining_gas);
        let return_result = |instruction_result: InstructionResult| {
            Ok(ItemOrResult::Result(FrameResult::Call(CallOutcome {
                result: InterpreterResult { result: instruction_result, gas, output: Bytes::new() },
                memory_offset: inputs.return_memory_offset.clone(),
                was_precompile_called: false,
                precompile_call_logs: Vec::new(),
                charged_new_account_state_gas,
            })))
        };

        if depth > CALL_STACK_LIMIT as usize {
            return return_result(InstructionResult::CallTooDeep)
        }

        let checkpoint = context.journal_mut().checkpoint();
        if let Some(result) = apply_call_value(context, &inputs)? {
            context.journal_mut().checkpoint_revert(checkpoint);
            return return_result(result)
        }

        // Native chain-ID-3 transactions from the zero address represent deposits. The value
        // transfer above is authoritative, but executing target bytecode would fabricate a second
        // state transition that does not exist in the native runtime.
        if stops_after_native_zero_caller_transfer(context) {
            context.journal_mut().checkpoint_commit();
            return return_result(InstructionResult::Stop)
        }

        let interpreter_input = InputsImpl {
            target_address: inputs.target_address,
            caller_address: inputs.caller,
            bytecode_address: Some(inputs.bytecode_address),
            input: inputs.input.clone(),
            call_value: inputs.value.get(),
        };
        let is_static = inputs.is_static;
        let gas_limit = inputs.gas_limit;

        if let Some(result) = precompiles.run(context, &inputs).map_err(ERROR::from_string)? {
            let mut logs = Vec::new();
            if result.result.is_ok() {
                context.journal_mut().checkpoint_commit();
            } else {
                logs = context.journal_mut().logs()[checkpoint.log_i..].to_vec();
                context.journal_mut().checkpoint_revert(checkpoint);
            }
            return Ok(ItemOrResult::Result(FrameResult::Call(CallOutcome {
                result,
                memory_offset: inputs.return_memory_offset.clone(),
                was_precompile_called: true,
                precompile_call_logs: logs,
                charged_new_account_state_gas,
            })))
        }

        let (bytecode_hash, bytecode) = inputs.known_bytecode.clone();
        if bytecode.is_empty() {
            context.journal_mut().checkpoint_commit();
            return return_result(InstructionResult::Stop)
        }

        this.get(|| Self(EthFrame::invalid())).0.clear(
            FrameData::Call(CallFrame { return_memory_range: inputs.return_memory_offset.clone() }),
            FrameInput::Call(inputs),
            depth,
            memory,
            ExtBytecode::new_with_hash(bytecode, bytecode_hash),
            interpreter_input,
            is_static,
            *context.cfg().spec(),
            gas_limit,
            reservoir_remaining_gas,
            checkpoint,
        );
        Ok(ItemOrResult::Item(this.consume()))
    }

    #[inline]
    fn make_create_frame<DB: Database, ERROR: From<DB::Error> + FromStringError>(
        mut this: OutFrame<'_, Self>,
        context: &mut TelosEvmContext<DB>,
        depth: usize,
        memory: SharedMemory,
        mut inputs: Box<CreateInputs>,
    ) -> Result<ItemOrResult<FrameToken, FrameResult>, ERROR> {
        let authenticated_context = has_authenticated_context(context);
        if authenticated_context && inputs.caller().is_zero() {
            // An inspector may have populated the upstream created-address cache with the zero
            // account's stored nonce. Telos always derives zero-caller CREATE from nonce zero, so
            // rebuild the inputs to clear that cache before deriving the address.
            inputs = Box::new(CreateInputs::new(
                inputs.caller(),
                inputs.scheme(),
                inputs.value(),
                inputs.init_code().clone(),
                inputs.gas_limit(),
                inputs.reservoir(),
            ));
        }
        let reservoir_remaining_gas = inputs.reservoir();
        let spec = *context.cfg().spec();
        let return_error = |result| {
            Ok(ItemOrResult::Result(FrameResult::Create(CreateOutcome {
                result: InterpreterResult {
                    result,
                    gas: Gas::new_with_regular_gas_and_reservoir(
                        inputs.gas_limit(),
                        reservoir_remaining_gas,
                    ),
                    output: Bytes::new(),
                },
                address: None,
            })))
        };

        if depth > CALL_STACK_LIMIT as usize {
            return return_error(InstructionResult::CallTooDeep)
        }

        let journal = context.journal_mut();
        let creation_nonce = {
            let mut caller = journal.load_account_mut(inputs.caller())?.data;
            if *caller.balance() < inputs.value() {
                return return_error(InstructionResult::OutOfFunds)
            }

            let Some(creation_nonce) =
                creation_nonce(&mut caller, inputs.caller(), authenticated_context)
            else {
                return return_error(InstructionResult::Return)
            };
            creation_nonce
        };
        let created_address = inputs.created_address(creation_nonce);
        let init_code_hash =
            matches!(inputs.scheme(), revm::interpreter::CreateScheme::Create2 { .. })
                .then(|| inputs.init_code_hash());
        journal.load_account(created_address)?;
        let checkpoint = match journal.create_account_checkpoint(
            inputs.caller(),
            created_address,
            inputs.value(),
            spec,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(error) => return return_error(error.into()),
        };

        let bytecode = ExtBytecode::new_with_optional_hash(
            Bytecode::new_legacy(inputs.init_code().clone()),
            init_code_hash,
        );
        let interpreter_input = InputsImpl {
            target_address: created_address,
            caller_address: inputs.caller(),
            bytecode_address: None,
            input: CallInput::Bytes(Bytes::new()),
            call_value: inputs.value(),
        };
        let gas_limit = inputs.gas_limit();

        this.get(|| Self(EthFrame::invalid())).0.clear(
            FrameData::Create(CreateFrame { created_address }),
            FrameInput::Create(inputs),
            depth,
            memory,
            bytecode,
            interpreter_input,
            false,
            spec,
            gas_limit,
            reservoir_remaining_gas,
            checkpoint,
        );
        Ok(ItemOrResult::Item(this.consume()))
    }
}

/// Applies the value-side effects that must be inside the call frame checkpoint.
fn apply_call_value<DB: Database>(
    context: &mut TelosEvmContext<DB>,
    inputs: &CallInputs,
) -> Result<Option<InstructionResult>, DB::Error> {
    let authenticated_context = has_authenticated_context(context);
    let revision = context.tx.revision();
    let chain_id = context.tx.chain_id();
    let journal = context.journal_mut();

    match inputs.value {
        CallValue::Transfer(value)
            if authenticated_context &&
                chain_id == Some(3) &&
                inputs.target_address.is_zero() &&
                !inputs.caller.is_zero() =>
        {
            // A native withdrawal to address zero is a burn. Debit and journal the source while
            // only touching the destination; crediting the zero account would create an EVM-only
            // balance. Preserve the legacy temporary-credit overflow check and its error order.
            let source_balance = {
                let source = journal.load_account_mut(inputs.caller)?.data;
                *source.balance()
            };
            if value > source_balance {
                return Ok(Some(InstructionResult::OutOfFunds))
            }
            let destination_balance = {
                let destination = journal.load_account_mut(Address::ZERO)?.data;
                *destination.balance()
            };
            if destination_balance.checked_add(value).is_none() {
                return Ok(Some(InstructionResult::OverflowPayment))
            }
            {
                let mut source = journal.load_account_mut(inputs.caller)?.data;
                let debited = source.decr_balance(value);
                debug_assert!(debited);
            }
            journal.load_account_mut(Address::ZERO)?.data.touch();
            Ok(None)
        }
        CallValue::Transfer(value)
            if authenticated_context &&
                chain_id == Some(3) &&
                inputs.target_address.is_zero() &&
                inputs.caller.is_zero() =>
        {
            // The withdrawal burn guard explicitly excludes the zero sender. A zero-to-zero
            // transfer therefore behaves as a checked self-transfer: it touches the account but
            // leaves the balance unchanged before the native zero-caller early stop.
            let mut account = journal.load_account_mut(Address::ZERO)?.data;
            if value > *account.balance() {
                return Ok(Some(InstructionResult::OutOfFunds))
            }
            account.touch();
            Ok(None)
        }
        CallValue::Transfer(value) => {
            Ok(journal.transfer_loaded(inputs.caller, inputs.target_address, value).map(Into::into))
        }
        CallValue::Apparent(value)
            if authenticated_context &&
                revision == Some(0) &&
                inputs.scheme == revm::interpreter::CallScheme::DelegateCall =>
        {
            // Legacy revm performed a same-account transfer here. Its observable effects are a
            // balance check and a touch; the account balance is unchanged.
            let mut target = journal.load_account_mut(inputs.target_address)?.data;
            if value > *target.balance() {
                return Ok(Some(InstructionResult::OutOfFunds))
            }
            target.touch();
            Ok(None)
        }
        CallValue::Apparent(_) => Ok(None),
    }
}

/// Returns the CREATE nonce while preserving Telos's zero-address nonce invariant.
fn creation_nonce<A: JournaledAccountTr>(
    account: &mut A,
    caller: Address,
    authenticated_context: bool,
) -> Option<u64> {
    let nonce = account.nonce();
    if nonce == u64::MAX {
        return None
    }
    if authenticated_context && caller.is_zero() {
        account.touch();
        return Some(0)
    }
    account.bump_nonce().then_some(nonce)
}

const fn has_authenticated_context<DB: Database>(context: &TelosEvmContext<DB>) -> bool {
    context.tx.context.is_some()
}

fn stops_after_native_zero_caller_transfer<DB: Database>(context: &TelosEvmContext<DB>) -> bool {
    has_authenticated_context(context) &&
        context.tx.chain_id() == Some(3) &&
        context.tx.caller().is_zero()
}

impl<DB, INSP, I, P> EvmTr for TelosEvmInner<DB, INSP, I, P>
where
    DB: Database,
    I: InstructionProvider<Context = TelosEvmContext<DB>, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
{
    type Context = TelosEvmContext<DB>;
    type Instructions = I;
    type Precompiles = P;
    type Frame = TelosFrame;

    fn all(
        &self,
    ) -> (&Self::Context, &Self::Instructions, &Self::Precompiles, &FrameStack<Self::Frame>) {
        (&self.ctx, &self.instruction, &self.precompiles, &self.frame_stack)
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        (&mut self.ctx, &mut self.instruction, &mut self.precompiles, &mut self.frame_stack)
    }

    fn frame_init(
        &mut self,
        frame_input: FrameInit,
    ) -> Result<revm::handler::evm::FrameInitResult<'_, Self::Frame>, ContextError<DB::Error>> {
        let is_first_init = self.frame_stack.index().is_none();
        let new_frame =
            if is_first_init { self.frame_stack.start_init() } else { self.frame_stack.get_next() };
        let result = TelosFrame::init_with_context(
            new_frame,
            &mut self.ctx,
            &mut self.precompiles,
            frame_input,
        )?;

        Ok(result.map_item(|token| {
            if is_first_init {
                // SAFETY: `token` was produced by `start_init` for this exact stack.
                unsafe { self.frame_stack.end_init(token) };
            } else {
                // SAFETY: `token` was produced by `get_next` for this exact stack.
                unsafe { self.frame_stack.push(token) };
            }
            self.frame_stack.get()
        }))
    }

    fn frame_run(&mut self) -> Result<FrameInitOrResult<Self::Frame>, ContextError<DB::Error>> {
        let frame = self.frame_stack.get();
        let action = frame.0.interpreter.run_plain(
            self.instruction.instruction_table(),
            self.instruction.gas_table(),
            &mut self.ctx,
        );
        frame.0.process_next_action(&mut self.ctx, action).inspect(|result| {
            if result.is_result() {
                frame.0.set_finished(true);
            }
        })
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextError<DB::Error>> {
        if self.frame_stack.get().0.is_finished() {
            self.frame_stack.pop();
        }
        if self.frame_stack.index().is_none() {
            return Ok(Some(result))
        }
        self.frame_stack
            .get()
            .0
            .return_result::<_, ContextError<DB::Error>>(&mut self.ctx, result)?;
        Ok(None)
    }
}

impl<DB, INSP, I, P> InspectorEvmTr for TelosEvmInner<DB, INSP, I, P>
where
    DB: Database,
    I: InstructionProvider<Context = TelosEvmContext<DB>, InterpreterTypes = EthInterpreter>,
    P: PrecompileProvider<TelosEvmContext<DB>, Output = InterpreterResult>,
    INSP: Inspector<TelosEvmContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        (&self.ctx, &self.instruction, &self.precompiles, &self.frame_stack, &self.inspector)
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        (
            &mut self.ctx,
            &mut self.instruction,
            &mut self.precompiles,
            &mut self.frame_stack,
            &mut self.inspector,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{TelosExecutionContext, TelosTxEnv};
    use alloy_primitives::{address, B256, U256};
    use revm::{
        context::{BlockEnv, CfgEnv, Context, TxEnv},
        database::InMemoryDB,
        primitives::hardfork::SpecId,
        state::AccountInfo,
    };

    fn context_with_accounts(
        tx: TelosTxEnv,
        accounts: &[(Address, U256)],
    ) -> TelosEvmContext<InMemoryDB> {
        let mut db = InMemoryDB::default();
        for (address, balance) in accounts {
            db.insert_account_info(
                *address,
                AccountInfo { balance: *balance, ..Default::default() },
            );
        }
        Context::<BlockEnv, TelosTxEnv, CfgEnv, _>::new(db, SpecId::BERLIN).with_tx(tx)
    }

    fn native_tx(caller: Address, chain_id: u64, revision: u64) -> TelosTxEnv {
        TelosTxEnv::new(TxEnv { caller, chain_id: Some(chain_id), ..Default::default() })
            .with_telos_context(TelosExecutionContext {
                fixed_gas_price: 1,
                revision,
                first_new_address: None,
            })
    }

    fn call_inputs(
        caller: Address,
        target: Address,
        value: CallValue,
        scheme: revm::interpreter::CallScheme,
    ) -> CallInputs {
        CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: 100_000,
            reservoir: 0,
            bytecode_address: target,
            known_bytecode: (B256::ZERO, Bytecode::default()),
            target_address: target,
            caller,
            value,
            scheme,
            is_static: false,
            charged_new_account_state_gas: false,
        }
    }

    #[test]
    fn chain_three_withdrawal_to_zero_burns_without_crediting_zero() {
        let caller = Address::repeat_byte(0x11);
        let mut context = context_with_accounts(
            native_tx(caller, 3, 1),
            &[(caller, U256::from(100)), (Address::ZERO, U256::from(7))],
        );
        context.journal_mut().load_account(caller).unwrap();
        context.journal_mut().load_account(Address::ZERO).unwrap();

        let inputs = call_inputs(
            caller,
            Address::ZERO,
            CallValue::Transfer(U256::from(25)),
            revm::interpreter::CallScheme::Call,
        );
        assert_eq!(apply_call_value(&mut context, &inputs).unwrap(), None);

        let state = context.journaled_state.finalize();
        assert_eq!(state[&caller].info.balance, U256::from(75));
        assert_eq!(state[&Address::ZERO].info.balance, U256::from(7));
    }

    #[test]
    fn unauthenticated_transfer_to_zero_uses_ethereum_credit() {
        let caller = Address::repeat_byte(0x11);
        let tx = TelosTxEnv::new(TxEnv { caller, chain_id: Some(40), ..Default::default() });
        let mut context =
            context_with_accounts(tx, &[(caller, U256::from(100)), (Address::ZERO, U256::from(7))]);
        context.journal_mut().load_account(caller).unwrap();
        context.journal_mut().load_account(Address::ZERO).unwrap();

        let inputs = call_inputs(
            caller,
            Address::ZERO,
            CallValue::Transfer(U256::from(25)),
            revm::interpreter::CallScheme::Call,
        );
        assert_eq!(apply_call_value(&mut context, &inputs).unwrap(), None);

        let state = context.journaled_state.finalize();
        assert_eq!(state[&caller].info.balance, U256::from(75));
        assert_eq!(state[&Address::ZERO].info.balance, U256::from(32));
    }

    #[test]
    fn chain_three_zero_caller_to_zero_self_transfer_is_balance_neutral() {
        let mut context = context_with_accounts(
            native_tx(Address::ZERO, 3, 1),
            &[(Address::ZERO, U256::from(100))],
        );
        context.journal_mut().load_account(Address::ZERO).unwrap();
        let inputs = call_inputs(
            Address::ZERO,
            Address::ZERO,
            CallValue::Transfer(U256::from(25)),
            revm::interpreter::CallScheme::Call,
        );

        assert_eq!(apply_call_value(&mut context, &inputs).unwrap(), None);
        assert!(stops_after_native_zero_caller_transfer(&context));
        let state = context.journaled_state.finalize();
        assert_eq!(state[&Address::ZERO].info.balance, U256::from(100));
        assert!(state[&Address::ZERO].is_touched());
    }

    #[test]
    fn chain_three_withdrawal_preserves_destination_overflow_error() {
        let caller = Address::repeat_byte(0x11);
        let mut context = context_with_accounts(
            native_tx(caller, 3, 1),
            &[(caller, U256::from(100)), (Address::ZERO, U256::MAX)],
        );
        context.journal_mut().load_account(caller).unwrap();
        context.journal_mut().load_account(Address::ZERO).unwrap();
        let inputs = call_inputs(
            caller,
            Address::ZERO,
            CallValue::Transfer(U256::from(25)),
            revm::interpreter::CallScheme::Call,
        );

        assert_eq!(
            apply_call_value(&mut context, &inputs).unwrap(),
            Some(InstructionResult::OverflowPayment)
        );
    }

    #[test]
    fn revision_zero_delegatecall_checks_balance_and_touches_account() {
        let target = Address::repeat_byte(0x22);
        let mut context = context_with_accounts(
            native_tx(Address::repeat_byte(0x11), 40, 0),
            &[(target, U256::from(5))],
        );
        context.journal_mut().load_account(target).unwrap();
        let inputs = call_inputs(
            target,
            target,
            CallValue::Apparent(U256::from(6)),
            revm::interpreter::CallScheme::DelegateCall,
        );

        assert_eq!(
            apply_call_value(&mut context, &inputs).unwrap(),
            Some(InstructionResult::OutOfFunds)
        );
    }

    #[test]
    fn revision_zero_delegatecall_preserves_balance_and_touches_account() {
        let target = Address::repeat_byte(0x22);
        let mut context = context_with_accounts(
            native_tx(Address::repeat_byte(0x11), 40, 0),
            &[(target, U256::from(5))],
        );
        context.journal_mut().load_account(target).unwrap();
        let inputs = call_inputs(
            target,
            target,
            CallValue::Apparent(U256::from(5)),
            revm::interpreter::CallScheme::DelegateCall,
        );

        assert_eq!(apply_call_value(&mut context, &inputs).unwrap(), None);
        let state = context.journaled_state.finalize();
        assert_eq!(state[&target].info.balance, U256::from(5));
    }

    #[test]
    fn only_authenticated_chain_three_zero_caller_stops_early() {
        let native = context_with_accounts(native_tx(Address::ZERO, 3, 1), &[]);
        assert!(stops_after_native_zero_caller_transfer(&native));

        let tx = TelosTxEnv::new(TxEnv {
            caller: Address::ZERO,
            chain_id: Some(3),
            ..Default::default()
        });
        let unauthenticated = context_with_accounts(tx, &[]);
        assert!(!stops_after_native_zero_caller_transfer(&unauthenticated));
    }

    #[test]
    fn authenticated_zero_create_caller_keeps_zero_nonce() {
        let mut context = context_with_accounts(
            native_tx(Address::ZERO, 3, 1),
            &[(Address::ZERO, U256::from(100))],
        );
        let mut account = context.journal_mut().load_account_mut(Address::ZERO).unwrap().data;

        assert_eq!(creation_nonce(&mut account, Address::ZERO, true), Some(0));
        assert_eq!(account.nonce(), 0);
    }

    #[test]
    fn authenticated_zero_create_caller_uses_nonce_zero_golden_address() {
        let mut context = context_with_accounts(
            native_tx(Address::ZERO, 3, 1),
            &[(Address::ZERO, U256::from(100))],
        );
        let mut account = context.journal_mut().load_account_mut(Address::ZERO).unwrap().data;
        let nonce = creation_nonce(&mut account, Address::ZERO, true).unwrap();
        let inputs = CreateInputs::new(
            Address::ZERO,
            revm::interpreter::CreateScheme::Create,
            U256::ZERO,
            Bytes::new(),
            100_000,
            0,
        );

        assert_eq!(
            inputs.created_address(nonce),
            address!("bd770416a3345f91e4b34576cb804a576fa48eb1")
        );
        assert_eq!(account.nonce(), 0);
    }

    #[test]
    fn unauthenticated_zero_create_caller_uses_ethereum_nonce() {
        let tx = TelosTxEnv::new(TxEnv { caller: Address::ZERO, ..Default::default() });
        let mut context = context_with_accounts(tx, &[(Address::ZERO, U256::from(100))]);
        let mut account = context.journal_mut().load_account_mut(Address::ZERO).unwrap().data;

        assert_eq!(creation_nonce(&mut account, Address::ZERO, false), Some(0));
        assert_eq!(account.nonce(), 1);
    }
}

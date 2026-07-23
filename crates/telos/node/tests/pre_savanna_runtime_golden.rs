//! Executes an authenticated pre-Savanna mainnet block through the production Telos EVM path.

use alloy_consensus::{proofs::calculate_transaction_root, BlockBody, Header, TxType};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{hex, keccak256, Address, Bloom, Bytes, B256, U256};
use reth_ethereum_primitives::{calculate_receipt_root_no_memo, Block, TransactionSigned};
use reth_evm::{block::BlockExecutor, ConfigureEvm, ExecutionReconciliation};
use reth_node_telos::{
    chainspec::TELOS_MAINNET,
    evm::TelosEvmConfig,
    execution::TelosTxEnv,
    sidecar::{
        InMemoryTelosSidecarStore, TelosChainIdentity, TelosExecutionAnchor, TelosExecutionSidecar,
        TelosSidecarStore, TELOS_EXECUTION_ANCHOR_VERSION,
    },
};
use reth_primitives_traits::{Block as _, RecoveredBlock, SignerRecoverable};
use reth_telos_rpc_engine_api::structs::{
    TelosAccountTableRow, TelosEngineApiExtraFields, TelosExecutionChange,
    TelosExecutionMetadataV3, TelosExtraFieldReceipt, TelosReceiptType,
    TELOS_EXECUTION_METADATA_VERSION,
};
use revm::{
    database::{CacheDB, EmptyDB, State},
    state::AccountInfo,
    Database,
};
use serde_json::Value;
use std::sync::Arc;

const FIXTURE: &str =
    include_str!("../testdata/execution-context/pre-savanna-runtime-mainnet-423015053.v1.json");

#[test]
fn authenticated_pre_savanna_block_executes_with_exact_receipts_and_post_state() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("valid runtime golden JSON");
    assert_eq!(
        string_at(&fixture, "/schema"),
        "telos-reth/pre-savanna-runtime-execution-golden/v1"
    );

    let native_block = u64_at(&fixture, "/evidence/block/native_block_number");
    let evm_block = u64_at(&fixture, "/evidence/block/evm_block_number");
    let delta = u64_at(&fixture, "/network/native_to_evm_block_delta");
    let savanna_activation =
        u64_at(&fixture, "/pre_savanna_binding/savanna_activation_block_number");
    assert_eq!(native_block.checked_sub(delta), Some(evm_block));
    assert!(native_block < savanna_activation);
    assert_eq!(
        savanna_activation - native_block,
        u64_at(&fixture, "/pre_savanna_binding/blocks_before_activation")
    );
    assert_eq!(string_at(&fixture, "/pre_savanna_binding/feature_codename"), "SAVANNA");

    let transactions = array_at(&fixture, "/evidence/transactions")
        .iter()
        .map(|item| {
            let raw = decode_hex(string_at(item, "/raw"));
            let expected_hash = b256_at(item, "/hash");
            assert_eq!(keccak256(&raw), expected_hash);
            let transaction =
                TransactionSigned::decode_2718_exact(&raw).expect("valid exact transaction RLP");
            assert_eq!(*transaction.tx_hash(), expected_hash);
            assert_eq!(
                transaction.recover_signer().expect("recover historical signer"),
                address_at(item, "/from")
            );
            transaction
        })
        .collect::<Vec<_>>();

    let block_evidence = value_at(&fixture, "/evidence/block");
    assert_eq!(string_at(block_evidence, "/logs_bloom"), "zero");
    assert!(value_at(block_evidence, "/base_fee_per_gas").is_null());
    let block = Block {
        header: Header {
            parent_hash: b256_at(block_evidence, "/parent_hash"),
            ommers_hash: b256_at(block_evidence, "/ommers_hash"),
            beneficiary: address_at(block_evidence, "/beneficiary"),
            state_root: b256_at(block_evidence, "/state_root"),
            transactions_root: b256_at(block_evidence, "/transactions_root"),
            receipts_root: b256_at(block_evidence, "/receipts_root"),
            logs_bloom: Bloom::ZERO,
            difficulty: u256_at(block_evidence, "/difficulty"),
            number: evm_block,
            gas_limit: quantity_u64_at(block_evidence, "/gas_limit"),
            gas_used: quantity_u64_at(block_evidence, "/gas_used"),
            timestamp: quantity_u64_at(block_evidence, "/timestamp"),
            extra_data: Bytes::from(decode_hex(string_at(block_evidence, "/extra_data"))),
            mix_hash: b256_at(block_evidence, "/mix_hash"),
            ..Default::default()
        },
        body: BlockBody { transactions, ..Default::default() },
    }
    .seal_slow();
    let block_hash = b256_at(block_evidence, "/hash");
    assert_eq!(block.hash(), block_hash);
    assert_eq!(
        calculate_transaction_root(&block.body().transactions),
        block.header().transactions_root
    );
    assert_eq!(
        block.header().extra_data.as_ref(),
        b256_at(block_evidence, "/native_block_id").as_slice()
    );

    let context = value_at(&fixture, "/evidence/execution_context");
    let starting_gas_price = u256_at(context, "/starting_fixed_gas_price");
    let starting_revision = u64_at(context, "/starting_revision");
    let parent_hash = block.header().parent_hash;
    let chain = TelosChainIdentity { chain_id: 40, genesis_hash: TELOS_MAINNET.genesis_hash() };
    let anchor = TelosExecutionAnchor {
        version: TELOS_EXECUTION_ANCHOR_VERSION,
        chain,
        parent_block_number: evm_block - 1,
        parent_block_hash: parent_hash,
        starting_gas_price,
        starting_revision,
    };

    let expected_receipts = array_at(&fixture, "/evidence/receipts")
        .iter()
        .map(|item| TelosExtraFieldReceipt {
            tx_type: TelosReceiptType::Number(0),
            success: bool_at(item, "/success"),
            cumulative_gas_used: u64_at(item, "/cumulative_gas_used"),
            logs: Vec::new(),
        })
        .collect::<Vec<_>>();
    let post_state = account_rows(&fixture, "/evidence/post_state");
    let gas_price_changes = array_at(context, "/gas_price_changes")
        .iter()
        .map(|change| TelosExecutionChange {
            boundary: u64_at(change, "/boundary"),
            value: u256_at(change, "/value"),
        })
        .collect::<Vec<_>>();
    let fields = TelosEngineApiExtraFields {
        statediffs_account: Some(post_state.clone()),
        statediffs_accountstate: Some(Vec::new()),
        execution: Some(TelosExecutionMetadataV3 {
            version: TELOS_EXECUTION_METADATA_VERSION,
            block_hash,
            parent_hash,
            transaction_count: block.body().transactions.len() as u64,
            execution_base_fee: u256_at(context, "/execution_base_fee"),
            starting_gas_price,
            starting_revision,
            gas_price_changes,
            revision_changes: Vec::new(),
        }),
        new_addresses_using_create: Some(Vec::new()),
        new_addresses_using_openwallet: Some(Vec::new()),
        receipts: Some(expected_receipts),
        ..Default::default()
    };
    let sidecar = TelosExecutionSidecar::new(
        chain,
        evm_block,
        block_hash,
        parent_hash,
        block.body().transactions.len() as u64,
        block.header().gas_used,
        fields,
    )
    .expect("valid authenticated historical sidecar");
    let store = Arc::new(InMemoryTelosSidecarStore::new(chain));
    store.put_pending(&sidecar).unwrap();
    store.mark_dispatched(block_hash, sidecar.digest()).unwrap();
    store.mark_accepted(block_hash, sidecar.digest()).unwrap();

    let config = TelosEvmConfig::new(TELOS_MAINNET.clone(), store, anchor);
    for boundary in 0..2 {
        let mut tx_env = TelosTxEnv::default();
        config.apply_rpc_transaction_context(&block, boundary, false, &mut tx_env).unwrap();
        assert_eq!(tx_env.fixed_gas_price(), u128::try_from(starting_gas_price).ok());
        assert_eq!(tx_env.revision(), Some(starting_revision));
    }
    let mut child_tx_env = TelosTxEnv::default();
    config.apply_rpc_transaction_context(&block, 2, false, &mut child_tx_env).unwrap();
    assert_eq!(
        child_tx_env.fixed_gas_price(),
        u128::try_from(u256_at(context, "/child_fixed_gas_price")).ok()
    );
    assert_eq!(child_tx_env.revision(), Some(u64_at(context, "/child_revision")));

    let mut database = CacheDB::<EmptyDB>::default();
    for row in account_rows(&fixture, "/evidence/pre_state") {
        database.insert_account_info(
            row.address,
            AccountInfo { balance: row.balance, nonce: row.nonce, ..Default::default() },
        );
    }
    let mut state = State::builder().with_database(database).with_bundle_update().build();
    let recovered = RecoveredBlock::new_sealed(block, vec![Address::ZERO, Address::ZERO]);

    let mut executor = config.executor_for_block(&mut state, recovered.sealed_block()).unwrap();
    executor.apply_pre_execution_changes().unwrap();
    for transaction in recovered.transactions_recovered() {
        executor.execute_transaction(transaction).unwrap();
    }
    let (evm, mut result) = executor.finish().unwrap();
    drop(evm);

    assert_eq!(result.gas_used, quantity_u64_at(block_evidence, "/gas_used"));
    assert_eq!(result.receipts.len(), 2);
    for (actual, expected) in result.receipts.iter().zip(array_at(&fixture, "/evidence/receipts")) {
        assert_eq!(actual.tx_type, TxType::Legacy);
        assert_eq!(actual.success, bool_at(expected, "/success"));
        assert_eq!(actual.cumulative_gas_used, u64_at(expected, "/cumulative_gas_used"));
        assert!(actual.logs.is_empty());
    }
    assert_eq!(
        calculate_receipt_root_no_memo(&result.receipts),
        recovered.sealed_block().header().receipts_root
    );

    let reconciliation = config
        .reconcile_block_execution(recovered.sealed_block(), &mut state, &mut result)
        .unwrap();
    assert_eq!(reconciliation, ExecutionReconciliation::Unchanged);
    for expected in post_state {
        let actual = state.basic(expected.address).unwrap().expect("expected account");
        assert_eq!(actual.balance, expected.balance, "{} balance", expected.address);
        assert_eq!(actual.nonce, expected.nonce, "{} nonce", expected.address);
        assert_eq!(actual.code_hash, keccak256([]), "{} code hash", expected.address);
    }
    assert!(state.basic(Address::ZERO).unwrap().is_none());
}

fn account_rows(fixture: &Value, pointer: &str) -> Vec<TelosAccountTableRow> {
    array_at(fixture, pointer)
        .iter()
        .map(|row| {
            assert_eq!(string_at(row, "/code"), "0x");
            TelosAccountTableRow {
                removed: false,
                address: address_at(row, "/address"),
                account: String::new(),
                nonce: u64_at(row, "/nonce"),
                code: Bytes::new(),
                balance: u256_at(row, "/balance"),
            }
        })
        .collect()
}

fn value_at<'a>(value: &'a Value, pointer: &str) -> &'a Value {
    value.pointer(pointer).unwrap_or_else(|| panic!("missing JSON pointer {pointer}"))
}

fn array_at<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value_at(value, pointer)
        .as_array()
        .unwrap_or_else(|| panic!("JSON pointer {pointer} is not an array"))
}

fn string_at<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value_at(value, pointer)
        .as_str()
        .unwrap_or_else(|| panic!("JSON pointer {pointer} is not a string"))
}

fn bool_at(value: &Value, pointer: &str) -> bool {
    value_at(value, pointer)
        .as_bool()
        .unwrap_or_else(|| panic!("JSON pointer {pointer} is not a bool"))
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value_at(value, pointer)
        .as_u64()
        .unwrap_or_else(|| panic!("JSON pointer {pointer} is not a u64"))
}

fn quantity_u64_at(value: &Value, pointer: &str) -> u64 {
    u64::from_str_radix(
        string_at(value, pointer).strip_prefix("0x").unwrap_or(string_at(value, pointer)),
        16,
    )
    .unwrap_or_else(|error| panic!("invalid u64 quantity at {pointer}: {error}"))
}

fn u256_at(value: &Value, pointer: &str) -> U256 {
    let quantity = string_at(value, pointer);
    U256::from_str_radix(quantity.strip_prefix("0x").unwrap_or(quantity), 16)
        .unwrap_or_else(|error| panic!("invalid U256 quantity at {pointer}: {error}"))
}

fn b256_at(value: &Value, pointer: &str) -> B256 {
    string_at(value, pointer)
        .parse()
        .unwrap_or_else(|error| panic!("invalid B256 at {pointer}: {error}"))
}

fn address_at(value: &Value, pointer: &str) -> Address {
    string_at(value, pointer)
        .parse()
        .unwrap_or_else(|error| panic!("invalid address at {pointer}: {error}"))
}

fn decode_hex(value: &str) -> Vec<u8> {
    hex::decode(value.strip_prefix("0x").unwrap_or(value))
        .unwrap_or_else(|error| panic!("invalid hex value: {error}"))
}

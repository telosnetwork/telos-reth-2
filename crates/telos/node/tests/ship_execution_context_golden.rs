//! Locks zero-based schedule behavior to an operator-authenticated SHIP archive capture.

use alloy_primitives::{hex, keccak256, B256};
use reth_node_telos::execution::{
    TelosBlockExecutionSchedule, TelosExecutionChange, TelosExecutionContext,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("../testdata/execution-context/ship-mainnet-423015053.v1.json");

#[test]
fn ship_boundary_at_transaction_count_is_child_only() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("valid SHIP fixture JSON");
    let expected = &fixture["expected_translation"];
    let transaction_count = expected["transaction_count"].as_u64().unwrap() as usize;
    let starting_gas_price =
        parse_quantity(expected["starting_context"]["fixed_gas_price"].as_str().unwrap());
    let starting_revision = expected["starting_context"]["revision"].as_u64().unwrap();
    let change = &expected["execution_changes"][0];
    let boundary = change["boundary"].as_u64().unwrap() as usize;
    let changed_gas_price = parse_quantity(change["value"].as_str().unwrap());

    assert_eq!(transaction_count, 2);
    assert_eq!(boundary, transaction_count);
    for item in expected["trace_order"].as_array().unwrap().iter().take(transaction_count) {
        let raw =
            hex::decode(item["raw_transaction"].as_str().unwrap().strip_prefix("0x").unwrap())
                .unwrap();
        let expected_hash: B256 = item["evm_transaction_hash"].as_str().unwrap().parse().unwrap();
        assert_eq!(keccak256(raw), expected_hash);
    }
    assert_eq!(
        fixture["capture"]["native_block_number"].as_u64().unwrap() -
            fixture["network"]["native_to_evm_block_delta"].as_u64().unwrap(),
        expected["evm_block_number"].as_u64().unwrap(),
    );

    let schedule = TelosBlockExecutionSchedule::new(
        transaction_count,
        TelosExecutionContext {
            fixed_gas_price: starting_gas_price,
            revision: starting_revision,
            first_new_address: None,
        },
        vec![TelosExecutionChange { boundary, value: changed_gas_price }],
        Vec::new(),
    )
    .unwrap();

    assert_eq!(schedule.context_for_transaction(0).unwrap().fixed_gas_price, starting_gas_price);
    assert_eq!(
        schedule.context_for_transaction(1).unwrap().fixed_gas_price,
        starting_gas_price,
        "a pre-incremented lookup would incorrectly apply the child context here"
    );
    assert_eq!(schedule.child_context().fixed_gas_price, changed_gas_price);
    assert_eq!(schedule.child_context().revision, starting_revision);
}

fn parse_quantity(value: &str) -> u128 {
    u128::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16).unwrap()
}

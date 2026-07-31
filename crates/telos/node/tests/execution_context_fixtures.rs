//! Validates the provenance, integrity, and semantic correlations of Telos execution fixtures.

use std::collections::BTreeSet;

use alloy_primitives::{hex, keccak256};
use serde_json::{Map, Value};

const FIXTURE_DIRECTORY: &str = "../testdata/execution-context";
const BOUNDARIES_PATH: &str = "boundaries.v1.json";
const PRE_SAVANNA_RUNTIME_PATH: &str = "pre-savanna-runtime-mainnet-423015053.v1.json";
const RAW_TRANSACTION_PATH: &str = "raw-transaction-mainnet-400000006.v1.json";
const PROVENANCE_PATH: &str = "PROVENANCE.v1.json";
const README_PATH: &str = "README.md";
const SHIP_PATH: &str = "ship-mainnet-423015053.v1.json";
const SHIP_VERIFIER_PATH: &str = "verify_ship_capture.py";

const EXPECTED_HASHED_FILES: [&str; 7] = [
    PROVENANCE_PATH,
    README_PATH,
    BOUNDARIES_PATH,
    PRE_SAVANNA_RUNTIME_PATH,
    RAW_TRANSACTION_PATH,
    SHIP_PATH,
    SHIP_VERIFIER_PATH,
];
const EXPECTED_CANONICAL_FRAGMENTS: [(&str, &str, &str); 14] = [
    (BOUNDARIES_PATH, "/cases/0/evidence", "mainnet-init-180904219"),
    (BOUNDARIES_PATH, "/cases/1/evidence", "mainnet-setrevision-332317532"),
    (BOUNDARIES_PATH, "/cases/2/evidence", "testnet-init-136350055"),
    (BOUNDARIES_PATH, "/cases/3/evidence", "testnet-setrevision-275001548"),
    (BOUNDARIES_PATH, "/cases/4/evidence", "testnet-setrevision-278492369"),
    (PRE_SAVANNA_RUNTIME_PATH, "/pre_savanna_binding", "pre-savanna-runtime-binding"),
    (PRE_SAVANNA_RUNTIME_PATH, "/provenance", "pre-savanna-runtime-provenance"),
    (PRE_SAVANNA_RUNTIME_PATH, "/evidence", "pre-savanna-runtime-evidence"),
    (RAW_TRANSACTION_PATH, "/evidence/native_action", "raw-native-action"),
    (RAW_TRANSACTION_PATH, "/evidence/native_deltas", "raw-native-deltas"),
    (RAW_TRANSACTION_PATH, "/evidence/account_index_resolution", "raw-account-index-resolution"),
    (RAW_TRANSACTION_PATH, "/evidence/evm_block", "raw-evm-block"),
    (RAW_TRANSACTION_PATH, "/evidence/evm_receipt", "raw-evm-receipt"),
    (RAW_TRANSACTION_PATH, "/evidence/debug_raw_transaction", "raw-debug-transaction"),
];

const BOUNDARIES_BYTES: &[u8] = include_bytes!("../testdata/execution-context/boundaries.v1.json");
const PRE_SAVANNA_RUNTIME_BYTES: &[u8] =
    include_bytes!("../testdata/execution-context/pre-savanna-runtime-mainnet-423015053.v1.json");
const RAW_TRANSACTION_BYTES: &[u8] =
    include_bytes!("../testdata/execution-context/raw-transaction-mainnet-400000006.v1.json");
const PROVENANCE_BYTES: &[u8] = include_bytes!("../testdata/execution-context/PROVENANCE.v1.json");
const SOURCE_HASHES_BYTES: &[u8] =
    include_bytes!("../testdata/execution-context/SOURCE_HASHES.json");
const README_BYTES: &[u8] = include_bytes!("../testdata/execution-context/README.md");
const SHIP_BYTES: &[u8] =
    include_bytes!("../testdata/execution-context/ship-mainnet-423015053.v1.json");
const SHIP_VERIFIER_BYTES: &[u8] =
    include_bytes!("../testdata/execution-context/verify_ship_capture.py");

#[test]
fn execution_context_fixture_corpus_is_self_consistent() {
    let boundaries = parse_json(BOUNDARIES_PATH, BOUNDARIES_BYTES);
    let pre_savanna_runtime = parse_json(PRE_SAVANNA_RUNTIME_PATH, PRE_SAVANNA_RUNTIME_BYTES);
    let raw_transaction = parse_json(RAW_TRANSACTION_PATH, RAW_TRANSACTION_BYTES);
    let provenance = parse_json(PROVENANCE_PATH, PROVENANCE_BYTES);
    let ship = parse_json(SHIP_PATH, SHIP_BYTES);
    let source_hashes = parse_json("SOURCE_HASHES.json", SOURCE_HASHES_BYTES);

    validate_boundary_fixtures(&boundaries);
    validate_pre_savanna_runtime_fixture(&pre_savanna_runtime, &ship);
    validate_raw_transaction_fixture(&raw_transaction);
    validate_provenance(&provenance);
    validate_source_hashes(
        &source_hashes,
        &boundaries,
        &pre_savanna_runtime,
        &raw_transaction,
        &provenance,
    );
}

fn validate_boundary_fixtures(fixture: &Value) {
    let networks = object_at(fixture, "/networks");
    let cases = array_at(fixture, "/cases");
    assert!(!cases.is_empty(), "boundary fixture must contain at least one case");

    for case in cases {
        let id = string_at(case, "/id");
        let network = string_at(case, "/network");
        let network_config = networks
            .get(network)
            .unwrap_or_else(|| panic!("{id}: missing network configuration for {network}"));
        let delta = u64_at(network_config, "/native_to_evm_block_delta");
        let native_block = u64_at(case, "/expected/native_block_number");
        let evm_block = u64_at(case, "/expected/evm_block_number");
        let boundary = u64_at(case, "/expected/schedule_change/boundary");

        assert_eq!(
            native_block.checked_sub(delta),
            Some(evm_block),
            "{id}: native-to-EVM block mapping"
        );
        assert_eq!(
            parse_quantity(string_at(case, "/evidence/evm_block/number")),
            evm_block as u128,
            "{id}: JSON-RPC block number"
        );
        assert_eq!(
            string_at(case, "/evidence/native_action/block_id"),
            string_at(case, "/evidence/native_config_delta/block_id"),
            "{id}: native action and config delta must bind to the same native block"
        );
        assert_eq!(
            string_at(case, "/evidence/evm_block/extraData"),
            format!("0x{}", string_at(case, "/evidence/native_action/block_id")),
            "{id}: EVM extraData must bind the native block ID"
        );
        assert_eq!(
            u64_at(case, "/evidence/native_action/block_num"),
            native_block,
            "{id}: native action height"
        );
        assert_eq!(
            u64_at(case, "/evidence/native_config_delta/block_num"),
            native_block,
            "{id}: native config-delta height"
        );

        let transactions = array_at(case, "/evidence/evm_block/transactions");
        assert_eq!(boundary, 0, "{id}: corpus boundary cases are boundary-zero evidence");
        assert!(
            transactions.is_empty(),
            "{id}: a boundary-zero empty block must become child-start context"
        );

        match string_at(case, "/expected/schedule_change/kind") {
            "fixed_gas_price" => {
                assert_eq!(
                    string_at(case, "/evidence/native_action/act/name"),
                    "init",
                    "{id}: fixed gas price fixture action"
                );
                assert_eq!(
                    normalize_hex(string_at(case, "/evidence/native_config_delta/data/gas_price")),
                    normalize_hex(string_at(case, "/expected/schedule_change/value")),
                    "{id}: initialized fixed gas price"
                );
            }
            "revision" => {
                assert_eq!(
                    string_at(case, "/evidence/native_action/act/name"),
                    "setrevision",
                    "{id}: revision fixture action"
                );
                assert_eq!(
                    u64_at(case, "/evidence/native_action/act/data/new_revision"),
                    u64_at(case, "/expected/schedule_change/value"),
                    "{id}: revision action value"
                );
                assert_eq!(
                    u64_at(case, "/evidence/native_config_delta/data/revision"),
                    u64_at(case, "/expected/schedule_change/value"),
                    "{id}: config-delta revision"
                );
            }
            kind => panic!("{id}: unsupported schedule change kind {kind}"),
        }
    }
}

fn validate_pre_savanna_runtime_fixture(fixture: &Value, ship: &Value) {
    assert_eq!(string_at(fixture, "/schema"), "telos-reth/pre-savanna-runtime-execution-golden/v1");

    let native_block = u64_at(fixture, "/evidence/block/native_block_number");
    let evm_block = u64_at(fixture, "/evidence/block/evm_block_number");
    let delta = u64_at(fixture, "/network/native_to_evm_block_delta");
    let savanna_activation =
        u64_at(fixture, "/pre_savanna_binding/savanna_activation_block_number");
    assert_eq!(
        native_block.checked_sub(delta),
        Some(evm_block),
        "runtime fixture native-to-EVM block mapping"
    );
    assert_eq!(
        u64_at(fixture, "/pre_savanna_binding/fixture_native_block_number"),
        native_block,
        "runtime fixture pre-Savanna binding"
    );
    assert_eq!(
        savanna_activation.checked_sub(native_block),
        Some(u64_at(fixture, "/pre_savanna_binding/blocks_before_activation")),
        "runtime fixture distance before SAVANNA"
    );

    assert_eq!(
        u64_at(ship, "/capture/native_block_number"),
        native_block,
        "runtime fixture/SHIP native block"
    );
    assert_eq!(
        u64_at(ship, "/expected_translation/evm_block_number"),
        evm_block,
        "runtime fixture/SHIP EVM block"
    );
    assert_eq!(
        u64_at(ship, "/network/native_to_evm_block_delta"),
        delta,
        "runtime fixture/SHIP block delta"
    );
    assert_hex_eq(
        string_at(fixture, "/evidence/block/native_block_id"),
        string_at(ship, "/capture/native_block_id"),
        "runtime fixture/SHIP native block ID",
    );
    assert_hex_eq(
        string_at(fixture, "/evidence/block/extra_data"),
        string_at(fixture, "/evidence/block/native_block_id"),
        "runtime fixture EVM extraData/native block binding",
    );
    assert_hex_eq(
        string_at(fixture, "/provenance/authenticated_ship_corpus/native_block_id"),
        string_at(ship, "/capture/native_block_id"),
        "runtime fixture provenance/SHIP native block binding",
    );

    let transactions = array_at(fixture, "/evidence/transactions");
    let receipts = array_at(fixture, "/evidence/receipts");
    let trace_order = array_at(ship, "/expected_translation/trace_order");
    let transaction_count = u64_at(ship, "/expected_translation/transaction_count");
    assert_eq!(transactions.len(), transaction_count as usize);
    assert_eq!(receipts.len(), transactions.len());
    assert!(trace_order.len() >= transactions.len());

    let mut cumulative_gas = 0u64;
    for (index, ((transaction, receipt), trace)) in
        transactions.iter().zip(receipts).zip(trace_order).enumerate()
    {
        assert_eq!(u64_at(transaction, "/index"), index as u64);
        let raw = decode_hex(string_at(transaction, "/raw"));
        let hash = format!("0x{}", hex::encode(keccak256(&raw)));
        assert_hex_eq(&hash, string_at(transaction, "/hash"), "runtime transaction Keccak");
        assert_hex_eq(
            string_at(transaction, "/hash"),
            string_at(trace, "/evm_transaction_hash"),
            "runtime fixture/SHIP transaction hash",
        );
        assert_eq!(
            raw,
            decode_hex(string_at(trace, "/raw_transaction")),
            "runtime fixture/SHIP raw transaction bytes"
        );
        assert_hex_eq(
            string_at(transaction, "/signed_gas_price"),
            string_at(fixture, "/evidence/execution_context/starting_fixed_gas_price"),
            "runtime transaction starting gas price",
        );
        assert_hex_eq(
            string_at(receipt, "/transaction_hash"),
            string_at(transaction, "/hash"),
            "runtime receipt transaction hash",
        );
        assert_eq!(
            bool_at(receipt, "/success"),
            parse_quantity(string_at(trace, "/receipt_status")) == 1,
            "runtime fixture/SHIP receipt status"
        );
        let gas_used = u64_at(receipt, "/gas_used");
        assert_eq!(
            gas_used as u128,
            parse_quantity(string_at(trace, "/receipt_gas_used")),
            "runtime fixture/SHIP receipt gas"
        );
        cumulative_gas = cumulative_gas.checked_add(gas_used).expect("cumulative gas overflow");
        assert_eq!(
            u64_at(receipt, "/cumulative_gas_used"),
            cumulative_gas,
            "runtime receipt cumulative gas"
        );
    }
    assert_eq!(
        cumulative_gas as u128,
        parse_quantity(string_at(fixture, "/evidence/block/gas_used")),
        "runtime block/receipt gas"
    );

    assert_hex_eq(
        string_at(fixture, "/evidence/execution_context/starting_fixed_gas_price"),
        string_at(ship, "/expected_translation/starting_context/fixed_gas_price"),
        "runtime fixture/SHIP starting gas price",
    );
    assert_eq!(
        u64_at(fixture, "/evidence/execution_context/starting_revision"),
        u64_at(ship, "/expected_translation/starting_context/revision"),
        "runtime fixture/SHIP starting revision"
    );
    let runtime_changes = array_at(fixture, "/evidence/execution_context/gas_price_changes");
    let ship_changes = array_at(ship, "/expected_translation/execution_changes");
    assert_eq!(runtime_changes.len(), 1);
    assert_eq!(ship_changes.len(), 1);
    assert_eq!(
        u64_at(&runtime_changes[0], "/boundary"),
        u64_at(&ship_changes[0], "/boundary"),
        "runtime fixture/SHIP gas-price boundary"
    );
    assert_hex_eq(
        string_at(&runtime_changes[0], "/value"),
        string_at(&ship_changes[0], "/value"),
        "runtime fixture/SHIP changed gas price",
    );
    assert_hex_eq(
        string_at(fixture, "/evidence/execution_context/child_fixed_gas_price"),
        string_at(ship, "/expected_translation/child_context/fixed_gas_price"),
        "runtime fixture/SHIP child gas price",
    );
    assert_eq!(
        u64_at(fixture, "/evidence/execution_context/child_revision"),
        u64_at(ship, "/expected_translation/child_context/revision"),
        "runtime fixture/SHIP child revision"
    );
}

fn validate_raw_transaction_fixture(fixture: &Value) {
    let native_block = u64_at(fixture, "/expected/native_block_number");
    let evm_block = u64_at(fixture, "/expected/evm_block_number");
    let delta = u64_at(fixture, "/network/native_to_evm_block_delta");
    assert_eq!(
        native_block.checked_sub(delta),
        Some(evm_block),
        "raw transaction native-to-EVM block mapping"
    );
    assert_eq!(
        parse_quantity(string_at(fixture, "/evidence/evm_block/number")),
        evm_block as u128,
        "raw transaction JSON-RPC block number"
    );
    assert_eq!(
        string_at(fixture, "/evidence/evm_block/extraData"),
        format!("0x{}", string_at(fixture, "/evidence/native_action/block_id")),
        "raw transaction EVM extraData/native block binding"
    );
    assert_eq!(
        u64_at(fixture, "/evidence/native_deltas/config/data/last_block"),
        evm_block,
        "raw transaction config.last_block"
    );

    let native_raw = decode_hex(string_at(fixture, "/evidence/native_action/act/data/tx"));
    let debug_raw = decode_hex(string_at(fixture, "/evidence/debug_raw_transaction"));
    assert_eq!(native_raw, debug_raw, "native raw RLP and debug raw transaction bytes");

    let expected_transaction_hash = string_at(fixture, "/expected/evm_transaction_hash");
    let actual_transaction_hash = format!("0x{}", hex::encode(keccak256(&native_raw)));
    assert_eq!(actual_transaction_hash, expected_transaction_hash, "raw transaction Keccak");

    let transaction = &array_at(fixture, "/evidence/evm_block/transactions")[0];
    assert_eq!(
        string_at(transaction, "/hash"),
        expected_transaction_hash,
        "transaction hash in EVM block"
    );
    assert_eq!(
        parse_quantity(string_at(transaction, "/transactionIndex")),
        u64_at(fixture, "/expected/evm_transaction_index") as u128,
        "EVM transaction index"
    );

    let config_gas_used =
        parse_quantity(string_at(fixture, "/evidence/native_deltas/config/data/gas_used_block"));
    let receipt_cumulative_gas =
        parse_quantity(string_at(fixture, "/evidence/evm_receipt/cumulativeGasUsed"));
    assert_eq!(config_gas_used, receipt_cumulative_gas, "config/receipt cumulative gas");
    assert_eq!(
        config_gas_used,
        parse_quantity(string_at(fixture, "/evidence/evm_receipt/gasUsed")),
        "single-transaction receipt gas"
    );
    assert_eq!(
        config_gas_used,
        parse_quantity(string_at(fixture, "/evidence/evm_block/gasUsed")),
        "single-transaction block gas"
    );

    let config_gas_price =
        parse_quantity(string_at(fixture, "/evidence/native_deltas/config/data/gas_price"));
    assert_eq!(
        config_gas_price,
        parse_quantity(string_at(fixture, "/evidence/evm_receipt/effectiveGasPrice")),
        "config/receipt effective gas price"
    );
    assert_eq!(
        config_gas_price,
        parse_quantity(string_at(transaction, "/gasPrice")),
        "config/transaction gas price"
    );

    assert_hex_eq(
        string_at(fixture, "/evidence/native_deltas/account/data/address"),
        string_at(transaction, "/from"),
        "native account delta must identify the EVM sender",
    );
    assert_hex_eq(
        string_at(fixture, "/evidence/account_index_resolution/observed_account_row/address"),
        string_at(transaction, "/to"),
        "accountstate scope must resolve to the EVM recipient",
    );

    let scope = string_at(fixture, "/evidence/native_deltas/accountstate/scope");
    let decoded_scope = antelope_name_to_u64(scope);
    assert_eq!(
        decoded_scope,
        u64_at(fixture, "/evidence/native_deltas/accountstate/scope_as_u64"),
        "accountstate Antelope scope decoding"
    );
    assert_eq!(
        decoded_scope,
        u64_at(fixture, "/evidence/account_index_resolution/decoded_u64"),
        "accountstate scope/account-index resolution"
    );

    assert_eq!(
        string_at(fixture, "/evidence/evm_receipt/transactionHash"),
        expected_transaction_hash,
        "receipt transaction hash"
    );
    assert_eq!(
        string_at(fixture, "/evidence/evm_receipt/blockHash"),
        string_at(fixture, "/evidence/evm_block/hash"),
        "receipt block hash"
    );
    assert_eq!(
        string_at(fixture, "/evidence/evm_receipt/blockNumber"),
        string_at(fixture, "/evidence/evm_block/number"),
        "receipt block number"
    );
}

fn validate_provenance(provenance: &Value) {
    assert_eq!(
        string_at(provenance, "/data_classification"),
        "public-and-operator-authenticated-no-secret"
    );
    let limitations = array_at(provenance, "/limitations");
    for required_id in [
        "authenticated-ship-capture-scope",
        "intra-block-context-boundary-coverage",
        "public-api-console-omission",
    ] {
        assert!(
            limitations.iter().any(|limitation| string_at(limitation, "/id") == required_id),
            "missing machine-readable limitation {required_id}"
        );
    }
}

fn validate_source_hashes(
    manifest: &Value,
    boundaries: &Value,
    pre_savanna_runtime: &Value,
    raw_transaction: &Value,
    provenance: &Value,
) {
    assert_eq!(
        hex::encode(sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "test-local SHA-256 implementation"
    );

    let files = array_at(manifest, "/files");
    let actual_files = files.iter().map(|file| string_at(file, "/path")).collect::<BTreeSet<_>>();
    assert_eq!(actual_files.len(), files.len(), "hash manifest contains duplicate file paths");
    assert_eq!(
        actual_files,
        EXPECTED_HASHED_FILES.into_iter().collect::<BTreeSet<_>>(),
        "hash manifest file inventory"
    );

    for file in files {
        let path = string_at(file, "/path");
        let bytes = fixture_file(path)
            .unwrap_or_else(|| panic!("hash manifest references unsupported file {path}"));
        assert_eq!(
            hex::encode(sha256(bytes)),
            string_at(file, "/sha256"),
            "full-file SHA-256 for {FIXTURE_DIRECTORY}/{path}"
        );
    }

    let fragments = array_at(manifest, "/canonical_fragments");
    let actual_fragments = fragments
        .iter()
        .map(|fragment| {
            (
                string_at(fragment, "/path"),
                string_at(fragment, "/json_pointer"),
                string_at(fragment, "/id"),
            )
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_fragments.len(),
        fragments.len(),
        "hash manifest contains duplicate canonical fragment identities"
    );
    assert_eq!(
        actual_fragments,
        EXPECTED_CANONICAL_FRAGMENTS.into_iter().collect::<BTreeSet<_>>(),
        "hash manifest canonical fragment inventory"
    );

    for fragment in fragments {
        let path = string_at(fragment, "/path");
        let pointer = string_at(fragment, "/json_pointer");
        let document = match path {
            BOUNDARIES_PATH => boundaries,
            PRE_SAVANNA_RUNTIME_PATH => pre_savanna_runtime,
            RAW_TRANSACTION_PATH => raw_transaction,
            PROVENANCE_PATH => provenance,
            _ => panic!("hash manifest references unsupported JSON document {path}"),
        };
        let value = document
            .pointer(pointer)
            .unwrap_or_else(|| panic!("hash manifest pointer {path}#{pointer} does not exist"));
        let canonical = canonical_json(value);
        assert_eq!(
            hex::encode(sha256(&canonical)),
            string_at(fragment, "/sha256"),
            "canonical fragment SHA-256 for {path}#{pointer}"
        );
    }
}

fn fixture_file(path: &str) -> Option<&'static [u8]> {
    match path {
        BOUNDARIES_PATH => Some(BOUNDARIES_BYTES),
        PRE_SAVANNA_RUNTIME_PATH => Some(PRE_SAVANNA_RUNTIME_BYTES),
        RAW_TRANSACTION_PATH => Some(RAW_TRANSACTION_BYTES),
        PROVENANCE_PATH => Some(PROVENANCE_BYTES),
        README_PATH => Some(README_BYTES),
        SHIP_PATH => Some(SHIP_BYTES),
        SHIP_VERIFIER_PATH => Some(SHIP_VERIFIER_BYTES),
        _ => None,
    }
}

fn parse_json(name: &str, bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|error| panic!("invalid fixture {name}: {error}"))
}

fn canonical_json(value: &Value) -> Vec<u8> {
    serde_json::to_vec(&sort_json(value))
        .unwrap_or_else(|error| panic!("failed to serialize canonical JSON: {error}"))
}

fn sort_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sort_json).collect()),
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sort_json(value)))
                    .collect::<Map<_, _>>(),
            )
        }
        _ => value.clone(),
    }
}

fn object_at<'a>(value: &'a Value, pointer: &str) -> &'a Map<String, Value> {
    value
        .pointer(pointer)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("fixture pointer {pointer} is not an object"))
}

fn array_at<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("fixture pointer {pointer} is not an array"))
}

fn string_at<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("fixture pointer {pointer} is not a string"))
}

fn bool_at(value: &Value, pointer: &str) -> bool {
    value
        .pointer(pointer)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| panic!("fixture pointer {pointer} is not a bool"))
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("fixture pointer {pointer} is not a u64"))
}

fn parse_quantity(value: &str) -> u128 {
    u128::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16)
        .unwrap_or_else(|error| panic!("invalid hex quantity {value}: {error}"))
}

fn normalize_hex(value: &str) -> String {
    let value = value.strip_prefix("0x").unwrap_or(value).trim_start_matches('0');
    if value.is_empty() {
        "0".to_string()
    } else {
        value.to_ascii_lowercase()
    }
}

fn decode_hex(value: &str) -> Vec<u8> {
    hex::decode(value.strip_prefix("0x").unwrap_or(value))
        .unwrap_or_else(|error| panic!("invalid hex bytes: {error}"))
}

fn assert_hex_eq(left: &str, right: &str, message: &str) {
    assert_eq!(normalize_hex(left), normalize_hex(right), "{message}");
}

fn antelope_name_to_u64(name: &str) -> u64 {
    assert!(name.len() <= 13, "Antelope name is longer than 13 characters: {name}");
    let mut value = 0u64;
    for index in 0..13 {
        let symbol = name.as_bytes().get(index).copied().unwrap_or(b'.');
        let symbol = match symbol {
            b'.' => 0,
            b'1'..=b'5' => symbol - b'1' + 1,
            b'a'..=b'z' => symbol - b'a' + 6,
            _ => panic!("invalid Antelope name symbol in {name}"),
        } as u64;
        if index < 12 {
            value |= (symbol & 0x1f) << (64 - 5 * (index + 1));
        } else {
            value |= symbol & 0x0f;
        }
    }
    value
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = u64::try_from(input.len())
        .expect("fixture input length fits u64")
        .checked_mul(8)
        .expect("fixture bit length fits u64");
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut hash = INITIAL;
    for block in padded.as_chunks::<64>().0 {
        let mut words = [0u32; 64];
        for (word, bytes) in words[..16].iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..64 {
            let sigma0 = words[index - 15].rotate_right(7) ^
                words[index - 15].rotate_right(18) ^
                (words[index - 15] >> 3);
            let sigma1 = words[index - 2].rotate_right(17) ^
                words[index - 2].rotate_right(19) ^
                (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(sigma0)
                .wrapping_add(words[index - 7])
                .wrapping_add(sigma1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (state, value) in hash.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }

    let mut output = [0u8; 32];
    for (output, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(hash) {
        output.copy_from_slice(&word.to_be_bytes());
    }
    output
}

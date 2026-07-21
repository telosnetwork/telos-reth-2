//! Source-diff inventory for a post-production Telos revm development commit.
//!
//! This verifies provenance metadata and named local tests only. It is not a legacy runtime oracle
//! and must not be treated as differential execution evidence for the production startup gate.

use alloy_primitives::{address, keccak256, Address};
use serde_json::Value;
use std::collections::BTreeSet;

const MATRIX: &str =
    include_str!("../testdata/revm-differential/legacy-telos-revm-237d6322.v1.json");
const HANDLER: &str = include_str!("../src/handler.rs");
const FRAME: &str = include_str!("../src/frame.rs");
const INSTRUCTIONS: &str = include_str!("../src/instructions.rs");
const RECEIPT: &str = include_str!("../src/receipt.rs");

const LEGACY_COMMIT: &str = "237d6322c6f5943af77fccff93fd0f85ecc204ed";
const LEGACY_PARENT: &str = "900409f134c1cbd4489d370a6b037f354afa4a5c";
const EXPECTED_SOURCES: [(&str, &str, &str, &str); 12] = [
    (
        "crates/interpreter/Cargo.toml",
        "813f174f6da65bc9d1b12b87d7fb444543f0f928",
        "3a44af394ddedc53995ec584da89b99b36f7eef9",
        "6f80ea29d39ec0c424c2bbc21816bf8b834fb4955d1ed7602d685535b413313a",
    ),
    (
        "crates/interpreter/src/instructions/contract.rs",
        "281e1475829cdc07fcf0d81bf271aa661d702c79",
        "7f09f2e7445f658144c50490b7585bceb76e3976",
        "6096795123ba8838b6e54b1ced8d293268d7b779e456cb74c1bf2e9c9ef0e837",
    ),
    (
        "crates/interpreter/src/instructions/host.rs",
        "612cbac5c12410c087f803bb4b16345fb4af122d",
        "eda6e51a7baf7a0749a279c7b87982c31daaa373",
        "d2b321ef0c691194cf4089c6e9cf9ca4590f9f1296d015414b22e0dc692b0390",
    ),
    (
        "crates/interpreter/src/instructions/host_env.rs",
        "ce934d492fc749360399c127b0412019b64a876f",
        "895a25f7a0b6a5c92a6adca5fea626b50e30d52e",
        "b107906c91db80977edb9ab3d56b79150def6a233f6e1f671e503c2579506123",
    ),
    (
        "crates/interpreter/src/instructions/stack.rs",
        "d75067e1f223ee25ecec1cba7e58b11a9124166d",
        "fbff7de8873d87c44bd84100d2cb7d7ac9d3d1b0",
        "98dd354cf815fad192f8b2e6fabd52fdba4bd88e335b211c16b1f10901374e11",
    ),
    (
        "crates/primitives/Cargo.toml",
        "e8c9cab7f9a05fd716711c3b7d2db7d52946fb85",
        "4fe8142ce5c584db5e5f31d347aca4a27070315a",
        "30f65fa9bd9f4227f94547026d32a9c162826dbfa8beb0dd99e8b65a48cf7dba",
    ),
    (
        "crates/primitives/src/env.rs",
        "e0d856df982748cb0c094f7834e5fd4a1206a61d",
        "d4b5ccf0d08ec7dca7dfc3060116c1dfe1f328f7",
        "2e213e1c9275886533b96780f793905810075c230afdb9387e4cad07ebdde982",
    ),
    (
        "crates/revm/Cargo.toml",
        "2ab2440d612c544c509a49879ee1a378582bd208",
        "810b1f1420227b4cedbdace316f7050a953b126a",
        "794e824865f161171333a33731fe1439b258c438d60ec69c8cde1550b37fb16f",
    ),
    (
        "crates/revm/src/context/evm_context.rs",
        "80f0257a39ef9795e0f32f27d0f0113e3c5b39b7",
        "e286c97221c0d967ad9f4e645f4442b397bfb15a",
        "add814888c2c3b9c2d8ff7bd8177522044135679e44a79eb24417a6363cd9863",
    ),
    (
        "crates/revm/src/handler/mainnet/post_execution.rs",
        "3e2f520e3e28502018290d72d695a0c4fa3b6bad",
        "0dfc812034dbd7b1bc22a61ad08597ace9d05570",
        "429bd6d9c19ce16661f1b515f36bded9c242b4d8394dec72e6d878b1dc72c5f3",
    ),
    (
        "crates/revm/src/handler/mainnet/pre_execution.rs",
        "5309618864015b335d9b5c142b526a5a762dca6d",
        "a53b6d0964f9f62a761bd682ceeae91f26359d2b",
        "c7f0504590972283a514e8f2ee96dff774d1f92b2b702e476839b2ffc4a297b1",
    ),
    (
        "crates/revm/src/journaled_state.rs",
        "7dce85a2f01b68c42a47f3ead63a191fe57f5194",
        "26f5b82aace56d106956b85dc8162d107222d214",
        "a2df3b6fd2e2c582bfb792ce9abeb1d1ca9932d3fb7fe55285cbbe1339303ea4",
    ),
];

const EXPECTED_BEHAVIORS: [&str; 15] = [
    "feature-plumbing",
    "fixed-gas-price",
    "chain-three-authentication",
    "native-nonce-exceptions",
    "zero-sender",
    "zero-address-burn",
    "no-beneficiary-reward",
    "first-new-address-introspection",
    "first-new-address-call-staticcall",
    "first-new-address-selfdestruct",
    "synthetic-blockhash",
    "legacy-gaslimit",
    "revision-push0",
    "revision-zero-delegatecall",
    "zero-sender-create",
];

#[test]
fn development_source_matrix_has_exact_provenance_and_named_port_tests() {
    let matrix: Value = serde_json::from_str(MATRIX).expect("valid differential matrix JSON");
    assert_eq!(string_at(&matrix, "/schema"), "telos-revm-differential-matrix/v1");
    assert_eq!(string_at(&matrix, "/evidence_classification"), "source-diff-inventory");
    assert_eq!(array_at(&matrix, "/limitations").len(), 3);
    assert_eq!(string_at(&matrix, "/legacy_source/commit"), LEGACY_COMMIT);
    assert_eq!(string_at(&matrix, "/legacy_source/parent"), LEGACY_PARENT);

    let changed_sources = array_at(&matrix, "/changed_sources");
    assert_eq!(changed_sources.len(), EXPECTED_SOURCES.len());
    for (actual, expected) in changed_sources.iter().zip(EXPECTED_SOURCES) {
        assert_eq!(string_at(actual, "/path"), expected.0);
        assert_eq!(string_at(actual, "/parent_blob"), expected.1);
        assert_eq!(string_at(actual, "/commit_blob"), expected.2);
        assert_eq!(string_at(actual, "/patch_sha256"), expected.3);
    }

    let expected_sources = EXPECTED_SOURCES.iter().map(|source| source.0).collect::<BTreeSet<_>>();
    let expected_behaviors = EXPECTED_BEHAVIORS.into_iter().collect::<BTreeSet<_>>();
    let behaviors = array_at(&matrix, "/behaviors");
    let actual_behaviors =
        behaviors.iter().map(|behavior| string_at(behavior, "/id")).collect::<BTreeSet<_>>();
    assert_eq!(actual_behaviors, expected_behaviors);

    let mut covered_sources = BTreeSet::new();
    for behavior in behaviors {
        let id = string_at(behavior, "/id");
        for source in array_at(behavior, "/legacy_sources") {
            let source = source.as_str().unwrap_or_else(|| panic!("{id}: source is not a string"));
            assert!(expected_sources.contains(source), "{id}: unknown legacy source {source}");
            covered_sources.insert(source);
        }
        let tests = array_at(behavior, "/tests");
        assert!(!tests.is_empty(), "{id}: behavior has no focused port test");
        for test in tests {
            assert_live_test(id, test);
        }
    }
    assert_eq!(covered_sources, expected_sources, "every changed legacy file must be classified");

    for claim in array_at(&matrix, "/supplemental_contract_evidence/claims") {
        assert_live_test(string_at(claim, "/id"), value_at(claim, "/port_test"));
    }
}

#[test]
fn fixture_constants_are_self_consistent_not_differential_outputs() {
    let matrix: Value = serde_json::from_str(MATRIX).expect("valid differential matrix JSON");

    let blockhash = behavior(&matrix, "synthetic-blockhash");
    let decimal_preimage = string_at(blockhash, "/golden/decimal_preimage");
    assert_eq!(u64_at(blockhash, "/golden/block_number").to_string(), decimal_preimage);
    assert_eq!(
        keccak256(decimal_preimage.as_bytes()).to_string(),
        string_at(blockhash, "/golden/keccak256")
    );

    let gaslimit = behavior(&matrix, "legacy-gaslimit");
    assert!(u64_at(gaslimit, "/golden/revision") < 2);
    assert_eq!(u64_at(gaslimit, "/golden/gaslimit"), 10_000_000);

    let gas_price = behavior(&matrix, "fixed-gas-price");
    assert_eq!(
        u64_at(gas_price, "/golden/charged"),
        u64_at(gas_price, "/golden/signed").min(u64_at(gas_price, "/golden/fixed"))
    );

    let create = behavior(&matrix, "zero-sender-create");
    let expected = address!("bd770416a3345f91e4b34576cb804a576fa48eb1");
    assert_eq!(Address::ZERO.create(u64_at(create, "/golden/nonce")), expected);
    assert_eq!(string_at(create, "/golden/created").parse::<Address>().unwrap(), expected);
}

fn assert_live_test(behavior: &str, test: &Value) {
    let path = string_at(test, "/path");
    let function = string_at(test, "/function");
    let source = match path {
        "src/handler.rs" => HANDLER,
        "src/frame.rs" => FRAME,
        "src/instructions.rs" => INSTRUCTIONS,
        "src/receipt.rs" => RECEIPT,
        _ => panic!("{behavior}: unsupported port-test source {path}"),
    };
    assert!(
        source.contains(&format!("fn {function}(")),
        "{behavior}: missing focused test {path}::{function}"
    );
}

fn behavior<'a>(matrix: &'a Value, id: &str) -> &'a Value {
    array_at(matrix, "/behaviors")
        .iter()
        .find(|behavior| string_at(behavior, "/id") == id)
        .unwrap_or_else(|| panic!("missing behavior {id}"))
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

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value_at(value, pointer)
        .as_u64()
        .unwrap_or_else(|| panic!("JSON pointer {pointer} is not a u64"))
}

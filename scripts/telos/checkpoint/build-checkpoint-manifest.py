#!/usr/bin/env python3
"""Build a trusted dual-root manifest from exact-legacy and native evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path

EMPTY_ROOT = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
NETWORKS = {
    "telos-mainnet": {
        "chain_id": 40,
        "genesis_hash": "0x36fe7024b760365e3970b7b403e161811c1e626edd68460272fcdfa276272563",
        "native_chain_id": "4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11",
    },
    "telos-testnet": {
        "chain_id": 41,
        "genesis_hash": "0xb25034033c9ca7a40e879ddcc29cf69071a22df06688b5fe8cc2d68b4e0528f9",
        "native_chain_id": "1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f",
    },
}
MAX_EVIDENCE_BYTES = 1024 * 1024
UINT32_MAX = 2**32 - 1
UINT64_MAX = 2**64 - 1
UINT256_MAX = 2**256 - 1


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return "0x" + digest.hexdigest()


def load_json_object(path: Path, label: str) -> dict[str, object]:
    try:
        metadata = path.stat()
    except OSError as error:
        raise SystemExit(f"cannot inspect {label} {path}: {error}") from error
    if not path.is_file() or metadata.st_size > MAX_EVIDENCE_BYTES:
        raise SystemExit(
            f"{label} must be a regular file no larger than {MAX_EVIDENCE_BYTES} bytes"
        )
    try:
        with path.open("r", encoding="utf-8") as stream:
            value = json.load(stream)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise SystemExit(f"cannot load {label} {path}: {error}") from error
    if not isinstance(value, dict):
        raise SystemExit(f"{label} is not a JSON object")
    return value


def quantity(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.startswith("0x"):
        raise SystemExit(f"{label} is not a hexadecimal quantity")
    digits = value[2:]
    if not digits or (len(digits) > 1 and digits.startswith("0")):
        raise SystemExit(f"{label} is not a canonical hexadecimal quantity")
    try:
        parsed = int(digits, 16)
    except ValueError as error:
        raise SystemExit(f"{label} is not hexadecimal: {error}") from error
    if parsed > UINT256_MAX:
        raise SystemExit(f"{label} is outside uint256")
    return hex(parsed)


def require_int(value: object, label: str, *, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise SystemExit(f"{label} is outside {minimum}..={maximum}")
    return value


def hex32(value: object, label: str, *, allow_zero: bool = True) -> str:
    if not isinstance(value, str):
        raise SystemExit(f"{label} is not a hexadecimal string")
    normalized = value.lower()
    if len(normalized) != 66 or not normalized.startswith("0x"):
        raise SystemExit(f"{label} is not a 32-byte value")
    try:
        raw = bytes.fromhex(normalized[2:])
    except ValueError as error:
        raise SystemExit(f"{label} is not valid hexadecimal: {error}") from error
    if not allow_zero and raw == bytes(32):
        raise SystemExit(f"{label} must not be zero")
    return normalized


def digest(value: object, label: str) -> str:
    return hex32(value, label, allow_zero=False)


def unprefixed_hex32(value: object, label: str, *, allow_zero: bool = True) -> str:
    if not isinstance(value, str) or value.startswith("0x"):
        raise SystemExit(f"{label} must be an unprefixed 32-byte hexadecimal value")
    return hex32("0x" + value, label, allow_zero=allow_zero)[2:]


def native_block_number(block_id: str) -> int:
    return int.from_bytes(bytes.fromhex(block_id.removeprefix("0x")[:8]), "big")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--network", choices=sorted(NETWORKS), required=True)
    parser.add_argument("--legacy-evidence", type=Path, required=True)
    parser.add_argument("--native-anchor-attestation", type=Path, required=True)
    parser.add_argument("--state-dump", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    if args.output.exists():
        raise SystemExit(f"refusing to overwrite {args.output}")

    exported = load_json_object(args.legacy_evidence, "exact-legacy evidence")
    export_metadata_sha = sha256_file(args.legacy_evidence)
    if exported.get("version") != 1 or exported.get("legacy_source_commit") != (
        "8c37741ea8d97eba713a8028e3f09132bb51abd6"
    ):
        raise SystemExit("unsupported exact-legacy export evidence")
    if exported.get("chain") != args.network:
        raise SystemExit("selected network does not match the chain attested by the MDBX copy")
    block_number = require_int(
        exported.get("block_number"),
        "checkpoint block number",
        minimum=1,
        maximum=UINT64_MAX - 1,
    )
    if exported.get("body_transaction_count") != 0 or isinstance(
        exported.get("body_transaction_count"), bool
    ):
        raise SystemExit("legacy export anchor is not transaction-free")
    count_fields = (
        "accounts",
        "storage_slots",
        "bytecode_accounts",
        "plain_accounts",
        "hashed_accounts",
        "plain_storage_slots",
        "hashed_storage_slots",
    )
    counts = {
        field: require_int(
            exported.get(field), f"legacy {field} count", minimum=0, maximum=UINT64_MAX
        )
        for field in count_fields
    }
    if counts["accounts"] == 0:
        raise SystemExit("legacy state export contains no accounts")
    if counts["bytecode_accounts"] > counts["accounts"]:
        raise SystemExit("legacy bytecode-account count exceeds the account count")
    bytecode_hash_overrides = exported.get("bytecode_hash_overrides", [])
    if not isinstance(bytecode_hash_overrides, list) or len(bytecode_hash_overrides) > counts[
        "bytecode_accounts"
    ]:
        raise SystemExit("legacy bytecode-hash override evidence is invalid")
    override_addresses: set[str] = set()
    for index, override in enumerate(bytecode_hash_overrides):
        if not isinstance(override, dict) or set(override) != {
            "address",
            "recorded_code_hash",
            "actual_code_hash",
        }:
            raise SystemExit(f"legacy bytecode-hash override {index} has an invalid shape")
        address = override["address"]
        if not isinstance(address, str) or len(address) != 42 or not address.startswith("0x"):
            raise SystemExit(f"legacy bytecode-hash override {index} has an invalid address")
        try:
            bytes.fromhex(address[2:])
        except ValueError as error:
            raise SystemExit(
                f"legacy bytecode-hash override {index} address is not hexadecimal: {error}"
            ) from error
        normalized_address = address.lower()
        if normalized_address in override_addresses:
            raise SystemExit("legacy bytecode-hash override evidence repeats an address")
        override_addresses.add(normalized_address)
        recorded = hex32(
            override["recorded_code_hash"],
            f"legacy bytecode-hash override {index} recorded hash",
            allow_zero=False,
        )
        actual = hex32(
            override["actual_code_hash"],
            f"legacy bytecode-hash override {index} actual hash",
            allow_zero=False,
        )
        if recorded == actual:
            raise SystemExit("legacy bytecode-hash override does not describe a mismatch")
    if counts["accounts"] != counts["plain_accounts"] or counts["storage_slots"] != counts[
        "plain_storage_slots"
    ]:
        raise SystemExit("legacy state export did not cover every plain-state row")
    if counts["plain_accounts"] != counts["hashed_accounts"] or counts[
        "plain_storage_slots"
    ] != counts["hashed_storage_slots"]:
        raise SystemExit("legacy plain and hashed state tables are not aligned")
    stages = exported.get("stage_checkpoints")
    if not isinstance(stages, dict):
        raise SystemExit("legacy evidence has no stage checkpoint map")
    execution_height = require_int(
        stages.get("Execution"),
        "legacy Execution checkpoint",
        minimum=1,
        maximum=UINT64_MAX,
    )
    if execution_height != block_number:
        raise SystemExit("legacy Execution checkpoint does not match the exported anchor")
    for stage in ("AccountHashing", "StorageHashing", "MerkleExecute"):
        stage_height = require_int(
            stages.get(stage), f"legacy {stage} checkpoint", minimum=1, maximum=UINT64_MAX
        )
        if stage_height != execution_height:
            raise SystemExit(f"legacy {stage} is not aligned with Execution")

    header_state_root = hex32(exported.get("header_state_root"), "header state root")
    actual_state_root = hex32(
        exported.get("actual_state_root"), "actual state root", allow_zero=False
    )
    if header_state_root != EMPTY_ROOT:
        raise SystemExit("exported canonical header does not carry EMPTY_ROOT_HASH")
    if actual_state_root == EMPTY_ROOT:
        raise SystemExit("exported actual state root must be non-empty")
    backup_manifest_sha = digest(
        exported.get("backup_manifest_sha256"), "backup manifest SHA-256"
    )
    backup_mdbx_sha = digest(exported.get("backup_mdbx_sha256"), "backup MDBX SHA-256")

    dump_sha = sha256_file(args.state_dump)
    exported_dump_sha = digest(exported.get("state_dump_sha256"), "state dump SHA-256")
    if dump_sha.lower() != exported_dump_sha:
        raise SystemExit("state dump SHA-256 does not match export metadata")
    with args.state_dump.open("rb") as stream:
        first_line = stream.readline(4096)
    try:
        declared_root = hex32(json.loads(first_line)["root"], "state dump declared root")
    except (KeyError, ValueError, TypeError) as error:
        raise SystemExit(f"invalid state dump root line: {error}") from error
    if declared_root != actual_state_root:
        raise SystemExit("state dump root line does not match export metadata")

    header_rlp = exported.get("header_rlp")
    if not isinstance(header_rlp, str) or not header_rlp.startswith("0x"):
        raise SystemExit("header RLP is not a 0x-prefixed hexadecimal string")
    try:
        header_bytes = bytes.fromhex(header_rlp.removeprefix("0x"))
    except ValueError as error:
        raise SystemExit(f"header RLP is not valid hexadecimal: {error}") from error
    if not header_bytes:
        raise SystemExit("header RLP must not be empty")
    header_sha = digest(exported.get("header_rlp_sha256"), "header RLP SHA-256")
    if "0x" + hashlib.sha256(header_bytes).hexdigest() != header_sha:
        raise SystemExit("header RLP SHA-256 does not match export metadata")

    native = load_json_object(args.native_anchor_attestation, "native anchor attestation")
    native_attestation_sha = sha256_file(args.native_anchor_attestation)
    if native.get("version") != 1 or native.get("chain") != args.network:
        raise SystemExit("native anchor attestation identifies the wrong schema or network")
    if digest(native.get("legacy_evidence_sha256"), "attested legacy evidence SHA-256") != (
        export_metadata_sha
    ):
        raise SystemExit("native anchor attestation does not bind the exact legacy evidence")

    network = NETWORKS[args.network]
    block_hash = hex32(exported.get("block_hash"), "block hash", allow_zero=False)
    evm_anchor = native.get("evm_anchor")
    evm_child = native.get("evm_first_child")
    native_anchor = native.get("native_anchor")
    native_child = native.get("native_first_child")
    if not all(
        isinstance(value, dict)
        for value in (evm_anchor, evm_child, native_anchor, native_child)
    ):
        raise SystemExit("native anchor attestation is incomplete")
    evm_anchor_number = require_int(
        evm_anchor.get("number"), "attested EVM anchor number", minimum=1, maximum=UINT64_MAX
    )
    evm_anchor_hash = hex32(evm_anchor.get("hash"), "attested EVM anchor hash", allow_zero=False)
    parent_block_hash = hex32(
        exported.get("parent_block_hash"), "legacy parent block hash", allow_zero=False
    )
    evm_anchor_parent = hex32(
        evm_anchor.get("parent_hash"), "attested EVM anchor parent", allow_zero=False
    )
    if evm_anchor_number != block_number or evm_anchor_hash != block_hash:
        raise SystemExit("native attestation EVM anchor does not match legacy evidence")
    if evm_anchor_parent != parent_block_hash:
        raise SystemExit("native attestation EVM anchor parent does not match legacy evidence")
    evm_child_number = require_int(
        evm_child.get("number"), "attested EVM child number", minimum=1, maximum=UINT64_MAX
    )
    evm_child_parent = hex32(
        evm_child.get("parent_hash"), "attested EVM child parent", allow_zero=False
    )
    if evm_child_number != block_number + 1 or evm_child_parent != block_hash:
        raise SystemExit("native attestation does not bind the exact first EVM child")
    native_block_number_value = require_int(
        exported.get("native_block_number"),
        "legacy native anchor number",
        minimum=1,
        maximum=UINT32_MAX - 1,
    )
    attested_native_number = require_int(
        native_anchor.get("number"),
        "attested native anchor number",
        minimum=1,
        maximum=UINT32_MAX - 1,
    )
    attested_native_child_number = require_int(
        native_child.get("number"),
        "attested native child number",
        minimum=1,
        maximum=UINT32_MAX,
    )
    if attested_native_number != native_block_number_value or attested_native_child_number != (
        native_block_number_value + 1
    ):
        raise SystemExit("native attestation block boundary does not match legacy evidence")
    exported_native_id = hex32(exported.get("native_block_id"), "legacy native block ID")
    native_anchor_id_raw = unprefixed_hex32(
        native_anchor.get("id"), "native anchor block ID", allow_zero=False
    )
    native_child_id_raw = unprefixed_hex32(
        native_child.get("id"), "native first-child block ID", allow_zero=False
    )
    native_previous_raw = unprefixed_hex32(
        native_child.get("previous"), "native first-child previous ID", allow_zero=False
    )
    native_anchor_previous_raw = unprefixed_hex32(
        native_anchor.get("previous"), "native anchor previous ID", allow_zero=False
    )
    if native_block_number(exported_native_id) != native_block_number_value:
        raise SystemExit("legacy native anchor ID does not encode its block number")
    if native_block_number(native_anchor_id_raw) != native_block_number_value:
        raise SystemExit("attested native anchor ID does not encode its block number")
    if native_block_number(native_anchor_previous_raw) != native_block_number_value - 1:
        raise SystemExit("attested native anchor previous ID does not encode the preceding block")
    if native_block_number(native_child_id_raw) != native_block_number_value + 1:
        raise SystemExit("attested native child ID does not encode its block number")
    if "0x" + native_anchor_id_raw != exported_native_id:
        raise SystemExit("native attestation block ID does not match legacy header extraData")
    evm_anchor_native_id = hex32(
        evm_anchor.get("native_block_id"), "attested EVM anchor native block ID", allow_zero=False
    )
    if evm_anchor_native_id != exported_native_id:
        raise SystemExit("attested EVM anchor native ID does not match legacy header extraData")
    evm_child_native_id = hex32(
        evm_child.get("native_block_id"), "attested EVM child native block ID", allow_zero=False
    )
    if evm_child_native_id != "0x" + native_child_id_raw:
        raise SystemExit("attested EVM child does not bind the native first child")
    if native_previous_raw != native_anchor_id_raw:
        raise SystemExit("native attestation first child does not extend the anchor")
    if native.get("native_chain_id") != network.get("native_chain_id"):
        raise SystemExit("native attestation has the wrong Antelope chain ID")
    if native.get("evm_chain_id") != network["chain_id"]:
        raise SystemExit("native attestation has the wrong EVM chain ID")
    irreversible_number = require_int(
        native.get("observed_last_irreversible_block_num"),
        "observed last irreversible block number",
        minimum=1,
        maximum=UINT32_MAX,
    )
    if irreversible_number < native_block_number_value + 1:
        raise SystemExit("native anchor and first child are not attested irreversible")
    irreversible_id = unprefixed_hex32(
        native.get("observed_last_irreversible_block_id"),
        "observed last irreversible block ID",
        allow_zero=False,
    )
    if native_block_number(irreversible_id) != irreversible_number:
        raise SystemExit("last irreversible block ID does not encode its reported number")
    response_digests = []
    for label, value in (
        ("EVM anchor response", evm_anchor.get("response_sha256")),
        ("EVM child response", evm_child.get("response_sha256")),
        ("native anchor response", native_anchor.get("response_sha256")),
        ("native child response", native_child.get("response_sha256")),
        ("nodeos info response", native.get("nodeos_info_response_sha256")),
    ):
        response_digests.append(digest(value, f"{label} SHA-256"))
    if len(set(response_digests)) != len(response_digests):
        raise SystemExit("native attestation reuses a response digest across distinct evidence")

    starting_gas_price = quantity(
        exported.get("starting_child_gas_price"), "legacy first-child gas price"
    )
    attested_gas_price = quantity(
        native.get("starting_child_gas_price"), "attested first-child gas price"
    )
    starting_revision = require_int(
        exported.get("starting_child_revision"),
        "legacy first-child revision",
        minimum=0,
        maximum=UINT64_MAX,
    )
    attested_revision = require_int(
        native.get("starting_child_revision"),
        "attested first-child revision",
        minimum=0,
        maximum=UINT64_MAX,
    )
    if attested_gas_price != starting_gas_price or attested_revision != starting_revision:
        raise SystemExit("native attestation and legacy header disagree on first-child context")

    native_chain_id = "0x" + unprefixed_hex32(
        native.get("native_chain_id"), "native chain ID", allow_zero=False
    )
    native_anchor_id = "0x" + native_anchor_id_raw
    native_child_id = "0x" + native_child_id_raw
    evm_child_hash = hex32(
        evm_child.get("hash"), "EVM first-child block hash", allow_zero=False
    )
    if evm_child_hash == block_hash:
        raise SystemExit("EVM first-child hash reuses the anchor hash")
    manifest = {
        "version": 2,
        "canonical_chain": {
            "chain_id": network["chain_id"],
            "genesis_hash": network["genesis_hash"],
        },
        "execution_anchor": {
            "version": 1,
            "chain": {"chain_id": network["chain_id"], "genesis_hash": block_hash},
            "parent_block_number": block_number,
            "parent_block_hash": block_hash,
            "starting_gas_price": starting_gas_price,
            "starting_revision": starting_revision,
        },
        "header_rlp": header_rlp.lower(),
        "header_rlp_sha256": header_sha,
        "state_dump_sha256": dump_sha.lower(),
        "export_metadata_sha256": export_metadata_sha.lower(),
        "native_anchor_attestation_sha256": native_attestation_sha.lower(),
        "native_anchor": {
            "chain_id": native_chain_id,
            "block_number": native_block_number_value,
            "block_id": native_anchor_id,
            "first_child_block_number": native_block_number_value + 1,
            "first_child_block_id": native_child_id,
            "evm_first_child_block_hash": evm_child_hash,
            "starting_gas_price": starting_gas_price,
            "starting_revision": starting_revision,
        },
        "backup_manifest_sha256": backup_manifest_sha,
        "backup_mdbx_sha256": backup_mdbx_sha,
        "actual_state_root": actual_state_root,
    }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_suffix(".tmp")
    with temporary.open("x", encoding="utf-8") as stream:
        json.dump(manifest, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(args.output)
    print(f"checkpoint_manifest={args.output}")
    print(f"checkpoint_chain=telos-checkpoint:{args.output}")


if __name__ == "__main__":
    main()

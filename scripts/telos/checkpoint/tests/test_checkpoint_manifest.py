#!/usr/bin/env python3
"""Adversarial tests for exact-legacy/native checkpoint evidence binding."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_DIR = Path(__file__).resolve().parents[1]
BUILD_SCRIPT = SCRIPT_DIR / "build-checkpoint-manifest.py"
ATTEST_SCRIPT = SCRIPT_DIR / "attest-native-anchor.py"
LEGACY_COMMIT = "8c37741ea8d97eba713a8028e3f09132bb51abd6"
EMPTY_ROOT = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
EMPTY_OMMERS = "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
ZERO_BLOOM = "0x" + "00" * 256
MAINNET_NATIVE_CHAIN_ID = (
    "4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11"
)
ANCHOR_NUMBER = 7
NATIVE_NUMBER = 43
ANCHOR_HASH = "0x" + "44" * 32
PARENT_HASH = "0x" + "33" * 32
CHILD_HASH = "0x" + "55" * 32
ACTUAL_ROOT = "0x" + "66" * 32


def sha256_bytes(value: bytes) -> str:
    return "0x" + hashlib.sha256(value).hexdigest()


def block_id(number: int, fill: int, *, prefix: bool) -> str:
    value = number.to_bytes(4, "big") + bytes([fill]) * 28
    return ("0x" if prefix else "") + value.hex()


ANCHOR_NATIVE_ID = block_id(NATIVE_NUMBER, 0x11, prefix=True)
CHILD_NATIVE_ID = block_id(NATIVE_NUMBER + 1, 0x22, prefix=True)


def load_script(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ATTEST = load_script("telos_attest_native_anchor", ATTEST_SCRIPT)


def fixture_evidence(state_dump: bytes) -> dict[str, object]:
    header_rlp = bytes.fromhex("c0")
    return {
        "version": 1,
        "legacy_source_commit": LEGACY_COMMIT,
        "chain": "telos-mainnet",
        "backup_manifest_sha256": "0x" + "10" * 32,
        "backup_mdbx_sha256": "0x" + "20" * 32,
        "block_number": ANCHOR_NUMBER,
        "block_hash": ANCHOR_HASH,
        "parent_block_hash": PARENT_HASH,
        "header_rlp": "0x" + header_rlp.hex(),
        "header_rlp_sha256": sha256_bytes(header_rlp),
        "header_state_root": EMPTY_ROOT,
        "actual_state_root": ACTUAL_ROOT,
        "state_dump_sha256": sha256_bytes(state_dump),
        "native_block_number": NATIVE_NUMBER,
        "native_block_id": ANCHOR_NATIVE_ID,
        "starting_child_gas_price": "0x7",
        "starting_child_revision": 1,
        "stage_checkpoints": {
            "Execution": ANCHOR_NUMBER,
            "AccountHashing": ANCHOR_NUMBER,
            "StorageHashing": ANCHOR_NUMBER,
            "MerkleExecute": ANCHOR_NUMBER,
        },
        "accounts": 1,
        "storage_slots": 1,
        "bytecode_accounts": 0,
        "plain_accounts": 1,
        "hashed_accounts": 1,
        "plain_storage_slots": 1,
        "hashed_storage_slots": 1,
        "body_transaction_count": 0,
    }


def fixture_attestation() -> dict[str, object]:
    return {
        "version": 1,
        "chain": "telos-mainnet",
        "legacy_evidence_sha256": "0x" + "bb" * 32,
        "evm_chain_id": 40,
        "native_chain_id": MAINNET_NATIVE_CHAIN_ID,
        "evm_anchor": {
            "number": ANCHOR_NUMBER,
            "hash": ANCHOR_HASH,
            "parent_hash": PARENT_HASH,
            "native_block_id": ANCHOR_NATIVE_ID,
            "response_sha256": "0x" + "a1" * 32,
        },
        "evm_first_child": {
            "number": ANCHOR_NUMBER + 1,
            "hash": CHILD_HASH,
            "parent_hash": ANCHOR_HASH,
            "native_block_id": CHILD_NATIVE_ID,
            "response_sha256": "0x" + "a2" * 32,
        },
        "native_anchor": {
            "number": NATIVE_NUMBER,
            "id": ANCHOR_NATIVE_ID[2:],
            "previous": block_id(NATIVE_NUMBER - 1, 0x09, prefix=False),
            "response_sha256": "0x" + "a3" * 32,
        },
        "native_first_child": {
            "number": NATIVE_NUMBER + 1,
            "id": CHILD_NATIVE_ID[2:],
            "previous": ANCHOR_NATIVE_ID[2:],
            "response_sha256": "0x" + "a4" * 32,
        },
        "observed_last_irreversible_block_num": NATIVE_NUMBER + 1,
        "observed_last_irreversible_block_id": CHILD_NATIVE_ID[2:],
        "nodeos_info_response_sha256": "0x" + "a5" * 32,
        "starting_child_gas_price": "0x7",
        "starting_child_revision": 1,
    }


class ManifestBuilderTests(unittest.TestCase):
    def run_builder(self, mutate=None, *, network: str = "telos-mainnet"):
        state_dump = (json.dumps({"root": ACTUAL_ROOT}, separators=(",", ":")) + "\n").encode()
        evidence = fixture_evidence(state_dump)
        attestation = fixture_attestation()
        preserve_digest = False
        if mutate is not None:
            preserve_digest = bool(mutate(evidence, attestation))

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state_path = root / "state.jsonl"
            evidence_path = root / "state.legacy-evidence.json"
            attestation_path = root / "native-anchor.attestation.json"
            output_path = root / "checkpoint.json"
            state_path.write_bytes(state_dump)
            evidence_path.write_text(
                json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            if not preserve_digest:
                attestation["legacy_evidence_sha256"] = sha256_bytes(evidence_path.read_bytes())
            attestation_path.write_text(
                json.dumps(attestation, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            result = subprocess.run(
                [
                    sys.executable,
                    str(BUILD_SCRIPT),
                    "--network",
                    network,
                    "--legacy-evidence",
                    str(evidence_path),
                    "--native-anchor-attestation",
                    str(attestation_path),
                    "--state-dump",
                    str(state_path),
                    "--output",
                    str(output_path),
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            manifest = json.loads(output_path.read_text()) if output_path.exists() else None
            return result, manifest

    def assert_rejected(self, mutate, message: str, *, network: str = "telos-mainnet"):
        result, manifest = self.run_builder(mutate, network=network)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIsNone(manifest)
        self.assertIn(message, result.stderr)

    def test_valid_evidence_derives_context_without_manual_inputs(self):
        result, manifest = self.run_builder()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(manifest["execution_anchor"]["starting_gas_price"], "0x7")
        self.assertEqual(manifest["native_anchor"]["starting_gas_price"], "0x7")
        self.assertEqual(manifest["native_anchor"]["starting_revision"], 1)

    def test_rejects_wrong_chain(self):
        self.assert_rejected(
            lambda _evidence, native: native.update(evm_chain_id=41),
            "wrong EVM chain ID",
        )

    def test_rejects_wrong_anchor_id(self):
        replacement = block_id(NATIVE_NUMBER, 0x99, prefix=False)
        self.assert_rejected(
            lambda _evidence, native: native["native_anchor"].update(id=replacement),
            "does not match legacy header extraData",
        )

    def test_rejects_wrong_execution_number(self):
        self.assert_rejected(
            lambda evidence, _native: evidence["stage_checkpoints"].update(Execution=8),
            "Execution checkpoint does not match",
        )

    def test_rejects_disconnected_child(self):
        replacement = block_id(NATIVE_NUMBER - 1, 0x77, prefix=False)
        self.assert_rejected(
            lambda _evidence, native: native["native_first_child"].update(previous=replacement),
            "does not extend the anchor",
        )

    def test_rejects_wrong_evidence_digest(self):
        def mutate(_evidence, native):
            native["legacy_evidence_sha256"] = "0x" + "00" * 32
            return True

        self.assert_rejected(mutate, "must not be zero")

    def test_rejects_context_substitution(self):
        self.assert_rejected(
            lambda _evidence, native: native.update(starting_child_gas_price="0x8"),
            "disagree on first-child context",
        )


class NativeAttestorTests(unittest.TestCase):
    def setUp(self):
        self.state_dump = (json.dumps({"root": ACTUAL_ROOT}) + "\n").encode()
        self.evidence = fixture_evidence(self.state_dump)
        self.anchor_block = {
            "number": hex(ANCHOR_NUMBER),
            "hash": ANCHOR_HASH,
            "parentHash": PARENT_HASH,
            "transactions": [],
            "stateRoot": EMPTY_ROOT,
            "transactionsRoot": EMPTY_ROOT,
            "receiptsRoot": EMPTY_ROOT,
            "sha3Uncles": EMPTY_OMMERS,
            "logsBloom": ZERO_BLOOM,
            "gasUsed": "0x0",
            "extraData": ANCHOR_NATIVE_ID,
        }
        self.child_block = {
            "number": hex(ANCHOR_NUMBER + 1),
            "hash": CHILD_HASH,
            "parentHash": ANCHOR_HASH,
            "extraData": CHILD_NATIVE_ID,
        }
        self.info = {
            "chain_id": MAINNET_NATIVE_CHAIN_ID,
            "last_irreversible_block_num": NATIVE_NUMBER + 1,
            "last_irreversible_block_id": CHILD_NATIVE_ID[2:],
        }
        self.native_anchor = {
            "block_num": NATIVE_NUMBER,
            "id": ANCHOR_NATIVE_ID[2:],
            "previous": block_id(NATIVE_NUMBER - 1, 0x09, prefix=False),
        }
        self.native_child = {
            "block_num": NATIVE_NUMBER + 1,
            "id": CHILD_NATIVE_ID[2:],
            "previous": ANCHOR_NATIVE_ID[2:],
        }

    def run_attestor(self, evidence_mutator=None, response_mutator=None):
        evidence = copy.deepcopy(self.evidence)
        anchor_block = copy.deepcopy(self.anchor_block)
        child_block = copy.deepcopy(self.child_block)
        info = copy.deepcopy(self.info)
        native_anchor = copy.deepcopy(self.native_anchor)
        native_child = copy.deepcopy(self.native_child)
        if evidence_mutator is not None:
            evidence_mutator(evidence)
        if response_mutator is not None:
            response_mutator(anchor_block, child_block, info, native_anchor, native_child)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "legacy-evidence.json"
            output_path = root / "native-attestation.json"
            evidence_path.write_text(
                json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )

            def fake_rpc(_url, method, params):
                if method == "eth_chainId":
                    return "0x28"
                if method == "eth_getBlockByNumber" and params[0] == hex(ANCHOR_NUMBER):
                    return anchor_block
                if method == "eth_getBlockByNumber" and params[0] == hex(ANCHOR_NUMBER + 1):
                    return child_block
                raise AssertionError((method, params))

            def fake_post(url, payload):
                if url.endswith("/v1/chain/get_info"):
                    return info
                if payload["block_num_or_id"] == NATIVE_NUMBER:
                    return native_anchor
                if payload["block_num_or_id"] == NATIVE_NUMBER + 1:
                    return native_child
                raise AssertionError((url, payload))

            argv = [
                str(ATTEST_SCRIPT),
                "--legacy-evidence",
                str(evidence_path),
                "--evm-rpc-url",
                "http://127.0.0.1:8545",
                "--nodeos-url",
                "http://127.0.0.1:8888",
                "--output",
                str(output_path),
            ]
            with mock.patch.object(sys, "argv", argv), mock.patch.object(
                ATTEST, "rpc", side_effect=fake_rpc
            ), mock.patch.object(ATTEST, "post_json", side_effect=fake_post):
                error = None
                try:
                    ATTEST.main()
                except SystemExit as caught:
                    error = str(caught)
            output = json.loads(output_path.read_text()) if output_path.exists() else None
            return error, output

    def test_attests_exact_anchor_and_child(self):
        error, output = self.run_attestor()
        self.assertIsNone(error)
        self.assertEqual(output["evm_anchor"]["native_block_id"], ANCHOR_NATIVE_ID)
        self.assertEqual(output["native_first_child"]["id"], CHILD_NATIVE_ID[2:])

    def test_rejects_wrong_reference_anchor_id(self):
        def mutate(anchor, _child, _info, _native_anchor, _native_child):
            anchor["extraData"] = block_id(NATIVE_NUMBER, 0x99, prefix=True)

        error, output = self.run_attestor(response_mutator=mutate)
        self.assertIsNone(output)
        self.assertIn("does not match legacy evidence", error)

    def test_rejects_wrong_native_child(self):
        def mutate(_anchor, _child, _info, _native_anchor, native_child):
            native_child["previous"] = block_id(NATIVE_NUMBER - 1, 0x77, prefix=False)

        error, output = self.run_attestor(response_mutator=mutate)
        self.assertIsNone(output)
        self.assertIn("does not extend", error)

    def test_rejects_untrusted_context_type(self):
        error, output = self.run_attestor(
            evidence_mutator=lambda evidence: evidence.update(starting_child_revision=True)
        )
        self.assertIsNone(output)
        self.assertIn("first-child revision", error)


if __name__ == "__main__":
    unittest.main()

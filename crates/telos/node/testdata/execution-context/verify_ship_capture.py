#!/usr/bin/env python3
"""Verify the retained Telos SHIP archive records without network access."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import struct
import zlib
from pathlib import Path
from typing import Any


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def expect(actual: Any, expected: Any, label: str) -> None:
    if actual != expected:
        raise ValueError(f"{label}: expected {expected!r}, got {actual!r}")


def verify_history_record(record: dict[str, Any], native_block_id: str) -> bytes:
    raw = base64.b64decode(record["record_base64"], validate=True)
    expect(len(raw), record["record_size"], "history record size")
    expect(sha256(raw), record["record_sha256"], "history record sha256")
    expect(
        record["next_record_offset"] - record["record_offset"],
        len(raw),
        "history index span",
    )

    magic = raw[:8]
    block_id = raw[8:40]
    payload_size = struct.unpack_from("<Q", raw, 40)[0]
    payload_end = 48 + payload_size
    payload = raw[48:payload_end]
    trailing_offset = struct.unpack_from("<Q", raw, payload_end)[0]

    expect(magic.hex(), record["ship_magic_bytes"], "SHIP magic")
    expect(block_id.hex(), native_block_id, "history header block id")
    expect(block_id.hex(), record["header_block_id"], "record metadata block id")
    expect(payload_size, record["compressed_payload_size"], "payload size")
    expect(sha256(payload), record["compressed_payload_sha256"], "payload sha256")
    expect(payload_end + 8, len(raw), "history record framing")
    expect(trailing_offset, record["record_offset"], "trailing record offset")

    compression_mode = struct.unpack_from("<I", payload, 0)[0]
    expect(compression_mode, record["compression_mode"], "compression mode")
    if compression_mode != 1 or len(payload) <= 12:
        raise ValueError("fixture expects state-history compression mode 1")
    decoded_size = struct.unpack_from("<Q", payload, 4)[0]
    decoded = zlib.decompress(payload[12:])
    expect(decoded_size, len(decoded), "embedded decoded size")
    expect(len(decoded), record["decoded_size"], "decoded size")
    expect(sha256(decoded), record["decoded_sha256"], "decoded sha256")
    return decoded


def verify_signed_block_record(
    record: dict[str, Any], native_block_number: int, native_block_id: str
) -> None:
    raw = base64.b64decode(record["record_base64"], validate=True)
    expect(len(raw), record["record_size"], "block-log record size")
    expect(sha256(raw), record["record_sha256"], "block-log record sha256")
    expect(
        record["next_record_offset"] - record["record_offset"],
        len(raw),
        "block-log index span",
    )

    signed_block = raw[:-8]
    trailing_offset = struct.unpack_from("<Q", raw, len(raw) - 8)[0]
    expect(len(signed_block), record["signed_block_size"], "signed block size")
    expect(sha256(signed_block), record["signed_block_sha256"], "signed block sha256")
    expect(trailing_offset, record["record_offset"], "trailing block-log offset")

    # This block has neither a proposed producer schedule nor header extensions. The packed
    # Antelope block_header therefore ends after the two zero discriminants at byte 116.
    expect(signed_block[114], 0, "new_producers discriminant")
    expect(signed_block[115], 0, "header_extensions length")
    calculated_id = bytearray(hashlib.sha256(signed_block[:116]).digest())
    calculated_id[:4] = native_block_number.to_bytes(4, "big")
    expect(calculated_id.hex(), native_block_id, "signed block id")


def main() -> None:
    default_fixture = Path(__file__).with_name("ship-mainnet-423015053.v1.json")
    parser = argparse.ArgumentParser()
    parser.add_argument("fixture", nargs="?", type=Path, default=default_fixture)
    args = parser.parse_args()

    fixture = json.loads(args.fixture.read_text())
    capture = fixture["capture"]
    records = fixture["archive_records"]
    native_block_number = capture["native_block_number"]
    native_block_id = capture["native_block_id"]

    verify_signed_block_record(records["signed_block"], native_block_number, native_block_id)
    traces = verify_history_record(records["traces"], native_block_id)
    chain_state = verify_history_record(records["chain_state"], native_block_id)

    expected = fixture["expected_translation"]
    expect(
        native_block_number - fixture["network"]["native_to_evm_block_delta"],
        expected["evm_block_number"],
        "native-to-EVM block mapping",
    )
    change = expected["execution_changes"]
    expect(len(change), 1, "execution change count")
    expect(change[0]["boundary"], expected["transaction_count"], "child-only boundary")

    trace_positions = []
    for item in expected["trace_order"]:
        transaction_id = bytes.fromhex(item["native_transaction_id"])
        expect(traces.count(transaction_id), 1, "native transaction id occurrence")
        trace_positions.append(traces.index(transaction_id))
        if "raw_transaction" in item:
            raw_transaction = bytes.fromhex(item["raw_transaction"].removeprefix("0x"))
            expect(traces.count(raw_transaction), 1, "raw transaction occurrence")
    expect(trace_positions, sorted(trace_positions), "native transaction trace order")

    changed_gas_price = int(change[0]["value"], 16).to_bytes(32, "big")
    expect(chain_state.count(changed_gas_price), 1, "changed gas price in chain-state delta")

    print(
        f"verified {args.fixture.name}: block {native_block_number}, "
        f"{expected['transaction_count']} transactions, gas-price boundary "
        f"{change[0]['boundary']}"
    )


if __name__ == "__main__":
    main()

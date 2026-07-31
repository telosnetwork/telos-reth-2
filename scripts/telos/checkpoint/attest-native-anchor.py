#!/usr/bin/env python3
"""Bind an exact legacy EVM checkpoint to irreversible Antelope blocks."""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import os
import socket
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

EMPTY_ROOT = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
EMPTY_OMMERS = "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
ZERO_BLOOM = "0x" + "00" * 256
NETWORKS = {
    "telos-mainnet": {
        "evm_chain_id": 40,
        "native_chain_id": "4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11",
    },
    "telos-testnet": {
        "evm_chain_id": 41,
        "native_chain_id": "1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f",
    },
}
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_EVIDENCE_BYTES = 1024 * 1024
UINT32_MAX = 2**32 - 1
UINT64_MAX = 2**64 - 1
UINT256_MAX = 2**256 - 1


class RejectRedirects(urllib.request.HTTPRedirectHandler):
    """Keep evidence requests on the endpoint that the operator approved."""

    def redirect_request(self, request, file_pointer, code, message, headers, new_url):
        return None


HTTP_OPENER = urllib.request.build_opener(RejectRedirects)


def sha256_bytes(value: bytes) -> str:
    return "0x" + hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return "0x" + digest.hexdigest()


def canonical_digest(value: object) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return sha256_bytes(encoded)


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


def require_hex(value: object, length: int, label: str, *, prefix: bool = True) -> str:
    if not isinstance(value, str):
        raise SystemExit(f"{label} is not a string")
    normalized = value.lower()
    raw = normalized[2:] if normalized.startswith("0x") else normalized
    if prefix and not normalized.startswith("0x"):
        raise SystemExit(f"{label} must have a 0x prefix")
    if len(raw) != length * 2:
        raise SystemExit(f"{label} is not {length} bytes")
    try:
        bytes.fromhex(raw)
    except ValueError as error:
        raise SystemExit(f"{label} is not hexadecimal: {error}") from error
    return ("0x" if prefix else "") + raw


def quantity(value: object, label: str) -> int:
    if not isinstance(value, str) or not value.startswith("0x"):
        raise SystemExit(f"{label} is not a JSON-RPC quantity")
    digits = value[2:]
    if not digits or (len(digits) > 1 and digits.startswith("0")):
        raise SystemExit(f"{label} is not a canonical JSON-RPC quantity")
    try:
        parsed = int(digits, 16)
    except ValueError as error:
        raise SystemExit(f"invalid {label}: {error}") from error
    if parsed < 0:
        raise SystemExit(f"{label} is negative")
    return parsed


def require_int(value: object, label: str, *, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise SystemExit(f"{label} is outside {minimum}..={maximum}")
    return value


def native_block_number(block_id: str, label: str) -> int:
    raw = block_id.removeprefix("0x")
    return int.from_bytes(bytes.fromhex(raw[:8]), "big")


def validate_legacy_evidence(
    evidence: dict[str, object],
) -> tuple[str, dict[str, object], int, int, str, int]:
    if evidence.get("version") != 1 or evidence.get("legacy_source_commit") != (
        "8c37741ea8d97eba713a8028e3f09132bb51abd6"
    ):
        raise SystemExit("unsupported legacy evidence provenance")
    network_name = evidence.get("chain")
    if not isinstance(network_name, str) or network_name not in NETWORKS:
        raise SystemExit("legacy evidence identifies an unsupported network")

    block_number = require_int(
        evidence.get("block_number"),
        "legacy EVM anchor number",
        minimum=1,
        maximum=UINT64_MAX - 1,
    )
    native_number = require_int(
        evidence.get("native_block_number"),
        "legacy native anchor number",
        minimum=1,
        maximum=UINT32_MAX - 1,
    )
    require_hex(evidence.get("block_hash"), 32, "legacy EVM anchor hash")
    require_hex(evidence.get("parent_block_hash"), 32, "legacy EVM anchor parent hash")
    native_id = require_hex(evidence.get("native_block_id"), 32, "legacy native anchor ID")
    if native_block_number(native_id, "legacy native anchor ID") != native_number:
        raise SystemExit("legacy native anchor ID does not encode its block number")
    if evidence.get("body_transaction_count") != 0 or isinstance(
        evidence.get("body_transaction_count"), bool
    ):
        raise SystemExit("legacy EVM anchor is not transaction-free")

    gas_price = evidence.get("starting_child_gas_price")
    parsed_gas_price = quantity(gas_price, "legacy first-child gas price")
    if parsed_gas_price > UINT256_MAX:
        raise SystemExit("legacy first-child gas price is outside uint256")
    revision = require_int(
        evidence.get("starting_child_revision"),
        "legacy first-child revision",
        minimum=0,
        maximum=UINT64_MAX,
    )
    return (
        network_name,
        NETWORKS[network_name],
        block_number,
        native_number,
        hex(parsed_gas_price),
        revision,
    )


def validate_endpoint(raw: str, label: str) -> str:
    parsed = urllib.parse.urlsplit(raw)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise SystemExit(f"{label} must be an absolute HTTP(S) URL")
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise SystemExit(f"{label} must not contain credentials, query, or fragment")
    if parsed.path not in {"", "/"}:
        raise SystemExit(f"{label} must not contain a path")
    if parsed.scheme == "http":
        host = parsed.hostname
        try:
            port = parsed.port or 80
        except ValueError as error:
            raise SystemExit(f"{label} has an invalid port: {error}") from error
        try:
            loopback = ipaddress.ip_address(host).is_loopback
        except ValueError:
            loopback = host.lower() == "localhost"
            if loopback:
                for info in socket.getaddrinfo(host, port, type=socket.SOCK_STREAM):
                    if not ipaddress.ip_address(info[4][0]).is_loopback:
                        loopback = False
                        break
        if not loopback:
            raise SystemExit(f"plaintext {label} is allowed only on loopback")
    return raw.rstrip("/")


def post_json(url: str, payload: object) -> object:
    body = json.dumps(payload, separators=(",", ":")).encode()
    request = urllib.request.Request(
        url,
        data=body,
        headers={"content-type": "application/json", "accept": "application/json"},
        method="POST",
    )
    try:
        with HTTP_OPENER.open(request, timeout=20) as response:
            if response.status != 200:
                raise SystemExit(f"HTTP {response.status} from {url}")
            raw = response.read(MAX_RESPONSE_BYTES + 1)
    except urllib.error.HTTPError as error:
        raise SystemExit(f"HTTP {error.code} from {url}; redirects are not allowed") from error
    except (urllib.error.URLError, OSError) as error:
        raise SystemExit(f"request to {url} failed: {error}") from error
    if len(raw) > MAX_RESPONSE_BYTES:
        raise SystemExit(f"oversized response from {url}")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as error:
        raise SystemExit(f"invalid JSON from {url}: {error}") from error


def rpc(url: str, method: str, params: list[object]) -> object:
    response = post_json(
        url,
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params},
    )
    response_id = response.get("id") if isinstance(response, dict) else None
    if (
        not isinstance(response, dict)
        or isinstance(response_id, bool)
        or response_id != 1
        or "error" in response
    ):
        raise SystemExit(f"invalid {method} response: {response!r}")
    return response.get("result")


def verify_evm_anchor(block: object, evidence: dict[str, object]) -> dict[str, object]:
    if not isinstance(block, dict):
        raise SystemExit("reference EVM RPC has no checkpoint block")
    number = quantity(block.get("number"), "EVM anchor number")
    if number != evidence["block_number"]:
        raise SystemExit("reference EVM anchor number does not match legacy evidence")
    block_hash = require_hex(block.get("hash"), 32, "EVM anchor hash")
    if block_hash != str(evidence["block_hash"]).lower():
        raise SystemExit("reference EVM anchor hash does not match legacy evidence")
    parent_hash = require_hex(block.get("parentHash"), 32, "EVM anchor parent hash")
    if parent_hash != str(evidence["parent_block_hash"]).lower():
        raise SystemExit("reference EVM anchor parent does not match legacy evidence")
    if block.get("transactions") != []:
        raise SystemExit("reference EVM anchor is not transaction-free")
    expected = {
        "stateRoot": EMPTY_ROOT,
        "transactionsRoot": EMPTY_ROOT,
        "receiptsRoot": EMPTY_ROOT,
        "sha3Uncles": EMPTY_OMMERS,
        "logsBloom": ZERO_BLOOM,
        "gasUsed": "0x0",
    }
    for field, value in expected.items():
        if str(block.get(field)).lower() != value:
            raise SystemExit(f"reference EVM anchor {field} is not sparse-anchor safe")
    native_id = require_hex(block.get("extraData"), 32, "EVM anchor native block ID")
    if native_id != str(evidence["native_block_id"]).lower():
        raise SystemExit("reference EVM native block ID does not match legacy evidence")
    return {
        "number": number,
        "hash": block_hash,
        "parent_hash": parent_hash,
        "native_block_id": native_id,
        "response_sha256": canonical_digest(block),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--legacy-evidence", type=Path, required=True)
    parser.add_argument("--evm-rpc-url", required=True)
    parser.add_argument("--nodeos-url", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    if args.output.exists():
        raise SystemExit(f"refusing to overwrite {args.output}")
    evm_url = validate_endpoint(args.evm_rpc_url, "EVM RPC endpoint")
    nodeos_url = validate_endpoint(args.nodeos_url, "nodeos endpoint")
    evidence = load_json_object(args.legacy_evidence, "legacy evidence")
    network_name, network, block_number, native_number, starting_gas_price, starting_revision = (
        validate_legacy_evidence(evidence)
    )

    chain_id = quantity(rpc(evm_url, "eth_chainId", []), "EVM chain ID")
    if chain_id != network["evm_chain_id"]:
        raise SystemExit("reference EVM endpoint is on the wrong chain")
    evm_anchor_raw = rpc(evm_url, "eth_getBlockByNumber", [hex(block_number), False])
    evm_anchor = verify_evm_anchor(evm_anchor_raw, evidence)
    evm_child_raw = rpc(evm_url, "eth_getBlockByNumber", [hex(block_number + 1), False])
    if not isinstance(evm_child_raw, dict):
        raise SystemExit("reference EVM RPC has no first child block")
    if quantity(evm_child_raw.get("number"), "EVM child number") != block_number + 1:
        raise SystemExit("reference EVM child number mismatch")
    child_parent_hash = require_hex(evm_child_raw.get("parentHash"), 32, "EVM child parent")
    if child_parent_hash != evm_anchor["hash"]:
        raise SystemExit("reference EVM child does not extend the anchor")
    child_hash = require_hex(evm_child_raw.get("hash"), 32, "EVM child hash")
    if child_hash == "0x" + "00" * 32 or child_hash == evm_anchor["hash"]:
        raise SystemExit("reference EVM child hash is missing or reuses the anchor hash")
    child_native_id = require_hex(evm_child_raw.get("extraData"), 32, "EVM child native block ID")
    if native_block_number(child_native_id, "EVM child native block ID") != native_number + 1:
        raise SystemExit("reference EVM child native block ID is not the anchor successor")

    info = post_json(f"{nodeos_url}/v1/chain/get_info", {})
    if not isinstance(info, dict):
        raise SystemExit("invalid nodeos get_info response")
    native_chain_id = require_hex(info.get("chain_id"), 32, "native chain ID", prefix=False)
    if native_chain_id != network["native_chain_id"]:
        raise SystemExit("nodeos endpoint is on the wrong native chain")
    native_anchor = post_json(
        f"{nodeos_url}/v1/chain/get_block", {"block_num_or_id": native_number}
    )
    native_child = post_json(
        f"{nodeos_url}/v1/chain/get_block", {"block_num_or_id": native_number + 1}
    )
    if not isinstance(native_anchor, dict) or not isinstance(native_child, dict):
        raise SystemExit("invalid nodeos anchor/child response")
    anchor_id = require_hex(native_anchor.get("id"), 32, "native anchor ID", prefix=False)
    if native_block_number(anchor_id, "native anchor ID") != native_number:
        raise SystemExit("nodeos anchor ID does not encode its block number")
    if "0x" + anchor_id != evm_anchor["native_block_id"]:
        raise SystemExit("nodeos anchor ID does not match EVM extraData")
    if require_int(
        native_anchor.get("block_num"),
        "nodeos anchor number",
        minimum=1,
        maximum=UINT32_MAX - 1,
    ) != native_number:
        raise SystemExit("nodeos anchor number mismatch")
    anchor_previous = require_hex(
        native_anchor.get("previous"), 32, "native anchor previous", prefix=False
    )
    if native_block_number(anchor_previous, "native anchor previous") != native_number - 1:
        raise SystemExit("nodeos anchor previous ID does not encode the preceding block")
    child_id = require_hex(native_child.get("id"), 32, "native child ID", prefix=False)
    if native_block_number(child_id, "native child ID") != native_number + 1:
        raise SystemExit("nodeos child ID does not encode its block number")
    if "0x" + child_id != child_native_id:
        raise SystemExit("nodeos child ID does not match EVM child extraData")
    if require_int(
        native_child.get("block_num"),
        "nodeos child number",
        minimum=1,
        maximum=UINT32_MAX,
    ) != native_number + 1:
        raise SystemExit("nodeos child number mismatch")
    if (
        require_hex(native_child.get("previous"), 32, "native child previous", prefix=False)
        != anchor_id
    ):
        raise SystemExit("native child does not extend the native anchor")
    lib_number = info.get("last_irreversible_block_num")
    if (
        isinstance(lib_number, bool)
        or not isinstance(lib_number, int)
        or lib_number < native_number + 1
    ):
        raise SystemExit("native anchor and first child are not irreversible")
    lib_id = require_hex(
        info.get("last_irreversible_block_id"), 32, "last irreversible block ID", prefix=False
    )
    if native_block_number(lib_id, "last irreversible block ID") != lib_number:
        raise SystemExit("nodeos last irreversible block ID does not encode its reported number")

    output = {
        "version": 1,
        "chain": network_name,
        "legacy_evidence_sha256": sha256_file(args.legacy_evidence),
        "evm_chain_id": chain_id,
        "native_chain_id": native_chain_id,
        "evm_anchor": evm_anchor,
        "evm_first_child": {
            "number": block_number + 1,
            "hash": child_hash,
            "parent_hash": child_parent_hash,
            "native_block_id": child_native_id,
            "response_sha256": canonical_digest(evm_child_raw),
        },
        "native_anchor": {
            "number": native_number,
            "id": anchor_id,
            "previous": anchor_previous,
            "response_sha256": canonical_digest(native_anchor),
        },
        "native_first_child": {
            "number": native_number + 1,
            "id": child_id,
            "previous": anchor_id,
            "response_sha256": canonical_digest(native_child),
        },
        "observed_last_irreversible_block_num": lib_number,
        "observed_last_irreversible_block_id": lib_id,
        "nodeos_info_response_sha256": canonical_digest(info),
        "starting_child_gas_price": starting_gas_price,
        "starting_child_revision": starting_revision,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_suffix(".tmp")
    if temporary.exists():
        raise SystemExit(f"refusing to overwrite {temporary}")
    with temporary.open("x", encoding="utf-8") as stream:
        json.dump(output, stream, indent=2, sort_keys=True)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(args.output)
    print(f"native_anchor_attestation={args.output}")
    print(f"evm_anchor_hash={evm_anchor['hash']}")
    print(f"native_anchor_id={anchor_id}")
    print(f"evm_first_child_hash={output['evm_first_child']['hash']}")


if __name__ == "__main__":
    main()

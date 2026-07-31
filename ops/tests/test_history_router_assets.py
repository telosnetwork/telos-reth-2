#!/usr/bin/env python3
"""Focused invariants for retained-history release and operations assets."""

from pathlib import Path
from urllib.parse import urlparse
import re


ROOT = Path(__file__).resolve().parents[2]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def parse_environment(path: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in read(path).splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        assert separator and key and key not in values, f"invalid or duplicate environment key: {line}"
        values[key] = value
    return values


router_environment = parse_environment("ops/config/mainnet-router.env.example")
node_environment = parse_environment("ops/config/mainnet.env.example")
archive_environment = parse_environment("ops/config/mainnet-archive.env.example")
required_environment = {
    "TELOS_RPC_ROUTER_BINARY_SHA256",
    "TELOS_RPC_ROUTER_LISTEN",
    "TELOS_RPC_ROUTER_LIVE_URL",
    "TELOS_RPC_ROUTER_ARCHIVE_URL",
    "TELOS_RPC_ROUTER_LIVE_HISTORY_START",
    "TELOS_RPC_ROUTER_CHAIN_ID",
    "TELOS_RPC_ROUTER_ANCHOR_HASH",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_NUMBER",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_HASH",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_ADDRESS",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_BALANCE",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_TRANSACTION_HASH",
    "TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_ADDRESS",
    "TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT",
    "TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_VALUE",
    "TELOS_RPC_ROUTER_MAX_HEAD_LAG",
    "TELOS_RPC_ROUTER_MAX_REQUEST_BYTES",
    "TELOS_RPC_ROUTER_MAX_RESPONSE_BYTES",
    "TELOS_RPC_ROUTER_MAX_BATCH_LEN",
    "TELOS_RPC_ROUTER_MAX_CONNECTIONS",
    "TELOS_RPC_ROUTER_MAX_INFLIGHT",
    "TELOS_RPC_ROUTER_REQUEST_TIMEOUT_MS",
    "TELOS_RPC_ROUTER_BACKEND_TIMEOUT_MS",
    "RUST_LOG",
}
assert router_environment.keys() == required_environment

listen_host, listen_port = router_environment["TELOS_RPC_ROUTER_LISTEN"].rsplit(":", 1)
assert listen_host == "127.0.0.1"
assert listen_port.isdecimal() and 0 < int(listen_port) < 65536

backend_ports: set[int] = set()
for name in ("TELOS_RPC_ROUTER_LIVE_URL", "TELOS_RPC_ROUTER_ARCHIVE_URL"):
    parsed = urlparse(router_environment[name])
    assert parsed.scheme == "http" and parsed.hostname == "127.0.0.1"
    assert parsed.path == "/" and parsed.params == parsed.query == parsed.fragment == ""
    assert parsed.username is None and parsed.password is None and parsed.port is not None
    backend_ports.add(parsed.port)
assert len(backend_ports) == 2
assert int(listen_port) not in backend_ports
assert urlparse(router_environment["TELOS_RPC_ROUTER_LIVE_URL"]).port == int(
    node_environment["HTTP_PORT"]
)
assert urlparse(router_environment["TELOS_RPC_ROUTER_ARCHIVE_URL"]).port == 28545
assert archive_environment.keys() == {
    "ARCHIVE_BINARY",
    "ARCHIVE_BINARY_SHA256",
    "ARCHIVE_DATADIR",
    "ARCHIVE_CHAIN",
    "ARCHIVE_CHAIN_ID",
    "ARCHIVE_HTTP_PORT",
    "ARCHIVE_AUTHRPC_PORT",
    "ARCHIVE_P2P_PORT",
    "ARCHIVE_METRICS_PORT",
    "ARCHIVE_HTTP_API",
    "ARCHIVE_CONSENSUS_BINARY",
    "ARCHIVE_CONSENSUS_BINARY_SHA256",
    "ARCHIVE_CONSENSUS_CONFIG",
}
assert archive_environment["ARCHIVE_DATADIR"] == "/var/lib/telos-reth-archive/mainnet"
assert archive_environment["ARCHIVE_CHAIN"] == "tevmmainnet"
assert archive_environment["ARCHIVE_CHAIN_ID"] == "40"
assert int(archive_environment["ARCHIVE_HTTP_PORT"]) == urlparse(
    router_environment["TELOS_RPC_ROUTER_ARCHIVE_URL"]
).port
archive_ports = {
    int(archive_environment[name])
    for name in (
        "ARCHIVE_HTTP_PORT",
        "ARCHIVE_AUTHRPC_PORT",
        "ARCHIVE_P2P_PORT",
        "ARCHIVE_METRICS_PORT",
    )
}
assert len(archive_ports) == 4
assert node_environment["HTTP_API"] == "eth,net,web3"
assert node_environment["WS_API"] == "eth,net,web3"
assert f'127.0.0.1:{node_environment["METRICS_PORT"]}' in read(
    "ops/prometheus/scrape.example.yml"
)
assert "telos-(reth|rpc-router|consensus-client)" in read("ops/prometheus/alerts.yml")
side_by_side_ports = {
    int(node_environment["HTTP_PORT"]),
    int(node_environment["WS_PORT"]),
    int(node_environment["AUTHRPC_PORT"]),
    int(node_environment["METRICS_PORT"]),
    int(listen_port),
    urlparse(router_environment["TELOS_RPC_ROUTER_ARCHIVE_URL"]).port,
}
assert len(side_by_side_ports) == 6

assert router_environment["TELOS_RPC_ROUTER_LIVE_HISTORY_START"] == "479294328"
assert router_environment["TELOS_RPC_ROUTER_CHAIN_ID"] == "40"
assert router_environment["TELOS_RPC_ROUTER_ANCHOR_HASH"] == (
    "0x7d62876c8248867708f934b13184ff03440c2b4447a0434562c10bbc783bef51"
)
assert router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_NUMBER"] == "423015017"
assert router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_HASH"] == (
    "0x9af24c613ebf3ba3cbd8a29d9b4c24a0cf5589544a162dfe66c98f25a1ce55c0"
)
assert router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_ADDRESS"] == (
    "0x1a7883121285dfe08fb89763d084d5c7966dcf92"
)
assert router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_BALANCE"] == "0x23b0c973e84998e4f"
assert router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_TRANSACTION_HASH"] == (
    "0x411b585bf0b052f527b1924f500686d4b7c7cab9da18f81cbacfa4405bd15819"
)
assert router_environment["TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_ADDRESS"] == (
    "0xd102ce6a4db07d247fcc28f366a623df0938ca9e"
)
assert router_environment["TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT"] == "0x2"
assert router_environment["TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_VALUE"] == (
    "0x0000000000000000000000000000000000000000000000000000000000000012"
)
assert router_environment["TELOS_RPC_ROUTER_MAX_BATCH_LEN"] == "64"
assert router_environment["TELOS_RPC_ROUTER_MAX_CONNECTIONS"] == "256"
assert router_environment["TELOS_RPC_ROUTER_MAX_INFLIGHT"] == "16"
assert router_environment["TELOS_RPC_ROUTER_REQUEST_TIMEOUT_MS"] == "30000"
for name in (
    "TELOS_RPC_ROUTER_ANCHOR_HASH",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_HASH",
    "TELOS_RPC_ROUTER_HISTORY_PROBE_TRANSACTION_HASH",
):
    assert re.fullmatch(r"0x[0-9a-f]{64}", router_environment[name])
assert re.fullmatch(
    r"0x[0-9a-f]{40}", router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_ADDRESS"]
)
assert re.fullmatch(
    r"0x(?:0|[1-9a-f][0-9a-f]*)", router_environment["TELOS_RPC_ROUTER_HISTORY_PROBE_BALANCE"]
)
assert re.fullmatch(
    r"0x[0-9a-f]{40}",
    router_environment["TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_ADDRESS"],
)
assert re.fullmatch(
    r"0x[0-9a-f]{1,64}",
    router_environment["TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT"],
)
assert re.fullmatch(
    r"0x[0-9a-f]{64}",
    router_environment["TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_VALUE"],
)
assert (
    "d697207a9137973f2dd578ed6c157f1f6c07e644146c5e6a101e73fbeebd14ea"
    in read("ops/config/mainnet-router.env.example")
)

unit = read("ops/systemd/telos-rpc-router@.service")
for directive in (
    "ConditionFileIsExecutable=/usr/local/bin/telos-rpc-router",
    "User=telos-rpc-router",
    "Group=telos-rpc-router",
    "EnvironmentFile=/etc/telos-reth/%i/router.env",
    "ExecStartPre=/usr/local/libexec/telos-rpc-router-preflight %i",
    "ExecStart=/usr/local/bin/telos-rpc-router",
    "Requires=telos-reth@%i.service",
    "NoNewPrivileges=yes",
    "ProtectSystem=strict",
    "ProtectHome=yes",
    "PrivateDevices=yes",
    "PrivateMounts=yes",
    "ProtectProc=invisible",
    "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
    "IPAddressAllow=localhost",
    "IPAddressDeny=any",
    "RestrictNamespaces=yes",
    "MemoryDenyWriteExecute=yes",
    "CapabilityBoundingSet=",
    "SystemCallFilter=@system-service",
):
    assert directive in unit, f"missing router unit invariant: {directive}"
assert "0.0.0.0" not in unit and "[::]" not in unit
for unit_path, binary in (
    ("ops/systemd/telos-reth@.service", "telos-reth"),
    ("ops/systemd/telos-consensus-client@.service", "telos-consensus-client"),
    ("ops/systemd/telos-rpc-router@.service", "telos-rpc-router"),
):
    unit_text = read(unit_path)
    assert f"ConditionFileIsExecutable=/usr/local/bin/{binary}" in unit_text
    assert "ConditionPathIsExecutable" not in unit_text
assert 'u telos-rpc-router - "Telos retained-history RPC router"' in read(
    "ops/sysusers.d/telos-reth.conf"
)
assert 'u telos-reth-archive - "Telos retained-history backend"' in read(
    "ops/sysusers.d/telos-reth.conf"
)
assert 'u telos-reth-archive-consensus - "Telos retained-history companion"' in read(
    "ops/sysusers.d/telos-reth.conf"
)

archive_unit = read("ops/systemd/telos-reth-archive@.service")
archive_companion_unit = read("ops/systemd/telos-reth-archive-consensus@.service")
for directive in (
    "User=telos-reth-archive",
    "LoadCredential=archive-jwt.hex:/etc/telos-reth/%i/archive-jwt.hex",
    "ExecStart=/usr/local/libexec/telos-reth-archive-run %i %d/archive-jwt.hex",
    "IPAddressAllow=localhost",
    "IPAddressDeny=any",
    "ReadWritePaths=/var/lib/telos-reth-archive/%i",
):
    assert directive in archive_unit, f"missing archive unit invariant: {directive}"
for directive in (
    "Requires=telos-reth-archive@%i.service",
    "User=telos-reth-archive-consensus",
    "ExecStart=/usr/local/libexec/telos-reth-archive-consensus-run %i %d/archive-jwt.hex",
    "IPAddressAllow=localhost",
    "IPAddressDeny=any",
    "ReadWritePaths=/var/lib/telos-reth-archive-consensus/%i",
):
    assert directive in archive_companion_unit, (
        f"missing archive companion unit invariant: {directive}"
    )
archive_run = read("ops/scripts/telos-reth-archive-run")
archive_companion_run = read("ops/scripts/telos-reth-archive-consensus-run")
assert "ARCHIVE_BINARY_SHA256" in archive_run
assert "--disable-discovery" in archive_run
assert "--max-outbound-peers 0" in archive_run
assert "ARCHIVE_CONSENSUS_BINARY_SHA256" in archive_companion_run
assert "jwt_secret_path" in archive_companion_run

package = read("scripts/release/package-telos.sh")
verify = read("scripts/release/verify-assets.sh")
dockerfile = read("Dockerfile.telos")
release_workflow = read(".github/workflows/release.yml")
reproducible_workflow = read(".github/workflows/reproducible-build.yml")
telos_ci = read(".github/workflows/telos-ci.yml")
release_helper = read("ops/scripts/telos-reth-release")
router_preflight = read("ops/scripts/telos-rpc-router-preflight")
for content in (package, verify, dockerfile, release_workflow, reproducible_workflow):
    assert "telos-rpc-router" in content
assert "rpc_router_sha256=" in package
assert 'format_version=4' in package
assert 'grep -Fxq "format_version=4"' in verify
assert "rpc_router_sha256" in verify
assert "--package telos-rpc-router" in dockerfile
assert "--bin telos-rpc-router" in dockerfile
assert "/src/target/maxperf/telos-rpc-router /telos-rpc-router" in dockerfile
assert "python3 ops/tests/test_history_router_readiness.py" in telos_ci
for packaged_readiness_asset in (
    "ops/scripts/telos-rpc-router-readiness",
    "ops/systemd/telos-rpc-router-readiness@.service",
    "ops/systemd/telos-rpc-router-readiness@.timer",
):
    assert packaged_readiness_asset in telos_ci
    assert packaged_readiness_asset in verify
assert "router)" in release_helper
assert "active_link=/usr/local/bin/telos-rpc-router" in release_helper
assert "TELOS_RPC_ROUTER_BINARY_SHA256" in router_preflight
assert "/usr/local/lib/telos-rpc-router/releases" in router_preflight

history_doc = read("docs/telos/history-routing.md")
for statement in (
    "not a full archive",
    "independent archive",
    "exact qualified Telos public policy",
    "replay-unsafe methods fail",
    "filter lifecycle",
    "`eth_feeHistory`",
    "do not stop, replace, or delete the incumbent",
    "host firewall rules",
):
    assert statement in history_doc, f"missing history-boundary statement: {statement}"
for namespace in ("`eth`", "`net`", "`web3`"):
    assert namespace in history_doc

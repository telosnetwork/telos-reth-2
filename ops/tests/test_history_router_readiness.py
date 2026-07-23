#!/usr/bin/env python3
"""Focused behavioral and deployment checks for router readiness observability."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = ROOT / "ops/scripts/telos-rpc-router-readiness"
SERVICE_PATH = ROOT / "ops/systemd/telos-rpc-router-readiness@.service"
TIMER_PATH = ROOT / "ops/systemd/telos-rpc-router-readiness@.timer"
ALERTS_PATH = ROOT / "ops/prometheus/alerts.yml"

ANCHOR_HASH = "0x7d62876c8248867708f934b13184ff03440c2b4447a0434562c10bbc783bef51"
HISTORY_HASH = "0x9af24c613ebf3ba3cbd8a29d9b4c24a0cf5589544a162dfe66c98f25a1ce55c0"
HISTORY_ADDRESS = "0x1a7883121285dfe08fb89763d084d5c7966dcf92"
HISTORY_BALANCE = "0x23b0c973e84998e4f"
HISTORY_TRANSACTION_HASH = (
    "0x411b585bf0b052f527b1924f500686d4b7c7cab9da18f81cbacfa4405bd15819"
)
COMMON_HASH = "0x" + "ab" * 32
HISTORY_STORAGE_ADDRESS = "0xd102ce6a4db07d247fcc28f366a623df0938ca9e"
HISTORY_STORAGE_SLOT = "0x2"
HISTORY_STORAGE_VALUE = "0x" + "00" * 31 + "12"

EXPECTED_METRICS = {
    "telos_rpc_router_readiness",
    "telos_rpc_router_readiness_last_check_timestamp_seconds",
    "telos_rpc_router_readiness_last_success_timestamp_seconds",
    "telos_rpc_router_identity_match",
    "telos_rpc_router_live_head_block",
    "telos_rpc_router_archive_head_block",
    "telos_rpc_router_common_head_block",
    "telos_rpc_router_backend_head_lag_blocks",
}


def write_executable(path: Path, content: str) -> None:
    path.write_text(content, encoding="utf-8")
    path.chmod(0o755)


def router_config(listen: str = "127.0.0.1:8645") -> str:
    return "\n".join(
        (
            f"TELOS_RPC_ROUTER_LISTEN={listen}",
            "TELOS_RPC_ROUTER_CHAIN_ID=40",
            "TELOS_RPC_ROUTER_LIVE_HISTORY_START=479294328",
            f"TELOS_RPC_ROUTER_ANCHOR_HASH={ANCHOR_HASH}",
            "TELOS_RPC_ROUTER_HISTORY_PROBE_NUMBER=423015017",
            f"TELOS_RPC_ROUTER_HISTORY_PROBE_HASH={HISTORY_HASH}",
            f"TELOS_RPC_ROUTER_HISTORY_PROBE_ADDRESS={HISTORY_ADDRESS}",
            f"TELOS_RPC_ROUTER_HISTORY_PROBE_BALANCE={HISTORY_BALANCE}",
            f"TELOS_RPC_ROUTER_HISTORY_PROBE_TRANSACTION_HASH={HISTORY_TRANSACTION_HASH}",
            f"TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_ADDRESS={HISTORY_STORAGE_ADDRESS}",
            f"TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT={HISTORY_STORAGE_SLOT}",
            f"TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_VALUE={HISTORY_STORAGE_VALUE}",
            "TELOS_RPC_ROUTER_MAX_HEAD_LAG=4",
            "",
        )
    )


def ready_response(chain_id: int = 40) -> dict[str, object]:
    return {
        "ready": True,
        "chain_id": chain_id,
        "live_history_start": 479294328,
        "anchor_hash": ANCHOR_HASH,
        "history_probe_number": 423015017,
        "history_probe_hash": HISTORY_HASH,
        "history_probe_address": HISTORY_ADDRESS,
        "history_probe_balance": HISTORY_BALANCE,
        "history_probe_transaction_hash": HISTORY_TRANSACTION_HASH,
        "history_storage_probe_address": HISTORY_STORAGE_ADDRESS,
        "history_storage_probe_slot": HISTORY_STORAGE_SLOT,
        "history_storage_probe_value": HISTORY_STORAGE_VALUE,
        "live_head": 500,
        "archive_head": 498,
        "common_head": 498,
        "common_hash": COMMON_HASH,
    }


def parse_metrics(path: Path) -> dict[str, int]:
    samples: dict[str, int] = {}
    sample_pattern = re.compile(
        r'^(telos_rpc_router_[a-z0-9_]+)\{network="mainnet"\} ([0-9]+)$'
    )
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        match = sample_pattern.fullmatch(line)
        assert match is not None, f"unexpected metric sample: {line}"
        name, value = match.groups()
        assert name not in samples, f"duplicate metric sample: {name}"
        samples[name] = int(value)
    assert samples.keys() == EXPECTED_METRICS
    return samples


with tempfile.TemporaryDirectory(prefix="telos-router-readiness-") as temporary:
    test_root = Path(temporary).resolve()
    config_root = test_root / "etc/telos-reth"
    state_root = test_root / "var/lib/telos-reth-health"
    instance_config = config_root / "mainnet"
    metrics_dir = state_root / "metrics"
    state_dir = state_root / "router-mainnet"
    fake_bin = test_root / "bin"
    for directory in (instance_config, metrics_dir, state_dir, fake_bin):
        directory.mkdir(parents=True, exist_ok=True)

    config_path = instance_config / "router.env"
    response_path = test_root / "response.json"
    curl_marker = test_root / "curl-called"
    config_path.write_text(router_config(), encoding="utf-8")
    response_path.write_text(json.dumps(ready_response()), encoding="utf-8")

    source = SCRIPT_PATH.read_text(encoding="utf-8")
    source = source.replace(
        "config_root=/etc/telos-reth",
        f"config_root={shlex.quote(str(config_root))}",
        1,
    )
    source = source.replace(
        "state_root=/var/lib/telos-reth-health",
        f"state_root={shlex.quote(str(state_root))}",
        1,
    )
    test_script = test_root / "telos-rpc-router-readiness"
    write_executable(test_script, source)

    write_executable(
        fake_bin / "stat",
        """#!/usr/bin/env bash
set -euo pipefail
[[ $# -eq 3 && $1 == -c ]]
format=$2
path=$3
case "$format:$path" in
    "%U:%G %a:$TEST_ROOT/var/lib/telos-reth-health")
        echo "root:root 755"
        ;;
    "%U:%G %a:$TEST_ROOT/var/lib/telos-reth-health/router-mainnet")
        echo "telos-monitor:telos-monitor 700"
        ;;
    "%U:%G %a:$TEST_ROOT/var/lib/telos-reth-health/metrics")
        echo "telos-monitor:telos-monitor 755"
        ;;
    "%U:%G %a:$TEST_ROOT/etc/telos-reth/mainnet")
        echo "root:telos-reth-config 750"
        ;;
    "%U:%G %a:$TEST_ROOT/etc/telos-reth/mainnet/router.env")
        echo "root:telos-reth-config 440"
        ;;
    *)
        echo "unexpected stat call: $format $path" >&2
        exit 1
        ;;
esac
""",
    )
    write_executable(
        fake_bin / "realpath",
        """#!/usr/bin/env bash
set -euo pipefail
[[ $# -eq 2 && $1 == -e ]]
python3 - "$2" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
""",
    )
    write_executable(
        fake_bin / "systemctl",
        """#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == "is-active --quiet telos-rpc-router@mainnet.service" ]]
""",
    )
    write_executable(
        fake_bin / "flock",
        """#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == "-w 10 9" ]]
""",
    )
    write_executable(
        fake_bin / "curl",
        """#!/usr/bin/env bash
set -euo pipefail
output=
for ((index = 1; index <= $#; index++)); do
    if [[ ${!index} == --output ]]; then
        ((index += 1))
        output=${!index}
    fi
done
[[ -n $output ]]
[[ ${!#} == "http://127.0.0.1:8645/readyz" ]]
printf 'called\n' > "$ROUTER_TEST_CURL_MARKER"
[[ ${ROUTER_TEST_CURL_FAIL:-0} == 0 ]]
cp "$ROUTER_TEST_RESPONSE" "$output"
printf '%s' "${ROUTER_TEST_HTTP_STATUS:-200}"
""",
    )

    environment = os.environ.copy()
    environment.update(
        {
            "PATH": f"{fake_bin}:/usr/bin:/bin",
            "TEST_ROOT": str(test_root),
            "ROUTER_TEST_RESPONSE": str(response_path),
            "ROUTER_TEST_CURL_MARKER": str(curl_marker),
        }
    )

    syntax = subprocess.run(
        ["bash", "-n", str(test_script)],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert syntax.returncode == 0, syntax.stderr

    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode == 0, result.stderr
    assert "ready at common block 498" in result.stderr
    assert curl_marker.exists()
    metrics_file = metrics_dir / "telos_rpc_router_mainnet.prom"
    assert metrics_file.stat().st_mode & 0o777 == 0o644
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 1
    assert metrics["telos_rpc_router_identity_match"] == 1
    assert metrics["telos_rpc_router_live_head_block"] == 500
    assert metrics["telos_rpc_router_archive_head_block"] == 498
    assert metrics["telos_rpc_router_common_head_block"] == 498
    assert metrics["telos_rpc_router_backend_head_lag_blocks"] == 2
    assert metrics["telos_rpc_router_readiness_last_check_timestamp_seconds"] > 0
    previous_success = metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"]
    assert previous_success > 0
    assert not list(metrics_dir.glob(".telos_rpc_router_mainnet.*"))

    non_200_environment = environment.copy()
    non_200_environment["ROUTER_TEST_HTTP_STATUS"] = "302"
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=non_200_environment,
    )
    assert result.returncode != 0
    assert "did not return HTTP 200" in result.stderr
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 0
    assert metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"] == previous_success

    response_path.write_text(json.dumps(ready_response(chain_id=41)), encoding="utf-8")
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode != 0
    assert "identity validation failed" in result.stderr
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 0
    assert metrics["telos_rpc_router_identity_match"] == 0
    assert metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"] == previous_success
    assert not list(metrics_dir.glob(".telos_rpc_router_mainnet.*"))

    invalid_history_response = ready_response()
    invalid_history_response["history_probe_balance"] = "0x1"
    response_path.write_text(json.dumps(invalid_history_response), encoding="utf-8")
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode != 0
    assert "history probe balance differs" in result.stderr
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 0
    assert metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"] == previous_success

    invalid_storage_response = ready_response()
    invalid_storage_response["history_storage_probe_value"] = "0x" + "01" * 32
    response_path.write_text(json.dumps(invalid_storage_response), encoding="utf-8")
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode != 0
    assert "history storage probe value differs" in result.stderr
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 0
    assert metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"] == previous_success

    response_path.write_text(json.dumps({"ready": False}), encoding="utf-8")
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode != 0
    assert "router response is not ready" in result.stderr
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 0
    assert metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"] == previous_success

    curl_marker.unlink()
    invalid_storage_config = router_config().replace(
        f"TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT={HISTORY_STORAGE_SLOT}",
        "TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT=0x",
    )
    config_path.write_text(invalid_storage_config, encoding="utf-8")
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode != 0
    assert "TELOS_RPC_ROUTER_HISTORY_STORAGE_PROBE_SLOT is malformed" in result.stderr
    assert not curl_marker.exists()

    config_path.write_text(router_config(listen="0.0.0.0:8645"), encoding="utf-8")
    result = subprocess.run(
        [str(test_script), "mainnet"],
        check=False,
        capture_output=True,
        text=True,
        env=environment,
    )
    assert result.returncode != 0
    assert "not loopback" in result.stderr
    assert not curl_marker.exists()
    metrics = parse_metrics(metrics_file)
    assert metrics["telos_rpc_router_readiness"] == 0
    assert metrics["telos_rpc_router_readiness_last_success_timestamp_seconds"] == previous_success


script = SCRIPT_PATH.read_text(encoding="utf-8")
for invariant in (
    "--proxy ''",
    "--proto '=http'",
    "--max-filesize 1048576",
    "--write-out '%{http_code}'",
    '[[ $http_status == 200 ]]',
    'mktemp "${metrics_dir}/.telos_rpc_router_${instance}.XXXXXX"',
    'mv -f "$tmp" "$metrics_file"',
    'systemctl is-active --quiet "telos-rpc-router@${instance}.service"',
):
    assert invariant in script, f"missing readiness invariant: {invariant}"

service = SERVICE_PATH.read_text(encoding="utf-8")
for directive in (
    "User=telos-monitor",
    "Group=telos-monitor",
    "SupplementaryGroups=telos-reth-config",
    "ExecStart=/usr/local/libexec/telos-rpc-router-readiness %i",
    "StateDirectory=telos-reth-health/router-%i",
    "NoNewPrivileges=yes",
    "ProtectSystem=strict",
    "ProtectHome=yes",
    "PrivateDevices=yes",
    "ProtectProc=invisible",
    "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
    "IPAddressAllow=localhost",
    "IPAddressDeny=any",
    "RestrictNamespaces=yes",
    "MemoryDenyWriteExecute=yes",
    "CapabilityBoundingSet=",
    "SystemCallFilter=@system-service",
    "ReadOnlyPaths=/etc/telos-reth",
    "ReadWritePaths=/var/lib/telos-reth-health/router-%i /var/lib/telos-reth-health/metrics",
):
    assert directive in service, f"missing service hardening: {directive}"

timer = TIMER_PATH.read_text(encoding="utf-8")
for directive in (
    "OnUnitActiveSec=30s",
    "Unit=telos-rpc-router-readiness@%i.service",
    "WantedBy=timers.target",
):
    assert directive in timer, f"missing timer invariant: {directive}"

alerts = ALERTS_PATH.read_text(encoding="utf-8")
for alert in (
    "TelosRpcRouterNotReady",
    "TelosRpcRouterReadinessCheckMissing",
    "TelosRpcRouterReadinessMetricsMissing",
):
    alert_block = alerts.split(f"- alert: {alert}", 1)[1].split("- alert:", 1)[0]
    assert "severity: critical" in alert_block
assert "telos_rpc_router_readiness == 0" in alerts
assert "time() - telos_rpc_router_readiness_last_check_timestamp_seconds > 90" in alerts
assert 'name=~"telos-rpc-router-readiness@.+\\\\.timer"' in alerts

#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source=../scripts/telos-reth-snapshot
source "$repo_root/ops/scripts/telos-reth-snapshot"

fail() {
    echo "snapshot data verification test: $*" >&2
    exit 1
}

captured_cache=
captured_calls=0
captured_args=()
fake_status=0

fake_restic() {
    captured_cache=${RESTIC_CACHE_DIR:-}
    captured_calls=$((captured_calls + 1))
    captured_args=("$@")
    return "$fake_status"
}

assert_valid_invocation() {
    local percent=$1 expected_subset="--read-data-subset=${1}%" index
    local -a expected_args
    captured_cache=
    captured_calls=0
    captured_args=()
    fake_status=0
    run_restic_data_check fake_restic /credentials/repository /credentials/password \
        /var/cache/telos-reth-backup/mainnet "$percent" ||
        fail "valid subset $percent was rejected"
    [[ $captured_cache == /var/cache/telos-reth-backup/mainnet ]] ||
        fail "RESTIC_CACHE_DIR was not scoped to the data check"
    [[ $captured_calls == 1 ]] || fail "restic was called $captured_calls times"
    expected_args=(
        --repository-file /credentials/repository
        --password-file /credentials/password
        check "$expected_subset"
    )
    [[ ${#captured_args[@]} == ${#expected_args[@]} ]] || fail "unexpected restic argument count"
    for index in "${!expected_args[@]}"; do
        [[ ${captured_args[index]} == "${expected_args[index]}" ]] ||
            fail "restic argument $index is ${captured_args[index]}, expected ${expected_args[index]}"
    done
}

assert_valid_invocation 1
assert_valid_invocation 10

for invalid in '' 0 01 11 1% 1.5 1/2 -1 arbitrary; do
    captured_calls=0
    if run_restic_data_check fake_restic /credentials/repository /credentials/password \
        /var/cache/telos-reth-backup/mainnet "$invalid"; then
        fail "invalid subset '$invalid' was accepted"
    fi
    [[ $captured_calls == 0 ]] || fail "invalid subset '$invalid' invoked restic"
done

fake_status=42
if run_restic_data_check fake_restic /credentials/repository /credentials/password \
    /var/cache/telos-reth-backup/mainnet 1; then
    fail "restic data-read failure was not propagated"
else
    status=$?
fi
[[ $captured_calls == 1 ]] || fail "failed restic check was not invoked exactly once"
[[ $status == 42 ]] || fail "restic failure status changed from 42 to $status"

echo "snapshot data verification test: passed"

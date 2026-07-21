#!/usr/bin/env bash
set -euo pipefail
umask 077

[[ $EUID -eq 0 ]] || {
    echo "restore integration test must run as root" >&2
    exit 1
}
[[ ${TELOS_RESTORE_INTEGRATION_DISPOSABLE_CI:-} == 1 ]] || {
    echo "restore integration test is restricted to an explicitly marked disposable CI host" >&2
    exit 1
}

test_root=$(mktemp -d /var/tmp/telos-reth-restore-test.XXXXXX)
instance=ci-restore-$$
execution_dir=/var/lib/telos-reth/${instance}
consensus_dir=/var/lib/telos-consensus/${instance}
config_dir=/etc/telos-reth/${instance}
snapshot_root=${test_root}/snapshots
state_dir=/var/lib/telos-reth-backup/${instance}
permit=/run/telos-reth-restore/${instance}.permit
execution_binary=/usr/local/bin/telos-reth
consensus_binary=/usr/local/bin/telos-consensus-client
binding_binary=/usr/local/libexec/telos-reth-consensus-binding
restore_script=$(realpath "${BASH_SOURCE[0]%/*}/../scripts/telos-reth-restore")
mock_bin=${test_root}/bin
systemctl_state=${test_root}/systemctl-state
systemctl_log=${test_root}/systemctl.log
mv_counter=${test_root}/mv-counter
consensus_binary_preserved=0
binding_binary_preserved=0
test_paths_owned=0
mkdir -p "$mock_bin" "$systemctl_state"

assert_test_paths_safe() {
    [[ $instance =~ ^ci-restore-[1-9][0-9]*$ &&
       $test_root == /var/tmp/telos-reth-restore-test.* &&
       -d $test_root && ! -L $test_root && $(realpath -e "$test_root") == "$test_root" &&
       $execution_dir == "/var/lib/telos-reth/${instance}" &&
       $consensus_dir == "/var/lib/telos-consensus/${instance}" &&
       $config_dir == "/etc/telos-reth/${instance}" &&
       $snapshot_root == "${test_root}/snapshots" &&
       $state_dir == "/var/lib/telos-reth-backup/${instance}" &&
       $permit == "/run/telos-reth-restore/${instance}.permit" ]]
}

assert_test_paths_safe || {
    echo "restore integration test resolved an unsafe test path" >&2
    exit 1
}

preserve_file() {
    local path=$1 name=$2
    if [[ -e $path || -L $path ]]; then
        cp -a --no-dereference "$path" "${test_root}/${name}"
        printf '1\n' > "${test_root}/${name}.present"
    else
        printf '0\n' > "${test_root}/${name}.present"
    fi
}

restore_file() {
    local path=$1 name=$2
    rm -f -- "$path" || return 1
    if [[ $(<"${test_root}/${name}.present") == 1 ]]; then
        cp -a --no-dereference "${test_root}/${name}" "$path" || return 1
    fi
}

remove_restore_objects() {
    assert_test_paths_safe || return 1
    rm -rf -- "$execution_dir" "$consensus_dir" "$state_dir"
    rm -f -- "$permit" \
        "${config_dir}/checkpoint.json" \
        "${config_dir}/checkpoint.audit.json" \
        "${config_dir}/checkpoint.anchor.json"
    local parent
    for parent in /var/lib/telos-reth /var/lib/telos-consensus; do
        if [[ -d $parent && ! -L $parent ]]; then
            find "$parent" -mindepth 1 -maxdepth 1 \
                \( -name "${instance}.restore-*.partial" -o \
                   -name "${instance}.pre-restore-*" -o \
                   -name "${instance}.failed-restore-*" \) \
                -exec rm -rf -- {} +
        fi
    done
    if [[ -d $config_dir && ! -L $config_dir ]]; then
        find "$config_dir" -mindepth 1 -maxdepth 1 \
            \( -name 'checkpoint.json.restore-*.partial' -o \
               -name 'checkpoint.audit.json.restore-*.partial' -o \
               -name 'checkpoint.anchor.json.restore-*.partial' -o \
               -name 'checkpoint.json.pre-restore-*' -o \
               -name 'checkpoint.audit.json.pre-restore-*' -o \
               -name 'checkpoint.anchor.json.pre-restore-*' -o \
               -name 'checkpoint.json.failed-restore-*' -o \
               -name 'checkpoint.audit.json.failed-restore-*' -o \
               -name 'checkpoint.anchor.json.failed-restore-*' \) \
            -exec rm -f -- {} +
    fi
    return 0
}

cleanup() {
    local rc=$? cleanup_failed=0
    trap - EXIT
    set +e
    if (( test_paths_owned == 1 )); then
        remove_restore_objects || cleanup_failed=1
        rm -rf -- "$config_dir" "$snapshot_root" || cleanup_failed=1
    fi
    if (( consensus_binary_preserved == 1 )); then
        if restore_file "$consensus_binary" consensus-binary; then
            consensus_binary_preserved=0
        else
            echo "CRITICAL: could not restore $consensus_binary" >&2
            cleanup_failed=1
        fi
    fi
    if (( binding_binary_preserved == 1 )); then
        if restore_file "$binding_binary" binding-binary; then
            binding_binary_preserved=0
        else
            echo "CRITICAL: could not restore $binding_binary" >&2
            cleanup_failed=1
        fi
    fi
    if (( cleanup_failed == 0 )); then
        if ! rm -rf -- "$test_root"; then
            echo "restore integration could not remove $test_root" >&2
            (( rc != 0 )) || rc=1
        fi
    else
        echo "restore integration cleanup was incomplete; retained $test_root" >&2
        (( rc != 0 )) || rc=1
    fi
    exit "$rc"
}
trap cleanup EXIT

for account in telos-reth telos-consensus; do
    getent passwd "$account" >/dev/null || {
        echo "required test account is missing: $account" >&2
        exit 1
    }
done
getent group telos-reth-config >/dev/null || {
    echo "required test group is missing: telos-reth-config" >&2
    exit 1
}
[[ -x $execution_binary ]] || {
    echo "the CI execution fixture must be installed at $execution_binary" >&2
    exit 1
}
[[ -x $consensus_binary ]] &&
    cmp -s "$execution_binary" /bin/true &&
    cmp -s "$consensus_binary" /bin/true || {
    echo "restore integration test requires the disposable /bin/true release fixtures" >&2
    exit 1
}

for parent in /var/lib /var/lib/telos-reth /var/lib/telos-consensus \
    /var/lib/telos-reth-backup /etc /etc/telos-reth /usr/local /usr/local/bin \
    /usr/local/libexec; do
    if [[ -e $parent || -L $parent ]]; then
        [[ -d $parent && ! -L $parent && $(realpath -e "$parent") == "$parent" &&
           $(stat -c '%U' "$parent") == root ]] || {
            echo "refusing unsafe restore integration parent: $parent" >&2
            exit 1
        }
    fi
done

for path in "$execution_dir" "$consensus_dir" "$config_dir" "$state_dir" "$permit"; do
    if [[ -e $path || -L $path ]]; then
        echo "refusing to reuse an existing restore integration path: $path" >&2
        exit 1
    fi
done
for parent in /var/lib/telos-reth /var/lib/telos-consensus; do
    if [[ -d $parent &&
          -n $(find "$parent" -mindepth 1 -maxdepth 1 \
              \( -name "${instance}.restore-*.partial" -o \
                 -name "${instance}.pre-restore-*" -o \
                 -name "${instance}.failed-restore-*" \) -print -quit) ]]; then
        echo "refusing to reuse restore integration artifacts for $instance" >&2
        exit 1
    fi
done
test_paths_owned=1

[[ ! -d $consensus_binary && ! -d $binding_binary ]] || {
    echo "restore integration binary fixtures must not be directories" >&2
    exit 1
}
preserve_file "$consensus_binary" consensus-binary
consensus_binary_preserved=1
preserve_file "$binding_binary" binding-binary
binding_binary_preserved=1
rm -f -- "$consensus_binary" "$binding_binary"
install -d -m 0755 /usr/local/bin /usr/local/libexec
cat > "$consensus_binary" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' 'telos-consensus-client 0.1.0-ci-restore'
EOF
chmod 0755 "$consensus_binary"
cat > "$binding_binary" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ $# -eq 11 ]]
[[ $1 == "telos-consensus-client@${TELOS_RESTORE_TEST_INSTANCE:?}.service" ]]
[[ $2 == /usr/local/bin/telos-consensus-client ]]
[[ $3 == "/etc/telos-reth/${TELOS_RESTORE_TEST_INSTANCE}/consensus.toml" ]]
[[ $4 == "${TELOS_RESTORE_TEST_CONSENSUS_DIR:?}" ]]
[[ $5 == 40 && $6 == 18551 && $7 == http://127.0.0.1:8888 ]]
[[ ${10} == "$TELOS_RESTORE_TEST_INSTANCE" && ${11} == */checkpoint/checkpoint.json ]]
printf '%s\n' "$*" >> "${TELOS_RESTORE_TEST_BINDING_LOG:?}"
EOF
chmod 0755 "$binding_binary"

cat > "${mock_bin}/systemctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state_dir=${TELOS_RESTORE_TEST_SYSTEMCTL_STATE:?}
log=${TELOS_RESTORE_TEST_SYSTEMCTL_LOG:?}
printf '%q ' "$@" >> "$log"
printf '\n' >> "$log"

state_path() {
    local unit=$1
    printf '%s/%s\n' "$state_dir" "${unit//[^a-zA-Z0-9_-]/_}"
}

read_state() {
    local path
    path=$(state_path "$1")
    [[ -f $path ]] && cat "$path" || printf 'inactive\n'
}

write_state() {
    printf '%s\n' "$2" > "$(state_path "$1")"
}

command=$1
shift
case "$command" in
    is-active)
        [[ ${1:-} == --quiet ]] && shift
        [[ $(read_state "$1") == active ]]
        ;;
    stop)
        for unit in "$@"; do
            write_state "$unit" inactive
        done
        ;;
    start)
        [[ ${1:-} == --wait ]] && shift
        for unit in "$@"; do
            write_state "$unit" active
        done
        ;;
    reset-failed)
        ;;
    show)
        unit=$1
        shift
        property=
        for argument in "$@"; do
            [[ $argument == --property=* ]] && property=${argument#--property=}
        done
        state=$(read_state "$unit")
        case "$property" in
            ActiveState) printf '%s\n' "$state" ;;
            MainPID|ControlPID)
                [[ $state == active ]] && printf '4242\n' || printf '0\n'
                ;;
            *) printf '\n' ;;
        esac
        ;;
    *)
        echo "unexpected mocked systemctl command: $command" >&2
        exit 2
        ;;
esac
EOF
chmod 0755 "${mock_bin}/systemctl"

cat > "${mock_bin}/mv" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${TELOS_RESTORE_TEST_CRASH_PUBLISH_AT:-0} =~ ^[1-9][0-9]*$ &&
      $# -eq 2 && ${1:-} == *.restore-*.partial &&
      ( ${2:-} == "${TELOS_RESTORE_TEST_EXECUTION_DIR:?}" ||
        ${2:-} == "${TELOS_RESTORE_TEST_CONSENSUS_DIR:?}" ||
        ${2:-} == "${TELOS_RESTORE_TEST_CHECKPOINT_MANIFEST:?}" ||
        ${2:-} == "${TELOS_RESTORE_TEST_CHECKPOINT_AUDIT:?}" ||
        ${2:-} == "${TELOS_RESTORE_TEST_EXECUTION_ANCHOR:?}" ) ]]; then
    count=0
    [[ -f ${TELOS_RESTORE_TEST_MV_COUNTER:?} ]] && count=$(<"$TELOS_RESTORE_TEST_MV_COUNTER")
    count=$((count + 1))
    printf '%s\n' "$count" > "$TELOS_RESTORE_TEST_MV_COUNTER"
    if (( count == TELOS_RESTORE_TEST_CRASH_PUBLISH_AT )); then
        kill -KILL "$PPID"
        exit 137
    fi
fi
exec /usr/bin/mv "$@"
EOF
chmod 0755 "${mock_bin}/mv"

export PATH="${mock_bin}:/usr/sbin:/usr/bin:/sbin:/bin"
export TELOS_RESTORE_TEST_SYSTEMCTL_STATE=$systemctl_state
export TELOS_RESTORE_TEST_SYSTEMCTL_LOG=$systemctl_log
export TELOS_RESTORE_TEST_BINDING_LOG=${test_root}/binding.log
export TELOS_RESTORE_TEST_MV_COUNTER=$mv_counter
export TELOS_RESTORE_TEST_INSTANCE=$instance
export TELOS_RESTORE_TEST_EXECUTION_DIR=$execution_dir
export TELOS_RESTORE_TEST_CONSENSUS_DIR=$consensus_dir
export TELOS_RESTORE_TEST_CHECKPOINT_MANIFEST=${config_dir}/checkpoint.json
export TELOS_RESTORE_TEST_CHECKPOINT_AUDIT=${config_dir}/checkpoint.audit.json
export TELOS_RESTORE_TEST_EXECUTION_ANCHOR=${config_dir}/checkpoint.anchor.json

install -d -o root -g root -m 0755 /var/lib/telos-reth /var/lib/telos-consensus
install -d -o root -g telos-reth-config -m 0750 "$config_dir"
install -d -o root -g root -m 0755 "$snapshot_root/${instance}"
printf '%s\n' 'test consensus configuration' > "${config_dir}/consensus.toml"
chown root:telos-reth-config "${config_dir}/consensus.toml"
chmod 0440 "${config_dir}/consensus.toml"

execution_digest=$(sha256sum "$execution_binary" | awk '{print $1}')
consensus_digest=$(sha256sum "$consensus_binary" | awk '{print $1}')
consensus_config_digest=$(sha256sum "${config_dir}/consensus.toml" | awk '{print $1}')
checkpoint_digest=$(printf '%s\n' '{"checkpoint":"new"}' | sha256sum | awk '{print $1}')
anchor_hash=0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

cat > "${config_dir}/node.env" <<EOF
CHAIN_ID=40
AUTHRPC_PORT=18551
NODEOS_URL=http://127.0.0.1:8888
CONSENSUS_UNIT=telos-consensus-client@${instance}.service
CONSENSUS_BINARY=${consensus_binary}
CONSENSUS_CONFIG=${config_dir}/consensus.toml
CONSENSUS_VERSION='telos-consensus-client 0.1.0-ci-restore'
CONSENSUS_SHA256=${consensus_digest}
BINARY_SHA256=${execution_digest}
CHECKPOINT_MANIFEST=${config_dir}/checkpoint.json
CHECKPOINT_MANIFEST_SHA256=${checkpoint_digest}
CHECKPOINT_AUDIT=${config_dir}/checkpoint.audit.json
EXECUTION_ANCHOR=${config_dir}/checkpoint.anchor.json
EXECUTION_ANCHOR_BLOCK_NUMBER=479294328
EXECUTION_ANCHOR_BLOCK_HASH=${anchor_hash}
EOF
chown root:telos-reth-config "${config_dir}/node.env"
chmod 0440 "${config_dir}/node.env"

cat > "${config_dir}/backup.env" <<EOF
CONSENSUS_DATA_DIR=${consensus_dir}
SNAPSHOT_ROOT=${snapshot_root}
RESTORE_HEALTH_TIMEOUT_SECONDS=1
EOF
chown root:root "${config_dir}/backup.env"
chmod 0600 "${config_dir}/backup.env"

snapshot_name=20260721T000000Z-${execution_digest:0:12}
snapshot=${snapshot_root}/${instance}/${snapshot_name}
mkdir -p "$snapshot/execution" "$snapshot/consensus" "$snapshot/checkpoint"
printf '%s\n' new-execution > "$snapshot/execution/marker"
printf '%s\n' new-consensus > "$snapshot/consensus/marker"
printf '%s\n' '{"checkpoint":"new"}' > "$snapshot/checkpoint/checkpoint.json"
printf '%s\n' '{"audit":"new"}' > "$snapshot/checkpoint/checkpoint.audit.json"
printf '%s\n' '{"anchor":"new"}' > "$snapshot/checkpoint/checkpoint.anchor.json"
checkpoint_audit_digest=$(sha256sum "$snapshot/checkpoint/checkpoint.audit.json" | awk '{print $1}')
checkpoint_anchor_digest=$(sha256sum "$snapshot/checkpoint/checkpoint.anchor.json" | awk '{print $1}')
jq -n \
    --arg instance "$instance" \
    --arg execution_digest "$execution_digest" \
    --arg consensus_digest "$consensus_digest" \
    --arg consensus_version 'telos-consensus-client 0.1.0-ci-restore' \
    --arg consensus_config_digest "$consensus_config_digest" \
    --arg checkpoint_digest "$checkpoint_digest" \
    --arg checkpoint_audit_digest "$checkpoint_audit_digest" \
    --arg checkpoint_anchor_digest "$checkpoint_anchor_digest" \
    '{schema:"telos-reth-snapshot/v2",instance:$instance,chain_id:40,
      data_layout:{execution:{owner:"telos-reth",group:"telos-reth"},
                   consensus:{owner:"telos-consensus",group:"telos-consensus"}},
      binary:{sha256:$execution_digest},
      consensus_companion:{sha256:$consensus_digest,version:$consensus_version,
                           config_sha256:$consensus_config_digest},
      checkpoint:{manifest_sha256:$checkpoint_digest,audit_sha256:$checkpoint_audit_digest,
                  execution_anchor_sha256:$checkpoint_anchor_digest}}' \
    > "$snapshot/manifest.json"
(
    cd "$snapshot"
    find manifest.json execution consensus checkpoint -xdev -type f -print0 \
        | LC_ALL=C sort -z \
        | xargs -0 sha256sum > SHA256SUMS
)

reset_systemctl_state() {
    rm -f "$systemctl_state"/* "$systemctl_log" "$mv_counter"
    printf '%s\n' active > "$systemctl_state/telos-reth_${instance}_service"
    printf '%s\n' active > "$systemctl_state/telos-consensus-client_${instance}_service"
    printf '%s\n' active > "$systemctl_state/telos-reth-readiness_${instance}_timer"
    printf '%s\n' inactive > "$systemctl_state/telos-reth-readiness_${instance}_service"
}

create_originals() {
    install -d -o telos-reth -g telos-reth -m 0750 "$execution_dir"
    install -d -o telos-consensus -g telos-consensus -m 0750 "$consensus_dir"
    printf '%s\n' old-execution > "$execution_dir/marker"
    printf '%s\n' old-consensus > "$consensus_dir/marker"
    chown telos-reth:telos-reth "$execution_dir/marker"
    chown telos-consensus:telos-consensus "$consensus_dir/marker"
    printf '%s\n' '{"checkpoint":"old"}' > "${config_dir}/checkpoint.json"
    printf '%s\n' '{"audit":"old"}' > "${config_dir}/checkpoint.audit.json"
    printf '%s\n' '{"anchor":"old"}' > "${config_dir}/checkpoint.anchor.json"
    chown root:telos-reth-config \
        "${config_dir}/checkpoint.json" \
        "${config_dir}/checkpoint.audit.json" \
        "${config_dir}/checkpoint.anchor.json"
    chmod 0440 \
        "${config_dir}/checkpoint.json" \
        "${config_dir}/checkpoint.audit.json" \
        "${config_dir}/checkpoint.anchor.json"
}

# Kill the restore process after two live objects have been published. SIGKILL bypasses every
# shell trap and leaves the durable pending journal as it would after a host/process crash.
remove_restore_objects
create_originals
reset_systemctl_state
set +e
TELOS_RESTORE_TEST_CRASH_PUBLISH_AT=3 \
    "$restore_script" "$instance" "$snapshot" --confirm
crash_status=$?
set -e
(( crash_status == 137 )) || {
    echo "injected restore crash returned $crash_status instead of SIGKILL status 137" >&2
    exit 1
}
jq -e '.status == "pending" and .had_original == "11111"' \
    "$state_dir/restore.transaction.json" >/dev/null
[[ $(<"$mv_counter") == 3 ]]
restore_id=$(jq -r .restore_id "$state_dir/restore.transaction.json")
[[ $(<"$execution_dir/marker") == new-execution ]]
[[ $(<"$consensus_dir/marker") == new-consensus ]]
[[ $(<"${execution_dir}.pre-restore-${restore_id}/marker") == old-execution ]]
[[ $(<"${consensus_dir}.pre-restore-${restore_id}/marker") == old-consensus ]]
[[ ! -e ${config_dir}/checkpoint.json && ! -L ${config_dir}/checkpoint.json ]]
grep -q '"checkpoint":"old"' "${config_dir}/checkpoint.json.pre-restore-${restore_id}"
grep -q '"checkpoint":"new"' "${config_dir}/checkpoint.json.restore-${restore_id}.partial"
grep -q '"audit":"old"' "${config_dir}/checkpoint.audit.json"
grep -q '"anchor":"old"' "${config_dir}/checkpoint.anchor.json"

# A second restore must respect the pending journal fence and leave the interrupted set untouched.
set +e
"$restore_script" "$instance" "$snapshot" --confirm
pending_restore_status=$?
set -e
(( pending_restore_status != 0 ))
jq -e '.status == "pending"' "$state_dir/restore.transaction.json" >/dev/null
[[ $(<"$mv_counter") == 3 ]]

"$restore_script" "$instance" --recover
jq -e '.status == "rolled_back"' "$state_dir/restore.transaction.json" >/dev/null
[[ $(<"$execution_dir/marker") == old-execution ]]
[[ $(<"$consensus_dir/marker") == old-consensus ]]
grep -q '"checkpoint":"old"' "${config_dir}/checkpoint.json"
[[ $(<"${execution_dir}.failed-restore-${restore_id}/marker") == new-execution ]]
[[ $(<"${consensus_dir}.failed-restore-${restore_id}/marker") == new-consensus ]]
grep -q '"checkpoint":"new"' "${config_dir}/checkpoint.json.restore-${restore_id}.partial"
[[ $(<"$systemctl_state/telos-reth_${instance}_service") == active ]]
[[ $(<"$systemctl_state/telos-consensus-client_${instance}_service") == active ]]
[[ $(<"$systemctl_state/telos-reth-readiness_${instance}_timer") == active ]]
[[ ! -e $permit && ! -L $permit ]]

# Exercise a complete replacement restore and verify both the new live set and rollback set.
remove_restore_objects
create_originals
reset_systemctl_state
"$restore_script" "$instance" "$snapshot" --confirm
journal=$state_dir/restore.transaction.json
jq -e '.status == "committed" and .had_original == "11111"' "$journal" >/dev/null
restore_id=$(jq -r .restore_id "$journal")
[[ $(<"$execution_dir/marker") == new-execution ]]
[[ $(<"$consensus_dir/marker") == new-consensus ]]
[[ $(<"${execution_dir}.pre-restore-${restore_id}/marker") == old-execution ]]
[[ $(<"${consensus_dir}.pre-restore-${restore_id}/marker") == old-consensus ]]
for artifact in checkpoint.json checkpoint.audit.json checkpoint.anchor.json; do
    cmp -s "$snapshot/checkpoint/$artifact" "${config_dir}/$artifact"
    grep -q '"old"' "${config_dir}/${artifact}.pre-restore-${restore_id}"
done
[[ ! -e $permit && ! -L $permit ]]

# Exercise the clean-host path, where none of the five restore targets exists beforehand.
remove_restore_objects
reset_systemctl_state
"$restore_script" "$instance" "$snapshot" --confirm
journal=$state_dir/restore.transaction.json
jq -e '.status == "committed" and .had_original == "00000"' "$journal" >/dev/null
restore_id=$(jq -r .restore_id "$journal")
[[ $(<"$execution_dir/marker") == new-execution ]]
[[ $(<"$consensus_dir/marker") == new-consensus ]]
for artifact in checkpoint.json checkpoint.audit.json checkpoint.anchor.json; do
    cmp -s "$snapshot/checkpoint/$artifact" "${config_dir}/$artifact"
done
for path in \
    "${execution_dir}.pre-restore-${restore_id}" \
    "${consensus_dir}.pre-restore-${restore_id}" \
    "${config_dir}/checkpoint.json.pre-restore-${restore_id}" \
    "${config_dir}/checkpoint.audit.json.pre-restore-${restore_id}" \
    "${config_dir}/checkpoint.anchor.json.pre-restore-${restore_id}"; do
    [[ ! -e $path && ! -L $path ]]
done
[[ ! -e $permit && ! -L $permit ]]
[[ $(wc -l < "$TELOS_RESTORE_TEST_BINDING_LOG") -eq 3 ]]

echo "restore integration test: crash recovery, replacement, and clean-host paths passed"

#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage:
  create-hot-mdbx-copy.sh \
    --legacy-reth /absolute/path/to/legacy-telos-reth \
    --source-datadir /absolute/path/to/legacy-datadir \
    --source-db /absolute/path/to/legacy-datadir/40/db \
    --chain telos-mainnet \
    --backup-db /absolute/new/path/checkpoint-mdbx \
    --mdbx-copy /absolute/path/to/mdbx_copy \
    --mdbx-chk /absolute/path/to/mdbx_chk

The source node may remain running. The script uses the standalone mdbx_copy built
from the legacy node's exact vendored libmdbx source, validates the immutable
destination with the matching mdbx_chk, and writes mdbx-copy.json. It never scans
live tables directly.
EOF
  exit 64
}

legacy_reth=""
source_datadir=""
source_db=""
chain=""
backup_db=""
mdbx_copy=""
mdbx_chk=""

while (($#)); do
  case "$1" in
    --legacy-reth) legacy_reth=${2:?}; shift 2 ;;
    --source-datadir) source_datadir=${2:?}; shift 2 ;;
    --source-db) source_db=${2:?}; shift 2 ;;
    --chain) chain=${2:?}; shift 2 ;;
    --backup-db) backup_db=${2:?}; shift 2 ;;
    --mdbx-copy) mdbx_copy=${2:?}; shift 2 ;;
    --mdbx-chk) mdbx_chk=${2:?}; shift 2 ;;
    *) usage ;;
  esac
done

[[ -n "$legacy_reth" && -n "$source_datadir" && -n "$source_db" && -n "$chain" && -n "$backup_db" && -n "$mdbx_copy" && -n "$mdbx_chk" ]] || usage
[[ "$legacy_reth" = /* && "$source_datadir" = /* && "$source_db" = /* && "$backup_db" = /* && "$mdbx_copy" = /* && "$mdbx_chk" = /* ]] || {
  echo "all paths must be absolute" >&2
  exit 64
}
[[ -x "$legacy_reth" ]] || { echo "legacy binary is not executable: $legacy_reth" >&2; exit 66; }
[[ -x "$mdbx_copy" ]] || { echo "mdbx_copy is not executable: $mdbx_copy" >&2; exit 66; }
[[ -x "$mdbx_chk" ]] || { echo "mdbx_chk is not executable: $mdbx_chk" >&2; exit 66; }
[[ -d "$source_datadir" ]] || { echo "source data directory does not exist: $source_datadir" >&2; exit 66; }
[[ -f "$source_db/mdbx.dat" ]] || { echo "source MDBX not found: $source_db/mdbx.dat" >&2; exit 66; }
[[ ! -e "$backup_db" ]] || { echo "backup destination already exists: $backup_db" >&2; exit 73; }
case "$chain" in
  telos-mainnet) legacy_chain=tevmmainnet ;;
  telos-testnet) legacy_chain=tevmtestnet ;;
  *) echo "chain must be telos-mainnet or telos-testnet" >&2; exit 64 ;;
esac

source_db=$(realpath "$source_db")
source_datadir=$(realpath "$source_datadir")
legacy_reth=$(realpath "$legacy_reth")
mdbx_copy=$(realpath "$mdbx_copy")
mdbx_chk=$(realpath "$mdbx_chk")
resolved_source_db=$(RUST_LOG=off "$legacy_reth" db --datadir "$source_datadir" --chain "$legacy_chain" path)
[[ -n "$resolved_source_db" && "$resolved_source_db" != *$'\n'* ]] || {
  echo "legacy db path probe returned malformed output" >&2
  exit 65
}
resolved_source_db=$(realpath "$resolved_source_db")
[[ "$resolved_source_db" == "$source_db" ]] || {
  echo "--source-db is not the database selected by --source-datadir and --chain" >&2
  echo "selected: $resolved_source_db" >&2
  echo "supplied: $source_db" >&2
  exit 64
}
mkdir -p "$(dirname "$backup_db")"
mkdir "$backup_db"
backup_db=$(realpath "$backup_db")
[[ "$source_db" != "$backup_db" ]] || { echo "source and backup MDBX paths are identical" >&2; exit 64; }
backup_file="$backup_db/mdbx.dat"

"$mdbx_copy" -c "$source_db/mdbx.dat" "$backup_file"

[[ -f "$backup_file" ]] || { echo "mdbx_env_copy did not create $backup_file" >&2; exit 74; }

check_log="$backup_db/mdbx-check.log"
"$mdbx_chk" "$backup_file" >"$check_log" 2>&1

backup_sha=$(sha256sum "$backup_file" | awk '{print $1}')
backup_size=$(stat -c '%s' "$backup_file")
legacy_sha=$(sha256sum "$legacy_reth" | awk '{print $1}')
legacy_version=$("$legacy_reth" --version | head -n 1)
mdbx_copy_sha=$(sha256sum "$mdbx_copy" | awk '{print $1}')
mdbx_copy_version=$("$mdbx_copy" -V 2>&1 | head -n 1)
mdbx_chk_sha=$(sha256sum "$mdbx_chk" | awk '{print $1}')
check_sha=$(sha256sum "$check_log" | awk '{print $1}')
created_at=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
manifest="$backup_db/mdbx-copy.json"

COPY_SOURCE_DB="$source_db" \
COPY_SOURCE_DATADIR="$source_datadir" \
COPY_CHAIN="$chain" \
COPY_LEGACY_CHAIN="$legacy_chain" \
COPY_BACKUP_DB="$backup_db" \
COPY_BACKUP_SIZE="$backup_size" \
COPY_BACKUP_SHA="$backup_sha" \
COPY_LEGACY_SHA="$legacy_sha" \
COPY_LEGACY_VERSION="$legacy_version" \
COPY_MDBX_COPY_SHA="$mdbx_copy_sha" \
COPY_MDBX_COPY_VERSION="$mdbx_copy_version" \
COPY_MDBX_CHK_SHA="$mdbx_chk_sha" \
COPY_CHECK_LOG="$check_log" \
COPY_CHECK_SHA="$check_sha" \
COPY_CREATED_AT="$created_at" \
python3 - "$manifest" <<'PY'
import json
import os
import pathlib
import sys

target = pathlib.Path(sys.argv[1])
record = {
    "version": 2,
    "copy_method": "libmdbx-mdbx_copy-compact",
    "verification": "mdbx_chk-ok",
    "chain": os.environ["COPY_CHAIN"],
    "legacy_chain": os.environ["COPY_LEGACY_CHAIN"],
    "source_datadir": os.environ["COPY_SOURCE_DATADIR"],
    "source_db": os.environ["COPY_SOURCE_DB"],
    "backup_db": os.environ["COPY_BACKUP_DB"],
    "mdbx_size": int(os.environ["COPY_BACKUP_SIZE"]),
    "mdbx_sha256": "0x" + os.environ["COPY_BACKUP_SHA"],
    "legacy_binary_sha256": "0x" + os.environ["COPY_LEGACY_SHA"],
    "legacy_binary_version": os.environ["COPY_LEGACY_VERSION"],
    "mdbx_copy_binary_sha256": "0x" + os.environ["COPY_MDBX_COPY_SHA"],
    "mdbx_copy_binary_version": os.environ["COPY_MDBX_COPY_VERSION"],
    "mdbx_check_binary_sha256": "0x" + os.environ["COPY_MDBX_CHK_SHA"],
    "mdbx_check_log": os.environ["COPY_CHECK_LOG"],
    "mdbx_check_log_sha256": "0x" + os.environ["COPY_CHECK_SHA"],
    "created_at_utc": os.environ["COPY_CREATED_AT"],
}
temporary = target.with_suffix(".tmp")
with temporary.open("x", encoding="utf-8") as stream:
    json.dump(record, stream, indent=2, sort_keys=True)
    stream.write("\n")
    stream.flush()
    os.fsync(stream.fileno())
temporary.replace(target)
PY

echo "verified_backup_manifest=$manifest"
echo "verified_backup_db=$backup_db"
echo "verified_backup_sha256=0x$backup_sha"

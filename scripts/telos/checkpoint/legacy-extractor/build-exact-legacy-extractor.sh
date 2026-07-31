#!/usr/bin/env bash
set -euo pipefail

EXPECTED_COMMIT=8c37741ea8d97eba713a8028e3f09132bb51abd6

usage() {
  echo "usage: $0 --legacy-worktree PATH --output-dir PATH" >&2
  exit 2
}

legacy_worktree=
output_dir=
while (($#)); do
  case "$1" in
    --legacy-worktree) legacy_worktree=${2:-}; shift 2 ;;
    --output-dir) output_dir=${2:-}; shift 2 ;;
    *) usage ;;
  esac
done

[[ -n $legacy_worktree && -n $output_dir ]] || usage
legacy_worktree=$(cd "$legacy_worktree" && pwd -P)
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd -P)

actual_commit=$(git -C "$legacy_worktree" rev-parse HEAD)
[[ $actual_commit == "$EXPECTED_COMMIT" ]] || {
  echo "legacy worktree must be exact commit $EXPECTED_COMMIT, got $actual_commit" >&2
  exit 1
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
source_file="$legacy_worktree/crates/telos/bin/src/legacy_checkpoint_export.rs"
manifest="$legacy_worktree/crates/telos/bin/Cargo.toml"
install -m 0644 "$script_dir/src/main.rs" "$source_file"

python3 - "$manifest" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
needle = "reth-db.workspace = true\n"
dependencies = """reth-primitives.workspace = true
reth-stages-types.workspace = true
reth-trie.workspace = true
reth-trie-db.workspace = true
alloy-rlp.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
eyre.workspace = true
"""
if "reth-trie-db.workspace = true" not in text:
    if text.count(needle) != 1:
        raise SystemExit("unexpected exact-legacy Cargo.toml dependency layout")
    text = text.replace(needle, needle + dependencies)

binary = """

[[bin]]
name = "telos-legacy-checkpoint-export"
path = "src/legacy_checkpoint_export.rs"
required-features = ["telos"]
"""
if 'name = "telos-legacy-checkpoint-export"' not in text:
    text += binary
path.write_text(text, encoding="utf-8")
PY

cargo_bin=${CARGO_BIN:-cargo}
original_lock=$(mktemp)
trap 'rm -f "$original_lock"' EXIT
cp "$legacy_worktree/Cargo.lock" "$original_lock"
(
  cd "$legacy_worktree"
  "$cargo_bin" build --release -p telos-reth --bin telos-legacy-checkpoint-export
)

python3 - "$original_lock" "$legacy_worktree/Cargo.lock" <<'PY'
import sys
import tomllib

with open(sys.argv[1], "rb") as stream:
    before = tomllib.load(stream)
with open(sys.argv[2], "rb") as stream:
    after = tomllib.load(stream)

def keyed(document):
    result = {}
    for package in document["package"]:
        key = (package["name"], package["version"], package.get("source"))
        if key in result:
            raise SystemExit(f"duplicate Cargo.lock package identity: {key}")
        result[key] = package
    return result

old = keyed(before)
new = keyed(after)
if old.keys() != new.keys():
    raise SystemExit("extractor build changed the exact legacy Cargo.lock package set")
for key in old:
    old_entry = old[key]
    new_entry = new[key]
    if key[0] == "telos-reth":
        old_entry = {k: v for k, v in old_entry.items() if k != "dependencies"}
        new_entry = {k: v for k, v in new_entry.items() if k != "dependencies"}
    if old_entry != new_entry:
        raise SystemExit(f"extractor build changed a pinned Cargo.lock entry: {key}")
PY

(
  cd "$legacy_worktree"
  "$cargo_bin" build --release --locked -p telos-reth --bin telos-legacy-checkpoint-export
)
target_dir=${CARGO_TARGET_DIR:-$legacy_worktree/target}
binary="$target_dir/release/telos-legacy-checkpoint-export"
install -m 0755 "$binary" "$output_dir/telos-legacy-checkpoint-export"
install -m 0644 "$original_lock" "$output_dir/Cargo.lock.legacy"
install -m 0644 "$legacy_worktree/Cargo.lock" "$output_dir/Cargo.lock.extractor"

python3 - "$legacy_worktree" "$script_dir/src/main.rs" "$output_dir" <<'PY'
from pathlib import Path
import hashlib
import json
import os
import subprocess
import sys

worktree = Path(sys.argv[1])
source = Path(sys.argv[2])
output = Path(sys.argv[3])
binary = output / "telos-legacy-checkpoint-export"

def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            h.update(chunk)
    return "0x" + h.hexdigest()

record = {
    "version": 1,
    "legacy_source_commit": "8c37741ea8d97eba713a8028e3f09132bb51abd6",
    "legacy_cargo_lock_sha256": sha256(output / "Cargo.lock.legacy"),
    "extractor_cargo_lock_sha256": sha256(output / "Cargo.lock.extractor"),
    "extractor_source_sha256": sha256(source),
    "extractor_binary_sha256": sha256(binary),
    "rustc_version": subprocess.check_output([os.environ.get("RUSTC_BIN", "rustc"), "--version"], text=True).strip(),
    "cargo_version": subprocess.check_output([os.environ.get("CARGO_BIN", "cargo"), "--version"], text=True).strip(),
}
temporary = output / "legacy-extractor.provenance.json.tmp"
final = output / "legacy-extractor.provenance.json"
if final.exists() or temporary.exists():
    raise SystemExit("refusing to overwrite legacy extractor provenance")
with temporary.open("x", encoding="utf-8") as stream:
    json.dump(record, stream, indent=2, sort_keys=True)
    stream.write("\n")
    stream.flush()
    os.fsync(stream.fileno())
temporary.replace(final)
print(f"legacy_extractor={binary}")
print(f"legacy_extractor_provenance={final}")
PY

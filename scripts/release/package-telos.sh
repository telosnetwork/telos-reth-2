#!/usr/bin/env bash

set -euo pipefail

usage() {
    echo "usage: $0 <binary> <version> <target> <source-commit> <upstream-release> <upstream-commit> <output-dir>" >&2
}

if [[ $# -ne 7 ]]; then
    usage
    exit 2
fi

binary=$1
version=$2
target=$3
source_commit=$4
upstream_release=$5
upstream_commit=$6
output_dir=$7

: "${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH must be set to the source commit timestamp}"

[[ -x "$binary" ]] || { echo "binary is missing or not executable: $binary" >&2; exit 1; }
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] || { echo "invalid SemVer version: $version" >&2; exit 1; }
[[ "$target" =~ ^(x86_64|aarch64)-unknown-linux-gnu$ ]] || { echo "unsupported target: $target" >&2; exit 1; }
[[ "$source_commit" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid source commit: $source_commit" >&2; exit 1; }
[[ "$upstream_release" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] || { echo "invalid upstream release: $upstream_release" >&2; exit 1; }
[[ "$upstream_commit" =~ ^[0-9a-f]{40}$ ]] || { echo "invalid upstream commit: $upstream_commit" >&2; exit 1; }
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]] || { echo "invalid SOURCE_DATE_EPOCH: $SOURCE_DATE_EPOCH" >&2; exit 1; }

binary_dir=$(cd "$(dirname "$binary")" && pwd)
checkpoint_bootstrap="$binary_dir/telos-checkpoint-bootstrap"
[[ -x "$checkpoint_bootstrap" ]] || { echo "checkpoint bootstrap binary is missing: $checkpoint_bootstrap" >&2; exit 1; }
rpc_router="$binary_dir/telos-rpc-router"
[[ -x "$rpc_router" ]] || { echo "RPC router binary is missing: $rpc_router" >&2; exit 1; }
legacy_extractor_build="scripts/telos/checkpoint/legacy-extractor/build-exact-legacy-extractor.sh"
legacy_extractor_source="scripts/telos/checkpoint/legacy-extractor/src/main.rs"
[[ -x "$legacy_extractor_build" ]] || { echo "exact-legacy extractor build script is missing or not executable: $legacy_extractor_build" >&2; exit 1; }
[[ -f "$legacy_extractor_source" && ! -L "$legacy_extractor_source" ]] || { echo "exact-legacy extractor source is missing or not a regular file: $legacy_extractor_source" >&2; exit 1; }

archive_root="telos-reth-${version}-${target}"
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)

staging_dir=$(mktemp -d)
trap 'rm -rf "$staging_dir"' EXIT
package_dir="$staging_dir/$archive_root"
mkdir -p "$package_dir"

install -m 0755 "$binary" "$package_dir/telos-reth"
install -m 0755 "$checkpoint_bootstrap" "$package_dir/telos-checkpoint-bootstrap"
install -m 0755 "$rpc_router" "$package_dir/telos-rpc-router"
cp LICENSE-APACHE LICENSE-MIT README.md "$package_dir/"
cp -R LICENSES "$package_dir/LICENSES"
cp -R ops "$package_dir/ops"
mkdir -p "$package_dir/scripts/telos"
cp -R scripts/telos/checkpoint "$package_dir/scripts/telos/checkpoint"
mkdir -p "$package_dir/docs"
cp -R docs/telos "$package_dir/docs/telos"
chmod 0755 "$package_dir"/ops/scripts/* "$package_dir"/scripts/telos/checkpoint/*
chmod 0755 "$package_dir/$legacy_extractor_build"
chmod 0644 "$package_dir/$legacy_extractor_source"

binary_sha256=$(sha256sum "$package_dir/telos-reth" | cut -d ' ' -f 1)
checkpoint_bootstrap_sha256=$(sha256sum "$package_dir/telos-checkpoint-bootstrap" | cut -d ' ' -f 1)
rpc_router_sha256=$(sha256sum "$package_dir/telos-rpc-router" | cut -d ' ' -f 1)
legacy_extractor_build_sha256=$(sha256sum "$package_dir/$legacy_extractor_build" | cut -d ' ' -f 1)
legacy_extractor_source_sha256=$(sha256sum "$package_dir/$legacy_extractor_source" | cut -d ' ' -f 1)
cat > "$package_dir/BUILD-METADATA" <<EOF
format_version=4
telos_version=${version}
source_repository=https://github.com/telosnetwork/telos-reth-2
source_commit=${source_commit}
source_date_epoch=${SOURCE_DATE_EPOCH}
upstream_repository=https://github.com/paradigmxyz/reth
upstream_release=${upstream_release}
upstream_commit=${upstream_commit}
rust_target=${target}
binary_sha256=${binary_sha256}
checkpoint_bootstrap_sha256=${checkpoint_bootstrap_sha256}
rpc_router_sha256=${rpc_router_sha256}
legacy_extractor_build_sha256=${legacy_extractor_build_sha256}
legacy_extractor_source_sha256=${legacy_extractor_source_sha256}
EOF

find "$package_dir" -exec touch -h -d "@${SOURCE_DATE_EPOCH}" {} +

archive="$output_dir/${archive_root}.tar.gz"
tar \
    --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$staging_dir" \
    -cf - "$archive_root" | gzip -n -9 > "$archive"

echo "$archive"

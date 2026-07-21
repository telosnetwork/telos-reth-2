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

archive_root="telos-reth-${version}-${target}"
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)

staging_dir=$(mktemp -d)
trap 'rm -rf "$staging_dir"' EXIT
package_dir="$staging_dir/$archive_root"
mkdir -p "$package_dir"

install -m 0755 "$binary" "$package_dir/telos-reth"
cp LICENSE-APACHE LICENSE-MIT README.md "$package_dir/"
cp -R LICENSES "$package_dir/LICENSES"

binary_sha256=$(sha256sum "$package_dir/telos-reth" | cut -d ' ' -f 1)
cat > "$package_dir/BUILD-METADATA" <<EOF
format_version=1
telos_version=${version}
source_repository=https://github.com/telosnetwork/telos-reth-2
source_commit=${source_commit}
source_date_epoch=${SOURCE_DATE_EPOCH}
upstream_repository=https://github.com/paradigmxyz/reth
upstream_release=${upstream_release}
upstream_commit=${upstream_commit}
rust_target=${target}
binary_sha256=${binary_sha256}
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

#!/usr/bin/env bash

set -euo pipefail

release_dir=${1:-.}
release_dir=$(cd "$release_dir" && pwd)

[[ -f "$release_dir/SHA256SUMS" ]] || { echo "SHA256SUMS is missing" >&2; exit 1; }

(
    cd "$release_dir"
    sha256sum --check SHA256SUMS
)

shopt -s nullglob
archives=("$release_dir"/telos-reth-*-unknown-linux-gnu.tar.gz)
[[ ${#archives[@]} -eq 2 ]] || { echo "expected exactly two Linux release archives" >&2; exit 1; }

expected_source_commit=
expected_upstream_release=
expected_upstream_commit=
expected_version=
declare -A seen_targets=()

for archive in "${archives[@]}"; do
    archive_name=$(basename "$archive")
    [[ "$archive_name" =~ ^telos-reth-(.+)-((x86_64|aarch64)-unknown-linux-gnu)\.tar\.gz$ ]] \
        || { echo "unexpected archive name: $archive_name" >&2; exit 1; }
    version=${BASH_REMATCH[1]}
    target=${BASH_REMATCH[2]}
    architecture=${BASH_REMATCH[3]}
    root="telos-reth-${version}-${target}"

    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]] \
        || { echo "invalid release version in $archive_name" >&2; exit 1; }
    [[ -z "${seen_targets[$target]+set}" ]] \
        || { echo "duplicate target archive: $target" >&2; exit 1; }
    seen_targets[$target]=1

    tar -tzf "$archive" "${root}/telos-reth" >/dev/null
    tar -tzf "$archive" "${root}/BUILD-METADATA" >/dev/null
    tar -tzf "$archive" "${root}/LICENSE-APACHE" >/dev/null
    tar -tzf "$archive" "${root}/LICENSE-MIT" >/dev/null

    staging_dir=$(mktemp -d)
    trap 'rm -rf "$staging_dir"' EXIT
    tar -xzf "$archive" -C "$staging_dir" "$root"

    binary="$staging_dir/$root/telos-reth"
    metadata="$staging_dir/$root/BUILD-METADATA"
    [[ -x "$binary" ]] || { echo "binary is not executable: $archive_name" >&2; exit 1; }

    grep -Fxq "format_version=1" "$metadata"
    grep -Fxq "telos_version=${version}" "$metadata"
    grep -Fxq "rust_target=${target}" "$metadata"
    source_commit=$(sed -n 's/^source_commit=//p' "$metadata")
    upstream_release=$(sed -n 's/^upstream_release=//p' "$metadata")
    upstream_commit=$(sed -n 's/^upstream_commit=//p' "$metadata")
    recorded_binary_sha=$(sed -n 's/^binary_sha256=//p' "$metadata")
    [[ "$source_commit" =~ ^[0-9a-f]{40}$ ]]
    [[ "$upstream_release" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]
    [[ "$upstream_commit" =~ ^[0-9a-f]{40}$ ]]
    [[ "$recorded_binary_sha" == "$(sha256sum "$binary" | cut -d ' ' -f 1)" ]]

    case "$architecture" in
        x86_64) file "$binary" | grep -Eq 'ELF 64-bit.*x86-64' ;;
        aarch64) file "$binary" | grep -Eq 'ELF 64-bit.*(ARM aarch64|ARM64)' ;;
    esac

    if [[ "$(uname -m)" == "$architecture" ]]; then
        "$binary" --version | grep -F "$version" >/dev/null
    fi

    if [[ -z "$expected_source_commit" ]]; then
        expected_version=$version
        expected_source_commit=$source_commit
        expected_upstream_release=$upstream_release
        expected_upstream_commit=$upstream_commit
    else
        [[ "$version" == "$expected_version" ]]
        [[ "$source_commit" == "$expected_source_commit" ]]
        [[ "$upstream_release" == "$expected_upstream_release" ]]
        [[ "$upstream_commit" == "$expected_upstream_commit" ]]
    fi

    rm -rf "$staging_dir"
done

[[ -n "${seen_targets[x86_64-unknown-linux-gnu]+set}" ]]
[[ -n "${seen_targets[aarch64-unknown-linux-gnu]+set}" ]]
if [[ -n "${TELOS_VERSION:-}" ]]; then
    [[ "$TELOS_VERSION" == "$expected_version" ]] \
        || { echo "TELOS_VERSION does not match release assets" >&2; exit 1; }
fi

sboms=("$release_dir"/telos-reth-*.spdx.json)
[[ ${#sboms[@]} -eq 2 ]] || { echo "expected exactly two binary SPDX SBOMs" >&2; exit 1; }
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    [[ -f "$release_dir/telos-reth-${expected_version}-${target}.spdx.json" ]] \
        || { echo "SBOM is missing for $target" >&2; exit 1; }
done

container_metadata=("$release_dir"/telos-reth-*-container.txt)
[[ ${#container_metadata[@]} -eq 1 ]] || { echo "expected exactly one container metadata file" >&2; exit 1; }
[[ "$(basename "${container_metadata[0]}")" == "telos-reth-${expected_version}-container.txt" ]] \
    || { echo "container metadata version does not match release assets" >&2; exit 1; }
grep -Eq '^image=ghcr\.io/telosnetwork/telos-reth@sha256:[0-9a-f]{64}$' "${container_metadata[0]}"
grep -Fxq "source_commit=${expected_source_commit}" "${container_metadata[0]}"
grep -Fxq "upstream_release=${expected_upstream_release}" "${container_metadata[0]}"
grep -Fxq "upstream_commit=${expected_upstream_commit}" "${container_metadata[0]}"

if command -v cosign >/dev/null 2>&1; then
    : "${TELOS_VERSION:?set TELOS_VERSION to verify Sigstore bundles}"
    identity="https://github.com/telosnetwork/telos-reth-2/.github/workflows/release.yml@refs/tags/telos-v${TELOS_VERSION}"
    issuer="https://token.actions.githubusercontent.com"

    signed_files=(
        "$release_dir"/*.tar.gz
        "$release_dir"/*.spdx.json
        "$release_dir"/telos-reth-*-container.txt
        "$release_dir/SHA256SUMS"
    )
    for signed_file in "${signed_files[@]}"; do
        bundle="${signed_file}.sigstore.json"
        [[ -f "$bundle" ]] || { echo "Sigstore bundle is missing: $bundle" >&2; exit 1; }
        cosign verify-blob \
            --bundle "$bundle" \
            --certificate-identity "$identity" \
            --certificate-oidc-issuer "$issuer" \
            "$signed_file"
    done
else
    echo "cosign not found; checksum and archive structure checks completed" >&2
fi

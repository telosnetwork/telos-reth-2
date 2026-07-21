#!/usr/bin/env bash

set -euo pipefail

release_dir=${1:-.}
release_dir=$(cd "$release_dir" && pwd)

command -v cosign >/dev/null 2>&1 \
    || { echo "cosign is required to authenticate release assets" >&2; exit 1; }
: "${TELOS_VERSION:?set TELOS_VERSION to verify Sigstore bundles}"
[[ "$TELOS_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || { echo "TELOS_VERSION is not a release-safe semantic version" >&2; exit 1; }

[[ -f "$release_dir/SHA256SUMS" ]] || { echo "SHA256SUMS is missing" >&2; exit 1; }

# Authenticate every primary asset before parsing checksums, opening archives, or executing a
# binary. A matching but unsigned SHA256SUMS must never authorize attacker-controlled content.
identity="https://github.com/telosnetwork/telos-reth-2/.github/workflows/release.yml@refs/tags/telos-v${TELOS_VERSION}"
issuer="https://token.actions.githubusercontent.com"
signed_files=(
    "$release_dir/telos-reth-${TELOS_VERSION}-x86_64-unknown-linux-gnu.tar.gz"
    "$release_dir/telos-reth-${TELOS_VERSION}-aarch64-unknown-linux-gnu.tar.gz"
    "$release_dir/telos-reth-${TELOS_VERSION}-x86_64-unknown-linux-gnu.spdx.json"
    "$release_dir/telos-reth-${TELOS_VERSION}-aarch64-unknown-linux-gnu.spdx.json"
    "$release_dir/telos-reth-${TELOS_VERSION}-container.txt"
    "$release_dir/SHA256SUMS"
)
for signed_file in "${signed_files[@]}"; do
    [[ -f "$signed_file" ]] || { echo "signed release asset is missing: $signed_file" >&2; exit 1; }
    bundle="${signed_file}.sigstore.json"
    [[ -f "$bundle" ]] || { echo "Sigstore bundle is missing: $bundle" >&2; exit 1; }
    cosign verify-blob \
        --bundle "$bundle" \
        --certificate-identity "$identity" \
        --certificate-oidc-issuer "$issuer" \
        "$signed_file"
done

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

    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
        || { echo "invalid release version in $archive_name" >&2; exit 1; }
    [[ -z "${seen_targets[$target]+set}" ]] \
        || { echo "duplicate target archive: $target" >&2; exit 1; }
    seen_targets[$target]=1

    tar -tzf "$archive" "${root}/telos-reth" >/dev/null
    tar -tzf "$archive" "${root}/telos-checkpoint-bootstrap" >/dev/null
    tar -tzf "$archive" "${root}/ops/scripts/telos-reth-restore" >/dev/null
    tar -tzf "$archive" "${root}/ops/checkpoints/telos-mainnet/479294328/checkpoint.json" >/dev/null
    tar -tzf "$archive" "${root}/scripts/telos/checkpoint/create-hot-mdbx-copy.sh" >/dev/null
    tar -tzf "$archive" "${root}/scripts/telos/checkpoint/legacy-extractor/build-exact-legacy-extractor.sh" >/dev/null
    tar -tzf "$archive" "${root}/scripts/telos/checkpoint/legacy-extractor/src/main.rs" >/dev/null
    tar -tzf "$archive" "${root}/docs/telos/checkpoint-bootstrap.md" >/dev/null
    tar -tzf "$archive" "${root}/docs/telos/compatibility.md" >/dev/null
    tar -tzf "$archive" "${root}/BUILD-METADATA" >/dev/null
    tar -tzf "$archive" "${root}/LICENSE-APACHE" >/dev/null
    tar -tzf "$archive" "${root}/LICENSE-MIT" >/dev/null

    staging_dir=$(mktemp -d)
    trap 'rm -rf "$staging_dir"' EXIT
    tar -xzf "$archive" -C "$staging_dir" "$root"

    binary="$staging_dir/$root/telos-reth"
    checkpoint_bootstrap="$staging_dir/$root/telos-checkpoint-bootstrap"
    legacy_extractor_build="$staging_dir/$root/scripts/telos/checkpoint/legacy-extractor/build-exact-legacy-extractor.sh"
    legacy_extractor_source="$staging_dir/$root/scripts/telos/checkpoint/legacy-extractor/src/main.rs"
    metadata="$staging_dir/$root/BUILD-METADATA"
    [[ -x "$binary" ]] || { echo "binary is not executable: $archive_name" >&2; exit 1; }
    [[ -x "$checkpoint_bootstrap" ]] \
        || { echo "checkpoint bootstrap is not executable: $archive_name" >&2; exit 1; }
    [[ -x "$legacy_extractor_build" ]] \
        || { echo "exact-legacy extractor build script is not executable: $archive_name" >&2; exit 1; }
    [[ -f "$legacy_extractor_source" && ! -L "$legacy_extractor_source" ]] \
        || { echo "exact-legacy extractor source is missing: $archive_name" >&2; exit 1; }

    grep -Fxq "format_version=3" "$metadata"
    grep -Fxq "telos_version=${version}" "$metadata"
    grep -Fxq "rust_target=${target}" "$metadata"
    source_commit=$(sed -n 's/^source_commit=//p' "$metadata")
    upstream_release=$(sed -n 's/^upstream_release=//p' "$metadata")
    upstream_commit=$(sed -n 's/^upstream_commit=//p' "$metadata")
    recorded_binary_sha=$(sed -n 's/^binary_sha256=//p' "$metadata")
    recorded_bootstrap_sha=$(sed -n 's/^checkpoint_bootstrap_sha256=//p' "$metadata")
    recorded_legacy_build_sha=$(sed -n 's/^legacy_extractor_build_sha256=//p' "$metadata")
    recorded_legacy_source_sha=$(sed -n 's/^legacy_extractor_source_sha256=//p' "$metadata")
    [[ "$source_commit" =~ ^[0-9a-f]{40}$ ]]
    [[ "$upstream_release" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$ ]]
    [[ "$upstream_commit" =~ ^[0-9a-f]{40}$ ]]
    [[ "$recorded_binary_sha" == "$(sha256sum "$binary" | cut -d ' ' -f 1)" ]]
    [[ "$recorded_bootstrap_sha" == "$(sha256sum "$checkpoint_bootstrap" | cut -d ' ' -f 1)" ]]
    [[ "$recorded_legacy_build_sha" == "$(sha256sum "$legacy_extractor_build" | cut -d ' ' -f 1)" ]]
    [[ "$recorded_legacy_source_sha" == "$(sha256sum "$legacy_extractor_source" | cut -d ' ' -f 1)" ]]

    for executable in "$binary" "$checkpoint_bootstrap"; do
        case "$architecture" in
            x86_64) file "$executable" | grep -Eq 'ELF 64-bit.*x86-64' ;;
            aarch64) file "$executable" | grep -Eq 'ELF 64-bit.*(ARM aarch64|ARM64)' ;;
        esac
    done

    if [[ "$(uname -m)" == "$architecture" ]]; then
        "$binary" --version | grep -F "$version" >/dev/null
        "$checkpoint_bootstrap" --help >/dev/null
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
[[ ${#sboms[@]} -eq 2 ]] || { echo "expected exactly two release-suite SPDX SBOMs" >&2; exit 1; }
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

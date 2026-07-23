# Telos Reth release process

Telos releases use the `telos-v<package-version>` tag namespace. Upstream Reth
tags such as `v2.4.1` never trigger Telos publication. The release workflow
checks the tag against the `telos-reth` Cargo package, records the exact source
commit and reviewed upstream Reth base, and creates a draft release.

## Repository controls

Before the first release, configure these GitHub controls:

- Protect `main`; require pull requests, two approvals, resolved conversations,
  signed commits, linear history, and the `telos / ci success` plus
  `telos / security success` checks.
- Protect the `telos-v*` tag namespace so only release managers can create or
  delete tags.
- Protect the `production-release` environment with a reviewer pool containing
  at least two independent maintainers, prevent self-review, and prevent
  administrators from bypassing approval. Select deployment branches and tags
  explicitly: allow the protected `main` branch for manual recovery and the
  `telos-v*` tag pattern for release and promotion events.
- Link the `ghcr.io/telosnetwork/telos-reth` package to this repository, grant
  Actions write access only to this repository, and remove PAT, manual, and
  other-repository publishers for versioned tags. If the package namespace is
  already shared and cannot be made exclusive, select a new package name and
  update both release workflows and this runbook before the first release.
- Enable private vulnerability reporting, dependency graph, Dependabot alerts,
  secret scanning, push protection, and immutable GitHub Releases.
- Restrict Actions to pinned, allow-listed actions and require SHA pinning at
  the organization level.

## Release gate

Do not create a release tag until all of the following evidence is attached to
the release issue:

1. The commit is on protected `main` and all required checks passed.
2. A Telos testnet node completed a clean sync and at least 72 hours of soak.
3. Restart recovery, Engine API authentication, transaction forwarding,
   snapshot restore, and alert delivery were exercised.
4. State roots and receipts were compared with the companion consensus client
   across the agreed historical range, including a forced reorg.
5. A rollback owner, prior known-good image digest, and maintenance window are
   recorded.

The workflow blocks publication on critical container vulnerabilities. A
release manager must still review non-critical findings and accepted advisory
exceptions in `deny.toml`. The built `telos-reth` binary exposes its two independent gates through
`telos-build-info`; a tag build cannot publish while `execution_ready` is false. The replay gate may
remain false so unqualified historical replay and diagnostic RPC stay unavailable.

Before creating a tag, dispatch `release.yml` with the current package version. The rehearsal
performs both native builds, byte-identical rebuild checks, runtime-container assembly and smoke
tests, SBOM generation, and the local Trivy scan without signing or publishing anything.

## Create a release

Update the explicit version of the `telos-reth` package without changing the
upstream workspace version. After the release commit is merged, create a signed,
annotated tag:

```bash
git switch main
git pull --ff-only
git tag -s telos-v0.1.0 -m "Telos Reth 0.1.0"
git push origin telos-v0.1.0
```

For a release candidate, first set the package version to a prerelease such as
`0.1.0-rc.1`, then use the matching `telos-v0.1.0-rc.1` tag.

Approve the `production-release` deployment after confirming the tag points to
the reviewed commit. The workflow builds native Linux amd64 and arm64 binaries,
independently rebuilds both binaries and requires byte-identical results,
and generates SPDX SBOMs plus deterministic archives and checksums. Only after
reproducibility succeeds and the protected environment is approved does it
produce GitHub build attestations and sign files with Sigstore keyless signing.
It verifies the signed archive before putting that exact binary into a
container, executes and scans each architecture image, and initially pushes it
only under a run-scoped `staging-<commit>-<run>-<attempt>-<arch>` tag. The
workflow fixes image timestamps to the source commit time, signs and attests the
captured architecture digests, creates and signs the multi-platform index under
a run-scoped tag, and creates the complete draft GitHub Release. It seals the
draft's release ID, notes, and complete asset name/digest/state manifest, then
rechecks that seal immediately before promotion. Only then does it copy those
already-signed digests to the semantic architecture and version tags; the
multi-platform version tag is copied last.

Each platform archive contains three release-built executables: `telos-reth`,
`telos-checkpoint-bootstrap`, and `telos-rpc-router`. `BUILD-METADATA` records the SHA-256 of each.
The container image remains the execution-client image; the router is installed from the signed
platform archive under its hardened systemd unit.

The publication tail is resumable for the same signed release tag. It refreshes
assets on the exact existing draft, replaces same-name workflow artifacts on a
full rerun, and accepts an existing semantic container tag only
when that tag already resolves to the expected signed digest. Use **Re-run
failed jobs** after a transient failure; **Re-run all jobs** is also safe and
uses a new attempt-scoped staging namespace. A different digest, a changed
draft seal, a published GitHub Release, multiple matching releases, or an
unreadable registry state fails closed. Run-scoped staging tags are retained as
an audit trail and are never deployment inputs. Exclusive package write access
remains a release prerequisite because GHCR does not offer an atomic
create-only tag operation.

Publishing the draft promotes the corresponding non-prerelease image to
`ghcr.io/telosnetwork/telos-reth:latest`. Release candidates never update
`latest`.

## Verify artifacts

Download every release asset into one directory, install `cosign`, and run.
The verification script fails closed if `cosign` is unavailable:

```bash
TELOS_VERSION=0.1.0 scripts/release/verify-assets.sh ./release-assets
gh attestation verify \
  ./release-assets/telos-reth-0.1.0-x86_64-unknown-linux-gnu.tar.gz \
  --repo telosnetwork/telos-reth-2
```

Verify the image by immutable digest, not by a mutable tag:

```bash
cosign verify \
  --certificate-identity-regexp \
  '^https://github.com/telosnetwork/telos-reth-2/.github/workflows/(release|release-promote)\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ghcr.io/telosnetwork/telos-reth@sha256:<digest>
```

The archive `BUILD-METADATA` file identifies both the Telos source commit and
the exact upstream Paradigm Reth release/commit, plus the execution, bootstrap, and router
digests. Match those values against the release notes before deploying.

## Rollback and promotion

Deployments must pin the release image digest. Roll back by restoring the prior
known-good digest; do not rebuild an old source tag. If `latest` must be moved
back, manually dispatch `telos / promote` with the published stable version and
the explicit confirmation value. The protected environment records and gates
that action.

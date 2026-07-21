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
  administrators from bypassing approval.
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
exceptions in `deny.toml`.

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
generates SPDX SBOMs, creates deterministic archives and checksums, produces
GitHub build attestations, signs files with Sigstore keyless signing, publishes
a multi-platform GHCR image, scans it, and creates a draft GitHub Release.

Publishing the draft promotes the corresponding non-prerelease image to
`ghcr.io/telosnetwork/telos-reth:latest`. Release candidates never update
`latest`.

## Verify artifacts

Download every release asset into one directory, install `cosign`, and run:

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
the exact upstream Paradigm Reth release/commit. Match those values against the
release notes before deploying.

## Rollback and promotion

Deployments must pin the release image digest. Roll back by restoring the prior
known-good digest; do not rebuild an old source tag. If `latest` must be moved
back, manually dispatch `telos / promote` with the published stable version and
the explicit confirmation value. The protected environment records and gates
that action.

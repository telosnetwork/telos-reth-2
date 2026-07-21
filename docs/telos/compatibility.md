# Telos compatibility matrix

Production deployments must pin an execution client, companion consensus client, database format,
and bootstrap snapshot as one reviewed set. Floating branches, mutable container tags, and snapshots
created by a different storage schema are not supported deployment inputs.

## Development baseline

| Component | Pinned reference | Status |
| --- | --- | --- |
| Telos Reth 2 | This repository at the candidate commit | Not production-qualified |
| Upstream Reth | `v2.4.1` / `8eb210175687c9f0c889a3b6795c16781d830e3a` | Source baseline |
| `telos-consensus-client` | `agent/reth-v2-sidecars` / `3aae1cadfdd0129c58abe7ab8277fa800ef299fd` | Immutable candidate; paired live qualification pending |
| Telos extra fields | V3 execution metadata, second `engine_newPayloadV1` parameter | Implemented by the pinned candidate pair |
| Telos EVM backend | Isolated revm 41 port; startup gate closed | Exact-build live qualification required |
| Reth database | Storage V2 created by this client version | Restore-test required |
| Bootstrap snapshot | Mainnet sparse candidate at EVM block `479294328`; manifest SHA-256 `c3517da39d0ee8003434ce1e8ed5f304562a86656da1c666b1609a9ea2ae342e` | Import, catch-up, restore, and live parity qualification pending |

The companion reference above is an exact signed candidate commit, not a floating deployment
recommendation. It becomes an eligible production pair only after its required CI and artifact
build pass and that exact companion artifact completes checkpoint catch-up, restart, and live parity
qualification with the exact Telos Reth artifact.

## Required release record

Every Telos release must publish this matrix in its release notes with all placeholders resolved:

| Field | Required value |
| --- | --- |
| Telos Reth image | Registry path and immutable digest |
| Telos Reth source | Signed `telos-v*` tag and commit |
| Upstream provenance | Stable Reth tag and commit |
| Companion client | Repository, immutable tag, and commit |
| Engine extension | Schema version and compatibility-test artifact |
| Database | Storage version and migration boundary |
| Mainnet snapshot | Remote object ID, block/hash, size, checksum, and creation tool version |
| Testnet snapshot | Remote object ID, block/hash, size, checksum, and creation tool version |
| Restore evidence | Restore-drill date, elapsed time, resulting head/hash, and approver |
| Rollback target | Prior image digest and compatible snapshot |

Do not start a node by restoring a Reth 1.x database or an upstream Ethereum snapshot into this
client. A snapshot is eligible only after restoration into the exact candidate binary, database
consistency checks, companion catch-up, and canonical receipt/state sampling all pass.

## Compatibility change policy

Changes to the Engine extension, chain specifications, Reth storage version, revm, receipt encoding,
or state-reconciliation hooks require a new compatibility entry and a fresh restore/replay test.
An upstream patch update still requires the Telos suite because execution and persistence changes
can affect reconciliation even when the Telos crates are unchanged.

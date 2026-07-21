# Telos compatibility matrix

Production deployments must pin an execution client, companion consensus client, database format,
and bootstrap snapshot as one reviewed set. Floating branches, mutable container tags, and snapshots
created by a different storage schema are not supported deployment inputs.

## Development baseline

| Component | Pinned reference | Status |
| --- | --- | --- |
| Telos Reth 2 | This repository at the candidate commit | Not production-qualified |
| Upstream Reth | `v2.4.1` / `8eb210175687c9f0c889a3b6795c16781d830e3a` | Source baseline |
| `telos-consensus-client` | `master` / `9fadee1fd565e3a7ad51c1142e2673df52bd9028` | Protocol reference only |
| Telos extra fields | v1, second `engine_newPayloadV1` parameter | Development contract |
| Telos EVM backend | revm 41 port not yet implemented | Production blocker |
| Reth database | Storage V2 created by this client version | Restore-test required |
| Bootstrap snapshot | None qualified yet | Production blocker |

The companion reference above documents the existing two-parameter Engine API call. It is not a
release recommendation. The first production candidate must replace it with an immutable,
reviewed companion tag or commit and record the passing contract-test and replay evidence.

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

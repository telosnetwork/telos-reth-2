# Telos compatibility matrix

Production deployments must pin an execution client, companion consensus client, database format,
and bootstrap snapshot as one reviewed set. Floating branches, mutable container tags, and snapshots
created by a different storage schema are not supported deployment inputs.

## Development baseline

| Component | Pinned reference | Status |
| --- | --- | --- |
| Telos Reth 2 | This repository at the candidate commit | Canonical execution capability enabled; production eligibility comes from the signed release record |
| Upstream Reth | `v2.4.1` / `8eb210175687c9f0c889a3b6795c16781d830e3a` | Source baseline |
| `telos-consensus-client` | `main` / `8a3000cd83b2d1c3d84c812517dd888995f2eee0` | Retains execution branches for restart/reorg recovery, accepts required Engine capabilities as a subset of a backend capability superset, and preserves Engine RPC errors and clean shutdown; release approval requires passing CI, a byte-identical artifact rebuild, and paired live evidence |
| Telos extra fields | V3 execution metadata, second `engine_newPayloadV1` parameter | Implemented by the pinned candidate pair |
| Telos EVM backend | Isolated revm 41 port; canonical startup gate open | Exact-build live qualification and signed release approval required; diagnostic replay remains closed |
| Reth database | Storage V2 created by this client version | The signed release record must carry restore-test evidence |
| Bootstrap snapshot | Mainnet sparse candidate at EVM block `479294328`; manifest SHA-256 `c3517da39d0ee8003434ce1e8ed5f304562a86656da1c666b1609a9ea2ae342e` | The signed release record must carry import, catch-up, restore, and live parity evidence |
| Historical RPC | `telos-rpc-router` plus an independently copied and fed archive | Boundary and historical block/state/receipt/log witnesses gate compatibility; the incumbent remains untouched during qualification but is not the router backend |

The companion reference above is an exact signed commit, not a floating deployment recommendation.
A release may pair it only when the release record proves that its required CI and artifact build
passed and that exact companion artifact completed checkpoint catch-up, restart, and live parity
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

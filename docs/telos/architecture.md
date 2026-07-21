# Telos execution architecture

This repository is a Telos execution client built from the unmodified Git history of upstream
Reth. The initial baseline is Reth `v2.4.1` at
`8eb210175687c9f0c889a3b6795c16781d830e3a`. The `upstream` Git remote must continue to point to
`https://github.com/paradigmxyz/reth` so every Telos release can be audited against an exact Reth
release.

## Consensus boundary

Telos EVM blocks are derived from Telos native state by `telos-consensus-client`. The companion
client submits two positional parameters in one JWT-authenticated request:

```text
engine_newPayloadV1(executionPayload, telosExtraFields)
```

The two parameters arrive as one authenticated request, but co-location alone does not
cryptographically bind the compatibility object to that payload. Extra fields must never be
accepted from a filesystem or an unauthenticated endpoint. Before production, a versioned
extension must identify the exact block and commit to its complete native state and receipt data so
a delayed or replayed object for another payload is rejected. The current schema lacks that binding,
which is one reason Telos startup remains disabled.

The currently deployed companion-client object is the Telos extra-fields v1 schema. All collection
fields required to execute a block must be present, even when empty:

- account changes;
- storage changes;
- addresses created by `create` and `openwallet`;
- one receipt per payload transaction.

Gas-price and EVM-revision transitions remain optional because most blocks do not contain either
transition. Future incompatible schemas require an explicit version negotiation; they must not be
silently accepted as v1.

## Validation invariants

The production Telos path must fail closed. A payload is invalid when its extra fields are absent,
malformed, oversized, replayed, bound to another block, duplicated inconsistently, or incomplete.
Provider and database failures are internal errors and must never be converted into empty accounts
or zero storage. The candidate enforces structural validation and provider-error handling; startup
remains gated until block binding and two-way completeness are implemented.

Reth still executes every payload transaction with revm. Native account and storage deltas are then
used as an authoritative reconciliation record, and native receipts are persisted. Reconciliation
must:

- compare bytecode bytes, not only bytecode length;
- hash EVM bytecode with Keccak-256;
- apply account and storage removals;
- prove that every locally executed account and storage mutation has an authoritative native row;
- retain original values so an in-memory or persisted reorg can unwind cleanly;
- reject unknown receipt types and require receipt count to equal transaction count;
- retain transaction-root, receipt-root, logs-bloom, gas-used, and structural Engine API checks.

Telos headers intentionally use an empty state-root placeholder and omit `baseFeePerGas`. The Telos
payload validator may make only the documented chain-specific exceptions needed for those legacy
fields and the native block-hash representation. Those exceptions are valid only for chain IDs 40
and 41; the stock `reth` binary remains the Ethereum client.

## Canonical chain anchors

Chain specifications are guarded by golden tests. The initial anchors are:

| Chain | ID | Genesis hash | Genesis timestamp | Genesis state root |
| --- | ---: | --- | ---: | --- |
| Telos mainnet | 40 | `0x36fe7024b760365e3970b7b403e161811c1e626edd68460272fcdfa276272563` | `0x5c114972` | `0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421` |
| Telos testnet | 41 | `0xb25034033c9ca7a40e879ddcc29cf69071a22df06688b5fe8cc2d68b4e0528f9` | `0x5d55db93` | `0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421` |

The accepted aliases include the established `tevmmainnet` and `tevmtestnet` names used by the
installer, plus the clearer `telos-mainnet` and `telos-testnet` names.

## Production boundary

The authenticated Engine API and Telos native RPC remain bound to loopback or a private service
network. Public JSON-RPC is deployed behind TLS, method allowlists, request limits, and rate limits.
Signer material and the Engine JWT are read from root-owned credential files and never passed in
process arguments or committed configuration.

A release is eligible for production promotion only after:

1. formatting, lint, unit, integration, dependency, and reproducible-build checks pass;
2. companion-client contract tests pass with the exact release pair;
3. replay produces matching canonical block hashes, receipts, logs, and sampled state against a
   trusted Telos endpoint;
4. forced restart and shallow/deep reorg tests preserve state and receipt parity;
5. a testnet soak and then a shadow-mainnet soak complete without divergence;
6. rollback artifacts and a verified remote snapshot are available.

Passing repository CI alone is necessary but not sufficient for production promotion.
Every candidate must also complete the [compatibility matrix](./compatibility.md); a compatible,
restore-tested Reth Storage V2 snapshot is currently an explicit launch blocker.

The current candidate also has an explicit
[Telos EVM execution-compatibility gate](./execution-compatibility.md). It must not be removed until
the Telos revm semantics are ported to the upstream revm version and replay-proven.

## Upstream maintenance

Upstream updates are rebased as auditable merge or cherry-pick series from signed stable Reth tags.
Each update records the upstream tag and commit, reruns the complete Telos compatibility suite, and
ships as a new Telos release. Telos changes should remain isolated in Telos crates and narrow,
documented extension points so upstream security fixes can be adopted quickly.

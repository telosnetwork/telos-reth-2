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

The candidate requires V3 execution metadata in the second parameter. It names the exact block and
parent hashes, transaction count, payload-only execution base fee, starting gas price and revision,
and every ordered in-block context change. The complete, chain-bound extension is canonicalized,
hashed, and persisted before Engine dispatch, so a delayed or replayed object for another payload
or chain is rejected. Extra fields are never accepted from a filesystem or unauthenticated
endpoint.

The historically deployed companion used the Telos extra-fields v1 schema; that object does not
satisfy this candidate's execution contract. The exact companion paired with this client must send
V3 metadata, and all collection fields required to execute a block must be present, even when empty:

- account changes;
- storage changes;
- addresses created by `create` and `openwallet`;
- one receipt per payload transaction.

The legacy scalar gas-price and revision fields are rejected. V3 carries ordered change lists,
which may be empty, and defines a boundary equal to the transaction count as the starting context
for the child. Future incompatible schemas require explicit version negotiation; they must not be
silently accepted as V3.

## Validation invariants

The production Telos path must fail closed. A payload is invalid when its extra fields are absent,
malformed, oversized, replayed, bound to another block, duplicated inconsistently, or incomplete.
Provider and database failures are internal errors and must never be converted into empty accounts
or zero storage. The candidate implements block binding, two-way state/receipt/gas reconciliation,
provider-error handling, and durable pending/dispatched/accepted sidecar lifecycle tests. Canonical
startup may be enabled only on an exact qualification candidate or a release whose signed record
proves those invariants with the exact build and companion against live canonical data.

Reth still executes every payload transaction with revm. Native account and storage deltas and
native receipts are authenticated validation records; they must never be used to overwrite a
different local result. The only post-execution state effect currently specified is nonce-one
materialization for a terminal native `create` event. Reconciliation must:

- compare bytecode bytes, not only bytecode length;
- hash EVM bytecode with Keccak-256;
- apply account and storage removals;
- prove that every locally executed account and storage mutation has an authoritative native row;
- reject any account, storage, receipt, or gas mismatch instead of correcting local execution;
- retain original values so an in-memory or persisted reorg can unwind cleanly;
- reject unknown receipt types and require receipt count to equal transaction count;
- retain transaction-root, receipt-root, logs-bloom, gas-used, and structural Engine API checks.

This release accepts only authenticated type-0 transactions. Canonical receipts therefore use the
standard untyped legacy RLP encoding, and JSON-RPC reports the transaction-indexed effective price:
the signed gas price capped by the native fixed price authenticated by the payload. Authenticated
typed envelopes and their receipt conversion fail closed until a separately named activation is
implemented and qualified.

Telos headers intentionally use an empty state-root placeholder and omit `baseFeePerGas`. The Telos
payload validator may make only the documented chain-specific exceptions needed for those legacy
fields and the native block-hash representation. Those exceptions are valid only for chain IDs 40
and 41; the stock `reth` binary remains the Ethereum client.

Telos native blocks are scheduled 500 ms apart, while Engine API header timestamps have whole-second
precision. Consecutive Telos EVM headers therefore require nondecreasing timestamps: equality is
valid, but a child timestamp below its parent remains invalid. This exception is enabled only by
the Telos node consensus builder; Ethereum nodes retain Reth's strictly increasing rule.

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
restore-tested Reth Storage V2 snapshot is an explicit launch prerequisite.

The client also has an explicit
[Telos EVM execution-compatibility gate](./execution-compatibility.md). The revm 41 port is present;
the canonical capability may be enabled for exact qualification, but production eligibility comes
only from a signed release record proving live companion ingestion, restart/reorg behavior, and
finalized parity. Historical replay and diagnostic RPC have a separate gate that remains closed
after canonical forward execution is qualified.

The bootstrap database is sparse. Public historical RPC therefore uses the
[retained-history router](./history-routing.md) with a still-live incumbent side by side. The
router is not an archive and does not authorize incumbent retirement; the incumbent remains the
backend for pre-boundary history, filter lifecycle, and `eth_feeHistory`.

## Upstream maintenance

Upstream updates are rebased as auditable merge or cherry-pick series from signed stable Reth tags.
Each update records the upstream tag and commit, reruns the complete Telos compatibility suite, and
ships as a new Telos release. Telos changes should remain isolated in Telos crates and narrow,
documented extension points so upstream security fixes can be adopted quickly.

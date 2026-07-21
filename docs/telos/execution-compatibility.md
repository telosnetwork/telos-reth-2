# Telos EVM execution compatibility

## Current blocker

The Telos Reth binary is intentionally fail closed for chain IDs 40 and 41 until the Telos EVM
semantics are ported to the revm version used by upstream Reth 2.4.1 and validated by replay.
Building successfully is not evidence that the execution backend is compatible.

The production Telos Reth 1.x line used `telosnetwork/telos-revm` branch `telos-main` at
`1706d6bea3f2771e4603e827994038a8786d1256` (revm 14). Its newest development branch is based on
revm 18. Upstream Reth 2.4.1 uses revm 41, whose standard Ethereum transaction environment does not
contain the Telos fields or behavior.

The removed v2 experiment used standard revm and compensated by skipping execution checks and
trusting a filesystem state-diff side channel. That design is not carried forward: it could accept
missing or partial state, was not payload-authenticated, and silently lost important execution
invariants.

## Semantics that must be ported

The implementation and its tests must cover at least:

- the native fixed gas price, including `GASPRICE`, up-front balance checks, deduction,
  reimbursement, and beneficiary behavior;
- the persisted native revision number and its transaction-indexed changes;
- revision-dependent new-address behavior for account/code/storage inspection, calls, and creates;
- native `create` and `openwallet` allocation timing;
- Telos legacy chain-ID and nonce exceptions still present in canonical history;
- historical EVM revision selection, including all opcode and call-context differences;
- canonical reorg and restart behavior for the persisted gas price and revision state.

The legacy chain-ID-3 transaction format also needs chain-wide support. Those transactions do not
use secp256k1 sender recovery: the sender is the high 160 bits of the signature `s` value. The live
Engine payload path implements and tests that extraction, but production also requires the same
rule in stored-block execution, sender-recovery stages, backfill, pruning, provider reads, and
debug/trace RPC. Public transaction-pool admission must continue to reject this forgeable native
format.

Gas price and revision are fork-specific chain state, not process-global settings. A production
implementation must persist each block's starting values and ordered transaction-boundary changes,
derive them from the exact parent hash, and restore them across sidechains, unwind, restart, and
historical replay. Boundary `transaction_count` means after the final transaction and establishes
the child's starting state.

Account diffs and receipts are too large to place in a header. If a stored block can be reexecuted,
the complete versioned extension must be persisted in a block-hash-keyed sidecar with a payload
commitment and duplicate-delivery consistency check. Otherwise all paths that cannot reconstruct
the extension must remain explicitly disabled. The current live-payload reconciliation is not a
substitute for that replay design.

Reconciliation must also prove completeness in both directions. Every account or storage change
made by local execution must have a corresponding authoritative native delta; applying only the
rows that the companion supplied would leave an erroneous extra local mutation in state. A mismatch
must reject the block, not merely overlay the supplied rows.

The Engine extension currently rejects gas-price or revision changes, and startup rejects Telos
execution, so an operator cannot accidentally mistake the incomplete backend for a production
client.

revm 41 has public transaction, handler, instruction-table, frame, and EVM-factory extension
points, so these semantics can remain isolated in Telos crates without maintaining a full revm
fork. The Telos node must disable revmc JIT until a separate JIT implementation is proven to apply
the same handler and opcode rules.

## Exit criteria

Remove the startup gate only in the same reviewed change that supplies:

1. an immutable, audited revm 41-compatible implementation with Telos-specific code isolated from
   the upstream Ethereum path;
2. Telos transaction and block primitives that apply chain-ID-3 recovery consistently in every
   execution, storage, networking, and RPC path;
3. block-hash-keyed persistence for gas/revision schedules and replay-required Engine sidecars,
   including schema/version and payload-integrity checks;
4. differential tests against the pinned Telos Reth 1.x/revm implementation for every semantic
   listed above;
5. native golden vectors that resolve legacy ambiguity around boundary indexing, multiple changes,
   future `BLOCKHASH`, zero-address burn/revert behavior, and synthetic transaction chain IDs;
6. replay across every historical gas-price and revision transition on mainnet and testnet;
7. transaction, receipt, logs, and sampled state parity from the qualified Storage V2 snapshot to
   the current finalized head;
8. forced shallow/deep reorg and crash/restart tests that preserve the native execution context;
9. an independent security and consensus-correctness review.

Until those artifacts exist, this repository is a hardened development baseline and release
candidate framework, not a production-promotable Telos execution client.

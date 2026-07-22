# Telos EVM execution compatibility

## Current qualification gate

The Telos EVM semantics are ported to the revm 41 interfaces used by upstream Reth 2.4.1. The port
is isolated in the Telos transaction environment, handler, instruction table, frame, block
executor, receipt converter, and EVM factory, and focused tests exercise the known legacy behavior.
`TELOS_REVM_EXECUTION_READY` is true only in an exact qualification candidate or an approved
release so chain IDs 40 and 41 can perform canonical execution. That capability bit is not
production approval: a build becomes eligible only when its signed release record identifies the
exact artifacts and attaches reviewed checkpoint, companion, restart/reorg, and finalized-RPC
evidence. A separate `TELOS_RPC_REPLAY_READY` gate remains false for historical replay and
diagnostic RPC.

The production Telos Reth 1.x line used `telosnetwork/telos-revm` branch `telos-main` at
`1706d6bea3f2771e4603e827994038a8786d1256` (revm 14). Its newest development branch is based on
revm 18. Upstream Reth 2.4.1 uses revm 41, whose standard Ethereum transaction environment does not
contain the Telos fields or behavior.

The checked-in `legacy-telos-revm-237d6322.v1.json` artifact is only a source-diff inventory for the
newer revm-18 development commit `237d6322c6f5943af77fccff93fd0f85ecc204ed`. Its tests verify blob
and patch hashes, fixture constants, and the presence of named port tests; they do not execute either
legacy revm or compare runtime outputs. It is therefore neither production provenance nor a
differential test oracle. Passing the focused port tests proves the behavior of this implementation,
not parity with production history. An approved release record must include evidence from an
immutable oracle built from the production commit above that matches transaction, receipt, gas,
log, and complete state outputs against the exact revm-41 candidate.

The removed v2 experiment used standard revm and compensated by skipping execution checks and
trusting a filesystem state-diff side channel. That design is not carried forward: it could accept
missing or partial state, was not payload-authenticated, and silently lost important execution
invariants.

## Implemented compatibility surface

The port and its focused tests cover:

- the authenticated native fixed-price cap (`min(signed gas price, native price)`), including
  `GASPRICE`, up-front balance checks, deduction, reimbursement, and beneficiary behavior;
- the persisted native revision number and its transaction-indexed changes;
- revision-dependent new-address behavior for account/code/storage inspection, calls, and creates;
- native `create` and `openwallet` allocation timing;
- Telos legacy chain-ID and nonce exceptions still present in canonical history;
- historical EVM revision selection, including all opcode and call-context differences;
- canonical reorg and restart behavior for the persisted gas price and revision state;
- strict authenticated type-0-only admission, with all typed envelopes rejected; and
- Telos's legacy receipt-trie encoding and JSON-RPC `effectiveGasPrice` derived from the
  authenticated transaction-indexed native fixed gas price.

The legacy chain-ID-3 transaction format does not use secp256k1 sender recovery: the sender is the
high 160 bits of the signature `s` value. The Engine payload and stored-block execution paths apply
that rule, canonical startup requires a durable sender row for every retained transaction, and
sender-recovery pruning is rejected. Public transaction-pool admission does not use this
authenticated native compatibility path. Historical debug/trace RPC remains disabled by the
independent replay gate.

Canonical ingestion and the optional public `eth_sendRawTransaction` native forwarder are both
limited to protected type-0 transactions. On 2026-07-21, a read-only scanner built against the exact
legacy production checkout `8c37741ea8d97eba713a8028e3f09132bb51abd6` decoded every transaction
available at the snapshot high-water marks: mainnet transaction numbers `0..=9703306` (9,703,307
transactions, through block `479307914`) and testnet transaction numbers `0..=3749000` (3,749,001
transactions, through block `435553362`). Both canonical sets contained zero type-2 transactions;
live testnet block 54 was empty, confirming that the companion's block-54 type-2 vector is synthetic.
Telos's production announcement also states that EIP-1559 remained
[under development and unsupported](https://telos.net/posts/upgrade-announcement-telos-evm-2-0-deployment-and-migration-guidelines).
Operators must not advertise typed-transaction support until a named native activation, ingestion
path, and receipt behavior are implemented and qualified together.

The reproducible ranges and scanner provenance are preserved in the
[canonical transaction-type audit](./transaction-type-audit.md).

Gas price and revision are fork-specific chain state, not process-global settings. V3 metadata and
the durable sidecar store persist each block's starting values and ordered transaction-boundary
changes, bind them to the exact parent hash, and validate continuity across forks, unwind, and
restart. Boundary `transaction_count` means after the final transaction and establishes the child's
starting state. Historical replay before the checkpoint boundary remains outside the enabled RPC
surface until separately qualified.

Account diffs and receipts are too large to place in a header. The candidate canonicalizes the
complete versioned extension into a chain- and payload-bound, block-hash-keyed sidecar. It persists
the exact digest before Engine dispatch, promotes only that digest after `VALID`, rejects conflicting
accepted metadata, and retains accepted sidecars for stored-block execution and RPC receipts.
Startup audits canonical coverage, sidecar continuity, readable state, and durable senders. Paths
that cannot prove this context, including pre-anchor diagnostic replay, remain disabled.

Reconciliation must also prove completeness in both directions. Every account or storage change
made by local execution must have a corresponding authoritative native delta; applying only the
rows that the companion supplied would leave an erroneous extra local mutation in state. Account,
storage, receipt, and gas mismatches must reject the block, not overlay or replace local execution.
Only separately specified native effects, currently terminal `create` nonce-one materialization,
may change state after validation.

The Engine extension accepts ordered gas-price and revision changes only through payload-bound V3
metadata; the ambiguous legacy scalar fields remain rejected. Opening canonical startup only on an
exact qualification candidate makes the live checks possible without treating an unqualified build
as a production-approved client.

revm 41's public transaction, handler, instruction-table, frame, and EVM-factory extension points
keep these semantics isolated in Telos crates without a full revm fork. The Telos node disables
revmc JIT; enabling it requires a separate implementation proven to apply the same handler and
opcode rules.

## Exit criteria

An isolated, loopback-only qualification candidate may enable `TELOS_REVM_EXECUTION_READY` so the
live checks can run. Do not tag or deploy that candidate as production until the resulting evidence
is reviewed and attached to its signed release record. Production promotion requires:

1. an immutable signed build of the isolated revm 41 implementation and exact companion pair;
2. differential outputs from the pinned Telos Reth 1.x/revm implementation for the semantics above,
   not only source-diff inventory and self-consistent constants;
3. replay across every historical gas-price and revision transition in the agreed mainnet and
   testnet corpus;
4. transaction, receipt, logs, and sampled state parity from the qualified Storage V2 checkpoint to
   the current finalized head;
5. forced shallow/deep reorg and crash/restart tests that preserve sidecar lifecycle and native
   execution context;
6. an independent security and consensus-correctness review plus the operational promotion evidence
   required by the compatibility matrix.

Until that evidence exists, the port is an implemented and tested release candidate, not a
production-approved Telos execution client. Qualifying canonical forward execution does not by
itself open `TELOS_RPC_REPLAY_READY`; replay, `debug`, `trace`, and `ots` remain disabled until their
own historical parity evidence is complete.

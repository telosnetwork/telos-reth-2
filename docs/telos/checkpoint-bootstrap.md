# Telos sparse checkpoint bootstrap

This procedure creates a **post-anchor-only** Telos Reth database. It is suitable for a shadow RPC
and then a production RPC whose advertised history starts at the checkpoint. It does not create a
full archive node and must not be presented as one.

To preserve older RPC history, deploy it with the independently copied archive and
[retained-history router](./history-routing.md). The incumbent remains untouched during
qualification but is not the router backend.

Telos' historical canonical headers carry `EMPTY_ROOT_HASH` even when EVM state is non-empty. The
normal Reth `init-state` command therefore rejects them, correctly, and remains unchanged. The
Telos-only importer instead requires all of the following to agree before it writes a completion
audit:

- the public chain ID and canonical block-zero genesis hash;
- the exact nonzero anchor number, canonical header RLP, and header hash;
- the header's historical `EMPTY_ROOT_HASH` placeholder;
- an empty transaction/receipt body at the finalized anchor (select an empty EVM block);
- the SHA-256 of an immutable JSONL dump;
- the SHA-256 of its exact-legacy export evidence, source-copy manifest, and copied `mdbx.dat`;
- the canonical native chain ID, irreversible native anchor and exact first child, with both native
  IDs cross-checked against the corresponding EVM headers' `extraData`;
- the first-child gas price and revision decoded from the legacy anchor header extension, never
  entered manually;
- a separately pinned, non-empty trie root declared by the dump; and
- the root recomputed from all imported accounts and storage in the new Reth v2 database.

The anchor header is the first available block in this sparse database and therefore its
database-local genesis hash. The manifest retains the public Telos block-zero identity separately.
P2P is disabled by the Telos node, so this checkpoint identity cannot be advertised as the public
network's devp2p genesis.

## Required artifacts

The signed platform release archive includes `telos-checkpoint-bootstrap` and
`scripts/telos/checkpoint/`. The exporter used for this migration is deliberately built inside a
disposable checkout of exact legacy commit `8c37741ea8d97eba713a8028e3f09132bb51abd6` by
`legacy-extractor/build-exact-legacy-extractor.sh`; a current-Reth exporter cannot decode the
legacy database by assumption. The legacy `mdbx_copy`/`mdbx_chk` pair is also separate because it
must come from the exact older libmdbx source that owns the source database.

Keep these immutable and checksum/sign the complete set before promotion:

1. `mdbx-copy.json`, `mdbx-check.log`, and the compact copied `mdbx.dat`, plus the exact
   `mdbx_copy` and `mdbx_chk` binary digests recorded by that manifest;
2. `telos-legacy-checkpoint-export`, both preserved Cargo lockfiles, and
   `legacy-extractor.provenance.json` from the exact-codec build;
3. `state.jsonl` and `state.legacy-evidence.json` from one read transaction on the immutable copy;
4. `native-anchor.attestation.json`, produced against authenticated EVM and nodeos endpoints after
   the exact native first child is irreversible;
5. `checkpoint.json` (the trusted dual-root and native-boundary manifest);
6. `checkpoint.anchor.json` and `checkpoint.audit.json`, written only after a successful import;
7. the resulting Reth v2 data directory.

The exact-legacy exporter refuses a live database path. It accepts only the canonical backup path
whose `mdbx.dat` size and SHA-256 match `mdbx-copy.json`, where the copy method is the standalone
compact `mdbx_copy` and an `mdbx_chk` run succeeded. Both tools must be built from the exact
vendored libmdbx revision used by the legacy node; their binary hashes and `mdbx_copy` version
output are recorded. Before copying, the script asks the pinned legacy binary to resolve
`--source-datadir` plus `--chain` and requires that result to equal `--source-db`; the manifest also
records the requested canonical chain, the legacy `tevmmainnet`/`tevmtestnet` alias used for the
probe, canonical data-directory/database paths, and all verification-binary hashes. This flow is
intentionally limited to the legacy storage-v1 layout; an MDBX-only copy of
a storage-v2 node is rejected because its authoritative data may be split across static files and
RocksDB.

## 1. Take a transactionally consistent hot copy

Run this on the legacy RPC host. The node may stay online. Legacy 1.0.8 does not provide a `db copy`
subcommand: use the standalone `mdbx_copy` and `mdbx_chk` built from that release's exact vendored
libmdbx source. The wrapper maps `telos-mainnet` to the legacy `tevmmainnet` selector (and testnet
to `tevmtestnet`) only for the read-only `db path` identity probe.

```bash
scripts/telos/checkpoint/create-hot-mdbx-copy.sh \
  --legacy-reth /opt/telos/bin/telos-reth \
  --source-datadir /srv/telos-reth \
  --source-db /srv/telos-reth/40/db \
  --chain telos-mainnet \
  --backup-db /srv/telos-checkpoints/run-001/checkpoint-mdbx \
  --mdbx-copy /opt/telos/db-tools/mdbx_copy \
  --mdbx-chk /opt/telos/db-tools/mdbx_chk
```

Do not replace this with separate `db list`, RPC account, storage, or bytecode reads against the
live node. Those calls do not share one MDBX read transaction and can mix different chain states.

Take the copy when the aligned execution checkpoint is a finalized EVM block with no transactions,
receipts, ommers, logs bloom, or gas used; this lets Reth represent the checkpoint header without
inventing a missing anchor body. The exact-legacy exporter reads that header and its Telos header
extension directly from the copied transaction and checks its hash against `CanonicalHeaders`.
Retry with a new hot copy if the selected execution tip is not sparse-anchor safe.

## 2. Build and run the exact-legacy extractor

Use a disposable, clean checkout at the pinned legacy commit; the build helper intentionally adds
one temporary binary target to that worktree. Preserve its provenance output with the checkpoint.

```bash
scripts/telos/checkpoint/legacy-extractor/build-exact-legacy-extractor.sh \
  --legacy-worktree /srv/build/telos-reth-legacy-8c37741 \
  --output-dir /srv/telos-checkpoints/run-001/exact-legacy-extractor
```

The export holds one read transaction on the immutable copy. Every account's bytecode is loaded by
its recorded table key; every storage duplicate is streamed into the same JSONL account record.
If a legacy account's recorded bytecode key differs from `keccak256(code)`, the dump preserves the
recorded `codeHash` explicitly and records both hashes in the signed export evidence. This exception
is accepted only by the verified Telos placeholder-root import path; ordinary Ethereum state-dump
imports still reject explicit bytecode hashes. Account hashing, storage hashing, and Merkle
checkpoints must equal the copied execution checkpoint.

```bash
/srv/telos-checkpoints/run-001/exact-legacy-extractor/telos-legacy-checkpoint-export \
  --backup-manifest /srv/telos-checkpoints/run-001/checkpoint-mdbx/mdbx-copy.json \
  --output /srv/telos-checkpoints/run-001/state.jsonl
```

This creates `state.jsonl` and `state.legacy-evidence.json`. The first JSONL line declares the real
trie root derived through the exact legacy hashed-state/trie codecs; the remaining lines contain
complete account, storage, bytecode, and any exact legacy bytecode-key overrides. The evidence also
binds the canonical header RLP, aligned stage heights, native block ID, and the gas price/revision
effective for the first child. The new database independently rebuilds the trie from those lines.

Do not substitute the current release's exporter, relax the pinned commit/lock checks, or migrate
the only production database read-write merely to make an extractor work.

## 3. Attest the irreversible native boundary

Use authenticated endpoints for the same canonical network. The EVM endpoint must retain the
anchor and first child; nodeos must report the matching native blocks and a last irreversible block
at or beyond the first child. Plain HTTP is accepted only on loopback, and redirects are rejected.

```bash
python3 scripts/telos/checkpoint/attest-native-anchor.py \
  --legacy-evidence /srv/telos-checkpoints/run-001/state.legacy-evidence.json \
  --evm-rpc-url https://trusted-evm.example \
  --nodeos-url https://trusted-nodeos.example \
  --output /srv/telos-checkpoints/run-001/native-anchor.attestation.json
```

The attestor requires the EVM anchor hash and native ID to match the exact-legacy evidence, then
requires the exact EVM and native successors to extend those anchors. Both native IDs must encode
their claimed block numbers, the EVM headers' `extraData` must contain those IDs, and the first
native child must already be irreversible.

## 4. Build the trusted manifest

The manifest builder takes no gas-price or revision arguments. Those values are decoded by the
exact-legacy codec from the anchor header extension and cross-checked through the attestation before
being copied into both the execution and native boundary records.

```bash
python3 scripts/telos/checkpoint/build-checkpoint-manifest.py \
  --network telos-mainnet \
  --legacy-evidence /srv/telos-checkpoints/run-001/state.legacy-evidence.json \
  --native-anchor-attestation /srv/telos-checkpoints/run-001/native-anchor.attestation.json \
  --state-dump /srv/telos-checkpoints/run-001/state.jsonl \
  --output /srv/telos-checkpoints/run-001/checkpoint.json
```

Review and sign `checkpoint.json` out of band before import. A valid manifest is powerful: it
authorizes the exact checkpoint header and state dump.

## 5. Import into a new Reth v2 directory

Use a new explicit data directory on the shadow host. The resolved data directory, static-files
directory, and RocksDB directory must not exist before the command starts; this also applies to
paths supplied with `--datadir.static-files` or `--datadir.rocksdb`. The bootstrap refuses storage
v1 and existing audit/anchor outputs.

```bash
./telos-checkpoint-bootstrap \
  --chain telos-mainnet \
  --datadir /srv/telos-reth-2-shadow \
  --storage.v2=true \
  --manifest /srv/telos-checkpoints/run-001/checkpoint.json \
  --state /srv/telos-checkpoints/run-001/state.jsonl
```

After importing, the bootstrap closes every writable backend, reopens the database read-only,
recomputes the root from hashed state without trusting cached trie nodes, verifies the persisted
trie root, and checks stage, static-file, and RocksDB placement at the anchor. Only then does it
atomically write `checkpoint.anchor.json` and `checkpoint.audit.json` beside the manifest. The audit
records that recomputed root and is the completion marker. If the command fails or is interrupted,
discard the entire new shadow data directory and retry; the state import is intentionally not
resumable or idempotent.

## 6. Start a loopback-only shadow RPC

The checkpoint chain parser requires the exact sibling `checkpoint.audit.json`; a missing or
mismatched audit fails before the node opens. Keep diagnostic replay namespaces disabled until the
Telos replay qualification gate is complete.

```bash
./telos-reth node \
  --chain telos-checkpoint:/srv/telos-checkpoints/run-001/checkpoint.json \
  --datadir /srv/telos-reth-2-shadow \
  --storage.v2=true \
  --telos.execution-anchor /srv/telos-checkpoints/run-001/checkpoint.anchor.json \
  --http \
  --http.addr 127.0.0.1 \
  --http.port 18545 \
  --http.api eth,net,web3 \
  --authrpc.addr 127.0.0.1 \
  --authrpc.port 18551 \
  --authrpc.jwtsecret /srv/telos-reth-2-shadow/jwt.hex \
  --ipcdisable
```

This command uses the no-peer Telos network implementation, does not configure a transaction
signer, does not invoke systemd, and uses only the explicit shadow data directory and loopback
ports. It therefore cannot mutate an existing service's database or forward transactions. Confirm
the two ports and the JWT file are unique before launch. Run this only with the exact, auditable
candidate in which `TELOS_REVM_EXECUTION_READY` permits canonical startup. That capability enables
the isolated shadow checks but is not production approval; promotion requires the resulting live
evidence and signed release record. Keep `TELOS_RPC_REPLAY_READY` false throughout this
qualification so historical replay and diagnostic RPC remain unavailable.

Before any public promotion, compare a finalized window from the anchor forward against the legacy
RPC, restart the shadow node, repeat the comparison, exercise a bounded reorg, and restore the
database plus checkpoint artifacts on a second host. Never bind this rehearsal to an existing
production port.

For a managed service install, copy `checkpoint.json`, `checkpoint.audit.json`, and
`checkpoint.anchor.json` to `/etc/telos-reth/<instance>/` as `root:telos-reth-config` mode `0440`.
Set `CHECKPOINT_MANIFEST_SHA256` in `node.env` to the lowercase, non-prefixed SHA-256 of the installed
manifest. Preflight and every readiness run recheck that pin; snapshots bind and restore the whole
trio with the execution and companion databases.

## History boundary

Anchor-only history is viable for current state, canonical blocks ingested after the checkpoint,
logs/receipts produced after it, and state queries whose required history is at or after it. Calls
for blocks, transactions, receipts, logs, proofs, traces, or state before the anchor are not
available and should be documented as such at the edge.

A full historical RPC requires an independently verified import of canonical headers, bodies,
receipts, Telos embedded senders, transaction lookup, account/storage changesets and history
indices, plus replay sidecars for every retained block. Because historical Telos headers do not
commit the real state root, that import also needs Telos-specific replay/golden verification at
checkpoints; the sparse state dump alone cannot prove or reconstruct pre-anchor history.

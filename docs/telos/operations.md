# Telos Reth production operations

This runbook defines the minimum supported deployment for `telos-reth-2`. It deliberately fails
closed: an RPC process that answers HTTP is not necessarily healthy, and a local snapshot on the
same host is not a backup.

## Security and consistency model

Telos block production remains authoritative in nodeos. The pinned `telos-consensus-client`
companion translates the native result and submits the block, receipts, and typed reconciliation
data in one JWT-authenticated Engine API request. The V3 reconciliation envelope commits to the
exact payload, transaction senders, receipts, gas price, revision, and parent execution context.
Pending reconciliation is not accepted history: only an Engine `VALID` result can promote the
matching digest, while `INVALID` removes it and conflicting accepted metadata is immutable. The
Telos execution rules and lifecycle are implemented and covered by focused tests, but production
startup remains deliberately gated until the exact candidate passes live companion, restart/reorg,
and finalized-RPC parity qualification. The independent replay-safety gate remains closed.

There is no production filesystem state-diff side channel. In particular, do not recreate
`/tmp/telos-extra-fields`, do not allow a companion to publish unauthenticated JSON, and do not add
a permissive fallback when reconciliation data is absent. If a temporary compatibility spool is
ever reintroduced, it requires a separate threat model, an instance-specific directory outside
`/tmp`, atomic `fsync` plus rename, strict producer/consumer ownership, size and age bounds, and a
block-bound authenticated envelope. Such a deployment is not covered by this runbook.

The Engine API, nodeos API, metrics, and unproxied JSON-RPC listeners bind loopback. HTTP is always
enabled; WebSocket is disabled by default and, when explicitly enabled, is also fixed to loopback.
Public JSON-RPC is exposed only through a TLS reverse proxy with request-size limits, per-method
policy, connection limits, and abuse controls. Never publish port 8551, a JWT, a signer credential,
`debug`, `admin`, or unrestricted trace methods.

The supported production JSON-RPC namespace allowlist is `eth,net,web3`. In addition to the proxy
allowlist, the binary has an independent replay-safety gate: until historical Telos replay is
qualified, startup rejects `debug`, `trace`, and `ots` on HTTP or WebSocket and disables regular IPC
because upstream IPC exposes every namespace. It also removes `eth_callBundle`, `eth_callMany`,
`eth_simulateV1`, all four `eth_getBlockAccessList*` variants, and `mev_simBundle` from regular and
authenticated transports. The JWT-authenticated Engine API, including optional authenticated
Engine IPC, is configured separately and its core `engine_*` methods remain available to the
companion.

The transaction forwarder pins both network identities before using its signer: EVM chain 40 must
pair with native chain `4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11`,
and EVM chain 41 must pair with native chain
`1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f`. The preflight,
readiness check, and client all reject a mismatched nodeos endpoint. `TELOS_ENDPOINT` and
`NODEOS_URL` must be the same configured URL so health checks cannot validate a different native
service than the forwarder uses.

## Supported host contract

Use a dedicated Linux host or VM for each production instance. Do not place both HA replicas, the
only nodeos source, or remote backups on one failure domain. The reference assets require:

- systemd 252 or newer, Bash 5, GNU coreutils, util-linux, curl, jq, Python 3.11 or newer, restic, Prometheus,
  and node_exporter;
- a 64-bit little-endian Linux release artifact from this repository, verified against its
  published SHA-256 checksum;
- XFS created with `reflink=1` or Btrfs, with execution data, companion data, and local snapshot
  staging on the same filesystem; mount their common parent, because the two live data directories
  themselves must remain renameable directories rather than mount points;
- ECC memory, production NVMe with power-loss protection, time synchronization, and at least 20%
  sustained free space;
- an independently administered, encrypted off-host restic repository, preferably backed by
  object lock or immutable retention.

The unit caps the client at 90% of host memory, applies pressure at 85%, disables swap for the
cgroup, allows 1,048,576 file descriptors, and limits it to 65,536 tasks. Adjust these only through
a reviewed systemd drop-in after load testing; do not remove the cgroup boundary. The five-minute
graceful shutdown window ends with systemd's normal final kill so a wedged unit cannot block an
upgrade forever.

## Install

The signed release archive contains the execution binary, both checkpoint tools, checkpoint
scripts, and the complete `ops/` tree. Create static service accounts and the read-only shared
configuration group. The companion must use its own `telos-consensus` account.

```bash
sudo install -o root -g root -m 0644 ops/sysusers.d/telos-reth.conf /etc/sysusers.d/
sudo systemd-sysusers /etc/sysusers.d/telos-reth.conf
sudo install -o root -g root -m 0755 ops/scripts/telos-reth-* /usr/local/libexec/
sudo install -o root -g root -m 0644 ops/systemd/* /etc/systemd/system/
sudo install -o root -g root -m 0644 ops/tmpfiles.d/telos-reth.conf /etc/tmpfiles.d/
sudo systemd-tmpfiles --create /etc/tmpfiles.d/telos-reth.conf
sudo systemctl daemon-reload
```

Install a release without activating it, then atomically activate the approved digest:

```bash
tar -xzf telos-reth-0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo /usr/local/libexec/telos-reth-release install execution \
  0.1.0 ./telos-reth-0.1.0-x86_64-unknown-linux-gnu/telos-reth APPROVED_BINARY_SHA256
sudo /usr/local/libexec/telos-reth-release activate execution \
  0.1.0 APPROVED_BINARY_SHA256
```

Repeat with `consensus` for the exact companion build in the release compatibility matrix. The
release helper never restarts services and never deletes an older release.

Copy the appropriate `ops/config/*.env.example` to `/etc/telos-reth/<instance>/node.env`. Replace
every placeholder, quote the exact companion `--version` output, and pin both binary digests plus
the lowercase, non-prefixed SHA-256 of `checkpoint.json`. `CONSENSUS_UNIT` appears only in
`node.env`; `backup.env` must never override deployment identity.

```bash
sudo install -d -o root -g telos-reth-config -m 0750 /etc/telos-reth/mainnet
sudo install -o root -g telos-reth-config -m 0440 ops/config/mainnet.env.example \
  /etc/telos-reth/mainnet/node.env
sudo install -o root -g root -m 0600 ops/config/backup.env.example \
  /etc/telos-reth/mainnet/backup.env
sudo sed -i 's/REPLACE_WITH_INSTANCE/mainnet/g' /etc/telos-reth/mainnet/backup.env
sudo install -o root -g telos-reth-config -m 0440 checkpoint.json checkpoint.audit.json \
  checkpoint.anchor.json /etc/telos-reth/mainnet/
sha256sum /etc/telos-reth/mainnet/checkpoint.json
```

Review the files after editing; do not paste credentials into either environment file. Provision
the JWT as a root-only source file. For a forwarding RPC, also provision the transaction signer and
set both signer identity fields. For a read-only or shadow RPC, leave both signer fields empty and
omit `signer.key`; the unit supplies a non-secret `disabled` credential fallback and the launcher omits
all four forwarder arguments. Partial forwarding configuration fails preflight. Credentials are
copied into a private systemd credential mount, so process arguments contain paths, never secret
values.

```bash
openssl rand -hex 32 | sudo tee /etc/telos-reth/mainnet/jwt.hex >/dev/null
# Forwarding nodes only:
sudo install -o root -g root -m 0400 /secure/provisioning/telos-signer.key \
  /etc/telos-reth/mainnet/signer.key
sudo chown root:root /etc/telos-reth/mainnet/jwt.hex
sudo chmod 0400 /etc/telos-reth/mainnet/jwt.hex
```

Install the companion's reviewed `consensus.toml` as
`/etc/telos-reth/<instance>/consensus.toml`, owned by `root:telos-reth-config` with mode `0440`.
It must pin the same chain, native endpoint, Engine port, sparse anchor number/hash, native chain
ID, native anchor number/ID, and first-child EVM hash from checkpoint manifest v2. Set `prev_hash`
to the execution anchor, require `validate_hash` to equal the manifest's
`native_anchor.evm_first_child_block_hash`, and set `evm_start_block` plus
`execution_context_anchor_block` to `anchor + 1`. `data_path` must be
`/var/lib/telos-consensus/<instance>` and `jwt_secret_path` must be
`/run/credentials/telos-consensus-client@<instance>.service/jwt.hex`.

```bash
sudo install -o root -g telos-reth-config -m 0440 /secure/provisioning/consensus-mainnet.toml \
  /etc/telos-reth/mainnet/consensus.toml
```

The same JWT source file is loaded by the companion unit. Its required service contract is:

- run as `telos-consensus`, with `NoNewPrivileges`, a strict filesystem sandbox, bounded restarts,
  and nodeos plus loopback Engine network access only;
- use the exact name `telos-consensus-client@<instance>.service`, execute
  `/usr/local/bin/telos-consensus-client --config /etc/telos-reth/<instance>/consensus.toml`, and
  include `SupplementaryGroups=telos-reth-config`;
- start after `telos-reth@<instance>.service`, stop before it, and read the Engine JWT through
  `LoadCredential=jwt.hex:/etc/telos-reth/<instance>/jwt.hex`;
- before starting, revalidate the complete checkpoint/companion binding and require an authenticated
  Engine capability probe to succeed;
- use `/var/lib/telos-consensus/<instance>` as its only writable persistent directory;
- submit the release-matrix reconciliation schema directly as the second parameter of the same
  authenticated `engine_newPayloadV1` request;
- expose no signer, JWT, or native privileged credential in argv, environment, TOML, or logs.

Preflight, readiness, snapshot, and restore reject a differently named unit, binary/config argv,
service account, writable-data sandbox, runtime executable, chain/endpoint/anchor config, JWT
credential binding, digest, or version. This makes `CONSENSUS_UNIT` one identity rather than a
label that could accidentally point at an unrelated service.

Initialize a remote restic repository once, using credential files outside the data directories.
For cloud backends, prefer host workload identity over long-lived access keys. The snapshot tool
rejects local repository paths and explicit loopback backends; use an `s3:`, `sftp:`, `rest:`,
`azure:`, `gs:`, `rclone:`, `b2:`, or `swift:` off-host repository.

```bash
sudo install -o root -g root -m 0400 /secure/provisioning/restic.repository \
  /etc/telos-reth/mainnet/restic.repository
sudo install -o root -g root -m 0400 /secure/provisioning/restic.password \
  /etc/telos-reth/mainnet/restic.password
sudo restic --repository-file /etc/telos-reth/mainnet/restic.repository \
  --password-file /etc/telos-reth/mainnet/restic.password init
```

Start the execution layer first, then its companion. A public load balancer must not add the node
until the readiness metric is `1` and its last-check timestamp is less than 60 seconds old.

```bash
sudo systemctl enable --now telos-reth@mainnet.service
sudo systemctl enable --now telos-consensus-client@mainnet.service
sudo systemctl enable --now telos-reth-readiness@mainnet.timer
sudo systemctl enable --now telos-reth-snapshot@mainnet.timer
sudo systemctl start --wait telos-reth-readiness@mainnet.service
```

Configure node_exporter with its systemd collector and
`--collector.textfile.directory=/var/lib/telos-reth-health/metrics`. Install
`ops/prometheus/alerts.yml`, adapt `ops/prometheus/scrape.example.yml`, and route every critical
alert to a staffed pager. Metrics must stay on a private monitoring network or loopback proxy.
Enable persistent journald storage with bounded retention, and ship execution, companion, kernel,
systemd, readiness, and backup logs off-host. Redact and page on any accidental signer or JWT
material instead of retaining it in the log platform.

## Readiness semantics

`telos-reth-readiness` returns success only when all of these checks pass in the same run:

1. the execution and exact pinned companion systemd units are active, and the companion unit,
   installed binary, configuration, state path, version, and SHA-256 match the deployment;
2. the checkpoint manifest still matches `CHECKPOINT_MANIFEST_SHA256`, and `TELOS_ENDPOINT` is the
   same normalized endpoint as `NODEOS_URL`;
3. a fresh short-lived JWT can call the exact two-method Telos Engine capability surface;
4. local and independent canonical EVM endpoints return the configured chain ID and anchor hash;
5. nodeos returns the pinned native chain ID, a nonzero LIB, and a fresh head timestamp;
6. local finalized height advances and stays within the configured lag bound;
7. hashes at the common finalized height and every configured depth match the canonical EVM RPC.

An unavailable canonical oracle makes the node not ready. This may reduce availability, but it
cannot leave a divergent node falsely green. HTTP 200, process liveness, `eth_syncing`, peer count,
or a single latest hash are not acceptable load-balancer checks. Keep `PARITY_DEPTHS=0,64,512` or
strengthen it; weakening or disabling finalized parity requires a release risk review.

## Backup and restore

The snapshot job stops the companion and then execution client, takes copy-on-write clones of both
databases plus the exact checkpoint trio, and restarts them before performing expensive hashes.
Each stop is tracked independently, and the job's mount namespace exposes both live databases
read-only. It rejects nested mounts, symlinks, sockets, devices, FIFOs, unsupported path names, and
an incomplete file inventory. Cloned data is made read-only, a complete `SHA256SUMS` is verified,
metadata is `fsync`ed, and the recovery-point directory is atomically published. The complete
`telos-reth-snapshot/v2` point is then uploaded to restic, its remote metadata is read back, and
`restic check --read-data-subset` performs an authenticated read of repository pack data. The
scheduled run fails until the local copy, remote upload, metadata readback, and data check succeed.

The local clone shares a host, filesystem, controller, and often physical media with the live
database. It protects short rollbacks only. It does not satisfy disaster recovery. Alert if no
off-host copy succeeds and its data verifies within eight hours. Use separate credentials and
retention administration so compromise of the RPC host cannot delete every backup.

`RESTIC_CHECK_READ_DATA_PERCENT` controls the random pack-data subset read after every six-hour
snapshot. It is required and restricted to an integer from 1 through 10; the example uses `1` to
bound routine bandwidth. A successful upload advances the remote-success metric, but only a
successful authenticated pack read advances
`telos_reth_snapshot_last_data_verified_timestamp_seconds`. A failed check therefore leaves the
last known data-verification timestamp stale and fails the snapshot service.

The per-snapshot subset is continuous corruption detection, not exhaustive coverage. Run a larger
authenticated repository check weekly and a full read at least monthly:

```bash
sudo restic --repository-file /etc/telos-reth/mainnet/restic.repository \
  --password-file /etc/telos-reth/mainnet/restic.password check --read-data-subset=10%
```

Retention deletion is intentionally not automated by this repository. After a successful check,
an authorized backup operator may apply the approved policy, for example 24 hourly, 14 daily, eight
weekly, and twelve monthly recovery points. Enable immutable/object-lock retention before granting
the node write access.

Perform a full restore drill on a clean, isolated host at least quarterly:

1. restore the selected restic snapshot to a temporary directory;
2. copy the complete timestamped recovery-point directory under the configured
   `SNAPSHOT_ROOT/<instance>`;
3. install and activate the exact execution and companion digests recorded in `manifest.json`;
4. install `node.env`, `backup.env`, `consensus.toml`, systemd units, users, and credentials, then
   update `BINARY_SHA256`, `CONSENSUS_SHA256`, and `CHECKPOINT_MANIFEST_SHA256` for the selected
   recovery point; `consensus.toml` must have the `config_sha256` recorded in `manifest.json`, while
   the live data directories and checkpoint trio may be absent on a clean host;
5. keep public traffic disabled, then run:

```bash
sudo /usr/local/libexec/telos-reth-restore mainnet \
  /var/lib/telos-reth-snapshots/mainnet/20260101T000000Z-0123456789ab --confirm
```

The restore tool verifies an exact file inventory, source and target-filesystem staged checksums,
service ownership, chain, snapshot schema, checkpoint digest, execution digest, companion digest,
and companion version. Two databases and three checkpoint files cannot be renamed atomically
across filesystems. Before the first rename, the tool therefore fsyncs
`/var/lib/telos-reth-backup/<instance>/restore.transaction.json` with the complete original-object
bitmap and rollback identity. A `pending` journal fences ordinary execution startup. Only the live
restore process may perform its validation start, using a root-only volatile permit bound to its
PID start time, nonce, and held instance lock. The journal becomes `committed` only after every
target filesystem is synced and the restored pair passes readiness.

On failed readiness, the tool first proves both units inactive again and idempotently reinstates
the entire pre-restore object set before durably recording `rolled_back`. If a unit cannot be
stopped, rollback deliberately performs no rename. A process kill or power loss can therefore
leave physical paths partway through publication, but that state remains visibly fenced and cannot
pass preflight. After power is stable, do not move any `.restore-*`, `.pre-restore-*`, or
`.failed-restore-*` path by hand. Recover the old complete set with:

```bash
sudo /usr/local/libexec/telos-reth-restore mainnet --recover
```

Recovery can be rerun after another interruption; it keeps the journal `pending` until all five
old-object outcomes are synced. Preserve every `.pre-restore-*`, `.restore-*.partial`, and
`.failed-restore-*` object until the restore or rollback soak is accepted; remove it only through a
separately reviewed change. A malformed journal, ambiguous path state, failed fsync, or unprovable
service stop remains a manual incident and deliberately keeps startup closed.

## Upgrade and rollback

Upgrade one replica at a time. Never restart all RPC replicas or both failure domains together.

1. Verify CI provenance, release signatures/checksums, upstream Reth base, chain specs, companion
   compatibility, and database migration notes.
2. Drain the canary from public and transaction-forwarding traffic. Confirm readiness is green.
3. Run `systemctl start --wait telos-reth-snapshot@<instance>.service`; require both its
   remote-success and remote-data-verification metrics, then record its snapshot and restic IDs.
4. Install both artifacts without activating them. Update `node.env` exact versions and digests in
   one reviewed change.
5. Stop the companion, then execution client. Atomically activate the companion and execution
   releases. Start execution first, then the companion.
6. Require readiness, canonical parity, test calls, receipt/log parity, and a signed forwarding
   transaction before a 30-minute shadow soak. Reintroduce the canary gradually and observe it for
   24 hours before the next replica.

For a code-only rollback with no database format change, drain the node, stop companion then
execution, activate the prior pinned release pair, start execution then companion, and require all
readiness gates. If either binary opened or migrated an incompatible database, do not start the old
binary against it. Activate the exact versions recorded by the pre-upgrade snapshot and use the
verified pair restore instead.

Rotate a JWT with coordinated companion/execution downtime because both sides must switch together.
Rotate the transaction signer independently, drain forwarding first, and test a low-value testnet
transaction. Never echo either credential into a shell history, command line, environment file, or
journal.

## Reorg or canonical mismatch incident

A finalized mismatch is a correctness incident, not an auto-heal trigger.

1. Remove the node from read and write rotation and preserve its process and databases.
2. Page execution, companion, and native-chain owners. Stop forwarding new transactions.
3. Record release digests, companion version, local/canonical finalized blocks, nodeos head and LIB,
   Engine errors, system time, and `journalctl` output. Preserve the last good snapshot IDs.
4. Determine whether the canonical oracle is unavailable, the companion selected a reversible
   fork, reconciliation failed, or durable state diverged. Compare with a second independent host.
5. Do not loop restarts, rewind by guess, delete MDBX/static files, copy individual tables, or mark
   the node healthy because its tip later advances.
6. Recover only from a proven common finalized point or a verified pair snapshot using the exact
   release matrix. Re-run the full promotion parity suite before returning traffic.

The old journal pattern matcher that restarted execution and consensus automatically is not part of
this deployment. It could erase evidence, oscillate, and make a divergent node appear recovered.

## Production promotion gate

Repository builds are candidates, not production approvals. Record an immutable compatibility
matrix containing the Telos release tag and digest, upstream Reth tag and commit, exact companion
tag/version/digest, reconciliation schema, both genesis hashes, and snapshot schema. Promotion is
blocked until all of the following have evidence attached to a release:

- a snapshot created by this Reth 2 line (`telos-reth-snapshot/v2`) restores on a clean host with
  the exact companion version and reaches canonical finalized parity;
- missing, oversized, malformed, wrong-block, partial-receipt, duplicate, and replayed reconciliation
  inputs all reject without a durable partial commit;
- mainnet and testnet replay match headers, state roots or canonical Telos state commitments,
  receipts, logs, and queries over the agreed historical corpus, including at least 10,000 sampled
  heights and every finalized block during the canary;
- `eth_call`, estimates, traces where supported, log filters, fee queries, and transaction receipts
  pass differential tests against the canonical endpoint;
- testnet signed transfers, contract calls, failed/reverted transactions, and duplicate submissions
  exercise the native forwarder end to end without exposing or logging its signer;
- reversible-fork and LIB advancement tests prove correct forkchoice, reorg rollback, and state
  reconciliation; process kill, disk-full, oracle outage, nodeos outage, and restart tests fail
  closed and recover durably;
- sustained load, resource exhaustion, database growth, restart, and a minimum 72-hour testnet plus
  seven-day mainnet shadow soak meet the published SLOs;
- alerts page the on-call path, two independent replicas can be drained separately, remote backup
  immutability is enabled, and a timed restore drill meets the approved RPO/RTO.

Any change to the upstream Reth base, companion version, reconciliation schema, chainspec, database
layout, signer path, or snapshot schema invalidates the affected evidence and requires the relevant
gates again.

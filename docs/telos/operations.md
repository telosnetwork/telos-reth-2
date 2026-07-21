# Telos Reth production operations

This runbook defines the minimum supported deployment for `telos-reth-2`. It deliberately fails
closed: an RPC process that answers HTTP is not necessarily healthy, and a local snapshot on the
same host is not a backup.

## Security and consistency model

Telos block production remains authoritative in nodeos. The pinned `telos-consensus-client`
companion translates the native result and submits the block, receipts, and typed reconciliation
data in one JWT-authenticated Engine API request. The current candidate validates the extension's
schema, receipt cardinality, and bounds, but the compatibility schema does not yet carry an
independent block identity or payload commitment. Production promotion therefore requires a
versioned, block-bound extension plus two-way reconciliation completeness; missing, replayed,
wrong-block, malformed, or mismatched data must reject the payload. Startup remains disabled until
those invariants and the Telos execution rules are implemented and replay-proven.

There is no production filesystem state-diff side channel. In particular, do not recreate
`/tmp/telos-extra-fields`, do not allow a companion to publish unauthenticated JSON, and do not add
a permissive fallback when reconciliation data is absent. If a temporary compatibility spool is
ever reintroduced, it requires a separate threat model, an instance-specific directory outside
`/tmp`, atomic `fsync` plus rename, strict producer/consumer ownership, size and age bounds, and a
block-bound authenticated envelope. Such a deployment is not covered by this runbook.

The Engine API, nodeos API, metrics, and unproxied JSON-RPC listeners bind loopback. Public JSON-RPC
is exposed only through a TLS reverse proxy with request-size limits, per-method policy, connection
limits, and abuse controls. Never publish port 8551, a JWT, a signer credential, `debug`, `admin`, or
unrestricted trace methods.

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

- systemd 252 or newer, Bash 5, GNU coreutils, util-linux, curl, jq, Python 3, restic, Prometheus,
  and node_exporter;
- a 64-bit little-endian Linux release artifact from this repository, verified against its
  published SHA-256 checksum;
- XFS created with `reflink=1` or Btrfs, with execution data, companion data, and local snapshot
  staging on the same filesystem;
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

Create static service accounts. The companion should use its own `telos-consensus` account.

```bash
sudo useradd --system --home /var/lib/telos-reth --shell /usr/sbin/nologin telos-reth
sudo useradd --system --home /var/lib/telos-reth-health --shell /usr/sbin/nologin telos-monitor
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
every placeholder, quote the exact companion `--version` output, and pin both binary digests.

```bash
sudo install -d -o root -g root -m 0750 /etc/telos-reth/mainnet
sudo install -o root -g root -m 0640 ops/config/mainnet.env.example \
  /etc/telos-reth/mainnet/node.env
sudo install -o root -g root -m 0640 ops/config/backup.env.example \
  /etc/telos-reth/mainnet/backup.env
sudo sed -i 's/REPLACE_WITH_INSTANCE/mainnet/g' /etc/telos-reth/mainnet/backup.env
```

Review the files after editing; do not paste credentials into either environment file. Provision
the JWT and transaction-forwarder signer as root-only source files. The unit copies them into a
private systemd credential mount, so process arguments contain paths, never secret values.

```bash
openssl rand -hex 32 | sudo tee /etc/telos-reth/mainnet/jwt.hex >/dev/null
sudo install -o root -g root -m 0400 /secure/provisioning/telos-signer.key \
  /etc/telos-reth/mainnet/signer.key
sudo chown root:root /etc/telos-reth/mainnet/jwt.hex
sudo chmod 0400 /etc/telos-reth/mainnet/jwt.hex
```

The same JWT source file is loaded by the companion unit. Its required service contract is:

- run as `telos-consensus`, with `NoNewPrivileges`, a strict filesystem sandbox, bounded restarts,
  and nodeos plus loopback Engine network access only;
- start after `telos-reth@<instance>.service`, stop before it, and read the Engine JWT through
  `LoadCredential=jwt.hex:/etc/telos-reth/<instance>/jwt.hex` and a `jwt_secret_file` option;
- use `/var/lib/telos-consensus/<instance>` as its only writable persistent directory;
- submit the release-matrix reconciliation schema directly as the second parameter of the same
  authenticated `engine_newPayloadV1` request;
- expose no signer, JWT, or native privileged credential in argv, environment, TOML, or logs.

Readiness and snapshot jobs intentionally fail until the configured companion unit is active and
its exact `--version` output matches `CONSENSUS_VERSION`. A differently named or configured unit
must meet this contract and be set explicitly in `CONSENSUS_UNIT`.

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

1. the execution and exact pinned companion systemd units are active;
2. a fresh short-lived JWT can call `engine_exchangeCapabilities` on loopback;
3. local and independent canonical EVM endpoints return the configured chain ID;
4. nodeos returns the pinned native chain ID, a nonzero LIB, and a fresh head timestamp;
5. local finalized height advances and stays within the configured lag bound;
6. hashes at the common finalized height and every configured depth match the canonical EVM RPC.

An unavailable canonical oracle makes the node not ready. This may reduce availability, but it
cannot leave a divergent node falsely green. HTTP 200, process liveness, `eth_syncing`, peer count,
or a single latest hash are not acceptable load-balancer checks. Keep `PARITY_DEPTHS=0,64,512` or
strengthen it; weakening or disabling finalized parity requires a release risk review.

## Backup and restore

The snapshot job stops the companion and then execution client, takes copy-on-write clones of both
databases, and restarts them before performing expensive hashes. It rejects symlinks, computes a
complete `SHA256SUMS`, verifies it, `fsync`s metadata, and atomically publishes the recovery-point
directory. It then uploads that complete directory to restic and reads the remote snapshot metadata
back. The scheduled run is failed until both local and remote stages succeed.

The local clone shares a host, filesystem, controller, and often physical media with the live
database. It protects short rollbacks only. It does not satisfy disaster recovery. Alert if no
off-host copy succeeds and verifies within eight hours. Use separate credentials and retention
administration so compromise of the RPC host cannot delete every backup.

Run an authenticated repository check weekly and a full read at least monthly:

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
4. update the deployment pins, keep public traffic disabled, then run:

```bash
sudo /usr/local/libexec/telos-reth-restore mainnet \
  /var/lib/telos-reth-snapshots/mainnet/20260101T000000Z-0123456789ab --confirm
```

The restore tool verifies source and staged checksums before downtime, refuses a different chain,
snapshot schema, execution digest, companion digest, or companion version, and swaps both database
directories while services are stopped. If fail-closed readiness does not recover within the
configured timeout, it automatically reinstates both pre-restore directories. On success, preserve
the `.pre-restore-*` pair until the soak is accepted; remove it only through a separately reviewed
change.

## Upgrade and rollback

Upgrade one replica at a time. Never restart all RPC replicas or both failure domains together.

1. Verify CI provenance, release signatures/checksums, upstream Reth base, chain specs, companion
   compatibility, and database migration notes.
2. Drain the canary from public and transaction-forwarding traffic. Confirm readiness is green.
3. Run `systemctl start --wait telos-reth-snapshot@<instance>.service`; require its remote-success
   metric and record its snapshot and restic IDs.
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

- a snapshot created by this Reth 2 line (`telos-reth-snapshot/v1`) restores on a clean host with
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

# Retained-history RPC routing

The sparse Telos Reth v2 database is not a full archive. Production history is preserved by a
separately copied, continuously updated retained-history backend. During qualification all three
execution processes run side by side: sparse v2, the independent archive, and the untouched
incumbent. `telos-rpc-router` sits between the first two loopback RPCs and the external TLS proxy:

```text
external TLS, rate limits, eth/net/web3 policy
                         |
                  127.0.0.1:8645
                 telos-rpc-router
                    /           \
     sparse v2, 127.0.0.1:18545  independent archive, 127.0.0.1:28545
          block >= 479294328       older history plus filter lifecycle/feeHistory
```

The ports are the reference values in `ops/config/mainnet-router.env.example`; an operator may
choose different non-conflicting loopback ports. The archive must have its own datadir, JWT,
process, and consensus-companion state. It must never open the incumbent's live datadir. The
incumbent remains running on its original endpoint during qualification but is not the router's
historical backend. This combined topology serves full history; the sparse v2 process alone does
not.

## Mainnet boundary

The reviewed sparse mainnet database begins at EVM block `479294328`, hash
`0x7d62876c8248867708f934b13184ff03440c2b4447a0434562c10bbc783bef51`.
The retained-history readiness probe is historical EVM block `423015017`, hash
`0x9af24c613ebf3ba3cbd8a29d9b4c24a0cf5589544a162dfe66c98f25a1ce55c0`.
That probe corresponds to authenticated native block `423015053`, using the established native/EVM
delta of 36.
Its nonzero storage witness is WTLOS contract
`0xd102ce6a4db07d247fcc28f366a623df0938ca9e`, slot `0x2`, value
`0x0000000000000000000000000000000000000000000000000000000000000012`. The
host-key-verified incumbent loopback and `https://rpc.telos.net/evm` independently returned the
same result at the configured EIP-1898 block hash; the canonical JSON-RPC response SHA-256 is
`d697207a9137973f2dd578ed6c157f1f6c07e644146c5e6a101e73fbeebd14ea`.

At startup and on every `GET /readyz`, the router fails closed unless:

- both backends report EVM chain ID 40;
- both backends return the exact boundary hash;
- the independent archive returns the exact historical probe block hash and pinned account balance;
- the independent archive returns the pinned transaction receipt with the exact transaction, block
  number, and block hash;
- the independent archive returns the pinned empty address-log result at that block;
- the public router path returns the pinned balance through an EIP-1898 canonical block-hash
  reference and synthesizes the configured nonzero storage witness through `eth_getStorageAt`;
- their heads differ by no more than the configured lag; and
- both backends return the same hash at their common head.

The readiness endpoint is for a loopback proxy health check, not public forwarding. A green result
proves the configured boundary, historical block/state/receipt/log witnesses, and current overlap;
it does not transform the sparse node into an archive or independently replay all historical state.
The historical witness is an execution and history-availability compatibility gate only. Finality
readiness and the promotion soak apply to current mainnet operation.

## Routing contract

The router accepts HTTP JSON-RPC only. Its live method inventory mirrors the
exact qualified Telos public policy in `crates/telos/node/src/rpc_policy.rs`; the only
retained-backend exceptions are
the complete filter lifecycle (including `eth_newPendingTransactionFilter`) and
`eth_feeHistory`. Unknown, Telos-disabled, and replay-unsafe methods fail with JSON-RPC
method-not-found. Every allowed method is in the `eth`, `net`, or `web3` namespace; keep those
namespaces as an additional explicit allowlist in the external TLS proxy. Never expose either
backend listener, `/readyz`, `debug`, `trace`, `admin`, authenticated Engine methods, or WebSocket
through this path.

The reference environment limits accepted connections to 256, backend concurrency to 16, JSON-RPC
batches to 64 calls, request bodies to 15 MiB, and both aggregate backend bytes and the final
compact JSON response to 64 MiB for one client request under a 2 GiB service memory cap.
Request-body collection and each backend call have 30-second deadlines, including time spent
waiting for a limiter permit. Treat those as upper bounds; the external proxy should impose
tighter per-method and client limits based on measured production traffic.
Historical `eth_getStorageValues` is capped at the same 1,024 aggregate slots as Reth and at 1,024
request-map addresses. The router additionally permits no more than 1,024 synthesized archive
calls across an entire top-level request or batch, reserves that allowance before making calls,
and requires each individual fan-out to finish within one router backend deadline.

| Request class | Backend |
| --- | --- |
| Current/head operations, transaction submission, `net_*`, and `web3_*` | sparse live v2 |
| Explicit block number below `479294328` or `earliest` | independent archive |
| Explicit block number at or above `479294328`, or a live block tag | sparse live v2 |
| Block/transaction/receipt lookup by hash | sparse live v2, then archive only when v2 returns a null result |
| State or call method with a block-hash selector | independent archive directly |
| Historical `eth_getStorageValues` | validated, bounded archive `eth_getStorageAt` fan-out with slot order preserved |
| `eth_getLogs` wholly below or above the boundary | matching backend |
| `eth_getLogs` spanning the boundary | two non-overlapping requests, validated and merged |
| Filter creation, polling, log retrieval, and removal | independent archive for the complete ID lifecycle |
| `eth_feeHistory` | independent archive |

Filter IDs are backend-local, and fee-history ranges may cross the sparse boundary. The archive
therefore remains required for those methods even when a request concerns recent blocks. Backend
transport failures, malformed responses, ID mismatches, oversized responses, and inconsistent log
ranges return a router error; they do not trigger an unsafe transport fallback.
The retained backend does not expose `eth_getStorageValues`, so the router implements that
historical method from bounded `eth_getStorageAt` calls. It rejects empty or malformed request
maps, duplicate addresses after normalization, more than 1,024 addresses or total slots, storage
keys outside 1–64 hexadecimal digits, and non-32-byte storage results. An all-empty slot map makes
one bounded `eth_getBalance` call for its first address at the same block reference, so an unknown
historical hash cannot produce a false success. A well-formed backend JSON-RPC error is returned
with the original client request ID; malformed backend error objects fail as router errors.

## Install and run side by side

Install and activate the router from the same signed platform archive as the execution binary,
using `rpc_router_sha256` from `BUILD-METADATA`:

```bash
sudo /usr/local/libexec/telos-reth-release install router \
  0.1.0 ./telos-reth-0.1.0-x86_64-unknown-linux-gnu/telos-rpc-router \
  APPROVED_ROUTER_SHA256
sudo /usr/local/libexec/telos-reth-release activate router \
  0.1.0 APPROVED_ROUTER_SHA256
```

Keep the incumbent on its existing loopback port. First create a transactional MDBX/static-file
copy in `/var/lib/telos-reth-archive/mainnet`; do not live-`rsync` the database file and do not copy
`jwt.hex`, `discovery-secret`, `known-peers.json`, logs, or any signer material. Install a fresh
archive JWT, the digest-pinned legacy archive binary, and the exact qualified consensus companion.
The reference services make both archive processes loopback-only and give them separate users and
state directories:

```bash
# SOURCE is the incumbent datadir; DEST must not exist. Use mdbx_copy from the exact
# legacy libmdbx revision that owns SOURCE/db.
sudo install -d -o root -g root -m 0700 /var/lib/telos-reth-archive/mainnet.copy
sudo install -d -o root -g root -m 0700 \
  /var/lib/telos-reth-archive/mainnet.copy/static_files
sudo install -d -o root -g root -m 0700 \
  /var/lib/telos-reth-archive/mainnet.copy/db
sudo ionice -c 3 rsync -a --numeric-ids --no-owner --no-group \
  --exclude=/lock SOURCE/static_files/ \
  /var/lib/telos-reth-archive/mainnet.copy/static_files/
sudo ionice -c 3 EXACT_LEGACY_MDBX_COPY -q -c \
  SOURCE/db /var/lib/telos-reth-archive/mainnet.copy/db/mdbx.dat
sudo install -m 0400 SOURCE/db/database.version \
  /var/lib/telos-reth-archive/mainnet.copy/db/database.version
sudo ionice -c 3 rsync -a --numeric-ids --no-owner --no-group --delete \
  --exclude=/lock SOURCE/static_files/ \
  /var/lib/telos-reth-archive/mainnet.copy/static_files/
sudo ionice -c 3 EXACT_LEGACY_MDBX_CHK -q \
  /var/lib/telos-reth-archive/mainnet.copy/db/mdbx.dat
sudo chown -R telos-reth-archive:telos-reth-archive \
  /var/lib/telos-reth-archive/mainnet.copy
sudo mv /var/lib/telos-reth-archive/mainnet.copy \
  /var/lib/telos-reth-archive/mainnet
```

The first static-file pass precedes the MDBX read transaction and the second follows it. Static
files are append-only except for the active tail, so the result contains every object referenced
by the database snapshot. Record the source paths, start/end times, database/static sizes, exact
copy/check-tool digests, successful check-log digest, and source head before and after the copy in
an immutable manifest. Start the archive companion with an empty, separate state directory and let
it replay from the configured anchor; never seed it from state newer than the database snapshot.

```bash
sudo install -o root -g telos-reth-config -m 0440 \
  ops/config/mainnet-archive.env.example /etc/telos-reth/mainnet/archive.env
sudo install -o root -g telos-reth-config -m 0440 \
  ops/config/mainnet-archive-consensus.toml.example \
  /etc/telos-reth/mainnet/archive-consensus.toml
sudo systemctl enable --now telos-reth-archive@mainnet.service
sudo systemctl enable --now telos-reth-archive-consensus@mainnet.service
```

Before using the archive, prove genesis and the configured history witnesses, then require three
advancing samples where archive, v2, and incumbent heads are within four blocks and have the same
hash at their common height. A copied archive is not qualified merely because its process starts.

Configure the v2 `node.env` with a different `HTTP_PORT` (18545 in the example). Replace
`TELOS_RPC_ROUTER_BINARY_SHA256` in the router example with the signed release's
`rpc_router_sha256`, then install the router environment and unit:

```bash
sudo install -o root -g telos-reth-config -m 0440 \
  ops/config/mainnet-router.env.example /etc/telos-reth/mainnet/router.env
sudo install -o root -g root -m 0644 \
  ops/systemd/telos-rpc-router@.service /etc/systemd/system/
```

Bind the two independently managed archive units explicitly before starting the router:

```ini
# /etc/systemd/system/telos-rpc-router@mainnet.service.d/archive.conf
[Unit]
Requires=telos-reth-archive@mainnet.service telos-reth-archive-consensus@mainnet.service
After=telos-reth-archive@mainnet.service telos-reth-archive-consensus@mainnet.service
```

Reload systemd and start the router:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now telos-rpc-router@mainnet.service
curl --fail --silent http://127.0.0.1:8645/readyz | jq .
```

Initially leave the external TLS proxy pointed at the incumbent and send only shadow/test traffic
to the router. Verify that every execution and router listener is loopback-only or protected by
host firewall rules that reject every non-loopback source. After the independent archive gate,
parity, readiness, recovery drills, and current-mainnet finality qualification pass, change only
the proxy's loopback upstream to `127.0.0.1:8645`; do not stop, replace, or delete the incumbent.
The proxy must withdraw the router whenever `/readyz` fails and must retain TLS termination, body
limits, connection limits, per-method limits, and the `eth,net,web3` namespace policy.

This topology supplies a separately qualified full-history source, including filter lifecycle and
`eth_feeHistory`, but it still does not authorize retirement of the incumbent. Retirement requires
an explicit later decision plus independent backup/restore and failure-injection evidence for the
archive pair.

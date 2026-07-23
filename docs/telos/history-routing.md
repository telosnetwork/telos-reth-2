# Retained-history RPC routing

The sparse Telos Reth v2 database is not a full archive. Production history is preserved by
running it beside the still-live incumbent and placing `telos-rpc-router` between both loopback
RPCs and the external TLS proxy:

```text
external TLS, rate limits, eth/net/web3 policy
                         |
                  127.0.0.1:8645
                 telos-rpc-router
                    /           \
     sparse v2, 127.0.0.1:18545  retained incumbent, 127.0.0.1:8545
          block >= 479294328       older history plus filter lifecycle/feeHistory
```

The ports are the reference values in `ops/config/mainnet-router.env.example`; an operator may
choose different non-conflicting loopback ports. The topology is the contract: the incumbent must
remain running and must retain its database. `TELOS_RPC_ROUTER_ARCHIVE_URL` is the router binary's
configuration name for that incumbent endpoint. It does not mean that the v2 node, router, or
combined release is a standalone full archive.

## Mainnet boundary

The reviewed sparse mainnet database begins at EVM block `479294328`, hash
`0x7d62876c8248867708f934b13184ff03440c2b4447a0434562c10bbc783bef51`.
The retained-history readiness probe is pre-Savanna EVM block `423015017`, hash
`0x9af24c613ebf3ba3cbd8a29d9b4c24a0cf5589544a162dfe66c98f25a1ce55c0`.
That probe corresponds to authenticated native block `423015053`, using the established native/EVM
delta of 36, and is 55,414,181 native blocks before SAVANNA activation at native block
`478429234`.

At startup and on every `GET /readyz`, the router fails closed unless:

- both backends report EVM chain ID 40;
- both backends return the exact boundary hash;
- the retained incumbent returns the exact pre-Savanna probe block hash and pinned account balance;
- the retained incumbent returns the pinned transaction receipt with the exact transaction, block
  number, and block hash;
- the retained incumbent returns the pinned empty address-log result at that block;
- their heads differ by no more than the configured lag; and
- both backends return the same hash at their common head.

The readiness endpoint is for a loopback proxy health check, not public forwarding. A green result
proves the configured boundary, historical block/state/receipt/log witnesses, and current overlap;
it does not transform the sparse node into an archive or independently replay all historical state.
The pre-Savanna witness is an execution and history-availability compatibility gate only. It makes
no claim about pre-Savanna finality timing. Finality readiness and the promotion soak apply to
post-Savanna instant-finality operation.

## Routing contract

The router accepts HTTP JSON-RPC only. Its live method inventory mirrors the
exact qualified Telos public policy in `crates/telos/node/src/rpc_policy.rs`; the only
retained-incumbent exceptions are
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

| Request class | Backend |
| --- | --- |
| Current/head operations, transaction submission, `net_*`, and `web3_*` | sparse live v2 |
| Explicit block number below `479294328` or `earliest` | retained incumbent |
| Explicit block number at or above `479294328`, or a live block tag | sparse live v2 |
| Block/transaction/receipt lookup by hash | sparse live v2, then incumbent only when v2 returns a null result |
| `eth_getLogs` wholly below or above the boundary | matching backend |
| `eth_getLogs` spanning the boundary | two non-overlapping requests, validated and merged |
| Filter creation, polling, log retrieval, and removal | retained incumbent for the complete ID lifecycle |
| `eth_feeHistory` | retained incumbent |

Filter IDs are backend-local, and fee-history ranges may cross the sparse boundary. The incumbent
therefore remains required for those methods even when a request concerns recent blocks. Backend
transport failures, malformed responses, ID mismatches, oversized responses, and inconsistent log
ranges return a router error; they do not trigger an unsafe transport fallback.

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

Keep the incumbent on its existing loopback port. Configure the v2 `node.env` with a different
`HTTP_PORT` (18545 in the example). Replace `TELOS_RPC_ROUTER_BINARY_SHA256` in the router example
with the signed archive's `rpc_router_sha256`, then install the router environment and unit:

```bash
sudo install -o root -g telos-reth-config -m 0440 \
  ops/config/mainnet-router.env.example /etc/telos-reth/mainnet/router.env
sudo install -o root -g root -m 0644 \
  ops/systemd/telos-rpc-router@.service /etc/systemd/system/
```

The repository cannot know the incumbent's local systemd unit name. Bind it explicitly before
starting the router:

```ini
# /etc/systemd/system/telos-rpc-router@mainnet.service.d/incumbent.conf
[Unit]
Requires=RETAINED_INCUMBENT_UNIT.service
After=RETAINED_INCUMBENT_UNIT.service
```

After replacing that placeholder with the exact existing unit, reload systemd and start the router:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now telos-rpc-router@mainnet.service
curl --fail --silent http://127.0.0.1:8645/readyz | jq .
```

Initially leave the external TLS proxy pointed at the incumbent and send only shadow/test traffic
to the router. Before any public cutover, verify that the incumbent listener itself is bound only
to loopback or is protected by host firewall rules that reject every non-loopback source. Merely
configuring the router to call `127.0.0.1` does not satisfy this gate if the incumbent also listens
on a public interface. After that isolation gate, parity, readiness, and post-Savanna
instant-finality qualification pass, change only the proxy's loopback upstream to
`127.0.0.1:8645`; do not stop, replace, or delete the incumbent. The proxy must withdraw the router
whenever `/readyz` fails and must retain TLS termination, body limits, connection limits,
per-method limits, and the `eth,net,web3` namespace policy.

Retiring the incumbent requires a separately qualified full-history source plus an explicit design
for filter lifecycle and `eth_feeHistory`. This release provides neither and does not authorize
that retirement.

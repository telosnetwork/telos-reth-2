# Canonical transaction-type audit

Telos Reth 2 admits only authenticated type-0 transactions. This record preserves the read-only
legacy-storage audit used to confirm that policy against both canonical networks on 2026-07-21.
The bounds are snapshot high-water marks, not claims about transactions appended after the audit.

## Oracle and scanner

The scanner was compiled against the exact legacy production checkout
`8c37741ea8d97eba713a8028e3f09132bb51abd6` (`reth/v1.0.8-8c37741e`). Its Cargo path dependencies
were that checkout's Telos-enabled `reth-primitives`, `reth-provider`, `reth-evm`, and
`reth-blockchain-tree-api` crates plus `reth-storage-api`. It opened each static-file directory with
`StaticFileProvider::read_only(path, false)`, decoded each requested transaction range with the
legacy codec, and counted `TxType::Eip1559` variants.

The exact scanner source is
[`scripts/telos/audit/legacy-transaction-type-scan.rs`](../../scripts/telos/audit/legacy-transaction-type-scan.rs),
SHA-256 `1aa6ef2bd0dcd84704a2d277cf1d392bc104e6aafa3a99095af26a0289351c6a`.
The one-off build's Cargo manifest SHA-256 was
`fe488a236cddaf6cdd7bd4ad19858f4745d25e48511a7221ba4939213c61a1cb`; the resulting scanner binary
SHA-256 was `a356a1510ece20e1db5c504bdff9eb1887a1770bd99e9584359563e19ed3f886`.

## Results

| Network | Exact decoded range | Count | Last transaction-bearing block | EIP-1559 count |
| --- | --- | ---: | ---: | ---: |
| Mainnet, chain 40 | `0..9703307` (end-exclusive) | 9,703,307 | 479,307,914 | 0 |
| Testnet, chain 41 | `0..3749001` (end-exclusive) | 3,749,001 | 435,553,362 | 0 |

The mainnet scan was split after the provider's descriptor cache reached the host file-descriptor
limit: `0..8900000`, `8900000..9703018`, and the live tail `9700000..9703307`. The overlapping tail
made the union exact; transaction `9703306` was the last readable record and `9703307` was the first
missing record. The immutable checkpoint MDBX copy was also fully decoded and contained no EIP-1559
variant.

Testnet static files were scanned in 500,000-transaction chunks. Its two retained MDBX transaction
rows, keys `3679415` and `3679416`, both decoded as legacy. Live canonical testnet block 54 was
empty, so the companion's chain-41 block-54 type-2 vector is a synthetic container fixture.

Telos's 2024 production announcement independently lists EIP-1559 as
[still under development and unsupported](https://telos.net/posts/upgrade-announcement-telos-evm-2-0-deployment-and-migration-guidelines).
Any future typed-transaction support therefore requires an explicit native activation/fork gate,
canonical oracle vectors, receipt-root behavior, and a new paired qualification record.

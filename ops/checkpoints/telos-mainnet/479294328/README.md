# Telos mainnet checkpoint 479294328

This directory contains the reviewable, small provenance set for the sparse Telos mainnet
checkpoint at EVM block `479294328`. The canonical anchor is an empty, irreversible block; its
real exported state root is `0x919ce792b6978624bf197e2f7085e7f3af963083a129c27a2748df2f8f2f9b59`.

The large immutable inputs are intentionally distributed outside Git:

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| `state.jsonl` | 760060343 | `91f83fe47e1e0f529c8e71add98e76b4c8599a1a14e0d8bad03b283b9ee0f20d` |
| copied `mdbx.dat` | 55834574848 | `efc2282d911830402e77773e036130411176de8faa3c3f8c0e655268d632bfa6` |
| `telos-legacy-checkpoint-export` | 2100936 | `3f2c5de6a5e547b6e7f16f09cc18418b6a21893ac041dc9fd5989435d759af8e` |

`checkpoint.json` is the trusted bootstrap manifest. Verify it and every available evidence file
against `SHA256SUMS`, obtain the large inputs by their exact hashes, and follow
`docs/telos/checkpoint-bootstrap.md`. The bootstrap creates the execution-anchor and completed
audit files; neither is pre-generated or stored here.

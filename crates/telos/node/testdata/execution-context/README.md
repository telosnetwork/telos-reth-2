# Telos execution-context golden evidence

These fixtures preserve small, public, no-secret evidence fragments for Telos execution-context
and reconciliation tests. They combine observations of the established Telos EVM v2 public RPC
and public Hyperion history APIs with an operator-authenticated, byte-retained SHIP archive capture.

## Block mapping

The translated EVM block number is the native Antelope block number minus a chain constant:

| Network | EVM chain ID | Native-to-EVM delta |
| --- | ---: | ---: |
| Mainnet | 40 | 36 |
| Testnet | 41 | 57 |

For every boundary fixture, the EVM header at `native_block - delta` has `extraData` equal to
`0x + native_block_id`. This is stronger mapping evidence than the contract `config.last_block`
field. `last_block` can lag at native blocks that contain an administrative action but no EVM
transaction; it must not be used as the payload block-number mapping.

The correlated raw transaction demonstrates the positive case: native block `400000006` maps to
EVM block `399999970`, the contract config row also reports `last_block = 399999970`, and the EVM
header carries the native block ID in `extraData`.

## Boundary/index semantics

`boundary` is a zero-based transaction boundary, expressed as the number of EVM transactions
already emitted before the native action:

- boundary `0` applies before EVM transaction index `0`;
- boundary `k` applies starting with zero-based EVM transaction index `k`;
- boundary equal to the payload transaction count changes child-start context.

The public INIT and setrevision boundary blocks are empty, so each of those boundaries is `0` and
becomes the starting context inherited by the child. The authenticated SHIP archive fixture at
native block `423015053` contains two translated raw transactions followed by `doresources`. Its
gas-price change is therefore at boundary `2 == transaction_count`: both transaction indexes `0`
and `1` retain the old value, and the new value is inherited only by the child. The legacy
translator computes each value with `transactions.len()` at the action's trace position, which has
exactly the semantics above. Native `action_ordinal`, contract `config.trx_index`, and EVM
`transactionIndex` are separate index domains and must not be substituted for one another.

The legacy Reth execution loop incremented its transaction counter before resolving the header
extension. That happens to preserve boundary `0`, but shifts boundary `2` one transaction early in
the two-transaction SHIP fixture. The regression test therefore directly distinguishes the correct
zero-based rule from the legacy bug. A separate fixture with multiple changes and a transaction
after a nonterminal change remains useful for broader schedule coverage, but is no longer needed to
prove the off-by-one behavior.

## Testnet revision history

Both testnet `setrevision` actions are retained. The legacy translator also contains a chain-41
special case at native block `276210867` that forces revision `0` after the first action and before
the second. That synthetic reset is code-derived and is deliberately not presented as public
native-action evidence here.

## Account/state correlation

In the raw-transaction fixture:

- the native raw bytes hash to the EVM transaction hash and equal `debug_getRawTransaction`;
- the native account delta address is the EVM sender;
- the Antelope `accountstate` scope `.........2h2` decodes to account index `39456`;
- a public current-state `account` lookup maps index `39456` to the EVM recipient.

Account-index resolution is the only non-historical fragment: the public Hyperion API exposed the
historical delta but not a filtered historical account-index lookup. Replay code must validate the
append-only account-index assumption against SHIP data rather than treating the current lookup as
a historical proof.

## Endpoint limitations

- `mainnet.telos.net` and `testnet.telos.net` nodeos `get_block` do not retain every historical
  block. Hyperion action/delta endpoints were used for history.
- `testnet.telos.net` exposed historical actions but returned `Unknown Endpoint` for
  `get_deltas`; `test.telos.eosusa.io` supplied the testnet delta evidence.
- Public Ethereum JSON-RPC exposes EVM blocks and receipts but not Telos execution-context sidecar
  fields. The schedule expectations combine public native evidence with the documented translator
  ordering rule.
- `ship-mainnet-423015053.v1.json` retains the exact indexed signed-block, trace-history, and
  chain-state-history archive records from an operator-controlled node. The running SHIP catalog no
  longer advertised that historical partition, so the official archive indexes selected the byte
  ranges directly and the offline verifier checks their framing, hashes, decompression, block ID,
  trace order, transactions, and gas-price delta.
- Public endpoints are independently operated and can be reindexed. `SOURCE_HASHES.json` pins the
  canonical, selected evidence fragments committed here.

## Canonical hashes

`SOURCE_HASHES.json` hashes selected JSON values after recursive key sorting and compact encoding.
The byte representation is `jq -S -c` output with the trailing line feed removed, hashed with
SHA-256. This avoids volatile API envelope fields such as query latency, cache status, and current
indexer head while still detecting changes to every committed evidence value.

//! Offline verification of the finalized Telos sidecar gas-price schedule.
//!
//! Static files are not part of MDBX's MVCC snapshot. Callers must run this scanner only against
//! a frozen full-datadir generation whose file manifest is independently authenticated.

use crate::{
    engine::recover_telos_sender,
    execution::{TelosBlockExecutionSchedule, TelosExecutionContext},
    sidecar::{
        finalized_coverage_from_transaction, get_record_by_hash_from_transaction,
        TelosChainIdentity, TelosExecutionAnchor, TelosExecutionSidecars,
        TelosExecutionSidecarsByNumberHash, TelosExecutionSidecarsByParentHash,
        TelosFinalizedCoverage, TelosSidecarFinalizedCoverage, TelosSidecarNumberHashKey,
        TelosSidecarState,
    },
};
use alloy_consensus::{proofs::calculate_transaction_root, BlockHeader};
use alloy_primitives::{Sealable, B256, U256};
use reth_db_api::{cursor::DbCursorRO, tables, transaction::DbTx};
use reth_ethereum_primitives::{calculate_receipt_root_no_memo, Receipt, TransactionSigned};
use reth_provider::{
    BlockBodyIndicesProvider, BlockHashReader, BlockNumReader, DBProvider, HeaderProvider,
    ReceiptProvider, RocksDBProviderFactory, StageCheckpointReader, TransactionsProvider,
};
use reth_stages_types::StageId;
use reth_telos_rpc_engine_api::{convert_receipts, structs::TelosExecutionChange};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Canonical JSON schema emitted by the offline schedule scanner.
pub const TELOS_GAS_PRICE_SCHEDULE_SCHEMA: &str = "telos-reth-sidecar-gas-price-schedule-core/v1";

/// Domain and field order for the RPC-observable chain transcript.
pub const TELOS_RPC_CHAIN_TRANSCRIPT_SCHEMA: &str =
    "utf8(\"telos-sidecar-rpc-chain-transcript/v1\")||0x00||[u64be(number),hash,parent_hash,u64be(transaction_count),u64be(gas_used),transactions_root]";

/// Domain and field order for the complete sidecar schedule transcript.
pub const TELOS_SIDECAR_TABLES_TRANSCRIPT_SCHEMA: &str =
    "utf8(\"telos-sidecar-tables-transcript/v1\")||0x00||[rpc_fields,sidecar_digest,u128be(starting_gas_price),u64be(starting_revision),u64be(gas_change_count),[u64be(boundary),u128be(value)],u64be(revision_change_count),[u64be(boundary),u64be(value)]]";

/// Domain and field order for the gas-price schedule transcript reproducible by RPC parity.
pub const TELOS_GAS_PRICE_TRANSCRIPT_SCHEMA: &str =
    "utf8(\"telos-sidecar-gas-price-transcript/v1\")||0x00||[u64be(number),hash,u64be(transaction_count),u128be(starting_gas_price),u64be(change_count),[u64be(boundary),u128be(value)]]";

const RPC_TRANSCRIPT_DOMAIN: &[u8] = b"telos-sidecar-rpc-chain-transcript/v1\0";
const SIDECAR_TRANSCRIPT_DOMAIN: &[u8] = b"telos-sidecar-tables-transcript/v1\0";
const GAS_PRICE_TRANSCRIPT_DOMAIN: &[u8] = b"telos-sidecar-gas-price-transcript/v1\0";

/// Complete result of verifying one frozen, finalized storage generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TelosGasPriceScheduleScan {
    /// Manifest schema.
    pub schema: &'static str,
    /// Public Telos block-zero identity from the trusted checkpoint.
    pub canonical_chain: TelosChainIdentity,
    /// Sparse database identity carried by the execution anchor and every sidecar.
    pub database_chain: TelosChainIdentity,
    /// Trusted sparse checkpoint boundary.
    pub anchor: TelosExecutionAnchor,
    /// Exact finalized and persisted tip of the frozen generation.
    pub tip: TelosFinalizedCoverage,
    /// Durability boundaries that must all equal `tip`.
    pub durability: TelosGasPriceScheduleDurability,
    /// Reconciled scan counts.
    pub counts: TelosGasPriceScheduleCounts,
    /// Native gas price inherited by the first child after `tip`.
    pub terminal_gas_price: U256,
    /// Native revision inherited by the first child after `tip`.
    pub terminal_revision: u64,
    /// Receipt-bearing blocks and empty blocks that contain a terminal gas-price change.
    pub schedule_blocks: Vec<TelosGasPriceScheduleBlock>,
    /// Encoding contract for `canonical_transcript_sha256`.
    pub canonical_transcript_schema: &'static str,
    /// SHA-256 over the RPC-observable identity of every block after the anchor.
    pub canonical_transcript_sha256: B256,
    /// Encoding contract for `sidecar_tables_transcript_sha256`.
    pub sidecar_tables_transcript_schema: &'static str,
    /// SHA-256 over every accepted sidecar digest and raw execution schedule.
    pub sidecar_tables_transcript_sha256: B256,
    /// Encoding contract for `gas_price_transcript_sha256`.
    pub gas_price_transcript_schema: &'static str,
    /// SHA-256 over the gas-price schedule and RPC-visible block identities.
    pub gas_price_transcript_sha256: B256,
}

/// Exact persisted boundaries checked before scanning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TelosGasPriceScheduleDurability {
    /// Finish stage checkpoint.
    pub finish_block: u64,
    /// Best block reported by the frozen provider.
    pub best_block: u64,
    /// Last canonical block available from static/database storage.
    pub static_tip: u64,
    /// Finalized sidecar coverage marker.
    pub finalized_block: u64,
}

/// Counts that prove the primary table and both indexes contain exactly one canonical row per
/// post-anchor height.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TelosGasPriceScheduleCounts {
    /// Blocks after the anchor through the inclusive tip.
    pub blocks: u64,
    /// Accepted sidecars scanned.
    pub accepted_sidecars: u64,
    /// Primary table rows.
    pub primary_entries: u64,
    /// Number/hash index rows.
    pub number_hash_entries: u64,
    /// Parent/hash index rows.
    pub parent_hash_entries: u64,
    /// Finalized coverage rows.
    pub finalized_entries: u64,
    /// Transactions bound by the scanned sidecars.
    pub transactions: u64,
    /// Exact transaction-hash lookup rows in `RocksDB`.
    pub transaction_hash_entries: u64,
    /// Native gas-price change records.
    pub gas_price_changes: u64,
    /// Native revision change records.
    pub revision_changes: u64,
    /// Rows retained in the compact schedule.
    pub schedule_blocks: u64,
}

/// Raw schedule for one receipt-bearing block or an empty block with a terminal gas-price change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TelosGasPriceScheduleBlock {
    /// Exact block number.
    pub block_number: u64,
    /// Exact block hash.
    pub block_hash: B256,
    /// Exact parent hash.
    pub parent_hash: B256,
    /// Transaction count committed by the block.
    pub transaction_count: u64,
    /// Gas used committed by the block.
    pub gas_used: u64,
    /// Digest from the integrity-framed sidecar record.
    pub sidecar_digest: B256,
    /// Native gas price effective before transaction zero.
    pub starting_gas_price: U256,
    /// Native revision effective before transaction zero.
    pub starting_revision: u64,
    /// Ordered native gas-price changes.
    pub gas_price_changes: Vec<TelosExecutionChange<U256>>,
}

/// Verifies a frozen full-datadir generation and extracts its gas-price schedule.
///
/// The caller must independently prove that no process can mutate the database, static files, or
/// `RocksDB` while this function runs. MDBX reads share one transaction, while canonical headers
/// and transactions are read from the same frozen provider generation. Both chain identities and
/// the anchor must come from a successfully loaded completed checkpoint audit; this low-level
/// function does not authenticate those caller-supplied values by itself.
pub fn scan_frozen_finalized_gas_price_schedule<P>(
    provider: &P,
    canonical_chain: TelosChainIdentity,
    anchor: TelosExecutionAnchor,
    tip: TelosFinalizedCoverage,
) -> eyre::Result<TelosGasPriceScheduleScan>
where
    P: DBProvider
        + BlockBodyIndicesProvider
        + BlockHashReader
        + BlockNumReader
        + HeaderProvider
        + ReceiptProvider<Receipt = Receipt>
        + RocksDBProviderFactory
        + StageCheckpointReader
        + TransactionsProvider<Transaction = TransactionSigned>,
{
    anchor.validate_for_chain(anchor.chain)?;
    if canonical_chain.chain_id != anchor.chain.chain_id {
        eyre::bail!(
            "canonical chain ID {} differs from sparse database chain ID {}",
            canonical_chain.chain_id,
            anchor.chain.chain_id
        );
    }
    let blocks = tip
        .block_number
        .checked_sub(anchor.parent_block_number)
        .filter(|blocks| *blocks > 0)
        .ok_or_else(|| eyre::eyre!("schedule tip must be strictly above the execution anchor"))?;
    let expected_after_tip = tip
        .block_number
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("schedule tip block number has no successor"))?;
    let expected_entries = usize::try_from(blocks)
        .map_err(|_| eyre::eyre!("schedule block count does not fit this platform"))?;

    let finish_block = provider
        .get_stage_checkpoint(StageId::Finish)?
        .ok_or_else(|| eyre::eyre!("frozen generation is missing the Finish stage checkpoint"))?
        .block_number;
    let best_block = provider.best_block_number()?;
    let static_tip = provider.last_block_number()?;
    if finish_block != tip.block_number ||
        best_block != tip.block_number ||
        static_tip != tip.block_number
    {
        eyre::bail!(
            "frozen generation tip mismatch: requested {}, Finish {finish_block}, best {best_block}, static {static_tip}",
            tip.block_number
        );
    }
    let chain_info = provider.chain_info()?;
    if chain_info.best_number != tip.block_number || chain_info.best_hash != tip.block_hash {
        eyre::bail!(
            "frozen generation chain info differs from requested tip {} ({})",
            tip.block_number,
            tip.block_hash
        );
    }
    require_canonical_hash(provider, tip.block_number, tip.block_hash, "tip")?;
    let anchor_block = verify_anchor(provider, &anchor)?;

    let tx = provider.tx_ref();
    let stored_anchor_number = tx.get::<tables::HeaderNumbers>(anchor.parent_block_hash)?;
    if stored_anchor_number != Some(anchor.parent_block_number) {
        eyre::bail!(
            "HeaderNumbers entry for anchor {} is {stored_anchor_number:?}, expected {}",
            anchor.parent_block_hash,
            anchor.parent_block_number
        );
    }
    let finalized = finalized_coverage_from_transaction(tx)?
        .ok_or_else(|| eyre::eyre!("frozen generation has no finalized sidecar coverage"))?;
    if finalized != tip {
        eyre::bail!(
            "finalized sidecar coverage {} ({}) differs from requested tip {} ({})",
            finalized.block_number,
            finalized.block_hash,
            tip.block_number,
            tip.block_hash
        );
    }

    let primary_entries = tx.entries::<TelosExecutionSidecars>()?;
    let number_hash_entries = tx.entries::<TelosExecutionSidecarsByNumberHash>()?;
    let parent_hash_entries = tx.entries::<TelosExecutionSidecarsByParentHash>()?;
    let finalized_entries = tx.entries::<TelosSidecarFinalizedCoverage>()?;
    for (label, actual) in [
        ("primary", primary_entries),
        ("number/hash", number_hash_entries),
        ("parent/hash", parent_hash_entries),
    ] {
        if actual != expected_entries {
            eyre::bail!(
                "sidecar {label} table has {actual} rows, expected exactly {expected_entries}"
            );
        }
    }
    if finalized_entries != 1 {
        eyre::bail!(
            "sidecar finalized coverage table has {finalized_entries} rows, expected exactly one"
        );
    }

    let first_number = anchor
        .parent_block_number
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("execution anchor block number has no successor"))?;
    let start = TelosSidecarNumberHashKey::new(first_number, B256::ZERO);
    let end = TelosSidecarNumberHashKey::new(tip.block_number, B256::repeat_byte(u8::MAX));
    let mut cursor = tx.cursor_read::<TelosExecutionSidecarsByNumberHash>()?;
    let mut expected_number = first_number;
    let mut expected_parent_hash = anchor.parent_block_hash;
    let mut expected_first_tx_num = anchor_block.next_tx_num;
    let mut expected_context = TelosExecutionContext {
        fixed_gas_price: u128::try_from(anchor.starting_gas_price)
            .map_err(|_| eyre::eyre!("anchor starting gas price exceeds u128"))?,
        revision: anchor.starting_revision,
        first_new_address: None,
    };
    let mut canonical_transcript = Sha256::new();
    canonical_transcript.update(RPC_TRANSCRIPT_DOMAIN);
    let mut sidecar_transcript = Sha256::new();
    sidecar_transcript.update(SIDECAR_TRANSCRIPT_DOMAIN);
    let mut gas_price_transcript = Sha256::new();
    gas_price_transcript.update(GAS_PRICE_TRANSCRIPT_DOMAIN);
    let mut accepted_sidecars = 0u64;
    let mut transactions = 0u64;
    let mut gas_price_changes = 0u64;
    let mut revision_changes = 0u64;
    let mut schedule_blocks = Vec::new();

    for row in cursor.walk_range(start..=end)? {
        let (index_key, indexed_hash) = row?;
        if index_key.block_number != expected_number {
            eyre::bail!(
                "sidecar number/hash index gap: expected block {expected_number}, got {}",
                index_key.block_number
            );
        }
        if index_key.block_hash != indexed_hash {
            eyre::bail!(
                "sidecar number/hash index key {} differs from value {indexed_hash} at block {expected_number}",
                index_key.block_hash
            );
        }

        let record = get_record_by_hash_from_transaction(tx, anchor.chain, indexed_hash)?
            .ok_or_else(|| {
                eyre::eyre!(
                    "sidecar number/hash index points to missing primary record {indexed_hash}"
                )
            })?;
        if record.state() != TelosSidecarState::Accepted {
            eyre::bail!(
                "sidecar {indexed_hash} at block {expected_number} is {:?}, expected Accepted",
                record.state()
            );
        }
        let sidecar = record.sidecar();
        let envelope = sidecar.envelope();
        if envelope.block_number != expected_number ||
            envelope.block_hash != indexed_hash ||
            envelope.parent_hash != expected_parent_hash
        {
            eyre::bail!("sidecar chain discontinuity at block {expected_number} ({indexed_hash})");
        }

        let stored_number = tx.get::<tables::HeaderNumbers>(indexed_hash)?;
        if stored_number != Some(expected_number) {
            eyre::bail!(
                "HeaderNumbers entry for {indexed_hash} is {stored_number:?}, expected {expected_number}"
            );
        }
        let block = verified_canonical_block(provider, expected_number, indexed_hash)?;
        if block.parent_hash != expected_parent_hash ||
            block.first_tx_num != expected_first_tx_num ||
            block.transaction_count != envelope.transaction_count ||
            block.gas_used != envelope.gas_used
        {
            eyre::bail!(
                "canonical block fields differ from sidecar envelope at block {expected_number}"
            );
        }
        let sidecar_receipts = convert_receipts(
            envelope
                .extra_fields
                .receipts
                .as_deref()
                .ok_or_else(|| eyre::eyre!("accepted sidecar has no receipts"))?,
        )?;
        if block.receipts != sidecar_receipts {
            eyre::bail!(
                "persisted receipts differ from accepted sidecar at block {expected_number}"
            );
        }

        let execution = envelope.extra_fields.execution.as_ref().ok_or_else(|| {
            eyre::eyre!("accepted sidecar {indexed_hash} has no execution metadata")
        })?;
        let starting_gas_price = u128::try_from(execution.starting_gas_price).map_err(|_| {
            eyre::eyre!("starting gas price exceeds u128 at block {expected_number}")
        })?;
        if starting_gas_price != expected_context.fixed_gas_price ||
            execution.starting_revision != expected_context.revision
        {
            eyre::bail!(
                "execution context discontinuity at block {expected_number} ({indexed_hash})"
            );
        }
        let schedule = TelosBlockExecutionSchedule::from_metadata(execution)?;

        update_rpc_transcript(
            &mut canonical_transcript,
            expected_number,
            indexed_hash,
            expected_parent_hash,
            envelope.transaction_count,
            envelope.gas_used,
            block.transactions_root,
        );
        update_sidecar_transcript(
            &mut sidecar_transcript,
            expected_number,
            indexed_hash,
            expected_parent_hash,
            envelope.transaction_count,
            envelope.gas_used,
            block.transactions_root,
            sidecar.digest(),
            execution,
        )?;
        update_gas_price_transcript(
            &mut gas_price_transcript,
            expected_number,
            indexed_hash,
            envelope.transaction_count,
            execution.starting_gas_price,
            &execution.gas_price_changes,
        )?;

        accepted_sidecars = accepted_sidecars
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("accepted sidecar count overflow"))?;
        transactions = transactions
            .checked_add(envelope.transaction_count)
            .ok_or_else(|| eyre::eyre!("transaction count overflow"))?;
        gas_price_changes = gas_price_changes
            .checked_add(
                u64::try_from(execution.gas_price_changes.len())
                    .map_err(|_| eyre::eyre!("gas-price change count overflow"))?,
            )
            .ok_or_else(|| eyre::eyre!("gas-price change count overflow"))?;
        revision_changes = revision_changes
            .checked_add(
                u64::try_from(execution.revision_changes.len())
                    .map_err(|_| eyre::eyre!("revision change count overflow"))?,
            )
            .ok_or_else(|| eyre::eyre!("revision change count overflow"))?;
        if envelope.transaction_count > 0 || !execution.gas_price_changes.is_empty() {
            schedule_blocks.push(TelosGasPriceScheduleBlock {
                block_number: expected_number,
                block_hash: indexed_hash,
                parent_hash: expected_parent_hash,
                transaction_count: envelope.transaction_count,
                gas_used: envelope.gas_used,
                sidecar_digest: sidecar.digest(),
                starting_gas_price: execution.starting_gas_price,
                starting_revision: execution.starting_revision,
                gas_price_changes: execution.gas_price_changes.clone(),
            });
        }

        expected_context = schedule.child_context();
        expected_parent_hash = indexed_hash;
        expected_first_tx_num = block.next_tx_num;
        expected_number = expected_number
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("sidecar block number overflow"))?;
    }

    if accepted_sidecars != blocks ||
        expected_parent_hash != tip.block_hash ||
        expected_number != expected_after_tip
    {
        eyre::bail!(
            "sidecar scan ended at block {} ({expected_parent_hash}) after {accepted_sidecars} rows; expected {} ({}) after {blocks} rows",
            expected_number.saturating_sub(1),
            tip.block_number,
            tip.block_hash
        );
    }

    let schedule_block_count = u64::try_from(schedule_blocks.len())
        .map_err(|_| eyre::eyre!("schedule block count overflow"))?;
    let rocksdb = provider.rocksdb_provider();
    let mut transaction_hash_entries = 0u64;
    for entry in rocksdb.iter::<tables::TransactionHashNumbers>()? {
        entry?;
        transaction_hash_entries = transaction_hash_entries
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("transaction-hash entry count overflow"))?;
    }
    if transaction_hash_entries != transactions {
        eyre::bail!(
            "RocksDB transaction-hash table has {transaction_hash_entries} rows, expected exactly {transactions}"
        );
    }
    let canonical_transcript_sha256 = B256::from(<[u8; 32]>::from(canonical_transcript.finalize()));
    let sidecar_tables_transcript_sha256 =
        B256::from(<[u8; 32]>::from(sidecar_transcript.finalize()));
    let gas_price_transcript_sha256 = B256::from(<[u8; 32]>::from(gas_price_transcript.finalize()));

    Ok(TelosGasPriceScheduleScan {
        schema: TELOS_GAS_PRICE_SCHEDULE_SCHEMA,
        canonical_chain,
        database_chain: anchor.chain,
        anchor,
        tip,
        durability: TelosGasPriceScheduleDurability {
            finish_block,
            best_block,
            static_tip,
            finalized_block: finalized.block_number,
        },
        counts: TelosGasPriceScheduleCounts {
            blocks,
            accepted_sidecars,
            primary_entries: u64::try_from(primary_entries)
                .map_err(|_| eyre::eyre!("primary entry count overflow"))?,
            number_hash_entries: u64::try_from(number_hash_entries)
                .map_err(|_| eyre::eyre!("number/hash entry count overflow"))?,
            parent_hash_entries: u64::try_from(parent_hash_entries)
                .map_err(|_| eyre::eyre!("parent/hash entry count overflow"))?,
            finalized_entries: u64::try_from(finalized_entries)
                .map_err(|_| eyre::eyre!("finalized entry count overflow"))?,
            transactions,
            transaction_hash_entries,
            gas_price_changes,
            revision_changes,
            schedule_blocks: schedule_block_count,
        },
        terminal_gas_price: U256::from(expected_context.fixed_gas_price),
        terminal_revision: expected_context.revision,
        schedule_blocks,
        canonical_transcript_schema: TELOS_RPC_CHAIN_TRANSCRIPT_SCHEMA,
        canonical_transcript_sha256,
        sidecar_tables_transcript_schema: TELOS_SIDECAR_TABLES_TRANSCRIPT_SCHEMA,
        sidecar_tables_transcript_sha256,
        gas_price_transcript_schema: TELOS_GAS_PRICE_TRANSCRIPT_SCHEMA,
        gas_price_transcript_sha256,
    })
}

#[derive(Clone, Debug)]
struct VerifiedCanonicalBlock {
    parent_hash: B256,
    first_tx_num: u64,
    next_tx_num: u64,
    transaction_count: u64,
    gas_used: u64,
    transactions_root: B256,
    receipts: Vec<Receipt>,
}

fn verify_anchor<P>(
    provider: &P,
    anchor: &TelosExecutionAnchor,
) -> eyre::Result<VerifiedCanonicalBlock>
where
    P: BlockBodyIndicesProvider
        + BlockHashReader
        + HeaderProvider
        + ReceiptProvider<Receipt = Receipt>
        + TransactionsProvider<Transaction = TransactionSigned>,
{
    require_canonical_hash(
        provider,
        anchor.parent_block_number,
        anchor.parent_block_hash,
        "anchor",
    )?;
    let block =
        verified_canonical_block(provider, anchor.parent_block_number, anchor.parent_block_hash)?;
    if block.transaction_count != 0 {
        eyre::bail!(
            "execution anchor {} contains {} transactions",
            anchor.parent_block_number,
            block.transaction_count
        );
    }
    Ok(block)
}

fn require_canonical_hash<P>(
    provider: &P,
    number: u64,
    expected: B256,
    label: &str,
) -> eyre::Result<()>
where
    P: BlockHashReader,
{
    let actual = provider.block_hash(number)?;
    if actual != Some(expected) {
        eyre::bail!(
            "frozen generation {label} hash at block {number} is {actual:?}, expected {expected}"
        );
    }
    Ok(())
}

fn verified_canonical_block<P>(
    provider: &P,
    number: u64,
    expected_hash: B256,
) -> eyre::Result<VerifiedCanonicalBlock>
where
    P: BlockBodyIndicesProvider
        + BlockHashReader
        + HeaderProvider
        + ReceiptProvider<Receipt = Receipt>
        + TransactionsProvider<Transaction = TransactionSigned>,
{
    require_canonical_hash(provider, number, expected_hash, "canonical")?;
    let sealed = provider
        .sealed_header(number)?
        .ok_or_else(|| eyre::eyre!("canonical header {number} is missing"))?;
    if sealed.hash() != expected_hash || sealed.header().hash_slow() != expected_hash {
        eyre::bail!("canonical header hash mismatch at block {number}");
    }
    let header = sealed.header();
    if header.number() != number {
        eyre::bail!("canonical header at block {number} declares number {}", header.number());
    }
    let body_indices = provider
        .block_body_indices(number)?
        .ok_or_else(|| eyre::eyre!("canonical block {number} has no body indices"))?;
    let next_tx_num = body_indices
        .first_tx_num
        .checked_add(body_indices.tx_count)
        .ok_or_else(|| eyre::eyre!("body transaction range overflows at block {number}"))?;
    let tx_range = body_indices.first_tx_num..next_tx_num;
    let transactions = if body_indices.tx_count == 0 {
        Vec::new()
    } else {
        provider.transactions_by_tx_range(tx_range.clone())?
    };
    let transaction_count = u64::try_from(transactions.len())
        .map_err(|_| eyre::eyre!("transaction count does not fit u64 at block {number}"))?;
    if body_indices.tx_count != transaction_count {
        eyre::bail!(
            "body index count {} differs from {} loaded transactions at block {number}",
            body_indices.tx_count,
            transaction_count
        );
    }
    let sender_count = if body_indices.tx_count == 0 {
        Vec::new()
    } else {
        provider.senders_by_tx_range(tx_range.clone())?
    };
    if u64::try_from(sender_count.len())
        .map_err(|_| eyre::eyre!("sender count does not fit u64 at block {number}"))? !=
        transaction_count
    {
        eyre::bail!(
            "sender row count {} differs from {transaction_count} transactions at block {number}",
            sender_count.len()
        );
    }
    for (index, (transaction, stored_sender)) in transactions.iter().zip(&sender_count).enumerate()
    {
        let recovered_sender = recover_telos_sender(transaction)?;
        if recovered_sender != *stored_sender {
            eyre::bail!(
                "sender row {stored_sender} differs from recovered {recovered_sender} at transaction {index} in block {number}"
            );
        }
        let transaction_number = body_indices
            .first_tx_num
            .checked_add(
                u64::try_from(index)
                    .map_err(|_| eyre::eyre!("transaction index does not fit u64"))?,
            )
            .ok_or_else(|| eyre::eyre!("transaction number overflows at block {number}"))?;
        let indexed_number = provider.transaction_id(*transaction.tx_hash())?;
        if indexed_number != Some(transaction_number) {
            eyre::bail!(
                "transaction hash index for {} is {indexed_number:?}, expected {transaction_number} at block {number}",
                transaction.tx_hash()
            );
        }
    }
    let receipts = if body_indices.tx_count == 0 {
        Vec::new()
    } else {
        provider.receipts_by_tx_range(tx_range)?
    };
    if u64::try_from(receipts.len())
        .map_err(|_| eyre::eyre!("receipt count does not fit u64 at block {number}"))? !=
        transaction_count
    {
        eyre::bail!(
            "persisted receipt count {} differs from {transaction_count} transactions at block {number}",
            receipts.len()
        );
    }
    let receipts_root = calculate_receipt_root_no_memo(&receipts);
    if receipts_root != header.receipts_root() {
        eyre::bail!(
            "recomputed receipt root {receipts_root} differs from header {} at block {number}",
            header.receipts_root()
        );
    }
    let transactions_root = calculate_transaction_root(&transactions);
    if transactions_root != header.transactions_root() {
        eyre::bail!(
            "recomputed transaction root {transactions_root} differs from header {} at block {number}",
            header.transactions_root()
        );
    }
    Ok(VerifiedCanonicalBlock {
        parent_hash: header.parent_hash(),
        first_tx_num: body_indices.first_tx_num(),
        next_tx_num,
        transaction_count,
        gas_used: header.gas_used(),
        transactions_root,
        receipts,
    })
}

fn update_rpc_transcript(
    transcript: &mut Sha256,
    number: u64,
    block_hash: B256,
    parent_hash: B256,
    transaction_count: u64,
    gas_used: u64,
    transactions_root: B256,
) {
    transcript.update(number.to_be_bytes());
    transcript.update(block_hash.as_slice());
    transcript.update(parent_hash.as_slice());
    transcript.update(transaction_count.to_be_bytes());
    transcript.update(gas_used.to_be_bytes());
    transcript.update(transactions_root.as_slice());
}

#[allow(clippy::too_many_arguments)]
fn update_sidecar_transcript(
    transcript: &mut Sha256,
    number: u64,
    block_hash: B256,
    parent_hash: B256,
    transaction_count: u64,
    gas_used: u64,
    transactions_root: B256,
    sidecar_digest: B256,
    execution: &reth_telos_rpc_engine_api::structs::TelosExecutionMetadataV3,
) -> eyre::Result<()> {
    update_rpc_transcript(
        transcript,
        number,
        block_hash,
        parent_hash,
        transaction_count,
        gas_used,
        transactions_root,
    );
    transcript.update(sidecar_digest.as_slice());
    transcript.update(
        u128::try_from(execution.starting_gas_price)
            .map_err(|_| eyre::eyre!("starting gas price exceeds u128 at block {number}"))?
            .to_be_bytes(),
    );
    transcript.update(execution.starting_revision.to_be_bytes());
    transcript.update(
        u64::try_from(execution.gas_price_changes.len())
            .map_err(|_| eyre::eyre!("gas-price change count overflow at block {number}"))?
            .to_be_bytes(),
    );
    for change in &execution.gas_price_changes {
        transcript.update(change.boundary.to_be_bytes());
        transcript.update(
            u128::try_from(change.value)
                .map_err(|_| {
                    eyre::eyre!(
                        "gas-price change at boundary {} exceeds u128 in block {number}",
                        change.boundary
                    )
                })?
                .to_be_bytes(),
        );
    }
    transcript.update(
        u64::try_from(execution.revision_changes.len())
            .map_err(|_| eyre::eyre!("revision change count overflow at block {number}"))?
            .to_be_bytes(),
    );
    for change in &execution.revision_changes {
        transcript.update(change.boundary.to_be_bytes());
        transcript.update(change.value.to_be_bytes());
    }
    Ok(())
}

fn update_gas_price_transcript(
    transcript: &mut Sha256,
    number: u64,
    block_hash: B256,
    transaction_count: u64,
    starting_gas_price: U256,
    gas_price_changes: &[TelosExecutionChange<U256>],
) -> eyre::Result<()> {
    transcript.update(number.to_be_bytes());
    transcript.update(block_hash.as_slice());
    transcript.update(transaction_count.to_be_bytes());
    transcript.update(
        u128::try_from(starting_gas_price)
            .map_err(|_| eyre::eyre!("starting gas price exceeds u128 at block {number}"))?
            .to_be_bytes(),
    );
    transcript.update(
        u64::try_from(gas_price_changes.len())
            .map_err(|_| eyre::eyre!("gas-price change count overflow at block {number}"))?
            .to_be_bytes(),
    );
    for change in gas_price_changes {
        transcript.update(change.boundary.to_be_bytes());
        transcript.update(
            u128::try_from(change.value)
                .map_err(|_| {
                    eyre::eyre!(
                        "gas-price change at boundary {} exceeds u128 in block {number}",
                        change.boundary
                    )
                })?
                .to_be_bytes(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn gas_price_transcript_has_stable_encoding() {
        let changes = [
            TelosExecutionChange { boundary: 0, value: U256::ZERO },
            TelosExecutionChange { boundary: 2, value: U256::from(75) },
        ];
        let mut transcript = Sha256::new();
        transcript.update(GAS_PRICE_TRANSCRIPT_DOMAIN);
        update_gas_price_transcript(
            &mut transcript,
            7,
            B256::repeat_byte(0x11),
            2,
            U256::from(100),
            &changes,
        )
        .unwrap();

        assert_eq!(
            B256::from(<[u8; 32]>::from(transcript.finalize())),
            b256!("6c5f830670da918fdbd9e72d38714bce223ad20f3855d2532d26cf554d178b94")
        );
    }

    #[test]
    fn gas_price_transcript_binds_terminal_change() {
        let terminal = [TelosExecutionChange { boundary: 2, value: U256::from(75) }];
        let interior = [TelosExecutionChange { boundary: 1, value: U256::from(75) }];

        let digest = |changes: &[TelosExecutionChange<U256>]| {
            let mut transcript = Sha256::new();
            transcript.update(GAS_PRICE_TRANSCRIPT_DOMAIN);
            update_gas_price_transcript(
                &mut transcript,
                7,
                B256::repeat_byte(0x11),
                2,
                U256::from(100),
                changes,
            )
            .unwrap();
            B256::from(<[u8; 32]>::from(transcript.finalize()))
        };

        assert_ne!(digest(&terminal), digest(&interior));
    }

    #[test]
    fn gas_price_transcript_rejects_values_above_u128() {
        let mut transcript = Sha256::new();
        transcript.update(GAS_PRICE_TRANSCRIPT_DOMAIN);

        assert!(update_gas_price_transcript(
            &mut transcript,
            7,
            B256::repeat_byte(0x11),
            0,
            U256::MAX,
            &[],
        )
        .is_err());
    }
}

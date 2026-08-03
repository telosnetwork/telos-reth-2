//! Exact-codec checkpoint extractor for Telos Reth 1.0.8.
//!
//! This file is deliberately built inside the pinned legacy source tree by
//! `build-exact-legacy-extractor.sh`. It must not be compiled against current Reth codecs.

#![allow(missing_docs, unused_crate_dependencies)]

use alloy_primitives::{hex, Bloom, B256, U256};
use alloy_rlp::Encodable;
use clap::Parser;
use reth_db::{
    cursor::{DbCursorRO, DbDupCursorRO},
    mdbx::DatabaseArguments,
    tables,
    transaction::DbTx,
    Database,
};
use reth_primitives::{constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH}, Header};
use reth_stages_types::StageId;
use reth_trie::StateRoot;
use reth_trie_db::{DatabaseStateRoot, DatabaseTrieCursorFactory, DatabaseHashedCursorFactory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

const LEGACY_SOURCE_COMMIT: &str = "8c37741ea8d97eba713a8028e3f09132bb51abd6";
const COPY_MANIFEST_VERSION: u8 = 2;
const EVIDENCE_VERSION: u8 = 1;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

type LegacyStateRoot<'a, TX> =
    StateRoot<DatabaseTrieCursorFactory<'a, TX>, DatabaseHashedCursorFactory<'a, TX>>;

#[derive(Debug, Parser)]
#[command(name = "telos-legacy-checkpoint-export")]
struct Command {
    #[arg(long, value_name = "PATH")]
    backup_manifest: PathBuf,
    #[arg(long, value_name = "PATH")]
    output: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyManifest {
    version: u8,
    copy_method: String,
    verification: String,
    chain: String,
    legacy_chain: String,
    source_datadir: PathBuf,
    source_db: PathBuf,
    backup_db: PathBuf,
    mdbx_size: u64,
    mdbx_sha256: B256,
    legacy_binary_sha256: B256,
    legacy_binary_version: String,
    mdbx_copy_binary_sha256: B256,
    mdbx_copy_binary_version: String,
    mdbx_check_binary_sha256: B256,
    mdbx_check_log: PathBuf,
    mdbx_check_log_sha256: B256,
    created_at_utc: String,
}

#[derive(Clone, Debug, Serialize)]
struct LegacyEvidence {
    version: u8,
    legacy_source_commit: &'static str,
    chain: String,
    backup_manifest_sha256: B256,
    backup_mdbx_sha256: B256,
    block_number: u64,
    block_hash: B256,
    parent_block_hash: B256,
    header_rlp: String,
    header_rlp_sha256: B256,
    header_state_root: B256,
    actual_state_root: B256,
    state_dump_sha256: B256,
    native_block_number: u32,
    native_block_id: B256,
    starting_child_gas_price: U256,
    starting_child_revision: u64,
    stage_checkpoints: BTreeMap<String, u64>,
    accounts: u64,
    storage_slots: u64,
    bytecode_accounts: u64,
    bytecode_hash_overrides: Vec<BytecodeHashOverride>,
    plain_accounts: u64,
    hashed_accounts: u64,
    plain_storage_slots: u64,
    hashed_storage_slots: u64,
    body_transaction_count: u64,
}

#[derive(Clone, Debug, Serialize)]
struct BytecodeHashOverride {
    address: alloy_primitives::Address,
    recorded_code_hash: B256,
    actual_code_hash: B256,
}

#[derive(Default)]
struct ExportCounts {
    accounts: u64,
    storage_slots: u64,
    bytecode_accounts: u64,
    bytecode_hash_overrides: Vec<BytecodeHashOverride>,
}

impl Command {
    fn execute(self) -> eyre::Result<()> {
        let (copy, manifest_sha) = load_and_verify_copy(&self.backup_manifest)?;
        refuse_output(&self.output)?;
        let evidence_path = self.output.with_extension("legacy-evidence.json");
        refuse_output(&evidence_path)?;

        let db = reth_db::open_db_read_only(&copy.backup_db, DatabaseArguments::default())?;
        let tx = db.tx()?;
        let stages = read_aligned_stages(&tx)?;
        let block_number = *stages
            .get(StageId::Execution.as_str())
            .ok_or_else(|| eyre::eyre!("missing Execution checkpoint"))?;
        let header = tx
            .get::<tables::Headers>(block_number)?
            .ok_or_else(|| eyre::eyre!("legacy copy has no header at {block_number}"))?;
        let canonical_hash = tx
            .get::<tables::CanonicalHeaders>(block_number)?
            .ok_or_else(|| eyre::eyre!("legacy copy has no canonical hash at {block_number}"))?;
        let block_hash = header.hash_slow();
        if canonical_hash != block_hash {
            eyre::bail!(
                "legacy header hash {block_hash} does not match canonical hash {canonical_hash}"
            )
        }
        validate_anchor_header(&header)?;
        let body = tx
            .get::<tables::BlockBodyIndices>(block_number)?
            .ok_or_else(|| eyre::eyre!("legacy copy has no body indices at {block_number}"))?;
        if body.tx_count != 0 {
            eyre::bail!("checkpoint anchor has {} transactions", body.tx_count)
        }

        let plain_accounts = u64::try_from(tx.entries::<tables::PlainAccountState>()?)?;
        let hashed_accounts = u64::try_from(tx.entries::<tables::HashedAccounts>()?)?;
        let plain_storage_slots = u64::try_from(tx.entries::<tables::PlainStorageState>()?)?;
        let hashed_storage_slots = u64::try_from(tx.entries::<tables::HashedStorages>()?)?;
        if plain_accounts == 0 ||
            hashed_accounts == 0 ||
            plain_accounts != hashed_accounts ||
            plain_storage_slots != hashed_storage_slots
        {
            eyre::bail!(
                "legacy state tables are not aligned: accounts {plain_accounts}/{hashed_accounts}, storage {plain_storage_slots}/{hashed_storage_slots}"
            )
        }

        let actual_state_root = LegacyStateRoot::from_tx(&tx).root()?;
        if actual_state_root == EMPTY_ROOT_HASH || actual_state_root == B256::ZERO
        {
            eyre::bail!("refusing an empty computed checkpoint state root")
        }

        let temporary = self.output.with_extension("jsonl.tmp");
        refuse_output(&temporary)?;
        let file = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
        let mut writer = BufWriter::new(file);
        writeln!(writer, "{{\"root\":\"{actual_state_root}\"}}")?;
        let counts = export_accounts(&tx, &mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        let state_dump_sha256 = sha256_file(&temporary)?;
        std::fs::rename(&temporary, &self.output)?;
        sync_parent(&self.output)?;

        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        if header_rlp.is_empty() {
            eyre::bail!("legacy header encoded to an empty RLP value")
        }
        let native_block_id = B256::try_from(header.extra_data.as_ref())
            .map_err(|_| eyre::eyre!("anchor extraData is not an exact 32-byte native block ID"))?;
        let native_block_number = u32::from_be_bytes(
            native_block_id.as_slice()[..4]
                .try_into()
                .expect("four-byte native block prefix"),
        );
        let evidence = LegacyEvidence {
            version: EVIDENCE_VERSION,
            legacy_source_commit: LEGACY_SOURCE_COMMIT,
            chain: copy.chain,
            backup_manifest_sha256: manifest_sha,
            backup_mdbx_sha256: copy.mdbx_sha256,
            block_number,
            block_hash,
            parent_block_hash: header.parent_hash,
            header_rlp: format!("0x{}", hex::encode(&header_rlp)),
            header_rlp_sha256: sha256(&header_rlp),
            header_state_root: header.state_root,
            actual_state_root,
            state_dump_sha256,
            native_block_number,
            native_block_id,
            starting_child_gas_price: header.telos_block_extension.get_last_gas_price(),
            starting_child_revision: header.telos_block_extension.get_last_revision(),
            stage_checkpoints: stages,
            accounts: counts.accounts,
            storage_slots: counts.storage_slots,
            bytecode_accounts: counts.bytecode_accounts,
            bytecode_hash_overrides: counts.bytecode_hash_overrides,
            plain_accounts,
            hashed_accounts,
            plain_storage_slots,
            hashed_storage_slots,
            body_transaction_count: body.tx_count,
        };
        write_json_atomically(&evidence_path, &evidence)?;

        println!("state_dump={}", self.output.display());
        println!("legacy_evidence={}", evidence_path.display());
        println!("block_number={block_number}");
        println!("block_hash={block_hash}");
        println!("native_block_number={native_block_number}");
        println!("native_block_id={native_block_id}");
        println!("actual_state_root={actual_state_root}");
        Ok(())
    }
}

fn read_aligned_stages<TX: DbTx>(tx: &TX) -> eyre::Result<BTreeMap<String, u64>> {
    let mut result = BTreeMap::new();
    for stage in StageId::ALL {
        let checkpoint = tx
            .get::<tables::StageCheckpoints>(stage.to_string())?
            .ok_or_else(|| eyre::eyre!("legacy copy is missing {stage} checkpoint"))?;
        result.insert(stage.to_string(), checkpoint.block_number);
    }
    let execution = result[StageId::Execution.as_str()];
    for stage in [StageId::AccountHashing, StageId::StorageHashing, StageId::MerkleExecute] {
        let value = result[stage.as_str()];
        if value != execution {
            eyre::bail!("legacy {stage} checkpoint {value} is not aligned with Execution {execution}")
        }
    }
    Ok(result)
}

fn validate_anchor_header(header: &Header) -> eyre::Result<()> {
    if header.number == 0 ||
        header.state_root != EMPTY_ROOT_HASH ||
        header.transactions_root != EMPTY_ROOT_HASH ||
        header.receipts_root != EMPTY_ROOT_HASH ||
        header.ommers_hash != EMPTY_OMMER_ROOT_HASH ||
        header.logs_bloom != Bloom::ZERO ||
        header.gas_used != 0
    {
        eyre::bail!("execution checkpoint is not a nonzero empty Telos sparse anchor")
    }
    if header.base_fee_per_gas.is_some() ||
        header.withdrawals_root.is_some() ||
        header.blob_gas_used.is_some() ||
        header.excess_blob_gas.is_some() ||
        header.parent_beacon_block_root.is_some() ||
        header.requests_root.is_some()
    {
        eyre::bail!("execution checkpoint has unsupported post-Berlin header fields")
    }
    Ok(())
}

fn export_accounts<TX: DbTx>(tx: &TX, writer: &mut impl Write) -> eyre::Result<ExportCounts> {
    let mut counts = ExportCounts::default();
    let mut accounts = tx.cursor_read::<tables::PlainAccountState>()?;
    let mut storage = tx.cursor_dup_read::<tables::PlainStorageState>()?;
    let mut account_walker = accounts.walk(None)?;
    while let Some(entry) = account_walker.next() {
        let (address, account) = entry?;
        writer.write_all(b"{\"balance\":")?;
        serde_json::to_writer(&mut *writer, &account.balance)?;
        if account.nonce != 0 {
            write!(writer, ",\"nonce\":\"0x{:x}\"", account.nonce)?;
        }
        if let Some(code_hash) =
            account.bytecode_hash.filter(|hash| *hash != alloy_primitives::keccak256([]))
        {
            let bytecode = tx.get::<tables::Bytecodes>(code_hash)?.ok_or_else(|| {
                eyre::eyre!("account {address} references missing bytecode {code_hash}")
            })?;
            let actual_code_hash = bytecode.hash_slow();
            if actual_code_hash != code_hash {
                counts.bytecode_hash_overrides.push(BytecodeHashOverride {
                    address,
                    recorded_code_hash: code_hash,
                    actual_code_hash,
                });
            }
            writer.write_all(b",\"code\":")?;
            serde_json::to_writer(&mut *writer, &bytecode.original_bytes())?;
            if actual_code_hash != code_hash {
                write!(writer, ",\"codeHash\":\"{code_hash}\"")?;
            }
            counts.bytecode_accounts += 1;
        }
        if let Some((stored_address, first)) = storage.seek_exact(address)? {
            if stored_address != address {
                eyre::bail!("plain storage cursor crossed account boundary")
            }
            writer.write_all(b",\"storage\":{")?;
            write_storage(writer, first.key, first.value)?;
            counts.storage_slots += 1;
            while let Some((stored_address, entry)) = storage.next_dup()? {
                if stored_address != address {
                    eyre::bail!("plain storage duplicate cursor crossed account boundary")
                }
                writer.write_all(b",")?;
                write_storage(writer, entry.key, entry.value)?;
                counts.storage_slots += 1;
            }
            writer.write_all(b"}")?;
        }
        write!(writer, ",\"address\":\"{address}\"}}\n")?;
        counts.accounts += 1;
        if counts.accounts % 100_000 == 0 {
            eprintln!("exported {} accounts and {} storage slots", counts.accounts, counts.storage_slots);
        }
    }
    let account_rows = u64::try_from(tx.entries::<tables::PlainAccountState>()?)?;
    let storage_rows = u64::try_from(tx.entries::<tables::PlainStorageState>()?)?;
    if counts.accounts != account_rows || counts.storage_slots != storage_rows {
        eyre::bail!(
            "export coverage mismatch: accounts {}/{}, storage {}/{}",
            counts.accounts,
            account_rows,
            counts.storage_slots,
            storage_rows
        )
    }
    Ok(counts)
}

fn write_storage(writer: &mut impl Write, key: B256, value: U256) -> eyre::Result<()> {
    write!(writer, "\"{key}\":\"{}\"", B256::from(value.to_be_bytes::<32>()))?;
    Ok(())
}

fn load_and_verify_copy(path: &Path) -> eyre::Result<(CopyManifest, B256)> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        eyre::bail!("invalid copy manifest {}", path.display())
    }
    let bytes = std::fs::read(path)?;
    let manifest_sha = sha256(&bytes);
    let manifest: CopyManifest = serde_json::from_slice(&bytes)?;
    if manifest.version != COPY_MANIFEST_VERSION ||
        manifest.copy_method != "libmdbx-mdbx_copy-compact" ||
        manifest.verification != "mdbx_chk-ok" ||
        !matches!(manifest.chain.as_str(), "telos-mainnet" | "telos-testnet")
    {
        eyre::bail!("copy manifest is not a supported verified Telos compact copy")
    }
    let expected_legacy = if manifest.chain == "telos-mainnet" { "tevmmainnet" } else { "tevmtestnet" };
    if manifest.legacy_chain != expected_legacy {
        eyre::bail!("copy manifest legacy chain selector mismatch")
    }
    let source = std::fs::canonicalize(&manifest.source_db)?;
    let source_datadir = std::fs::canonicalize(&manifest.source_datadir)?;
    let backup = std::fs::canonicalize(&manifest.backup_db)?;
    if source == backup || source != manifest.source_db || backup != manifest.backup_db || !source.starts_with(source_datadir) {
        eyre::bail!("copy manifest paths do not prove an immutable non-source database")
    }
    let database_file = backup.join("mdbx.dat");
    let database_metadata = std::fs::metadata(&database_file)?;
    if !database_metadata.is_file() || database_metadata.len() != manifest.mdbx_size {
        eyre::bail!("copy database size does not match manifest")
    }
    if sha256_file(&database_file)? != manifest.mdbx_sha256 {
        eyre::bail!("copy database SHA-256 does not match manifest")
    }
    let check_log = std::fs::canonicalize(&manifest.mdbx_check_log)?;
    if check_log.parent() != Some(backup.as_path()) || sha256_file(&check_log)? != manifest.mdbx_check_log_sha256 {
        eyre::bail!("mdbx_chk evidence does not match manifest")
    }
    if manifest.legacy_binary_sha256 == B256::ZERO ||
        manifest.mdbx_copy_binary_sha256 == B256::ZERO ||
        manifest.mdbx_check_binary_sha256 == B256::ZERO ||
        manifest.legacy_binary_version.is_empty() ||
        manifest.mdbx_copy_binary_version.is_empty() ||
        manifest.created_at_utc.is_empty()
    {
        eyre::bail!("copy manifest is missing provenance")
    }
    Ok((manifest, manifest_sha))
}

fn sha256(bytes: &[u8]) -> B256 {
    B256::from(<[u8; 32]>::from(Sha256::digest(bytes)))
}

fn sha256_file(path: &Path) -> eyre::Result<B256> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break
        }
        hasher.update(&buffer[..read]);
    }
    Ok(B256::from(<[u8; 32]>::from(hasher.finalize())))
}

fn refuse_output(path: &Path) -> eyre::Result<()> {
    if path.exists() {
        eyre::bail!("refusing to overwrite {}", path.display())
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_json_atomically(path: &Path, value: &impl Serialize) -> eyre::Result<()> {
    let temporary = path.with_extension("tmp");
    refuse_output(&temporary)?;
    let file = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    std::fs::rename(&temporary, path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn main() -> eyre::Result<()> {
    Command::parse().execute()
}

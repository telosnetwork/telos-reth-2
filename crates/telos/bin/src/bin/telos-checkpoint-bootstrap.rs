//! Imports a verified Telos state dump at an exact nonzero canonical checkpoint.

#![allow(missing_docs, unused_crate_dependencies)]

use alloy_consensus::constants::KECCAK_EMPTY;
use clap::Parser;
use reth_chainspec::EthChainSpec;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_cli_runner::CliRunner;
use reth_db_api::{
    cursor::DbCursorRO, models::IntegerList, table::Table, tables, transaction::DbTx,
};
use reth_db_common::init::init_from_state_dump_with_empty_header_root;
use reth_node_ethereum::EthereumNode;
use reth_node_telos::{
    checkpoint::{
        checkpoint_audit_path, checkpoint_execution_anchor_path, TelosCheckpointAudit,
        TelosCheckpointManifest,
    },
    TelosChainSpecParser,
};
use reth_provider::{
    BlockBodyIndicesProvider, BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory,
    MetadataProvider, RocksDBProviderFactory, StageCheckpointReader, StaticFileProviderFactory,
    StaticFileSegment, StorageSettings, StorageSettingsCache,
};
use reth_stages::StageId;
use reth_trie::{trie_cursor::noop::NoopTrieCursorFactory, StateRoot};
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseTrieCursorFactory};
use std::{
    io::{BufReader, ErrorKind},
    path::{Path, PathBuf},
};
use tracing::info;

/// Imports one immutable Telos checkpoint into a brand-new Reth v2 data directory.
#[derive(Debug, Parser)]
#[command(
    name = "telos-checkpoint-bootstrap",
    about = "Import a fail-closed, post-anchor-only Telos checkpoint"
)]
struct Command {
    /// Reth storage and chain configuration.
    ///
    /// `--chain` must be the canonical `telos-mainnet` or `telos-testnet` selected by the
    /// manifest. Subsequent node starts use `telos-checkpoint:<MANIFEST_PATH>` after the audit
    /// exists.
    #[command(flatten)]
    env: EnvironmentArgs<TelosChainSpecParser>,

    /// Trusted checkpoint manifest.
    #[arg(long, value_name = "PATH")]
    manifest: PathBuf,

    /// Exact JSONL state dump bound by the manifest SHA-256.
    #[arg(long, value_name = "PATH")]
    state: PathBuf,
}

impl Command {
    async fn execute(self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        let (manifest, manifest_sha256) = TelosCheckpointManifest::load(&self.manifest)?;
        manifest.validate()?;
        manifest.verify_state_dump(&self.state)?;

        if self.env.chain.chain().id() != manifest.canonical_chain.chain_id ||
            self.env.chain.genesis_hash() != manifest.canonical_chain.genesis_hash
        {
            eyre::bail!(
                "--chain must select the exact canonical Telos network pinned by the manifest"
            )
        }
        if !self.env.storage.v2 {
            eyre::bail!("Telos checkpoint bootstrap requires Reth storage v2")
        }
        if !self.env.datadir.datadir.is_some() {
            eyre::bail!("Telos checkpoint bootstrap requires an explicit --datadir")
        }

        let resolved_data_dir = self.env.datadir.clone().resolve_datadir(self.env.chain.chain());
        let static_files_path = resolved_data_dir.static_files();
        let rocksdb_path = resolved_data_dir.rocksdb();
        require_new_storage_paths(resolved_data_dir.data_dir(), &static_files_path, &rocksdb_path)?;

        let audit_output = checkpoint_audit_path(&self.manifest);
        let execution_anchor_output = checkpoint_execution_anchor_path(&self.manifest);
        refuse_existing_output(&audit_output)?;
        refuse_existing_output(&execution_anchor_output)?;

        let mut env = self.env;
        env.chain = manifest.checkpoint_chain_spec()?;
        let Environment { config, provider_factory, data_dir: _ } =
            env.init::<EthereumNode>(AccessRights::RW, runtime.clone())?;

        verify_storage_v2(&provider_factory)?;
        verify_fresh_checkpoint_database(&provider_factory, &manifest)?;

        let reader = BufReader::new(reth_fs_util::open(&self.state)?);
        let outcome = init_from_state_dump_with_empty_header_root(
            reader,
            &provider_factory,
            config.stages.etl,
            manifest.actual_state_root,
        )?;
        if outcome.block_number != manifest.execution_anchor.parent_block_number ||
            outcome.block_hash != manifest.execution_anchor.parent_block_hash ||
            outcome.computed_state_root != manifest.actual_state_root
        {
            eyre::bail!(
                "verified import outcome does not match the Telos checkpoint manifest; discard the entire data directory"
            )
        }

        // Drop every writable backend before independently reopening the completed database.
        drop(provider_factory);
        let Environment { provider_factory, .. } =
            env.init::<EthereumNode>(AccessRights::RO, runtime)?;
        let reopened_state_root =
            verify_completed_checkpoint_database(&provider_factory, &manifest)?;
        if reopened_state_root != outcome.computed_state_root {
            eyre::bail!(
                "reopened checkpoint state root {reopened_state_root} does not match import outcome {}; discard the entire data directory",
                outcome.computed_state_root
            )
        }
        drop(provider_factory);

        // The audit is the completion marker and is intentionally written last. A crash or error
        // before this point leaves no valid audit; the entire fresh data directory must be removed.
        write_json_atomically(&execution_anchor_output, &manifest.execution_anchor)?;
        let audit =
            TelosCheckpointAudit::completed(&manifest, manifest_sha256, reopened_state_root)?;
        write_json_atomically(&audit_output, &audit)?;

        info!(
            target: "reth::cli",
            block = outcome.block_number,
            hash = %outcome.block_hash,
            actual_state_root = %outcome.computed_state_root,
            audit = %audit_output.display(),
            "Telos checkpoint import completed; RPC history is available only from this anchor forward"
        );
        Ok(())
    }
}

fn require_new_storage_paths(
    data_dir: &Path,
    static_files_path: &Path,
    rocksdb_path: &Path,
) -> eyre::Result<()> {
    let paths = [
        ("data directory", data_dir),
        ("static-files directory", static_files_path),
        ("RocksDB directory", rocksdb_path),
    ];
    for (index, (label, path)) in paths.iter().enumerate() {
        for (other_label, other_path) in paths.iter().skip(index + 1) {
            if path == other_path {
                eyre::bail!(
                    "checkpoint {label} and {other_label} resolve to the same path {}; use three distinct storage paths",
                    path.display()
                )
            }
        }
        match std::fs::symlink_metadata(path) {
            Ok(_) => eyre::bail!(
                "checkpoint {label} already exists at {}; choose a path that does not exist",
                path.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).map_err(|error| {
                    eyre::eyre!("failed to inspect checkpoint {label} {}: {error}", path.display())
                })
            }
        }
    }
    Ok(())
}

fn verify_storage_v2<PF>(provider_factory: &PF) -> eyre::Result<()>
where
    PF: DatabaseProviderFactory + StorageSettingsCache,
    PF::Provider: MetadataProvider,
{
    let provider = provider_factory.database_provider_ro()?;
    let persisted = provider
        .storage_settings()?
        .ok_or_else(|| eyre::eyre!("checkpoint database is missing persisted storage settings"))?;
    let expected = StorageSettings::v2();
    if persisted != expected || provider_factory.cached_storage_settings() != expected {
        eyre::bail!(
            "checkpoint database is not storage v2: persisted {persisted:?}, cached {:?}",
            provider_factory.cached_storage_settings()
        )
    }
    Ok(())
}

fn verify_fresh_checkpoint_database<PF>(
    provider_factory: &PF,
    manifest: &TelosCheckpointManifest,
) -> eyre::Result<()>
where
    PF: DatabaseProviderFactory,
    PF::Provider: DBProvider + BlockHashReader + BlockNumReader,
{
    let provider = provider_factory.database_provider_ro()?;
    let block = manifest.execution_anchor.parent_block_number;
    let last_block = provider.last_block_number()?;
    if last_block != block {
        eyre::bail!(
            "checkpoint data directory is not fresh: highest block is {last_block}, expected only anchor {block}"
        )
    }
    let stored_hash = provider.block_hash(block)?.ok_or_else(|| {
        eyre::eyre!("fresh checkpoint database is missing canonical anchor block {block}")
    })?;
    if stored_hash != manifest.execution_anchor.parent_block_hash {
        eyre::bail!(
            "fresh checkpoint database anchor mismatch at {block}: stored {stored_hash}, manifest {}",
            manifest.execution_anchor.parent_block_hash
        )
    }

    let tx = provider.tx_ref();
    let occupied = [
        (tables::PlainAccountState::NAME, tx.entries::<tables::PlainAccountState>()?),
        (tables::PlainStorageState::NAME, tx.entries::<tables::PlainStorageState>()?),
        (tables::HashedAccounts::NAME, tx.entries::<tables::HashedAccounts>()?),
        (tables::HashedStorages::NAME, tx.entries::<tables::HashedStorages>()?),
        (tables::Bytecodes::NAME, tx.entries::<tables::Bytecodes>()?),
        (tables::AccountsTrie::NAME, tx.entries::<tables::AccountsTrie>()?),
        (tables::StoragesTrie::NAME, tx.entries::<tables::StoragesTrie>()?),
    ]
    .into_iter()
    .filter(|(_, entries)| *entries != 0)
    .collect::<Vec<_>>();
    if !occupied.is_empty() {
        eyre::bail!(
            "checkpoint data directory is not fresh (occupied state tables: {occupied:?}); discard it and retry from an empty directory"
        )
    }
    Ok(())
}

fn verify_completed_checkpoint_database<PF>(
    provider_factory: &PF,
    manifest: &TelosCheckpointManifest,
) -> eyre::Result<alloy_primitives::B256>
where
    PF: DatabaseProviderFactory
        + RocksDBProviderFactory
        + StaticFileProviderFactory
        + StorageSettingsCache,
    PF::Provider: DBProvider
        + BlockHashReader
        + BlockBodyIndicesProvider
        + BlockNumReader
        + MetadataProvider
        + StageCheckpointReader
        + StorageSettingsCache,
{
    verify_storage_v2(provider_factory)?;

    let block = manifest.execution_anchor.parent_block_number;
    let provider = provider_factory.database_provider_ro()?;
    if provider.last_block_number()? != block ||
        provider.block_hash(block)? != Some(manifest.execution_anchor.parent_block_hash)
    {
        eyre::bail!(
            "reopened checkpoint database no longer binds exact anchor {} ({})",
            block,
            manifest.execution_anchor.parent_block_hash
        )
    }
    let body_indices = provider
        .block_body_indices(block)?
        .ok_or_else(|| eyre::eyre!("reopened checkpoint anchor has no block-body indices"))?;
    if body_indices.tx_count != 0 {
        eyre::bail!(
            "reopened checkpoint anchor contains {} transactions; the sparse anchor must be empty",
            body_indices.tx_count
        )
    }
    for stage in StageId::ALL {
        let checkpoint = provider
            .get_stage_checkpoint(stage)?
            .ok_or_else(|| eyre::eyre!("reopened checkpoint database is missing {stage} stage"))?;
        if checkpoint.block_number != block {
            eyre::bail!(
                "reopened checkpoint {stage} stage is at {}, expected anchor {block}",
                checkpoint.block_number
            )
        }
    }

    let tx = provider.tx_ref();
    let plain_accounts = tx.entries::<tables::PlainAccountState>()?;
    let plain_storages = tx.entries::<tables::PlainStorageState>()?;
    let account_changesets = tx.entries::<tables::AccountChangeSets>()?;
    let storage_changesets = tx.entries::<tables::StorageChangeSets>()?;
    let hashed_accounts = tx.entries::<tables::HashedAccounts>()?;
    let hashed_storages = tx.entries::<tables::HashedStorages>()?;
    if plain_accounts != 0 ||
        plain_storages != 0 ||
        account_changesets != 0 ||
        storage_changesets != 0 ||
        hashed_accounts == 0
    {
        eyre::bail!(
            "reopened checkpoint has invalid storage-v2 routing: plain accounts/storage {plain_accounts}/{plain_storages}, MDBX account/storage changesets {account_changesets}/{storage_changesets}, hashed accounts/storage {hashed_accounts}/{hashed_storages}"
        )
    }
    let mut accounts = tx.cursor_read::<tables::HashedAccounts>()?;
    for entry in accounts.walk(None)? {
        let (hashed_address, account) = entry?;
        if let Some(code_hash) = account.bytecode_hash.filter(|hash| *hash != KECCAK_EMPTY) {
            let bytecode = tx.get::<tables::Bytecodes>(code_hash)?.ok_or_else(|| {
                eyre::eyre!(
                    "reopened checkpoint account {hashed_address} references missing bytecode {code_hash}"
                )
            })?;
            let actual_hash = bytecode.hash_slow();
            if actual_hash != code_hash {
                eyre::bail!(
                    "reopened checkpoint bytecode hash mismatch for account {hashed_address}: account {code_hash}, bytes {actual_hash}"
                )
            }
        }
    }

    // Recompute without consulting persisted trie nodes, then separately check the persisted trie.
    let recomputed_state_root =
        StateRoot::new(NoopTrieCursorFactory::default(), DatabaseHashedCursorFactory::new(tx))
            .root()?;
    let persisted_trie_root = reth_trie_db::with_adapter!(&provider, |A| {
        StateRoot::new(
            DatabaseTrieCursorFactory::<_, A>::new(tx),
            DatabaseHashedCursorFactory::new(tx),
        )
        .root()
    })?;
    drop(provider);
    if recomputed_state_root != manifest.actual_state_root ||
        persisted_trie_root != recomputed_state_root
    {
        eyre::bail!(
            "reopened checkpoint root mismatch: manifest {}, recomputed hashed state {recomputed_state_root}, persisted trie {persisted_trie_root}",
            manifest.actual_state_root
        )
    }

    let static_files = provider_factory.static_file_provider();
    for segment in StaticFileSegment::iter() {
        let range = static_files.get_lowest_range(segment).ok_or_else(|| {
            eyre::eyre!("reopened checkpoint is missing {segment} static-file segment")
        })?;
        if range.start() != block ||
            range.end() != block ||
            static_files.get_highest_static_file_block(segment) != Some(block)
        {
            eyre::bail!(
                "reopened checkpoint {segment} static-file range is {range:?}, expected only {block}"
            )
        }
    }

    let rocksdb = provider_factory.rocksdb_provider();
    if rocksdb.first::<tables::TransactionHashNumbers>()?.is_some() {
        eyre::bail!("reopened checkpoint has transaction-hash history at its empty anchor")
    }
    let mut account_histories = 0usize;
    for entry in rocksdb.iter::<tables::AccountsHistory>()? {
        let (_, history) = entry?;
        validate_anchor_history("account", block, &history)?;
        account_histories = account_histories
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("account-history count overflow"))?;
    }
    let mut storage_histories = 0usize;
    for entry in rocksdb.iter::<tables::StoragesHistory>()? {
        let (_, history) = entry?;
        validate_anchor_history("storage", block, &history)?;
        storage_histories = storage_histories
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("storage-history count overflow"))?;
    }
    if account_histories != hashed_accounts || storage_histories != hashed_storages {
        eyre::bail!(
            "reopened checkpoint RocksDB history count mismatch: accounts {account_histories}/{hashed_accounts}, storage {storage_histories}/{hashed_storages}"
        )
    }

    Ok(recomputed_state_root)
}

fn validate_anchor_history(label: &str, block: u64, history: &IntegerList) -> eyre::Result<()> {
    if history.len() != 1 || !history.contains(block) {
        eyre::bail!(
            "reopened checkpoint {label} history {history:?} is not bound exclusively to anchor {block}"
        )
    }
    Ok(())
}

fn refuse_existing_output(path: &std::path::Path) -> eyre::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => eyre::bail!("refusing to overwrite checkpoint artifact {}", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).map_err(|error| {
                eyre::eyre!("failed to inspect checkpoint artifact {}: {error}", path.display())
            })
        }
    }
    Ok(())
}

fn write_json_atomically(
    path: &std::path::Path,
    value: &impl serde::Serialize,
) -> eyre::Result<()> {
    reth_fs_util::atomic_write_file(path, |file| serde_json::to_writer_pretty(file, value))?;
    Ok(())
}

fn main() -> eyre::Result<()> {
    let command = Command::parse();
    let runner = CliRunner::try_default_runtime()?;
    let runtime = runner.runtime();
    runner.run_blocking_until_ctrl_c(command.execute(runtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_storage_paths_must_not_exist_or_alias() {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let static_files = root.path().join("static-files");
        let rocksdb = root.path().join("rocksdb");

        require_new_storage_paths(&data_dir, &static_files, &rocksdb).unwrap();
        std::fs::create_dir(&static_files).unwrap();
        assert!(require_new_storage_paths(&data_dir, &static_files, &rocksdb)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        assert!(require_new_storage_paths(&data_dir, &data_dir, &rocksdb)
            .unwrap_err()
            .to_string()
            .contains("same path"));
    }

    #[test]
    fn checkpoint_histories_must_bind_only_the_anchor() {
        let anchor = 42;
        validate_anchor_history("account", anchor, &IntegerList::new([anchor]).unwrap()).unwrap();

        assert!(validate_anchor_history(
            "account",
            anchor,
            &IntegerList::new([anchor - 1, anchor]).unwrap(),
        )
        .unwrap_err()
        .to_string()
        .contains("exclusively"));
        assert!(validate_anchor_history(
            "account",
            anchor,
            &IntegerList::new([anchor + 1]).unwrap(),
        )
        .unwrap_err()
        .to_string()
        .contains("exclusively"));
    }
}

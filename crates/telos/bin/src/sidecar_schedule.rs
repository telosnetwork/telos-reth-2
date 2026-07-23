//! Offline Telos sidecar schedule verification command.

use alloy_primitives::B256;
use clap::{Args, ValueEnum};
use reth_chainspec::EthChainSpec;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_cli_runner::CliRunner;
use reth_node_ethereum::EthereumNode;
use reth_node_telos::{
    checkpoint::TelosCheckpointAudit,
    sidecar::TelosFinalizedCoverage,
    sidecar_schedule::{scan_frozen_finalized_gas_price_schedule, TelosGasPriceScheduleScan},
    TelosChainSpecParser,
};
use reth_provider::{
    providers::RocksDBProvider, DBProvider, DatabaseProviderFactory, MetadataProvider,
    StorageSettings, StorageSettingsCache,
};
use serde::Serialize;
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

const OUTPUT_SCHEMA: &str = "telos-reth-sidecar-gas-price-schedule-command/v1";

/// Verifies a frozen Telos storage generation and emits its authenticated gas-price schedule.
#[derive(Debug, Args)]
pub(crate) struct TelosSidecarScheduleCommand {
    /// Reth storage and checkpoint-chain configuration.
    ///
    /// `--chain` must select the same completed checkpoint manifest supplied below.
    #[command(flatten)]
    env: EnvironmentArgs<TelosChainSpecParser>,

    /// Trusted checkpoint manifest with a matching completed audit.
    #[arg(long, value_name = "PATH")]
    checkpoint_manifest: PathBuf,

    /// Exact finalized and persisted tip number of the frozen generation.
    #[arg(long)]
    tip_number: u64,

    /// Exact finalized and persisted tip hash of the frozen generation.
    #[arg(long)]
    tip_hash: B256,

    /// MDBX access mode for this scan.
    ///
    /// Use `exclusive` for a stopped source. `cooperative` is only safe when an external
    /// qualification lock proves the complete MDBX, static-file, and `RocksDB` generation cannot
    /// change during the scan.
    #[arg(long, value_enum)]
    database_access: DatabaseAccess,
}

impl TelosSidecarScheduleCommand {
    /// Runs the blocking storage scan on the CLI runtime.
    pub(crate) fn execute(self, runner: CliRunner) -> eyre::Result<()> {
        let runtime = runner.runtime();
        runner.run_blocking_until_ctrl_c(self.scan(runtime))
    }

    async fn scan(mut self, runtime: reth_tasks::Runtime) -> eyre::Result<()> {
        if !self.env.storage.v2 {
            eyre::bail!("sidecar schedule scanning requires Reth storage v2");
        }
        if self.env.datadir.datadir.as_ref().is_none() {
            eyre::bail!("sidecar schedule scanning requires an explicit --datadir");
        }

        let (checkpoint, checkpoint_manifest_sha256) =
            TelosCheckpointAudit::load_completed_with_sha256(&self.checkpoint_manifest)?;
        let checkpoint_chain = checkpoint.checkpoint_chain_spec()?;
        if self.env.chain.as_ref() != checkpoint_chain.as_ref() {
            eyre::bail!(
                "--chain must select the exact audited checkpoint manifest supplied to --checkpoint-manifest"
            );
        }

        let requested_exclusive = self.database_access.is_exclusive();
        if let Some(configured_exclusive) = self.env.db.exclusive &&
            configured_exclusive != requested_exclusive
        {
            eyre::bail!(
                "--db.exclusive={configured_exclusive} conflicts with --database-access {}",
                self.database_access.as_str()
            );
        }
        self.env.db.exclusive = Some(requested_exclusive);

        let resolved_data_dir = self.env.datadir.clone().resolve_datadir(self.env.chain.chain());
        let database_path = resolved_data_dir.db();
        let static_files_path = resolved_data_dir.static_files();
        let rocksdb_path = resolved_data_dir.rocksdb();
        preflight_storage_directories(&database_path, &static_files_path, &rocksdb_path)?;
        if !RocksDBProvider::exists(&rocksdb_path) {
            eyre::bail!(
                "frozen generation is missing RocksDB at {}; refusing read-only initialization that would create it",
                rocksdb_path.display()
            );
        }

        let Environment { provider_factory, .. } =
            self.env.init::<EthereumNode>(AccessRights::RO, runtime)?;
        // The full-range scan can legitimately exceed MDBX's default long-read timeout. This is
        // safe only because `exclusive` requires a stopped source and `cooperative` requires the
        // external qualification lock documented on `--database-access`.
        let provider =
            provider_factory.database_provider_ro()?.disable_long_read_transaction_safety();
        verify_storage_v2(&provider_factory, &provider)?;

        let schedule = scan_frozen_finalized_gas_price_schedule(
            &provider,
            checkpoint.canonical_chain,
            checkpoint.execution_anchor,
            TelosFinalizedCoverage { block_number: self.tip_number, block_hash: self.tip_hash },
        )?;
        drop(provider);
        drop(provider_factory);

        let output = TelosSidecarScheduleOutput {
            schema: OUTPUT_SCHEMA,
            database_access: self.database_access,
            checkpoint_manifest_sha256,
            schedule,
        };
        write_canonical_json_line(io::stdout().lock(), &output)
    }
}

/// The database locking contract selected for a schedule scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum DatabaseAccess {
    /// Share the environment; an external lock must freeze every storage backend.
    Cooperative,
    /// Require MDBX exclusive access to a stopped source.
    Exclusive,
}

impl DatabaseAccess {
    const fn is_exclusive(self) -> bool {
        matches!(self, Self::Exclusive)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Cooperative => "cooperative",
            Self::Exclusive => "exclusive",
        }
    }
}

/// Stable wrapper that binds the scan result to its database locking contract.
#[derive(Debug, Serialize)]
struct TelosSidecarScheduleOutput {
    schema: &'static str,
    database_access: DatabaseAccess,
    checkpoint_manifest_sha256: B256,
    schedule: TelosGasPriceScheduleScan,
}

fn preflight_storage_directories(
    database_path: &Path,
    static_files_path: &Path,
    rocksdb_path: &Path,
) -> eyre::Result<()> {
    let paths =
        [("MDBX", database_path), ("static files", static_files_path), ("RocksDB", rocksdb_path)];
    for (label, path) in paths {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            eyre::eyre!("failed to inspect frozen {label} directory {}: {error}", path.display())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            eyre::bail!(
                "frozen {label} path must be an existing real directory, not a symlink or non-directory: {}",
                path.display()
            );
        }
    }

    let canonical_paths = [
        std::fs::canonicalize(database_path)?,
        std::fs::canonicalize(static_files_path)?,
        std::fs::canonicalize(rocksdb_path)?,
    ];
    for index in 0..canonical_paths.len() {
        for other in (index + 1)..canonical_paths.len() {
            if canonical_paths[index] == canonical_paths[other] {
                eyre::bail!(
                    "frozen storage paths must be pairwise distinct; {} and {} resolve to {}",
                    paths[index].0,
                    paths[other].0,
                    canonical_paths[index].display()
                );
            }
            if canonical_paths[index].starts_with(&canonical_paths[other]) ||
                canonical_paths[other].starts_with(&canonical_paths[index])
            {
                eyre::bail!(
                    "frozen storage paths must not contain one another; {} is {} and {} is {}",
                    paths[index].0,
                    canonical_paths[index].display(),
                    paths[other].0,
                    canonical_paths[other].display()
                );
            }
        }
    }
    Ok(())
}

fn verify_storage_v2<PF, P>(provider_factory: &PF, provider: &P) -> eyre::Result<()>
where
    PF: StorageSettingsCache,
    P: MetadataProvider,
{
    let persisted = provider
        .storage_settings()?
        .ok_or_else(|| eyre::eyre!("frozen generation is missing persisted storage settings"))?;
    let expected = StorageSettings::v2();
    if persisted != expected || provider_factory.cached_storage_settings() != expected {
        eyre::bail!(
            "frozen generation is not storage v2: persisted {persisted:?}, cached {:?}",
            provider_factory.cached_storage_settings()
        );
    }
    Ok(())
}

fn write_canonical_json_line(mut writer: impl Write, output: &impl Serialize) -> eyre::Result<()> {
    serde_json::to_writer(&mut writer, output)?;
    writeln!(writer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_access_has_stable_cli_and_json_names() {
        assert_eq!(
            DatabaseAccess::from_str("cooperative", false).unwrap(),
            DatabaseAccess::Cooperative
        );
        assert_eq!(
            DatabaseAccess::from_str("exclusive", false).unwrap(),
            DatabaseAccess::Exclusive
        );
        assert_eq!(DatabaseAccess::Cooperative.as_str(), "cooperative");
        assert_eq!(DatabaseAccess::Exclusive.as_str(), "exclusive");
        assert_eq!(serde_json::to_string(&DatabaseAccess::Cooperative).unwrap(), "\"cooperative\"");
        assert_eq!(serde_json::to_string(&DatabaseAccess::Exclusive).unwrap(), "\"exclusive\"");
    }

    #[test]
    fn storage_directories_must_be_existing_and_distinct() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("db");
        let static_files = root.path().join("static_files");
        let rocksdb = root.path().join("rocksdb");
        for path in [&database, &static_files, &rocksdb] {
            std::fs::create_dir(path).unwrap();
        }

        preflight_storage_directories(&database, &static_files, &rocksdb).unwrap();
        assert!(preflight_storage_directories(&database, &database, &rocksdb).is_err());
        assert!(preflight_storage_directories(
            &root.path().join("missing"),
            &static_files,
            &rocksdb
        )
        .is_err());

        let nested = database.join("nested");
        std::fs::create_dir(&nested).unwrap();
        assert!(preflight_storage_directories(&database, &nested, &rocksdb).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn storage_directories_reject_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("db");
        let static_files = root.path().join("static_files");
        let rocksdb = root.path().join("rocksdb");
        let linked_rocksdb = root.path().join("linked-rocksdb");
        for path in [&database, &static_files, &rocksdb] {
            std::fs::create_dir(path).unwrap();
        }
        std::os::unix::fs::symlink(&rocksdb, &linked_rocksdb).unwrap();

        assert!(preflight_storage_directories(&database, &static_files, &linked_rocksdb).is_err());
    }

    #[test]
    fn canonical_json_is_compact_and_newline_terminated() {
        #[derive(Serialize)]
        struct Record {
            schema: &'static str,
            database_access: DatabaseAccess,
        }

        let mut output = Vec::new();
        write_canonical_json_line(
            &mut output,
            &Record { schema: "test/v1", database_access: DatabaseAccess::Exclusive },
        )
        .unwrap();

        assert_eq!(
            output,
            br#"{"schema":"test/v1","database_access":"exclusive"}
"#
        );
    }
}

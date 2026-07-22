//! Telos-reth binary entrypoint.
#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// Required for "override_allocator_on_supported_platforms".
#[cfg(all(feature = "jemalloc", unix))]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

use clap::{Parser, Subcommand};
use reth::{
    cli::{Cli, Commands},
    rpc::builder::RethRpcModule,
    version::{default_reth_version_metadata, try_init_version_metadata, RethCliVersionConsts},
};
use reth_cli_runner::CliRunner;
use reth_ethereum_cli::ExtendedCommand;
use reth_node_telos::{
    rpc_policy::{
        enforce_exact_auth_rpc_surface, enforce_exact_public_rpc_surface, enforce_telos_rpc_policy,
        validate_telos_transaction_count_block, REPLAY_UNSAFE_RPC_METHODS,
        TELOS_FORWARDER_REQUIRED_RPC_METHODS, TELOS_UNSUPPORTED_AUTH_METHODS,
        TELOS_UNSUPPORTED_RPC_METHODS,
    },
    sidecar::{ProviderTelosSidecarStore, TelosChainIdentity, TelosSidecarTables},
    startup::validate_telos_startup,
    TelosArgs, TelosChainSpecParser, TelosNode, TELOS_REVM_EXECUTION_READY, TELOS_RPC_REPLAY_READY,
};
use reth_rpc_server_types::DefaultRpcModuleValidator;
use reth_telos_rpc::TelosClient;
use std::{borrow::Cow, io::Write};
use tracing::info;

const MISSING_TELOS_EXECUTION_BACKEND: &str =
    "Telos execution is disabled in this release candidate: the revm 41 Telos backend must pass \
exact-build checkpoint, live-companion, restart/reorg, and finalized-RPC qualification before the \
production gate is opened";

#[derive(Debug, Subcommand)]
enum TelosCommands {
    /// Print the machine-readable qualification gates compiled into this exact binary.
    #[command(name = "telos-build-info")]
    BuildInfo,
}

impl ExtendedCommand for TelosCommands {
    fn execute(self, _runner: CliRunner) -> eyre::Result<()> {
        match self {
            Self::BuildInfo => write_telos_build_info(std::io::stdout().lock()),
        }
    }
}

fn telos_build_info() -> serde_json::Value {
    serde_json::json!({
        "schema": "telos-reth-build-info/v1",
        "execution_ready": TELOS_REVM_EXECUTION_READY,
        "rpc_replay_ready": TELOS_RPC_REPLAY_READY,
    })
}

fn write_telos_build_info(mut writer: impl Write) -> eyre::Result<()> {
    serde_json::to_writer(&mut writer, &telos_build_info())?;
    writeln!(writer)?;
    Ok(())
}

fn is_broken_pipe(error: &eyre::Report) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe) ||
        error
            .downcast_ref::<serde_json::Error>()
            .and_then(serde_json::Error::io_error_kind)
            .is_some_and(|kind| kind == std::io::ErrorKind::BrokenPipe)
}

fn telos_version_metadata(upstream: RethCliVersionConsts) -> RethCliVersionConsts {
    let version = env!("CARGO_PKG_VERSION");
    let dev_suffix = if upstream
        .short_version
        .split_whitespace()
        .next()
        .is_some_and(|value| value.ends_with("-dev"))
    {
        "-dev"
    } else {
        ""
    };
    let long_version = format!(
        "Version: {version}{dev_suffix}\n\
         Upstream Reth: {}\n\
         Commit SHA: {}\n\
         Build Timestamp: {}\n\
         Build Features: {}\n\
         Build Profile: {}",
        upstream.cargo_pkg_version,
        upstream.vergen_git_sha_long,
        upstream.vergen_build_timestamp,
        upstream.vergen_cargo_features,
        upstream.build_profile_name,
    );
    let p2p_client_version = format!(
        "telos-reth/v{version}-{}/{}",
        upstream.vergen_git_sha, upstream.vergen_cargo_target_triple
    );

    RethCliVersionConsts {
        name_client: Cow::Borrowed("Telos Reth"),
        cargo_pkg_version: Cow::Borrowed(version),
        short_version: Cow::Owned(format!("{version}{dev_suffix} ({})", upstream.vergen_git_sha)),
        long_version: Cow::Owned(long_version),
        p2p_client_version: Cow::Owned(p2p_client_version),
        ..upstream
    }
}

fn init_telos_version_metadata() {
    if try_init_version_metadata(telos_version_metadata(default_reth_version_metadata())).is_err() {
        eprintln!("Error: Telos build metadata was already initialized; this is a bug");
        std::process::exit(1);
    }
}

fn validate_execution_backend(chain_id: u64) -> eyre::Result<()> {
    if !matches!(chain_id, 40 | 41) {
        eyre::bail!(
            "Telos node only supports chain IDs 40 and 41; configured chain ID is {chain_id}"
        );
    }
    if !TELOS_REVM_EXECUTION_READY {
        eyre::bail!(MISSING_TELOS_EXECUTION_BACKEND);
    }
    Ok(())
}

fn main() {
    // Install Telos distribution metadata before clap, RPC, or node startup reads it.
    init_telos_version_metadata();
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    let cli =
        Cli::<TelosChainSpecParser, TelosArgs, DefaultRpcModuleValidator, TelosCommands>::parse();

    // Keep the build-info contract machine-readable even when the global log default writes to
    // stdout. All other commands initialize tracing through `CliApp::run_with_components`.
    if matches!(&cli.command, Commands::Ext(TelosCommands::BuildInfo)) {
        if let Err(error) = write_telos_build_info(std::io::stdout().lock()) &&
            !is_broken_pipe(&error)
        {
            eprintln!("Error: {error:?}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(err) = cli.run(async move |mut builder, telos_args| {
            info!(target: "reth::cli", "Launching Telos node");
            telos_args.validate()?;
            if enforce_telos_rpc_policy(
                &mut builder.config_mut().rpc,
                TELOS_REVM_EXECUTION_READY,
                TELOS_RPC_REPLAY_READY,
            )? {
                info!(target: "reth::cli", "regular IPC disabled by the Telos replay-safety gate");
            }
            let chain_id = builder.config().chain.chain().id();
            validate_execution_backend(chain_id)?;
            let chain = TelosChainIdentity {
                chain_id,
                genesis_hash: builder.config().chain.genesis_hash(),
            };
            let execution_anchor = telos_args.load_execution_anchor(chain)?;

            let forwarder = if telos_args.forwarder_configured() {
                Some(TelosClient::new(telos_args.clone().into(), chain_id)?)
            } else {
                None
            };

            let mut builder = builder.node(TelosNode::new(telos_args, execution_anchor));
            builder
                .db_mut()
                .create_and_track_tables_for::<TelosSidecarTables>()
                .map_err(|error| eyre::eyre!("failed to initialize Telos sidecar tables: {error}"))?;

            let handle = builder
                .on_component_initialized(move |ctx| {
                    let sidecar_store = ProviderTelosSidecarStore::new(
                        ctx.provider.clone(),
                        execution_anchor.chain,
                    );
                    validate_telos_startup(&ctx.provider, &execution_anchor, &sidecar_store)
                })
                .extend_rpc_modules(move |ctx| {
                    let forwarder_enabled = forwarder.is_some();
                    for method in TELOS_UNSUPPORTED_AUTH_METHODS {
                        ctx.auth_module.remove_auth_method(method);
                    }
                    for method in TELOS_UNSUPPORTED_RPC_METHODS {
                        ctx.modules.remove_method_from_configured(method);
                        ctx.auth_module.remove_auth_method(method);
                    }
                    info!(
                        target: "reth::cli",
                        methods = ?TELOS_UNSUPPORTED_RPC_METHODS,
                        "removed RPC methods incompatible with canonical Telos headers"
                    );
                    if !TELOS_REVM_EXECUTION_READY || !TELOS_RPC_REPLAY_READY {
                        for method in REPLAY_UNSAFE_RPC_METHODS {
                            ctx.modules.remove_method_from_configured(method);
                            ctx.auth_module.remove_auth_method(method);
                        }
                        info!(
                            target: "reth::cli",
                            methods = ?REPLAY_UNSAFE_RPC_METHODS,
                            "removed RPC methods blocked by the Telos replay-safety gate"
                        );
                    }
                    let eth_api = ctx.registry.eth_api().clone();
                    let mut nonce_module = jsonrpsee::RpcModule::new(());
                    nonce_module.register_async_method(
                        "eth_getTransactionCount",
                        move |params, _ctx, _ext| {
                            let eth_api = eth_api.clone();
                            async move {
                                let mut params = params.sequence();
                                let address: alloy_primitives::Address = params.next()?;
                                let block: Option<alloy_eips::BlockId> = params.optional_next()?;
                                validate_telos_transaction_count_block(block)
                                    .map_err(|error| {
                                        jsonrpsee::types::ErrorObjectOwned::owned(
                                            -32000,
                                            error,
                                            None::<()>,
                                        )
                                    })?;
                                reth::rpc::eth::EthApiServer::transaction_count(
                                    &eth_api, address, block,
                                )
                                .await
                            }
                        },
                    )?;
                    ctx.modules
                        .add_or_replace_if_module_configured(RethRpcModule::Eth, nonce_module)?;
                    if let Some(client) = forwarder {
                        ctx.auth_module.remove_auth_method("eth_sendRawTransaction");
                        info!(target: "reth::cli", endpoint = client.endpoint(), "installing Telos transaction forwarder");
                        let module = client
                            .build_forwarder_module()
                            .map_err(|error| eyre::eyre!("failed to build Telos forwarder: {error}"))?;
                        if !ctx
                            .modules
                            .module_config()
                            .contains_any(&RethRpcModule::Eth)
                        {
                            eyre::bail!(
                                "Telos forwarder is configured but the eth namespace is not enabled on any RPC transport"
                            );
                        }
                        ctx.modules
                            .add_or_replace_if_module_configured(RethRpcModule::Eth, module)?;
                        info!(target: "reth::cli", "Telos transaction forwarder installed");
                    } else {
                        for method in TELOS_FORWARDER_REQUIRED_RPC_METHODS {
                            ctx.modules.remove_method_from_configured(method);
                            ctx.auth_module.remove_auth_method(method);
                        }
                        info!(
                            target: "reth::cli",
                            methods = ?TELOS_FORWARDER_REQUIRED_RPC_METHODS,
                            "removed RPC methods that require a native Telos forwarder"
                        );
                    }
                    enforce_exact_public_rpc_surface(ctx.modules, forwarder_enabled)?;
                    enforce_exact_auth_rpc_surface(ctx.auth_module.module_mut())?;
                    Ok(())
                })
                .launch_with_debug_capabilities()
                .await?;

            handle.wait_for_node_exit().await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_telos_chain_is_rejected_as_unsupported() {
        let error = validate_execution_backend(1).unwrap_err().to_string();
        assert_eq!(error, "Telos node only supports chain IDs 40 and 41; configured chain ID is 1");
    }

    #[test]
    fn telos_chain_validation_tracks_release_gate() {
        for chain_id in [40, 41] {
            let result = validate_execution_backend(chain_id);
            if TELOS_REVM_EXECUTION_READY {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().to_string(), MISSING_TELOS_EXECUTION_BACKEND);
            }
        }
    }

    #[test]
    fn binary_metadata_uses_telos_distribution_identity() {
        let upstream = default_reth_version_metadata();
        let upstream_extra_data = upstream.extra_data.clone();
        let upstream_version = upstream.cargo_pkg_version.clone();
        let metadata = telos_version_metadata(upstream);

        assert_eq!(metadata.name_client, "Telos Reth");
        assert_eq!(metadata.cargo_pkg_version, env!("CARGO_PKG_VERSION"));
        assert!(metadata.short_version.starts_with(env!("CARGO_PKG_VERSION")));
        assert!(metadata
            .long_version
            .starts_with(&format!("Version: {}", env!("CARGO_PKG_VERSION"))));
        assert!(metadata.long_version.contains(&format!("Upstream Reth: {upstream_version}")));
        assert!(metadata
            .p2p_client_version
            .starts_with(&format!("telos-reth/v{}-", env!("CARGO_PKG_VERSION"))));
        assert_eq!(metadata.extra_data, upstream_extra_data);
    }

    #[test]
    fn build_info_reports_both_independent_qualification_gates() {
        let info = telos_build_info();
        assert_eq!(info["schema"], "telos-reth-build-info/v1");
        assert_eq!(info["execution_ready"], TELOS_REVM_EXECUTION_READY);
        assert_eq!(info["rpc_replay_ready"], TELOS_RPC_REPLAY_READY);
    }

    #[test]
    fn build_info_output_is_exactly_one_json_line() {
        let mut output = Vec::new();
        write_telos_build_info(&mut output).unwrap();

        assert_eq!(output.last(), Some(&b'\n'));
        assert!(!output[..output.len() - 1].contains(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
            telos_build_info()
        );
    }
}

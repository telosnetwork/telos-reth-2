//! Telos-reth binary entrypoint.
#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// Required for "override_allocator_on_supported_platforms".
#[cfg(all(feature = "jemalloc", unix))]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

use clap::Parser;
use reth::{
    cli::Cli,
    version::{default_reth_version_metadata, try_init_version_metadata, RethCliVersionConsts},
};
use reth_node_telos::{TelosArgs, TelosChainSpecParser, TelosNode, TELOS_REVM_EXECUTION_READY};
use reth_telos_rpc::TelosClient;
use std::borrow::Cow;
use tracing::info;

const MISSING_TELOS_EXECUTION_BACKEND: &str =
    "Telos execution is disabled: upstream revm 41 lacks the verified Telos fixed_gas_price, \
revision_number, and first_new_address transaction context; port and validate the Telos revm \
backend before production launch";

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

    if let Err(err) =
        Cli::<TelosChainSpecParser, TelosArgs>::parse().run(async move |builder, telos_args| {
            info!(target: "reth::cli", "Launching Telos node");
            telos_args.validate()?;
            let chain_id = builder.config().chain.chain().id();
            validate_execution_backend(chain_id)?;

            let forwarder = if telos_args.forwarder_configured() {
                Some(TelosClient::new(telos_args.clone().into(), chain_id)?)
            } else {
                None
            };

            let handle = builder
                .node(TelosNode::new(telos_args))
                .extend_rpc_modules(move |ctx| {
                    if let Some(client) = forwarder {
                        info!(target: "reth::cli", endpoint = client.endpoint(), "installing Telos transaction forwarder");
                        let module = client
                            .build_forwarder_module()
                            .map_err(|error| eyre::eyre!("failed to build Telos forwarder: {error}"))?;
                        if !ctx.modules.replace_configured(module)? {
                            eyre::bail!(
                                "Telos forwarder methods were not enabled on any configured RPC transport"
                            );
                        }
                        info!(target: "reth::cli", "Telos transaction forwarder installed");
                    }
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
    fn telos_chains_are_rejected_by_missing_backend_gate() {
        for chain_id in [40, 41] {
            let error = validate_execution_backend(chain_id).unwrap_err().to_string();
            assert_eq!(error, MISSING_TELOS_EXECUTION_BACKEND);
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
}

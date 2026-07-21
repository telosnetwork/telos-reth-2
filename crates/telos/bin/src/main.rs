//! Telos-reth binary entrypoint.
#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

// Required for "override_allocator_on_supported_platforms".
#[cfg(all(feature = "jemalloc", unix))]
use reth_cli_util::allocator::tikv_jemalloc_sys as _;

use clap::Parser;
use reth::cli::Cli;
use reth_node_telos::{TelosArgs, TelosChainSpecParser, TelosNode, TELOS_REVM_EXECUTION_READY};
use reth_telos_rpc::TelosClient;
use tracing::info;

const MISSING_TELOS_EXECUTION_BACKEND: &str =
    "Telos execution is disabled: upstream revm 41 lacks the verified Telos fixed_gas_price, \
revision_number, and first_new_address transaction context; port and validate the Telos revm \
backend before production launch";

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
                Some(TelosClient::new(telos_args.clone().into())?)
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
}

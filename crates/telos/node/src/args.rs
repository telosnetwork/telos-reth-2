//! Command-line arguments for Telos integration.

use crate::sidecar::{TelosChainIdentity, TelosExecutionAnchor};
use reth_telos_rpc::telos_client::TelosClientArgs;
use std::path::PathBuf;

/// Telos node options.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
#[clap(next_help_heading = "Telos")]
pub struct TelosArgs {
    /// Trusted snapshot boundary used to start durable execution-sidecar coverage.
    #[arg(long = "telos.execution-anchor", value_name = "PATH")]
    pub execution_anchor: Option<PathBuf>,

    /// Native Telos HTTP endpoint used for transaction forwarding and gas-price reads.
    #[arg(
        long = "telos.endpoint",
        visible_alias = "telos.telos_endpoint",
        value_name = "HTTP_URL"
    )]
    pub telos_endpoint: Option<String>,

    /// Antelope account that authorizes forwarded transactions.
    #[arg(long = "telos.signer-account", value_name = "ACCOUNT")]
    pub signer_account: Option<String>,

    /// Antelope permission used by the signer account.
    #[arg(long = "telos.signer-permission", value_name = "PERMISSION")]
    pub signer_permission: Option<String>,

    /// File containing the Antelope signer WIF. The file must be a regular, owner-only file.
    #[arg(long = "telos.signer-key-file", value_name = "PATH")]
    pub signer_key_file: Option<PathBuf>,

    /// Seconds to cache the on-chain gas price.
    #[arg(long = "telos.gas-cache-seconds", default_value_t = 8)]
    pub gas_cache_seconds: u32,
}

impl Default for TelosArgs {
    fn default() -> Self {
        Self {
            execution_anchor: None,
            telos_endpoint: None,
            signer_account: None,
            signer_permission: None,
            signer_key_file: None,
            gas_cache_seconds: 8,
        }
    }
}

impl TelosArgs {
    /// Loads and validates the trusted snapshot execution anchor for the selected chain.
    pub fn load_execution_anchor(
        &self,
        chain: TelosChainIdentity,
    ) -> eyre::Result<TelosExecutionAnchor> {
        const MAX_ANCHOR_BYTES: u64 = 64 * 1024;

        let path = self.execution_anchor.as_ref().ok_or_else(|| {
            eyre::eyre!(
                "missing --telos.execution-anchor; production execution requires a trusted snapshot boundary"
            )
        })?;
        let metadata = reth_fs_util::metadata(path)?;
        if !metadata.is_file() {
            eyre::bail!("Telos execution anchor is not a regular file: {}", path.display());
        }
        if metadata.len() > MAX_ANCHOR_BYTES {
            eyre::bail!(
                "Telos execution anchor exceeds {MAX_ANCHOR_BYTES} bytes: {}",
                path.display()
            );
        }
        let anchor: TelosExecutionAnchor = reth_fs_util::read_json_file(path)?;
        anchor.validate_for_chain(chain)?;
        Ok(anchor)
    }

    /// Returns true when any transaction-forwarder option was supplied.
    pub const fn forwarder_configured(&self) -> bool {
        self.telos_endpoint.is_some() ||
            self.signer_account.is_some() ||
            self.signer_permission.is_some() ||
            self.signer_key_file.is_some()
    }

    /// Validates that transaction-forwarder options are either all present or all absent.
    pub fn validate(&self) -> eyre::Result<()> {
        if !self.forwarder_configured() {
            return Ok(())
        }

        let mut missing = Vec::new();
        if self.telos_endpoint.is_none() {
            missing.push("--telos.endpoint")
        }
        if self.signer_account.is_none() {
            missing.push("--telos.signer-account")
        }
        if self.signer_permission.is_none() {
            missing.push("--telos.signer-permission")
        }
        if self.signer_key_file.is_none() {
            missing.push("--telos.signer-key-file")
        }
        if !missing.is_empty() {
            eyre::bail!(
                "incomplete Telos transaction-forwarder configuration; missing {}",
                missing.join(", ")
            );
        }
        Ok(())
    }
}

impl From<TelosArgs> for TelosClientArgs {
    fn from(args: TelosArgs) -> Self {
        Self {
            telos_endpoint: args.telos_endpoint,
            signer_account: args.signer_account,
            signer_permission: args.signer_permission,
            signer_key_file: args.signer_key_file,
            gas_cache_seconds: Some(args.gas_cache_seconds),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::TELOS_EXECUTION_ANCHOR_VERSION;
    use alloy_primitives::{B256, U256};
    use clap::{Args, Parser};

    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[clap(flatten)]
        args: T,
    }

    #[test]
    fn defaults_do_not_enable_forwarding() {
        let args = CommandParser::<TelosArgs>::parse_from(["reth"]).args;
        assert_eq!(args, TelosArgs::default());
        assert!(!args.forwarder_configured());
        assert!(args.validate().is_ok());
    }

    #[test]
    fn partial_forwarder_configuration_is_rejected() {
        let args = CommandParser::<TelosArgs>::parse_from([
            "reth",
            "--telos.endpoint",
            "https://example.invalid",
        ])
        .args;
        let error = args.validate().unwrap_err().to_string();
        assert!(error.contains("--telos.signer-key-file"));
    }

    #[test]
    fn signer_key_is_only_accepted_as_a_file() {
        let parsed = CommandParser::<TelosArgs>::try_parse_from([
            "reth",
            "--telos.signer-key",
            "private-key",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn execution_anchor_is_required_and_chain_bound() {
        let chain = TelosChainIdentity { chain_id: 40, genesis_hash: B256::repeat_byte(0x40) };
        let missing = TelosArgs::default().load_execution_anchor(chain).unwrap_err();
        assert!(missing.to_string().contains("--telos.execution-anchor"));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("anchor.json");
        let anchor = TelosExecutionAnchor {
            version: TELOS_EXECUTION_ANCHOR_VERSION,
            chain,
            parent_block_number: 7,
            parent_block_hash: B256::repeat_byte(0x77),
            starting_gas_price: U256::from(7),
            starting_revision: 1,
        };
        reth_fs_util::write_json_file(&path, &anchor).unwrap();
        let args = TelosArgs { execution_anchor: Some(path), ..Default::default() };
        assert_eq!(args.load_execution_anchor(chain).unwrap(), anchor);

        let wrong_chain = TelosChainIdentity { chain_id: 41, ..chain };
        assert!(args.load_execution_anchor(wrong_chain).unwrap_err().to_string().contains("chain"));
    }
}

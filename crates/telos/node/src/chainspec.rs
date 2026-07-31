//! Telos chain specification parser.
//!
//! Extends the Ethereum chain spec parser with Telos-specific chains.

use crate::checkpoint::{checkpoint_manifest_path, TelosCheckpointAudit};
use reth_chainspec::ChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use std::sync::Arc;

/// Telos Mainnet chain spec (chain ID 40)
pub static TELOS_MAINNET: once_cell::sync::Lazy<Arc<ChainSpec>> =
    once_cell::sync::Lazy::new(|| {
        let genesis: alloy_genesis::Genesis =
            serde_json::from_str(include_str!("../res/telos-mainnet.json"))
                .expect("Failed to parse telos-mainnet.json");
        Arc::new(genesis.into())
    });

/// Telos Testnet chain spec (chain ID 41)
pub static TELOS_TESTNET: once_cell::sync::Lazy<Arc<ChainSpec>> =
    once_cell::sync::Lazy::new(|| {
        let genesis: alloy_genesis::Genesis =
            serde_json::from_str(include_str!("../res/telos-testnet.json"))
                .expect("Failed to parse telos-testnet.json");
        Arc::new(genesis.into())
    });

/// Chains supported by the Telos node.
pub const SUPPORTED_CHAINS: &[&str] = &[
    "telos",
    "telos-mainnet",
    "telos-testnet",
    "tevmmainnet",
    "tevmtestnet",
    "telos-checkpoint:<manifest>",
];

/// Clap value parser for [`ChainSpec`]s that includes Telos chains.
pub fn telos_chain_value_parser(s: &str) -> eyre::Result<Arc<ChainSpec>, eyre::Error> {
    if let Some(path) = checkpoint_manifest_path(s) {
        let manifest = TelosCheckpointAudit::load_completed(&path)?;
        return manifest.checkpoint_chain_spec()
    }

    Ok(match s {
        "telos-mainnet" | "telos" | "tevmmainnet" => TELOS_MAINNET.clone(),
        "telos-testnet" | "tevmtestnet" => TELOS_TESTNET.clone(),
        // Fall back to the Ethereum chain spec parser
        _ => reth_ethereum_cli::chainspec::chain_value_parser(s)?,
    })
}

/// Telos chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TelosChainSpecParser;

impl ChainSpecParser for TelosChainSpecParser {
    type ChainSpec = ChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<ChainSpec>> {
        telos_chain_value_parser(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_telos_chains() {
        assert!(TelosChainSpecParser::parse("telos-mainnet").is_ok());
        assert!(TelosChainSpecParser::parse("telos-testnet").is_ok());
        assert!(TelosChainSpecParser::parse("telos").is_ok());
        assert!(TelosChainSpecParser::parse("tevmmainnet").is_ok());
        assert!(TelosChainSpecParser::parse("tevmtestnet").is_ok());
    }

    #[test]
    fn telos_mainnet_chain_id() {
        let spec = TelosChainSpecParser::parse("telos-mainnet").unwrap();
        assert_eq!(spec.chain().id(), 40);
    }

    #[test]
    fn telos_testnet_chain_id() {
        let spec = TelosChainSpecParser::parse("telos-testnet").unwrap();
        assert_eq!(spec.chain().id(), 41);
    }

    #[test]
    fn canonical_genesis_hashes_match_live_networks() {
        use alloy_primitives::b256;

        assert_eq!(
            TELOS_MAINNET.genesis_hash(),
            b256!("36fe7024b760365e3970b7b403e161811c1e626edd68460272fcdfa276272563")
        );
        assert_eq!(
            TELOS_TESTNET.genesis_hash(),
            b256!("b25034033c9ca7a40e879ddcc29cf69071a22df06688b5fe8cc2d68b4e0528f9")
        );
    }
}

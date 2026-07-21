//! Telos-specific Reth configuration and builder types.
//!
//! This crate provides the Telos node type that extends the standard Ethereum
//! node with Telos-specific functionality including native chain transaction
//! forwarding and state diff comparison.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/telosnetwork/telos-reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use reth_revm as _;
use revm as _;

pub mod args;
pub mod chainspec;
pub mod engine;
pub mod evm;
pub mod node;
pub mod rpc;
pub mod types;

pub use args::TelosArgs;
pub use chainspec::TelosChainSpecParser;
pub use node::TelosNode;

/// Default persistence threshold for engine experimental mode
pub const DEFAULT_PERSISTENCE_THRESHOLD: u64 = 16;
/// Default memory block buffer target
pub const DEFAULT_MEMORY_BLOCK_BUFFER_TARGET: u64 = 16;
/// Default maximum execute block batch size
pub const DEFAULT_MAX_EXECUTE_BLOCK_BATCH_SIZE: usize = 50;

/// Whether the selected revm backend implements the Telos per-transaction execution context.
///
/// Upstream revm 41 does not expose Telos's fixed gas price, revision number, or first-new-address
/// transaction fields. In addition, the payload-local sender helper cannot cover stored-block,
/// backfill, pruning, or RPC recovery; those paths require a Telos transaction type or equivalent
/// chain-aware recovery throughout Reth. Keep startup fail-closed until both gaps are ported and
/// verified.
pub const TELOS_REVM_EXECUTION_READY: bool = false;

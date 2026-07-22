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
pub mod block;
pub mod chainspec;
pub mod checkpoint;
pub mod engine;
pub mod evm;
pub mod execution;
pub mod frame;
pub mod handler;
pub mod instructions;
pub mod node;
pub mod receipt;
pub mod rpc;
pub mod rpc_policy;
pub mod sidecar;
pub mod startup;
pub mod tree;
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
/// The revm 41 execution port and chain-aware sender recovery are implemented, but this release
/// gate remains closed until the exact build has completed checkpoint bootstrap, live companion
/// ingestion, restart/reorg, and finalized-RPC parity qualification. Opening the gate is therefore
/// an explicit promotion decision rather than an implementation fallback.
pub const TELOS_REVM_EXECUTION_READY: bool = true;

/// Whether historical replay and tracing paths are proven to apply Telos execution semantics.
///
/// Keep this independent from [`TELOS_REVM_EXECUTION_READY`]: canonical block execution can be
/// production-qualified before every diagnostic replay implementation is safe to expose.
pub const TELOS_RPC_REPLAY_READY: bool = false;

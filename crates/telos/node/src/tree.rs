//! Telos engine-tree validation wiring.

use crate::engine::TelosEngineValidatorBuilder;
use reth_chain_state::StateTrieOverlayManager;
use reth_chainspec::EthChainSpec;
use reth_evm::ConfigureEngineEvm;
use reth_node_api::{
    AddOnsContext, BlockTy, FullNodeComponents, NodeTypes, PayloadTypes, PrimitivesTy, TreeConfig,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{BasicEngineValidator, ChangesetCache, EngineValidatorBuilder, PayloadValidatorBuilder},
};
use std::sync::Arc;

/// Builds the Telos engine-tree validator with a reconciliation-safe state-root strategy.
#[derive(Debug, Clone, Copy, Default)]
pub struct TelosEngineTreeValidatorBuilder;

impl<Node> EngineValidatorBuilder<Node> for TelosEngineTreeValidatorBuilder
where
    Node: FullNodeComponents<
        Evm: ConfigureEngineEvm<
            <<Node::Types as NodeTypes>::Payload as PayloadTypes>::ExecutionData,
        >,
    >,
    TelosEngineValidatorBuilder: PayloadValidatorBuilder<Node>,
    <TelosEngineValidatorBuilder as PayloadValidatorBuilder<Node>>::Validator:
        reth_engine_primitives::PayloadValidator<
                <Node::Types as NodeTypes>::Payload,
                Block = BlockTy<Node::Types>,
            > + Clone,
{
    type EngineValidator = BasicEngineValidator<
        Node::Provider,
        Node::Evm,
        <TelosEngineValidatorBuilder as PayloadValidatorBuilder<Node>>::Validator,
    >;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
        changeset_cache: ChangesetCache,
        state_trie_overlays: StateTrieOverlayManager<PrimitivesTy<Node::Types>>,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = TelosEngineValidatorBuilder.build(ctx).await?;
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;

        Ok(BasicEngineValidator::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            reconciliation_safe_tree_config(tree_config),
            invalid_block_hook,
            changeset_cache,
            state_trie_overlays,
            ctx.node.task_executor().clone(),
        ))
    }
}

/// Native reconciliation applies authoritative account and storage changes after transaction
/// execution. The sparse state-root stream only observes transaction execution, so it cannot be
/// authoritative for Telos. Synchronous calculation hashes the final reconciled bundle exactly
/// once and still persists complete trie updates for proofs and historical state.
const fn reconciliation_safe_tree_config(config: TreeConfig) -> TreeConfig {
    config.with_state_root_fallback(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telos_always_hashes_the_final_reconciled_bundle() {
        let config = reconciliation_safe_tree_config(
            TreeConfig::default().with_has_enough_parallelism(true),
        );

        assert!(config.state_root_fallback());
        assert!(!config.use_state_root_task());
        assert!(!config.skip_state_root());
    }
}

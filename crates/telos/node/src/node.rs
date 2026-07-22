//! Telos node type and component wiring.

use crate::{
    args::TelosArgs,
    engine::TelosEngineValidatorBuilder,
    evm::TelosExecutorBuilder,
    execution::TelosTxEnvConverter,
    receipt::TelosReceiptConverter,
    rpc::TelosEngineApiBuilder,
    sidecar::{
        ProviderTelosSidecarStore, TelosChainIdentity, TelosExecutionAnchor, TelosSidecarStore,
    },
    tree::TelosEngineTreeValidatorBuilder,
    types::TelosEngineTypes,
};
use alloy_network::Ethereum;
use alloy_rpc_types_eth::TransactionRequest;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks, Hardforks};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{eth::spec::EthExecutorSpec, ConfigureEvm};
use reth_network::EthNetworkPrimitives;
use reth_node_api::{FullNodeComponents, HeaderTy, PayloadAttributesBuilder, PrimitivesTy};
use reth_node_builder::{
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ConsensusBuilder, NoopNetworkBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::{EthApiBuilder, EthApiCtx, Identity, RpcAddOns},
    BuilderContext, DebugNode, Node, NodeAdapter,
};
use reth_node_ethereum::node::{EthereumPayloadBuilder, EthereumPoolBuilder};
use reth_payload_primitives::PayloadTypes;
use reth_provider::{
    providers::ProviderFactoryBuilder, ChainSpecProvider, DatabaseProviderFactory, EthStorage,
};
use reth_rpc::EthApi;
use reth_rpc_convert::{RpcConvert, RpcConverter, SignableTxRequest};
use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;
use reth_rpc_eth_types::{error::FromEvmError, EthApiError};
use reth_stages::StageId;
use std::sync::Arc;

/// Staged sync is not chain-aware for Telos senders or native execution metadata.
///
/// Keep only `Finish` enabled so Reth's pipeline consistency check has an inert checkpoint to
/// inspect. All stages capable of downloading, recovering, executing, indexing, or pruning data
/// remain disabled; canonical progression is exclusively through the authenticated Engine API.
const TELOS_DISABLED_STAGES: &[StageId] = &[
    StageId::Era,
    StageId::Headers,
    StageId::Bodies,
    StageId::SenderRecovery,
    StageId::Execution,
    StageId::PruneSenderRecovery,
    StageId::MerkleUnwind,
    StageId::AccountHashing,
    StageId::StorageHashing,
    StageId::MerkleExecute,
    StageId::TransactionLookup,
    StageId::IndexStorageHistory,
    StageId::IndexAccountHistory,
    StageId::Prune,
];

/// Type configuration for a Telos execution node.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TelosNode {
    /// Additional Telos arguments.
    pub args: TelosArgs,
    /// Trusted boundary where durable native execution sidecars begin.
    pub execution_anchor: TelosExecutionAnchor,
}

impl TelosNode {
    /// Creates a Telos node configuration.
    pub const fn new(args: TelosArgs, execution_anchor: TelosExecutionAnchor) -> Self {
        Self { args, execution_anchor }
    }

    /// Returns the production Telos component set.
    pub fn components<N>(
        &self,
    ) -> ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        NoopNetworkBuilder<EthNetworkPrimitives>,
        TelosExecutorBuilder,
        TelosConsensusBuilder,
    >
    where
        N: FullNodeTypes<
            Types: NodeTypes<
                ChainSpec: Hardforks + EthereumHardforks + EthExecutorSpec,
                Primitives = EthPrimitives,
            >,
        >,
        <N::Types as NodeTypes>::Payload:
            PayloadTypes<BuiltPayload = EthBuiltPayload, PayloadAttributes = EthPayloadAttributes>,
    {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(EthereumPoolBuilder::default())
            .executor(TelosExecutorBuilder::new(self.execution_anchor))
            .payload(BasicPayloadServiceBuilder::default())
            .noop_network::<EthNetworkPrimitives>()
            .consensus(TelosConsensusBuilder)
    }

    /// Instantiates the provider factory builder for a Telos node.
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

/// Builds Ethereum header validation with Telos's nondecreasing timestamp rule.
#[derive(Debug, Default, Clone, Copy)]
pub struct TelosConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for TelosConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
{
    type Consensus = Arc<EthBeaconConsensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(telos_consensus(ctx.chain_spec())))
    }
}

const fn telos_consensus<ChainSpec>(chain_spec: Arc<ChainSpec>) -> EthBeaconConsensus<ChainSpec>
where
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    EthBeaconConsensus::new(chain_spec).with_allow_equal_timestamps(true)
}

impl NodeTypes for TelosNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = TelosEngineTypes;
}

type TelosRpcConverterFor<N> = RpcConverter<
    Ethereum,
    <N as FullNodeComponents>::Evm,
    TelosReceiptConverter<
        <<N as reth_node_api::FullNodeTypes>::Provider as ChainSpecProvider>::ChainSpec,
    >,
    (),
    (),
    (),
    (),
    TelosTxEnvConverter,
>;

type TelosEthApiFor<N> = EthApi<N, TelosRpcConverterFor<N>>;

/// Builds the Ethereum-compatible RPC surface with Telos' transaction environment converter.
#[derive(Debug, Default)]
pub struct TelosEthApiBuilder {
    execution_anchor: Option<TelosExecutionAnchor>,
}

impl TelosEthApiBuilder {
    /// Creates an RPC builder bound to the trusted execution-sidecar anchor.
    pub const fn new(execution_anchor: TelosExecutionAnchor) -> Self {
        Self { execution_anchor: Some(execution_anchor) }
    }
}

impl<N> EthApiBuilder<N> for TelosEthApiBuilder
where
    N: FullNodeComponents<
        Types: NodeTypes<ChainSpec: Hardforks + EthereumHardforks, Primitives = EthPrimitives>,
        Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
    >,
    TelosRpcConverterFor<N>: RpcConvert<
        Primitives = PrimitivesTy<N::Types>,
        Error = EthApiError,
        Network = Ethereum,
        Evm = N::Evm,
    >,
    TransactionRequest: SignableTxRequest<reth_node_api::TxTy<N::Types>>,
    EthApiError: FromEvmError<N::Evm>,
    N::Provider: DatabaseProviderFactory,
{
    type EthApi = TelosEthApiFor<N>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let execution_anchor = self
            .execution_anchor
            .ok_or_else(|| eyre::eyre!("Telos RPC builder is missing its execution anchor"))?;
        let chain_spec = ctx.components.provider().chain_spec();
        let chain = TelosChainIdentity {
            chain_id: chain_spec.chain().id(),
            genesis_hash: chain_spec.genesis_hash(),
        };
        execution_anchor.validate_for_chain(chain)?;
        let sidecar_store: Arc<dyn TelosSidecarStore> =
            Arc::new(ProviderTelosSidecarStore::new(ctx.components.provider().clone(), chain));
        let converter = RpcConverter::new(TelosReceiptConverter::new(
            chain_spec,
            sidecar_store,
            execution_anchor,
        ))
        .with_tx_env_converter(TelosTxEnvConverter);
        Ok(ctx.eth_api_builder().with_rpc_converter(converter).build())
    }
}

type TelosAddOns<N> = RpcAddOns<
    NodeAdapter<N>,
    TelosEthApiBuilder,
    TelosEngineValidatorBuilder,
    TelosEngineApiBuilder<TelosEngineValidatorBuilder>,
    TelosEngineTreeValidatorBuilder,
>;

impl<N> Node<N> for TelosNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        NoopNetworkBuilder<EthNetworkPrimitives>,
        TelosExecutorBuilder,
        TelosConsensusBuilder,
    >;

    type AddOns = TelosAddOns<N>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
    }

    fn add_ons(&self) -> Self::AddOns {
        RpcAddOns::new(
            TelosEthApiBuilder::new(self.execution_anchor),
            TelosEngineValidatorBuilder,
            TelosEngineApiBuilder::new(TelosEngineValidatorBuilder, self.execution_anchor),
            TelosEngineTreeValidatorBuilder,
            Default::default(),
            Identity::new(),
        )
    }

    fn disabled_stages() -> &'static [StageId] {
        TELOS_DISABLED_STAGES
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for TelosNode {
    type RpcBlock = alloy_rpc_types_eth::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_ethereum_primitives::Block {
        rpc_block.into_consensus().convert_transactions()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes> {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use reth_consensus::HeaderValidator;
    use reth_primitives_traits::constants::MINIMUM_GAS_LIMIT;

    #[test]
    fn telos_consensus_uses_nondecreasing_timestamps() {
        let consensus = telos_consensus(Arc::new(ChainSpec::default()));
        // Native Telos blocks are produced every 500 ms but Engine API headers use whole seconds.
        const PARENT_NUMBER: u64 = 479_294_328;
        const TIMESTAMP: u64 = 1_784_663_988;
        let parent = reth_primitives_traits::SealedHeader::seal_slow(Header {
            number: PARENT_NUMBER,
            timestamp: TIMESTAMP,
            gas_limit: MINIMUM_GAS_LIMIT,
            ..Default::default()
        });
        let child = |timestamp| {
            reth_primitives_traits::SealedHeader::seal_slow(Header {
                parent_hash: parent.hash(),
                number: PARENT_NUMBER + 1,
                timestamp,
                gas_limit: MINIMUM_GAS_LIMIT,
                ..Default::default()
            })
        };

        assert!(consensus.validate_header_against_parent(&child(TIMESTAMP), &parent).is_ok());
        assert!(matches!(
            consensus.validate_header_against_parent(&child(TIMESTAMP - 1), &parent),
            Err(reth_consensus::ConsensusError::TimestampIsInPast {
                timestamp,
                parent_timestamp,
            })
            if timestamp == TIMESTAMP - 1 && parent_timestamp == TIMESTAMP
        ));
    }

    #[test]
    fn staged_sync_cannot_ingest_or_mutate_telos_chain_data() {
        for stage in StageId::ALL {
            assert_eq!(
                TELOS_DISABLED_STAGES.contains(&stage),
                stage != StageId::Finish,
                "unexpected Telos pipeline policy for {stage}"
            );
        }
    }
}

//! Telos node type and component wiring.

use crate::{
    args::TelosArgs, engine::TelosEngineValidatorBuilder, evm::TelosExecutorBuilder,
    rpc::TelosEngineApiBuilder, types::TelosEngineTypes,
};
use reth_chainspec::{ChainSpec, EthereumHardforks, Hardforks};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_ethereum_engine_primitives::{EthBuiltPayload, EthPayloadAttributes};
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::eth::spec::EthExecutorSpec;
use reth_node_api::{FullNodeComponents, PayloadAttributesBuilder};
use reth_node_builder::{
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
    node::{FullNodeTypes, NodeTypes},
    rpc::{BasicEngineValidatorBuilder, Identity, RpcAddOns},
    DebugNode, Node, NodeAdapter,
};
use reth_node_ethereum::node::{
    EthereumConsensusBuilder, EthereumEthApiBuilder, EthereumNetworkBuilder,
    EthereumPayloadBuilder, EthereumPoolBuilder,
};
use reth_payload_primitives::PayloadTypes;
use reth_provider::{providers::ProviderFactoryBuilder, EthStorage};
use std::sync::Arc;

/// Type configuration for a Telos execution node.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct TelosNode {
    /// Additional Telos arguments.
    pub args: TelosArgs,
}

impl TelosNode {
    /// Creates a Telos node configuration.
    pub const fn new(args: TelosArgs) -> Self {
        Self { args }
    }

    /// Returns the production Telos component set.
    pub fn components<N>() -> ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        TelosExecutorBuilder,
        EthereumConsensusBuilder,
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
            .executor(TelosExecutorBuilder)
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    /// Instantiates the provider factory builder for a Telos node.
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl NodeTypes for TelosNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = TelosEngineTypes;
}

type TelosAddOns<N> = RpcAddOns<
    NodeAdapter<N>,
    EthereumEthApiBuilder,
    TelosEngineValidatorBuilder,
    TelosEngineApiBuilder<TelosEngineValidatorBuilder>,
    BasicEngineValidatorBuilder<TelosEngineValidatorBuilder>,
>;

impl<N> Node<N> for TelosNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        TelosExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns = TelosAddOns<N>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components()
    }

    fn add_ons(&self) -> Self::AddOns {
        RpcAddOns::new(
            EthereumEthApiBuilder::default(),
            TelosEngineValidatorBuilder,
            TelosEngineApiBuilder::new(TelosEngineValidatorBuilder),
            BasicEngineValidatorBuilder::new(TelosEngineValidatorBuilder),
            Default::default(),
            Identity::new(),
        )
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

//! Authenticated Engine API wiring for Telos payload extensions.

use crate::types::TelosEngineTypes;
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionData, ExecutionPayloadSidecar, ExecutionPayloadV1,
};
use jsonrpsee::{types::ErrorObjectOwned, RpcModule};
use reth_chainspec::EthereumHardforks;
use reth_engine_primitives::EngineApiValidator;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EngineApiBuilder, PayloadValidatorBuilder};
use reth_node_core::version::{version_metadata, CLIENT_CODE};
use reth_payload_builder::PayloadStore;
use reth_rpc_api::IntoEngineApiRpcModule;
use reth_rpc_engine_api::{EngineApi, EngineCapabilities};
use reth_telos_rpc_engine_api::{
    payload::TelosExecutionData, structs::TelosEngineApiExtraFields, validate_extra_fields,
};

/// Type-erased Telos Engine API module.
#[derive(Debug)]
pub struct TelosEngineRpcModule(RpcModule<()>);

impl IntoEngineApiRpcModule for TelosEngineRpcModule {
    fn into_rpc_module(self) -> RpcModule<()> {
        self.0
    }
}

/// Builds the Engine API and installs the two-parameter Telos `engine_newPayloadV1` handler.
#[derive(Debug, Default)]
pub struct TelosEngineApiBuilder<PVB> {
    payload_validator_builder: PVB,
}

impl<PVB> TelosEngineApiBuilder<PVB> {
    /// Creates a builder with the given payload validator.
    pub const fn new(payload_validator_builder: PVB) -> Self {
        Self { payload_validator_builder }
    }
}

impl<N, PVB> EngineApiBuilder<N> for TelosEngineApiBuilder<PVB>
where
    N: FullNodeComponents<
        Types: NodeTypes<ChainSpec: EthereumHardforks, Payload = TelosEngineTypes>,
    >,
    PVB: PayloadValidatorBuilder<N>,
    PVB::Validator: EngineApiValidator<TelosEngineTypes>,
{
    type EngineApi = TelosEngineRpcModule;

    async fn build_engine_api(
        self,
        ctx: &reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::EngineApi> {
        let validator = self.payload_validator_builder.build(ctx).await?;
        let client = ClientVersionV1 {
            code: CLIENT_CODE,
            name: version_metadata().name_client.to_string(),
            version: version_metadata().cargo_pkg_version.to_string(),
            commit: version_metadata().vergen_git_sha.to_string(),
        };
        let engine = EngineApi::new(
            ctx.node.provider().clone(),
            ctx.config.chain.clone(),
            ctx.beacon_engine_handle.clone(),
            PayloadStore::new(ctx.node.payload_builder_handle().clone()),
            ctx.node.pool().clone(),
            ctx.node.task_executor().clone(),
            client,
            EngineCapabilities::default(),
            validator,
            ctx.config.engine.accept_execution_requests_hash,
            ctx.node.network().clone(),
        );

        let telos_handler = engine.clone();
        let mut module = engine.into_rpc_module();
        if module.remove_method("engine_newPayloadV1").is_none() {
            eyre::bail!("stock engine_newPayloadV1 handler was not registered")
        }
        module.register_async_method("engine_newPayloadV1", move |params, _ctx, _ext| {
            let engine = telos_handler.clone();
            async move {
                let (payload, extra_fields): (ExecutionPayloadV1, TelosEngineApiExtraFields) =
                    params.parse().map_err(invalid_params)?;
                validate_extra_fields(&extra_fields, payload.transactions.len(), payload.gas_used)
                    .map_err(invalid_params)?;

                let payload = TelosExecutionData::new(
                    ExecutionData {
                        payload: payload.into(),
                        sidecar: ExecutionPayloadSidecar::none(),
                    },
                    extra_fields,
                );
                engine.new_payload_v1_metered(payload).await.map_err(ErrorObjectOwned::from)
            }
        })?;

        Ok(TelosEngineRpcModule(module))
    }
}

fn invalid_params(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32602, format!("invalid Telos payload extension: {error}"), None::<()>)
}

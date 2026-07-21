//! Authenticated Engine API wiring for Telos payload extensions.

use crate::{
    sidecar::{
        validate_accepted_sidecar_continuity, ProviderTelosSidecarStore, TelosChainIdentity,
        TelosExecutionAnchor, TelosExecutionSidecar, TelosSidecarError, TelosSidecarStore,
    },
    types::TelosEngineTypes,
};
use alloy_rpc_types_engine::{
    ClientVersionV1, ExecutionData, ExecutionPayloadSidecar, ExecutionPayloadV1, ForkchoiceState,
};
use jsonrpsee::{types::ErrorObjectOwned, RpcModule};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_engine_primitives::EngineApiValidator;
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EngineApiBuilder, PayloadValidatorBuilder};
use reth_node_core::version::{version_metadata, CLIENT_CODE};
use reth_payload_builder::PayloadStore;
use reth_rpc_api::IntoEngineApiRpcModule;
use reth_rpc_engine_api::{EngineApi, EngineCapabilities};
use reth_telos_rpc_engine_api::{
    payload::TelosExecutionData, structs::TelosEngineApiExtraFields,
    validate_extra_fields_for_payload,
};
use std::sync::Arc;

/// Engine methods supported by the Telos follower protocol.
///
/// Telos block production happens in nodeos, so this execution client neither builds payloads nor
/// accepts the Ethereum hardfork-specific payload variants. Keep this list exact: the companion
/// verifies it during its startup handshake.
const TELOS_ENGINE_CAPABILITIES: [&str; 2] = ["engine_forkchoiceUpdatedV1", "engine_newPayloadV1"];

const TELOS_ENGINE_MODULE_METHODS: [&str; 3] =
    ["engine_exchangeCapabilities", "engine_forkchoiceUpdatedV1", "engine_newPayloadV1"];

/// Stock Engine methods that must not remain callable on the authenticated Telos endpoint.
const UNSUPPORTED_TELOS_ENGINE_METHODS: [&str; 23] = [
    "engine_forkchoiceUpdatedV2",
    "engine_forkchoiceUpdatedV3",
    "engine_forkchoiceUpdatedV4",
    "engine_getBlobsV1",
    "engine_getBlobsV2",
    "engine_getBlobsV3",
    "engine_getBlobsV4",
    "engine_getClientVersionV1",
    "engine_getPayloadBodiesByHashV1",
    "engine_getPayloadBodiesByHashV2",
    "engine_getPayloadBodiesByRangeV1",
    "engine_getPayloadBodiesByRangeV2",
    "engine_getPayloadV1",
    "engine_getPayloadV2",
    "engine_getPayloadV3",
    "engine_getPayloadV4",
    "engine_getPayloadV5",
    "engine_getPayloadV6",
    "engine_hasBlobs",
    "engine_newPayloadV2",
    "engine_newPayloadV3",
    "engine_newPayloadV4",
    "engine_newPayloadV5",
];

/// Type-erased Telos Engine API module.
#[derive(Debug)]
pub struct TelosEngineRpcModule(RpcModule<()>);

impl IntoEngineApiRpcModule for TelosEngineRpcModule {
    fn into_rpc_module(self) -> RpcModule<()> {
        self.0
    }
}

/// Builds the exact follower-only Telos Engine API surface and durable lifecycle guards.
#[derive(Debug)]
pub struct TelosEngineApiBuilder<PVB> {
    payload_validator_builder: PVB,
    execution_anchor: TelosExecutionAnchor,
}

impl<PVB> TelosEngineApiBuilder<PVB> {
    /// Creates a builder with the given payload validator.
    pub const fn new(
        payload_validator_builder: PVB,
        execution_anchor: TelosExecutionAnchor,
    ) -> Self {
        Self { payload_validator_builder, execution_anchor }
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
        let execution_anchor = self.execution_anchor;
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
            EngineCapabilities::new(TELOS_ENGINE_CAPABILITIES),
            validator,
            ctx.config.engine.accept_execution_requests_hash,
            ctx.node.network().clone(),
        );
        let chain = TelosChainIdentity {
            chain_id: ctx.config.chain.chain().id(),
            genesis_hash: ctx.config.chain.genesis_hash(),
        };
        let sidecar_store: Arc<dyn TelosSidecarStore> =
            Arc::new(ProviderTelosSidecarStore::new(ctx.node.provider().clone(), chain));
        let new_payload_handler = engine.clone();
        let forkchoice_handler = engine.clone();
        let new_payload_store = sidecar_store.clone();
        let forkchoice_store = sidecar_store;
        let new_payload_executor = ctx.node.task_executor().clone();
        let forkchoice_executor = ctx.node.task_executor().clone();
        let forkchoice_serialization = Arc::new(tokio::sync::Mutex::new(()));
        let mut module = engine.into_rpc_module();
        if module.remove_method("engine_newPayloadV1").is_none() {
            eyre::bail!("stock engine_newPayloadV1 handler was not registered");
        }
        if module.remove_method("engine_forkchoiceUpdatedV1").is_none() {
            eyre::bail!("stock engine_forkchoiceUpdatedV1 handler was not registered");
        }
        for method in UNSUPPORTED_TELOS_ENGINE_METHODS {
            if module.remove_method(method).is_none() {
                eyre::bail!("stock {method} handler was not registered");
            }
        }
        module.register_async_method("engine_newPayloadV1", move |params, _ctx, _ext| {
            let engine = new_payload_handler.clone();
            let sidecar_store = new_payload_store.clone();
            let task_executor = new_payload_executor.clone();
            let execution_anchor = execution_anchor;
            async move {
                let (payload, extra_fields): (ExecutionPayloadV1, TelosEngineApiExtraFields) =
                    params.parse().map_err(invalid_params)?;
                validate_extra_fields_for_payload(
                    &extra_fields,
                    payload.transactions.len(),
                    payload.gas_used,
                    payload.base_fee_per_gas,
                    payload.block_hash,
                    payload.parent_hash,
                )
                .map_err(invalid_params)?;

                let transaction_count = u64::try_from(payload.transactions.len())
                    .map_err(|_| invalid_params("payload transaction count exceeds u64"))?;
                let sidecar = TelosExecutionSidecar::new(
                    sidecar_store.chain_identity(),
                    payload.block_number,
                    payload.block_hash,
                    payload.parent_hash,
                    transaction_count,
                    payload.gas_used,
                    extra_fields.clone(),
                )
                .map_err(invalid_params)?;
                let block_hash = sidecar.envelope().block_hash;
                let sidecar_digest = sidecar.digest();
                let transition_store = sidecar_store.clone();
                let transition_sidecar = sidecar.clone();
                task_executor
                    .spawn_blocking(move || {
                        sidecar_store.validate_and_mark_dispatched(&execution_anchor, &sidecar)
                    })
                    .await
                    .map_err(internal_error)?
                    .map_err(internal_error)?;

                let payload = TelosExecutionData::new(
                    ExecutionData {
                        payload: payload.into(),
                        sidecar: ExecutionPayloadSidecar::none(),
                    },
                    extra_fields,
                );
                let status =
                    engine.new_payload_v1_metered(payload).await.map_err(ErrorObjectOwned::from)?;

                if status.is_valid() {
                    let sidecar_store = transition_store.clone();
                    task_executor
                        .spawn_blocking(move || {
                            validate_accepted_sidecar_continuity(
                                sidecar_store.as_ref(),
                                &execution_anchor,
                                &transition_sidecar,
                            )?;
                            sidecar_store.mark_accepted(block_hash, sidecar_digest)
                        })
                        .await
                        .map_err(internal_error)?
                        .map_err(internal_error)?;
                } else if status.is_invalid() {
                    let sidecar_store = transition_store;
                    task_executor
                        .spawn_blocking(move || {
                            sidecar_store.remove_pending(block_hash, sidecar_digest)
                        })
                        .await
                        .map_err(internal_error)?
                        .map_err(internal_error)?;
                }

                Ok::<_, ErrorObjectOwned>(status)
            }
        })?;

        module.register_async_method("engine_forkchoiceUpdatedV1", move |params, _ctx, _ext| {
            let engine = forkchoice_handler.clone();
            let sidecar_store = forkchoice_store.clone();
            let task_executor = forkchoice_executor.clone();
            let forkchoice_serialization = forkchoice_serialization.clone();
            async move {
                // Keep preflight, the Engine mutation, and durable finality in one ordered critical
                // section. Otherwise two concurrent FCUs can both validate an old marker before one
                // prunes the other's selected fork.
                let _forkchoice_guard = forkchoice_serialization.lock_owned().await;
                let (state, payload_attributes): (ForkchoiceState, Option<EthPayloadAttributes>) =
                    params.parse().map_err(invalid_params)?;
                validate_forkchoice_payload_attributes(&payload_attributes)
                    .map_err(invalid_params)?;
                let preflight_store = sidecar_store.clone();
                let preflight_state = state;
                let preflight = task_executor
                    .spawn_blocking(move || {
                        preflight_store.validate_forkchoice_state(
                            &execution_anchor,
                            preflight_state.head_block_hash,
                            preflight_state.safe_block_hash,
                            preflight_state.finalized_block_hash,
                        )
                    })
                    .await
                    .map_err(internal_error)?;

                let finalized_hash = state.finalized_block_hash;
                let dispatch = dispatch_after_forkchoice_preflight(preflight, || {
                    engine.fork_choice_updated_v1_metered(state, payload_attributes)
                })?;
                let updated = dispatch.await.map_err(ErrorObjectOwned::from)?;
                if updated.payload_status.is_valid() &&
                    finalized_hash != alloy_primitives::B256::ZERO
                {
                    task_executor
                        .spawn_blocking(move || {
                            sidecar_store.finalize_and_prune(&execution_anchor, finalized_hash)
                        })
                        .await
                        .map_err(internal_error)?
                        .map_err(internal_error)?;
                }
                Ok::<_, ErrorObjectOwned>(updated)
            }
        })?;

        let mut registered_methods = module.method_names().collect::<Vec<_>>();
        registered_methods.sort_unstable();
        if registered_methods.as_slice() != TELOS_ENGINE_MODULE_METHODS {
            eyre::bail!(
                "unexpected Telos Engine RPC surface after filtering: {registered_methods:?}"
            );
        }
        drop(registered_methods);

        Ok(TelosEngineRpcModule(module))
    }
}

const fn validate_forkchoice_payload_attributes<T>(
    attributes: &Option<T>,
) -> Result<(), &'static str> {
    if attributes.is_some() {
        return Err("Telos forkchoiceUpdatedV1 requires null payloadAttributes")
    }
    Ok(())
}

fn dispatch_after_forkchoice_preflight<T>(
    preflight: Result<(), TelosSidecarError>,
    dispatch: impl FnOnce() -> T,
) -> Result<T, ErrorObjectOwned> {
    preflight.map_err(internal_error)?;
    Ok(dispatch())
}

fn invalid_params(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        -32602,
        format!("invalid Telos Engine API parameters: {error}"),
        None::<()>,
    )
}

fn internal_error(error: impl std::fmt::Display) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        -32603,
        format!("failed to transition durable Telos payload metadata: {error}"),
        None::<()>,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use std::cell::Cell;

    #[test]
    fn rejected_forkchoice_preflights_never_dispatch_the_inner_engine() {
        let hash = B256::repeat_byte(0x11);
        let errors = [
            TelosSidecarError::ZeroForkchoiceHead,
            TelosSidecarError::ForkchoiceSidecarMissing { role: "safe", block_hash: hash },
            TelosSidecarError::ForkchoiceSidecarNotAccepted {
                role: "finalized",
                block_hash: hash,
                state: crate::sidecar::TelosSidecarState::Dispatched,
            },
            TelosSidecarError::FinalizedConflict {
                block_number: 7,
                current_hash: hash,
                new_hash: B256::repeat_byte(0x22),
            },
            TelosSidecarError::ForkchoiceAncestryMismatch {
                descendant_hash: hash,
                expected_ancestor: B256::repeat_byte(0x33),
                actual_ancestor: B256::repeat_byte(0x44),
            },
        ];

        for error in errors {
            let dispatches = Cell::new(0);
            let result = dispatch_after_forkchoice_preflight(Err(error), || {
                dispatches.set(dispatches.get() + 1);
            });
            assert!(result.is_err());
            assert_eq!(dispatches.get(), 0);
        }
    }

    #[test]
    fn successful_forkchoice_preflight_dispatches_the_inner_engine_once() {
        let dispatches = Cell::new(0);
        dispatch_after_forkchoice_preflight(Ok(()), || {
            dispatches.set(dispatches.get() + 1);
        })
        .unwrap();
        assert_eq!(dispatches.get(), 1);
    }

    #[test]
    fn forkchoice_rejects_payload_building_attributes() {
        assert!(validate_forkchoice_payload_attributes(&None::<()>).is_ok());
        assert_eq!(
            validate_forkchoice_payload_attributes(&Some(())).unwrap_err(),
            "Telos forkchoiceUpdatedV1 requires null payloadAttributes"
        );
    }

    #[test]
    fn custom_engine_module_surface_is_exact() {
        assert_eq!(
            TELOS_ENGINE_MODULE_METHODS,
            ["engine_exchangeCapabilities", "engine_forkchoiceUpdatedV1", "engine_newPayloadV1",]
        );
    }
}

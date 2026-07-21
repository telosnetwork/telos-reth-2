//! Engine types that carry Telos execution extensions through the engine pipeline.

use alloy_primitives::Bytes;
use alloy_rpc_types_engine::{ExecutionData, ExecutionPayload};
use reth_engine_primitives::EngineTypes;
use reth_ethereum_engine_primitives::{
    EthBuiltPayload, EthPayloadAttributes, ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3,
    ExecutionPayloadEnvelopeV4, ExecutionPayloadEnvelopeV5, ExecutionPayloadEnvelopeV6,
    ExecutionPayloadV1,
};
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{NodePrimitives, SealedBlock};
use reth_telos_rpc_engine_api::{payload::TelosExecutionData, structs::TelosEngineApiExtraFields};
use serde::{Deserialize, Serialize};

/// Telos engine API type configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[non_exhaustive]
pub struct TelosEngineTypes;

impl PayloadTypes for TelosEngineTypes {
    type ExecutionData = TelosExecutionData;
    type BuiltPayload = EthBuiltPayload;
    type PayloadAttributes = EthPayloadAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
        bal: Option<Bytes>,
    ) -> Self::ExecutionData {
        let (payload, sidecar) = ExecutionPayload::from_block_unchecked_with_extras(
            block.hash(),
            &block.into_block(),
            bal,
        );
        TelosExecutionData::new(
            ExecutionData { payload, sidecar },
            TelosEngineApiExtraFields::default(),
        )
    }
}

impl EngineTypes for TelosEngineTypes {
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
    type ExecutionPayloadEnvelopeV6 = ExecutionPayloadEnvelopeV6;
}

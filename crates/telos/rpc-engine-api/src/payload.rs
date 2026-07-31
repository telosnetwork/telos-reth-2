use crate::structs::TelosEngineApiExtraFields;
use alloy_eips::{eip4895::Withdrawal, BlockNumHash};
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_engine::ExecutionData;
use reth_ethereum_engine_primitives::EthBuiltPayload;
use reth_payload_primitives::ExecutionPayload;
use serde::{Deserialize, Serialize};

/// Execution payload and its authenticated Telos state-reconciliation extension.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelosExecutionData {
    /// Standard Ethereum execution payload data.
    pub inner: ExecutionData,
    /// Telos state diffs, receipts, and per-block metadata.
    pub extra_fields: TelosEngineApiExtraFields,
}

impl TelosExecutionData {
    /// Creates a payload with its Telos extension.
    pub const fn new(inner: ExecutionData, extra_fields: TelosEngineApiExtraFields) -> Self {
        Self { inner, extra_fields }
    }
}

impl From<ExecutionData> for TelosExecutionData {
    fn from(inner: ExecutionData) -> Self {
        Self { inner, extra_fields: TelosEngineApiExtraFields::default() }
    }
}

impl From<EthBuiltPayload> for TelosExecutionData {
    fn from(payload: EthBuiltPayload) -> Self {
        Self { inner: payload.into(), extra_fields: TelosEngineApiExtraFields::default() }
    }
}

impl ExecutionPayload for TelosExecutionData {
    fn parent_hash(&self) -> B256 {
        self.inner.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.inner.block_hash()
    }

    fn block_number(&self) -> u64 {
        self.inner.block_number()
    }

    fn num_hash(&self) -> BlockNumHash {
        self.inner.num_hash()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals()
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        self.inner.block_access_list()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn transaction_count(&self) -> usize {
        self.inner.transaction_count()
    }

    fn slot_number(&self) -> Option<u64> {
        self.inner.slot_number()
    }
}

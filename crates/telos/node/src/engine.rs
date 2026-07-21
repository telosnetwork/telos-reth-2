//! Telos execution-payload validation.

use alloy_primitives::Address;
use alloy_rpc_types_engine::{ExecutionData, PayloadError};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_engine_primitives::{EngineApiValidator, PayloadValidator};
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_ethereum_primitives::{Block, TransactionSigned};
use reth_node_api::{NodeTypes, PayloadTypes};
use reth_payload_primitives::{
    validate_execution_requests, validate_version_specific_fields, EngineApiMessageVersion,
    EngineObjectValidationError, NewPayloadError, PayloadOrAttributes,
};
use reth_payload_validator::{cancun, prague, shanghai};
use reth_primitives_traits::{
    transaction::signed::RecoveryError, Block as _, RecoveredBlock, SealedBlock, SignerRecoverable,
};
use reth_telos_rpc_engine_api::{payload::TelosExecutionData, validate_extra_fields};
use std::sync::Arc;

/// Payload validator for Telos's legacy header representation.
#[derive(Debug, Clone)]
pub struct TelosEngineValidator<ChainSpec = reth_chainspec::ChainSpec> {
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> TelosEngineValidator<ChainSpec> {
    /// Creates a validator.
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec }
    }
}

/// Sender recovery failed for a transaction outside Telos's chain-ID-3 compatibility path.
#[derive(Debug, thiserror::Error)]
#[error("invalid transaction signature outside the Telos chain-ID-3 compatibility path: {0}")]
pub(crate) struct TelosSenderError(RecoveryError);

/// Recovers a sender using Telos's historical chain-ID-3 compatibility rule.
///
/// Every legacy transaction with chain ID 3 stores its sender in the high 160 bits of `s`. This
/// applies even when those bytes are zero. All other transactions use standard Ethereum recovery
/// and malformed signatures fail closed.
pub(crate) fn recover_telos_sender(
    transaction: &TransactionSigned,
) -> Result<Address, TelosSenderError> {
    if let TransactionSigned::Legacy(transaction) = transaction &&
        transaction.tx().chain_id == Some(3)
    {
        let encoded_sender = transaction.signature().s().to_be_bytes::<32>();
        return Ok(Address::from_slice(&encoded_sender[..20]))
    }

    transaction.recover_signer().map_err(TelosSenderError)
}

fn telos_ensure_well_formed_payload(
    chain_spec: &impl EthereumHardforks,
    data: TelosExecutionData,
) -> Result<SealedBlock<Block>, NewPayloadError> {
    validate_extra_fields(
        &data.extra_fields,
        data.inner.payload.transactions().len(),
        data.inner.payload.as_v1().gas_used,
    )
    .map_err(NewPayloadError::other)?;

    let ExecutionData { payload, sidecar } = data.inner;
    let expected_hash = payload.block_hash();
    let block: Block = payload.try_into_block_with_sidecar(&sidecar)?;
    let alloy_consensus::Block { mut header, body } = block;

    // Telos canonical block hashes omit baseFeePerGas even though the companion payload carries
    // it for Engine API compatibility. Recompute and verify that canonical representation rather
    // than accepting an unchecked hash from the companion client.
    header.base_fee_per_gas = None;
    let sealed_block = alloy_consensus::Block { header, body }.seal_slow();
    if sealed_block.hash() != expected_hash {
        return Err(PayloadError::BlockHash {
            execution: sealed_block.hash(),
            consensus: expected_hash,
        }
        .into())
    }

    shanghai::ensure_well_formed_fields(
        sealed_block.body(),
        chain_spec.is_shanghai_active_at_timestamp(sealed_block.timestamp),
    )?;
    cancun::ensure_well_formed_fields(
        &sealed_block,
        sidecar.cancun(),
        chain_spec.is_cancun_active_at_timestamp(sealed_block.timestamp),
    )?;
    prague::ensure_well_formed_fields(
        sealed_block.body(),
        sidecar.prague(),
        chain_spec.is_prague_active_at_timestamp(sealed_block.timestamp),
    )?;
    Ok(sealed_block)
}

impl<ChainSpec, Types> PayloadValidator<Types> for TelosEngineValidator<ChainSpec>
where
    ChainSpec: EthChainSpec + EthereumHardforks + 'static,
    Types: PayloadTypes<ExecutionData = TelosExecutionData>,
{
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        payload: TelosExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        telos_ensure_well_formed_payload(self.chain_spec.as_ref(), payload)
    }

    fn ensure_well_formed_payload(
        &self,
        payload: TelosExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let sealed = <Self as PayloadValidator<Types>>::convert_payload_to_block(self, payload)?;
        let hash = sealed.hash();
        let (sealed_header, body) = sealed.split_sealed_header_body();
        let senders = body
            .transactions
            .iter()
            .map(recover_telos_sender)
            .collect::<Result<Vec<_>, _>>()
            .map_err(NewPayloadError::other)?;
        Ok(RecoveredBlock::new(Block { header: sealed_header.unseal(), body }, senders, hash))
    }
}

impl<ChainSpec, Types> EngineApiValidator<Types> for TelosEngineValidator<ChainSpec>
where
    ChainSpec: EthChainSpec + EthereumHardforks + 'static,
    Types:
        PayloadTypes<PayloadAttributes = EthPayloadAttributes, ExecutionData = TelosExecutionData>,
{
    fn validate_version_specific_fields(
        &self,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, TelosExecutionData, EthPayloadAttributes>,
    ) -> Result<(), EngineObjectValidationError> {
        let mapped = match payload_or_attrs {
            PayloadOrAttributes::ExecutionPayload(payload) => {
                if let Some(requests) = payload.inner.sidecar.requests() {
                    validate_execution_requests(requests)?;
                }
                PayloadOrAttributes::ExecutionPayload(&payload.inner)
            }
            PayloadOrAttributes::PayloadAttributes(attributes) => {
                PayloadOrAttributes::PayloadAttributes(attributes)
            }
        };
        validate_version_specific_fields(&self.chain_spec, version, mapped)
    }

    fn ensure_well_formed_attributes(
        &self,
        version: EngineApiMessageVersion,
        attributes: &EthPayloadAttributes,
    ) -> Result<(), EngineObjectValidationError> {
        validate_version_specific_fields::<ExecutionData, EthPayloadAttributes, _>(
            &self.chain_spec,
            version,
            PayloadOrAttributes::from(attributes),
        )
    }
}

use reth_node_builder::{rpc::PayloadValidatorBuilder, FullNodeComponents};

/// Builds the Telos payload validator for RPC and tree validation.
#[derive(Debug, Clone, Default)]
pub struct TelosEngineValidatorBuilder;

impl<Node, Types> PayloadValidatorBuilder<Node> for TelosEngineValidatorBuilder
where
    Types: NodeTypes<
        ChainSpec: reth_chainspec::Hardforks + EthereumHardforks + Clone + 'static,
        Payload: PayloadTypes<
            ExecutionData = TelosExecutionData,
            PayloadAttributes = EthPayloadAttributes,
        >,
        Primitives = reth_ethereum_primitives::EthPrimitives,
    >,
    Node: FullNodeComponents<Types = Types>,
{
    type Validator = TelosEngineValidator<Types::ChainSpec>;

    async fn build(
        self,
        ctx: &reth_node_api::AddOnsContext<'_, Node>,
    ) -> eyre::Result<Self::Validator> {
        Ok(TelosEngineValidator::new(ctx.config.chain.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxLegacy};
    use alloy_primitives::{Signature, U256};
    use alloy_rpc_types_engine::ExecutionPayloadV1;
    use reth_telos_rpc_engine_api::structs::TelosEngineApiExtraFields;

    fn empty_payload() -> ExecutionPayloadV1 {
        ExecutionPayloadV1 {
            parent_hash: Default::default(),
            fee_recipient: Default::default(),
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom: Default::default(),
            prev_randao: Default::default(),
            block_number: 0,
            gas_limit: 0,
            gas_used: 0,
            timestamp: 0,
            extra_data: Default::default(),
            base_fee_per_gas: Default::default(),
            block_hash: Default::default(),
            transactions: Vec::new(),
        }
    }

    fn invalid_legacy(
        chain_id: u64,
        embedded_sender: Address,
        low_s_byte: u8,
    ) -> TransactionSigned {
        let mut s = [0u8; 32];
        s[..20].copy_from_slice(embedded_sender.as_slice());
        s[31] = low_s_byte;
        TxLegacy { chain_id: Some(chain_id), ..Default::default() }
            .into_signed(Signature::new(U256::MAX, U256::from_be_bytes(s), false))
            .into()
    }

    #[test]
    fn incomplete_extension_fails_before_block_conversion() {
        let data = TelosExecutionData::new(
            ExecutionData {
                payload: empty_payload().into(),
                sidecar: alloy_rpc_types_engine::ExecutionPayloadSidecar::none(),
            },
            TelosEngineApiExtraFields::default(),
        );
        let error =
            telos_ensure_well_formed_payload(&*crate::chainspec::TELOS_MAINNET, data).unwrap_err();
        assert!(error.to_string().contains("statediffs_account"));
    }

    #[test]
    fn arbitrary_invalid_signature_fails_closed() {
        assert!(recover_telos_sender(&invalid_legacy(1, Address::repeat_byte(0x11), 0)).is_err());
    }

    #[test]
    fn chain_id_three_uses_embedded_sender_for_every_legacy_signature() {
        let sender = Address::repeat_byte(0x11);
        assert_eq!(recover_telos_sender(&invalid_legacy(3, sender, 0)).unwrap(), sender);
        assert_eq!(recover_telos_sender(&invalid_legacy(3, sender, 1)).unwrap(), sender);
    }

    #[test]
    fn chain_id_three_accepts_zero_embedded_sender() {
        assert_eq!(
            recover_telos_sender(&invalid_legacy(3, Address::ZERO, 1)).unwrap(),
            Address::ZERO
        );
    }
}

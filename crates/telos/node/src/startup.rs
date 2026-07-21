//! Fail-closed startup validation for Telos execution nodes.

use crate::sidecar::{TelosExecutionAnchor, TelosExecutionSidecarEnvelope, TelosSidecarStore};
use alloy_primitives::{Address, B256};
use reth_ethereum_primitives::Block;
use reth_provider::{
    AccountReader, BlockHashReader, BlockNumReader, BlockReader, DBProvider,
    DatabaseProviderFactory, StateProviderFactory,
};

/// Canonical block fields that must remain bound to an accepted execution sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelosStartupBlockBinding {
    /// Canonical block hash.
    pub hash: B256,
    /// Canonical parent hash.
    pub parent_hash: B256,
    /// Number of transactions in the stored body.
    pub transaction_count: usize,
    /// Number of durable sender rows over the block's exact transaction-number range.
    pub sender_count: usize,
    /// Gas used committed by the header.
    pub gas_used: u64,
}

/// Read-only observations required before a Telos node can start ingesting Engine API payloads.
///
/// This small interface keeps the policy independently testable while the blanket implementation
/// below binds production startup to Reth's effective provider and pruning configuration.
pub trait TelosStartupProvider {
    /// Returns the canonical hash stored for a block number.
    fn canonical_block_hash(&self, block_number: u64) -> eyre::Result<Option<B256>>;

    /// Proves that historical state at the exact block hash can serve a real account read.
    fn probe_historical_state(&self, block_hash: B256) -> eyre::Result<()>;

    /// Returns whether sender-recovery data is configured to be pruned.
    fn sender_recovery_pruning_enabled(&self) -> eyre::Result<bool>;

    /// Returns the current canonical tip.
    fn best_block_number(&self) -> eyre::Result<u64>;

    /// Returns the exact canonical block fields required for sidecar coverage validation.
    fn canonical_block_binding(
        &self,
        block_number: u64,
    ) -> eyre::Result<Option<TelosStartupBlockBinding>>;
}

impl<P> TelosStartupProvider for P
where
    P: BlockHashReader
        + BlockNumReader
        + BlockReader<Block = Block>
        + StateProviderFactory
        + DatabaseProviderFactory,
    P::Provider: DBProvider,
{
    fn canonical_block_hash(&self, block_number: u64) -> eyre::Result<Option<B256>> {
        self.block_hash(block_number)
            .map_err(|error| eyre::eyre!("failed to read canonical block {block_number}: {error}"))
    }

    fn probe_historical_state(&self, block_hash: B256) -> eyre::Result<()> {
        let state = self.history_by_block_hash(block_hash).map_err(|error| {
            eyre::eyre!(
                "failed to open historical state at Telos execution anchor {block_hash}: {error}"
            )
        })?;
        state.basic_account(&Address::ZERO).map_err(|error| {
            eyre::eyre!(
                "failed to read historical state at Telos execution anchor {block_hash}: {error}"
            )
        })?;
        Ok(())
    }

    fn sender_recovery_pruning_enabled(&self) -> eyre::Result<bool> {
        let provider = self.database_provider_ro().map_err(|error| {
            eyre::eyre!("failed to inspect effective Telos pruning configuration: {error}")
        })?;
        Ok(provider.prune_modes_ref().sender_recovery.is_some())
    }

    fn best_block_number(&self) -> eyre::Result<u64> {
        BlockNumReader::best_block_number(self)
            .map_err(|error| eyre::eyre!("failed to read canonical Telos tip: {error}"))
    }

    fn canonical_block_binding(
        &self,
        block_number: u64,
    ) -> eyre::Result<Option<TelosStartupBlockBinding>> {
        let Some(block) = self.block_by_number(block_number).map_err(|error| {
            eyre::eyre!("failed to read canonical Telos block {block_number}: {error}")
        })?
        else {
            return Ok(None)
        };
        let hash = block.header.hash_slow();
        let indexed_hash = self.block_hash(block_number).map_err(|error| {
            eyre::eyre!("failed to read canonical Telos hash {block_number}: {error}")
        })?;
        if indexed_hash != Some(hash) {
            eyre::bail!(
                "canonical Telos block {block_number} hash/index mismatch: body {hash}, index {indexed_hash:?}"
            )
        }
        let body_indices = self
            .block_body_indices(block_number)
            .map_err(|error| {
                eyre::eyre!("failed to read Telos block body indices {block_number}: {error}")
            })?
            .ok_or_else(|| eyre::eyre!("Telos block {block_number} has no block body indices"))?;
        let indexed_transaction_count = usize::try_from(body_indices.tx_count).map_err(|_| {
            eyre::eyre!("Telos block {block_number} indexed transaction count exceeds usize")
        })?;
        if indexed_transaction_count != block.body.transactions.len() {
            eyre::bail!(
                "Telos block {block_number} body/index transaction-count mismatch: body {}, index {indexed_transaction_count}",
                block.body.transactions.len()
            )
        }
        let sender_count = if body_indices.tx_count == 0 {
            0
        } else {
            self.senders_by_tx_range(body_indices.tx_num_range())
                .map_err(|error| {
                    eyre::eyre!(
                        "failed to read durable Telos sender rows for block {block_number}: {error}"
                    )
                })?
                .len()
        };
        Ok(Some(TelosStartupBlockBinding {
            hash,
            parent_hash: block.header.parent_hash,
            transaction_count: block.body.transactions.len(),
            sender_count,
            gas_used: block.header.gas_used,
        }))
    }
}

/// Validates the opened database against the trusted execution anchor and required retention.
///
/// Telos senders are not recoverable through Ethereum's standard sender-recovery stage. Sender
/// rows must therefore remain durable until every provider path is chain-aware.
pub fn validate_telos_startup(
    provider: &impl TelosStartupProvider,
    anchor: &TelosExecutionAnchor,
    sidecar_store: &dyn TelosSidecarStore,
) -> eyre::Result<()> {
    anchor.validate_for_chain(sidecar_store.chain_identity())?;
    let actual_hash =
        provider.canonical_block_hash(anchor.parent_block_number)?.ok_or_else(|| {
            eyre::eyre!(
                "Telos execution anchor block {} is missing from the opened database",
                anchor.parent_block_number
            )
        })?;
    if actual_hash != anchor.parent_block_hash {
        eyre::bail!(
            "Telos execution anchor mismatch at block {}: configured {}, database {}",
            anchor.parent_block_number,
            anchor.parent_block_hash,
            actual_hash
        )
    }

    provider.probe_historical_state(anchor.parent_block_hash)?;

    if provider.sender_recovery_pruning_enabled()? {
        eyre::bail!(
            "Telos sender-recovery pruning is unsafe and must be disabled; embedded Telos senders cannot be reconstructed by Ethereum sender recovery"
        )
    }

    validate_canonical_sidecar_coverage(provider, anchor, sidecar_store)?;

    Ok(())
}

fn validate_canonical_sidecar_coverage(
    provider: &impl TelosStartupProvider,
    anchor: &TelosExecutionAnchor,
    sidecar_store: &dyn TelosSidecarStore,
) -> eyre::Result<()> {
    let best = provider.best_block_number()?;
    if best < anchor.parent_block_number {
        eyre::bail!(
            "canonical Telos tip {best} is below execution anchor {}",
            anchor.parent_block_number
        )
    }

    let mut covered_number = anchor.parent_block_number;
    let mut parent_hash = anchor.parent_block_hash;
    let mut gas_price = anchor.starting_gas_price;
    let mut revision = anchor.starting_revision;
    if let Some(marker) = sidecar_store.finalized_coverage()? {
        if marker.block_number < anchor.parent_block_number {
            eyre::bail!(
                "Telos finalized sidecar coverage block {} is below execution anchor {}",
                marker.block_number,
                anchor.parent_block_number
            )
        }
        if marker.block_number > best {
            eyre::bail!(
                "Telos finalized sidecar coverage block {} is above canonical tip {best}",
                marker.block_number
            )
        }
        if marker.block_number == anchor.parent_block_number {
            if marker.block_hash != anchor.parent_block_hash {
                eyre::bail!(
                    "Telos finalized sidecar coverage at the anchor height binds {}, expected {}",
                    marker.block_hash,
                    anchor.parent_block_hash
                )
            }
        } else {
            let block =
                provider.canonical_block_binding(marker.block_number)?.ok_or_else(|| {
                    eyre::eyre!(
                        "finalized Telos coverage block {} is missing below tip {best}",
                        marker.block_number
                    )
                })?;
            if block.hash != marker.block_hash {
                eyre::bail!(
                    "Telos finalized sidecar coverage mismatch at block {}: marker {}, canonical {}",
                    marker.block_number,
                    marker.block_hash,
                    block.hash
                )
            }
            let sidecar =
                sidecar_store.get_accepted_by_hash(marker.block_hash)?.ok_or_else(|| {
                    eyre::eyre!(
                        "finalized canonical Telos block {} ({}) has no accepted execution sidecar",
                        marker.block_number,
                        marker.block_hash
                    )
                })?;
            validate_finalized_marker_binding(
                marker.block_number,
                block,
                sidecar.envelope(),
                anchor,
            )?;
            let execution =
                sidecar.envelope().extra_fields.execution.as_ref().ok_or_else(|| {
                    eyre::eyre!(
                        "accepted Telos sidecar for finalized block {} has no execution metadata",
                        marker.block_number
                    )
                })?;
            gas_price = execution
                .gas_price_changes
                .last()
                .map_or(execution.starting_gas_price, |change| change.value);
            revision = execution
                .revision_changes
                .last()
                .map_or(execution.starting_revision, |change| change.value);
            covered_number = marker.block_number;
            parent_hash = marker.block_hash;
        }
    }

    if covered_number == best {
        return Ok(())
    }
    let first_uncovered = covered_number.checked_add(1).ok_or_else(|| {
        eyre::eyre!("Telos finalized sidecar coverage block number cannot be incremented")
    })?;
    for number in first_uncovered..=best {
        let block = provider.canonical_block_binding(number)?.ok_or_else(|| {
            eyre::eyre!("canonical Telos block {number} is missing below tip {best}")
        })?;
        if block.parent_hash != parent_hash {
            eyre::bail!(
                "canonical Telos block {number} parent mismatch: expected {parent_hash}, got {}",
                block.parent_hash
            )
        }

        let sidecar = sidecar_store.get_accepted_by_hash(block.hash)?.ok_or_else(|| {
            eyre::eyre!(
                "canonical Telos block {number} ({}) has no accepted execution sidecar",
                block.hash
            )
        })?;
        validate_coverage_binding(
            number,
            block,
            sidecar.envelope(),
            parent_hash,
            gas_price,
            revision,
            anchor,
        )?;
        let execution = sidecar.envelope().extra_fields.execution.as_ref().ok_or_else(|| {
            eyre::eyre!("accepted Telos sidecar for block {number} has no execution metadata")
        })?;
        gas_price = execution
            .gas_price_changes
            .last()
            .map_or(execution.starting_gas_price, |change| change.value);
        revision = execution
            .revision_changes
            .last()
            .map_or(execution.starting_revision, |change| change.value);
        parent_hash = block.hash;
    }
    Ok(())
}

fn validate_finalized_marker_binding(
    number: u64,
    block: TelosStartupBlockBinding,
    sidecar: &TelosExecutionSidecarEnvelope,
    anchor: &TelosExecutionAnchor,
) -> eyre::Result<()> {
    if block.sender_count != block.transaction_count {
        eyre::bail!(
            "canonical Telos block {number} ({}) has {} durable sender rows for {} transactions",
            block.hash,
            block.sender_count,
            block.transaction_count
        )
    }
    let expected_transaction_count = u64::try_from(block.transaction_count)
        .map_err(|_| eyre::eyre!("Telos block {number} transaction count exceeds u64"))?;
    if sidecar.chain != anchor.chain ||
        sidecar.block_number != number ||
        sidecar.block_hash != block.hash ||
        sidecar.parent_hash != block.parent_hash ||
        sidecar.transaction_count != expected_transaction_count ||
        sidecar.gas_used != block.gas_used
    {
        eyre::bail!(
            "accepted Telos sidecar binding mismatch at finalized canonical block {number} ({})",
            block.hash
        )
    }
    Ok(())
}

fn validate_coverage_binding(
    number: u64,
    block: TelosStartupBlockBinding,
    sidecar: &TelosExecutionSidecarEnvelope,
    parent_hash: B256,
    gas_price: alloy_primitives::U256,
    revision: u64,
    anchor: &TelosExecutionAnchor,
) -> eyre::Result<()> {
    if block.sender_count != block.transaction_count {
        eyre::bail!(
            "canonical Telos block {number} ({}) has {} durable sender rows for {} transactions",
            block.hash,
            block.sender_count,
            block.transaction_count
        )
    }
    let expected_transaction_count = u64::try_from(block.transaction_count)
        .map_err(|_| eyre::eyre!("Telos block {number} transaction count exceeds u64"))?;
    if sidecar.chain != anchor.chain ||
        sidecar.block_number != number ||
        sidecar.block_hash != block.hash ||
        sidecar.parent_hash != parent_hash ||
        sidecar.transaction_count != expected_transaction_count ||
        sidecar.gas_used != block.gas_used
    {
        eyre::bail!(
            "accepted Telos sidecar binding mismatch at canonical block {number} ({})",
            block.hash
        )
    }
    let execution = sidecar.extra_fields.execution.as_ref().ok_or_else(|| {
        eyre::eyre!("accepted Telos sidecar for block {number} has no execution metadata")
    })?;
    if execution.starting_gas_price != gas_price || execution.starting_revision != revision {
        eyre::bail!(
            "accepted Telos execution-context discontinuity at canonical block {number} ({})",
            block.hash
        )
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::{
        InMemoryTelosSidecarStore, TelosChainIdentity, TelosExecutionSidecar,
        TELOS_EXECUTION_ANCHOR_VERSION,
    };
    use alloy_primitives::U256;
    use reth_telos_rpc_engine_api::structs::{
        TelosEngineApiExtraFields, TelosExecutionMetadataV3, TELOS_EXECUTION_METADATA_VERSION,
    };
    use std::{collections::BTreeMap, sync::Mutex};

    #[derive(Debug)]
    struct TestProvider {
        canonical_hashes: BTreeMap<u64, B256>,
        state_error: Option<&'static str>,
        sender_pruning: bool,
        best_block_number: u64,
        block_bindings: BTreeMap<u64, TelosStartupBlockBinding>,
        probes: Mutex<Vec<B256>>,
        binding_reads: Mutex<Vec<u64>>,
    }

    impl TelosStartupProvider for TestProvider {
        fn canonical_block_hash(&self, block_number: u64) -> eyre::Result<Option<B256>> {
            Ok(self.canonical_hashes.get(&block_number).copied())
        }

        fn probe_historical_state(&self, block_hash: B256) -> eyre::Result<()> {
            self.probes.lock().unwrap().push(block_hash);
            if let Some(error) = self.state_error {
                eyre::bail!(error)
            }
            Ok(())
        }

        fn sender_recovery_pruning_enabled(&self) -> eyre::Result<bool> {
            Ok(self.sender_pruning)
        }

        fn best_block_number(&self) -> eyre::Result<u64> {
            Ok(self.best_block_number)
        }

        fn canonical_block_binding(
            &self,
            block_number: u64,
        ) -> eyre::Result<Option<TelosStartupBlockBinding>> {
            self.binding_reads.lock().unwrap().push(block_number);
            Ok(self.block_bindings.get(&block_number).copied())
        }
    }

    fn anchor() -> TelosExecutionAnchor {
        TelosExecutionAnchor {
            version: TELOS_EXECUTION_ANCHOR_VERSION,
            chain: TelosChainIdentity { chain_id: 40, genesis_hash: B256::repeat_byte(0x40) },
            parent_block_number: 7,
            parent_block_hash: B256::repeat_byte(0x77),
            starting_gas_price: U256::from(7),
            starting_revision: 1,
        }
    }

    fn provider_for_anchor(
        anchor: TelosExecutionAnchor,
        canonical_hash: Option<B256>,
    ) -> TestProvider {
        let mut canonical_hashes = BTreeMap::new();
        if let Some(canonical_hash) = canonical_hash {
            canonical_hashes.insert(anchor.parent_block_number, canonical_hash);
        }
        TestProvider {
            canonical_hashes,
            state_error: None,
            sender_pruning: false,
            best_block_number: anchor.parent_block_number,
            block_bindings: BTreeMap::new(),
            probes: Mutex::default(),
            binding_reads: Mutex::default(),
        }
    }

    fn sidecar_store(anchor: TelosExecutionAnchor) -> InMemoryTelosSidecarStore {
        InMemoryTelosSidecarStore::new(anchor.chain)
    }

    fn accepted_sidecar(
        store: &InMemoryTelosSidecarStore,
        anchor: TelosExecutionAnchor,
        block_number: u64,
        block_hash: B256,
        parent_hash: B256,
    ) -> TelosExecutionSidecar {
        let sidecar = TelosExecutionSidecar::new(
            anchor.chain,
            block_number,
            block_hash,
            parent_hash,
            0,
            0,
            TelosEngineApiExtraFields {
                statediffs_account: Some(Vec::new()),
                statediffs_accountstate: Some(Vec::new()),
                revision_changes: None,
                gasprice_changes: None,
                execution: Some(TelosExecutionMetadataV3 {
                    version: TELOS_EXECUTION_METADATA_VERSION,
                    block_hash,
                    parent_hash,
                    transaction_count: 0,
                    execution_base_fee: U256::ZERO,
                    starting_gas_price: anchor.starting_gas_price,
                    starting_revision: anchor.starting_revision,
                    gas_price_changes: Vec::new(),
                    revision_changes: Vec::new(),
                }),
                new_addresses_using_create: Some(Vec::new()),
                new_addresses_using_openwallet: Some(Vec::new()),
                receipts: Some(Vec::new()),
            },
        )
        .unwrap();
        store.put_pending(&sidecar).unwrap();
        store.mark_dispatched(block_hash, sidecar.digest()).unwrap();
        store.mark_accepted(block_hash, sidecar.digest()).unwrap();
        sidecar
    }

    fn empty_binding(hash: B256, parent_hash: B256) -> TelosStartupBlockBinding {
        TelosStartupBlockBinding {
            hash,
            parent_hash,
            transaction_count: 0,
            sender_count: 0,
            gas_used: 0,
        }
    }

    #[test]
    fn exact_anchor_with_readable_state_and_retained_senders_is_accepted() {
        let anchor = anchor();
        let provider = provider_for_anchor(anchor, Some(anchor.parent_block_hash));
        let store = sidecar_store(anchor);

        validate_telos_startup(&provider, &anchor, &store).unwrap();
        assert_eq!(*provider.probes.lock().unwrap(), vec![anchor.parent_block_hash]);
    }

    #[test]
    fn missing_or_mismatched_anchor_is_rejected_before_state_access() {
        let anchor = anchor();
        for canonical_hash in [None, Some(B256::repeat_byte(0x88))] {
            let provider = provider_for_anchor(anchor, canonical_hash);
            let store = sidecar_store(anchor);

            let error = validate_telos_startup(&provider, &anchor, &store).unwrap_err().to_string();
            assert!(error.contains("anchor"));
            assert!(provider.probes.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn unreadable_anchor_state_and_sender_pruning_are_rejected() {
        let anchor = anchor();
        let mut unreadable = provider_for_anchor(anchor, Some(anchor.parent_block_hash));
        unreadable.state_error = Some("anchor state was pruned");
        let store = sidecar_store(anchor);
        assert!(validate_telos_startup(&unreadable, &anchor, &store)
            .unwrap_err()
            .to_string()
            .contains("pruned"));

        let mut pruning = provider_for_anchor(anchor, Some(anchor.parent_block_hash));
        pruning.sender_pruning = true;
        assert!(validate_telos_startup(&pruning, &anchor, &store)
            .unwrap_err()
            .to_string()
            .contains("sender-recovery pruning"));
    }

    #[test]
    fn canonical_block_above_anchor_requires_an_accepted_sidecar() {
        let anchor = anchor();
        let mut provider = provider_for_anchor(anchor, Some(anchor.parent_block_hash));
        provider.best_block_number = anchor.parent_block_number + 1;
        provider.block_bindings.insert(
            anchor.parent_block_number + 1,
            TelosStartupBlockBinding {
                hash: B256::repeat_byte(0x88),
                parent_hash: anchor.parent_block_hash,
                transaction_count: 0,
                sender_count: 0,
                gas_used: 0,
            },
        );
        let store = sidecar_store(anchor);

        let error = validate_telos_startup(&provider, &anchor, &store).unwrap_err().to_string();
        assert!(error.contains("no accepted execution sidecar"));
    }

    #[test]
    fn finalized_coverage_marker_bounds_startup_scan_to_marker_and_tail() {
        let anchor = anchor();
        let hash8 = B256::repeat_byte(0x88);
        let hash9 = B256::repeat_byte(0x99);
        let hash10 = B256::repeat_byte(0xaa);
        let store = sidecar_store(anchor);
        accepted_sidecar(&store, anchor, 8, hash8, anchor.parent_block_hash);
        accepted_sidecar(&store, anchor, 9, hash9, hash8);
        accepted_sidecar(&store, anchor, 10, hash10, hash9);
        store.note_persisted_canonical_block(8, hash8).unwrap();
        store.note_persisted_canonical_block(9, hash9).unwrap();
        store.finalize_and_prune(&anchor, hash9).unwrap();

        let mut provider = provider_for_anchor(anchor, Some(anchor.parent_block_hash));
        provider.best_block_number = 10;
        provider.canonical_hashes.extend([(8, hash8), (9, hash9), (10, hash10)]);
        provider.block_bindings.extend([
            (8, empty_binding(hash8, anchor.parent_block_hash)),
            (9, empty_binding(hash9, hash8)),
            (10, empty_binding(hash10, hash9)),
        ]);

        validate_telos_startup(&provider, &anchor, &store).unwrap();
        assert_eq!(*provider.binding_reads.lock().unwrap(), vec![9, 10]);
    }

    #[test]
    fn scanned_tail_requires_one_durable_sender_row_per_transaction() {
        let anchor = anchor();
        let block_hash = B256::repeat_byte(0x88);
        let store = sidecar_store(anchor);
        accepted_sidecar(
            &store,
            anchor,
            anchor.parent_block_number + 1,
            block_hash,
            anchor.parent_block_hash,
        );
        let mut provider = provider_for_anchor(anchor, Some(anchor.parent_block_hash));
        provider.best_block_number = anchor.parent_block_number + 1;
        provider.canonical_hashes.insert(provider.best_block_number, block_hash);
        let mut binding = empty_binding(block_hash, anchor.parent_block_hash);
        binding.sender_count = 1;
        provider.block_bindings.insert(provider.best_block_number, binding);

        let error = validate_telos_startup(&provider, &anchor, &store).unwrap_err().to_string();
        assert!(error.contains("durable sender rows"));
    }
}

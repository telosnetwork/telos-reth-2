//! Durable, content-addressed storage for payload-bound Telos execution sidecars.
//!
//! Sidecars are persisted before Engine dispatch, frozen to an exact digest while validation is in
//! flight, and exposed to replay/RPC only after `VALID`. Every lifecycle transition and its
//! block-number/hash index update is transactional.

use alloy_primitives::{keccak256, Bytes, B256, U256};
use reth_db_api::{
    cursor::DbCursorRO,
    table::{Decode, Encode, Table, TableInfo},
    tables,
    transaction::{DbTx, DbTxMut},
    Database, DatabaseError, TableSet,
};
use reth_provider::{DBProvider, DatabaseProviderFactory};
use reth_stages_types::StageId;
use reth_telos_rpc_engine_api::{
    structs::{
        TelosEngineApiExtraFields, TelosReceiptType, MAX_EXTRA_FIELDS_BYTES,
        TELOS_EXECUTION_METADATA_VERSION,
    },
    validate_extra_fields_for_payload,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::{Arc, RwLock},
};
use thiserror::Error;

/// Current canonical sidecar-envelope version.
pub const TELOS_EXECUTION_SIDECAR_VERSION: u8 = 1;

/// Current lifecycle-aware on-disk record version.
pub const TELOS_EXECUTION_SIDECAR_RECORD_VERSION: u8 = 3;

/// Current trusted execution-anchor file version.
pub const TELOS_EXECUTION_ANCHOR_VERSION: u8 = 1;

/// Hard limit for the canonical envelope stored for one payload.
pub const MAX_TELOS_EXECUTION_SIDECAR_BYTES: usize = MAX_EXTRA_FIELDS_BYTES;

const RECORD_MAGIC: [u8; 8] = *b"TLSCAR03";
const RECORD_HEADER_LEN: usize = RECORD_MAGIC.len() + 1 + 1 + 32 + 4;

/// One validated and canonicalized Telos execution sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelosExecutionSidecar {
    envelope: TelosExecutionSidecarEnvelope,
    canonical_bytes: Bytes,
    digest: B256,
}

impl TelosExecutionSidecar {
    /// Validates and canonicalizes a complete sidecar envelope.
    pub fn new(
        chain: TelosChainIdentity,
        block_number: u64,
        block_hash: B256,
        parent_hash: B256,
        transaction_count: u64,
        gas_used: u64,
        extra_fields: TelosEngineApiExtraFields,
    ) -> Result<Self, TelosSidecarError> {
        Self::from_envelope(TelosExecutionSidecarEnvelope {
            version: TELOS_EXECUTION_SIDECAR_VERSION,
            chain,
            block_number,
            block_hash,
            parent_hash,
            transaction_count,
            gas_used,
            extra_fields,
        })
    }

    /// Reconstructs a sidecar from its canonical envelope.
    pub fn from_envelope(
        mut envelope: TelosExecutionSidecarEnvelope,
    ) -> Result<Self, TelosSidecarError> {
        validate_envelope(&envelope)?;
        canonicalize_extra_fields(&mut envelope.extra_fields)?;

        let canonical_bytes = serde_json::to_vec(&envelope)
            .map_err(|error| TelosSidecarError::Encoding(error.to_string()))?;
        check_size(canonical_bytes.len())?;
        let digest = keccak256(&canonical_bytes);

        Ok(Self { envelope, canonical_bytes: canonical_bytes.into(), digest })
    }

    /// Decodes bytes only if they use the unique canonical representation of the envelope.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, TelosSidecarError> {
        check_size(bytes.len())?;
        let envelope: TelosExecutionSidecarEnvelope = serde_json::from_slice(bytes)
            .map_err(|error| TelosSidecarError::Decoding(error.to_string()))?;
        let sidecar = Self::from_envelope(envelope)?;
        if sidecar.canonical_bytes.as_ref() != bytes {
            return Err(TelosSidecarError::NonCanonical)
        }
        Ok(sidecar)
    }

    /// Returns the immutable canonical envelope.
    pub const fn envelope(&self) -> &TelosExecutionSidecarEnvelope {
        &self.envelope
    }

    /// Returns the canonical serialized envelope.
    pub const fn canonical_bytes(&self) -> &Bytes {
        &self.canonical_bytes
    }

    /// Returns the Keccak-256 digest of the canonical envelope.
    pub const fn digest(&self) -> B256 {
        self.digest
    }
}

/// Durable lifecycle state for one exact sidecar digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TelosSidecarState {
    /// Metadata is durable but has not yet been dispatched to the Engine.
    Pending = 0,
    /// The exact digest was dispatched and is immutable while Engine validation is unresolved.
    Dispatched = 1,
    /// The Engine API has returned `VALID`, so replay and public RPC may consume the record.
    Accepted = 2,
}

impl TryFrom<u8> for TelosSidecarState {
    type Error = TelosSidecarError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Dispatched),
            2 => Ok(Self::Accepted),
            _ => Err(TelosSidecarError::InvalidRecordState(value)),
        }
    }
}

/// One lifecycle-tagged durable sidecar record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelosStoredSidecar {
    sidecar: TelosExecutionSidecar,
    state: TelosSidecarState,
}

impl TelosStoredSidecar {
    const fn pending(sidecar: TelosExecutionSidecar) -> Self {
        Self { sidecar, state: TelosSidecarState::Pending }
    }

    /// Returns the immutable canonical sidecar.
    pub const fn sidecar(&self) -> &TelosExecutionSidecar {
        &self.sidecar
    }

    /// Returns the durable lifecycle state.
    pub const fn state(&self) -> TelosSidecarState {
        self.state
    }

    /// Consumes the lifecycle wrapper.
    pub fn into_sidecar(self) -> TelosExecutionSidecar {
        self.sidecar
    }

    fn encode_record(&self) -> Bytes {
        let mut record = Vec::with_capacity(RECORD_HEADER_LEN + self.sidecar.canonical_bytes.len());
        record.extend_from_slice(&RECORD_MAGIC);
        record.push(TELOS_EXECUTION_SIDECAR_RECORD_VERSION);
        record.push(self.state as u8);
        record.extend_from_slice(self.sidecar.digest.as_slice());
        record.extend_from_slice(&(self.sidecar.canonical_bytes.len() as u32).to_be_bytes());
        record.extend_from_slice(&self.sidecar.canonical_bytes);
        record.into()
    }

    fn decode_record(
        record: &[u8],
        expected_chain: TelosChainIdentity,
    ) -> Result<Self, TelosSidecarError> {
        if record.len() < RECORD_HEADER_LEN {
            return Err(TelosSidecarError::CorruptRecord("record header is truncated"))
        }
        if record[..RECORD_MAGIC.len()] != RECORD_MAGIC {
            return Err(TelosSidecarError::CorruptRecord("record magic is invalid"))
        }

        let version = record[RECORD_MAGIC.len()];
        if version != TELOS_EXECUTION_SIDECAR_RECORD_VERSION {
            return Err(TelosSidecarError::UnsupportedRecordVersion {
                expected: TELOS_EXECUTION_SIDECAR_RECORD_VERSION,
                actual: version,
            })
        }
        let state = TelosSidecarState::try_from(record[RECORD_MAGIC.len() + 1])?;

        let digest_start = RECORD_MAGIC.len() + 2;
        let digest_end = digest_start + 32;
        let stored_digest = B256::from_slice(&record[digest_start..digest_end]);
        let length_end = digest_end + 4;
        let declared_length = u32::from_be_bytes(
            record[digest_end..length_end]
                .try_into()
                .map_err(|_| TelosSidecarError::CorruptRecord("record length is invalid"))?,
        ) as usize;
        check_size(declared_length)?;
        if record.len() != RECORD_HEADER_LEN + declared_length {
            return Err(TelosSidecarError::CorruptRecord(
                "record length does not match its canonical payload",
            ))
        }

        let canonical_bytes = &record[RECORD_HEADER_LEN..];
        let actual_digest = keccak256(canonical_bytes);
        if stored_digest != actual_digest {
            return Err(TelosSidecarError::DigestMismatch { stored_digest, actual_digest })
        }

        let sidecar = TelosExecutionSidecar::from_canonical_bytes(canonical_bytes)?;
        if sidecar.digest != stored_digest {
            return Err(TelosSidecarError::DigestMismatch {
                stored_digest,
                actual_digest: sidecar.digest,
            })
        }
        ensure_chain(expected_chain, sidecar.envelope.chain)?;
        Ok(Self { sidecar, state })
    }
}

/// Identity that prevents a valid sidecar from being replayed into a different chain database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosChainIdentity {
    /// EIP-155 chain identifier.
    pub chain_id: u64,
    /// Genesis block hash for the exact chain instance.
    pub genesis_hash: B256,
}

/// Trusted boundary between an imported snapshot and sidecar-covered execution.
///
/// The first accepted sidecar must describe the exact child of this parent and must start with
/// these native execution values. Once that record exists, every later sidecar inherits its
/// starting values from the exact hash-bound parent record instead of consulting this anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosExecutionAnchor {
    /// Anchor schema version.
    pub version: u8,
    /// Exact chain and genesis database identity.
    pub chain: TelosChainIdentity,
    /// Last block supplied by the imported snapshot.
    pub parent_block_number: u64,
    /// Exact canonical hash of the snapshot head.
    pub parent_block_hash: B256,
    /// Native gas price effective before transaction zero of the first covered child.
    pub starting_gas_price: U256,
    /// Native revision effective before transaction zero of the first covered child.
    pub starting_revision: u64,
}

impl TelosExecutionAnchor {
    /// Validates the schema and exact chain binding of this anchor.
    pub fn validate_for_chain(
        &self,
        expected_chain: TelosChainIdentity,
    ) -> Result<(), TelosSidecarError> {
        if self.version != TELOS_EXECUTION_ANCHOR_VERSION {
            return Err(TelosSidecarError::UnsupportedAnchorVersion {
                expected: TELOS_EXECUTION_ANCHOR_VERSION,
                actual: self.version,
            })
        }
        ensure_chain(expected_chain, self.chain)?;
        self.parent_block_number
            .checked_add(1)
            .ok_or(TelosSidecarError::AnchorBlockNumberOverflow)?;
        Ok(())
    }
}

/// Canonical, self-contained data committed by a [`TelosExecutionSidecar`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosExecutionSidecarEnvelope {
    /// Envelope schema version.
    pub version: u8,
    /// Exact chain identity.
    pub chain: TelosChainIdentity,
    /// Payload block number.
    pub block_number: u64,
    /// Payload block hash and primary store key.
    pub block_hash: B256,
    /// Payload parent hash.
    pub parent_hash: B256,
    /// Number of transactions committed by the payload.
    pub transaction_count: u64,
    /// Payload gas used, which binds the canonical receipt list.
    pub gas_used: u64,
    /// Complete, payload-bound Engine API extension.
    pub extra_fields: TelosEngineApiExtraFields,
}

/// Result of atomically persisting an unaccepted sidecar candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelosSidecarPutOutcome {
    /// A new pending primary record and number/hash index entry were committed.
    InsertedPending,
    /// The exact pending candidate was already present.
    AlreadyPending,
    /// A conflicting, still-pending candidate was atomically replaced.
    ReplacedPending,
    /// The exact digest was already dispatched and was not made replaceable.
    AlreadyDispatched,
    /// The exact record was already accepted and was not demoted.
    AlreadyAccepted,
}

/// Result of accepting one exact pending sidecar digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelosSidecarAcceptOutcome {
    /// The exact pending record was atomically promoted.
    Accepted,
    /// The exact record was already accepted.
    AlreadyAccepted,
}

/// Result of freezing one exact pending digest before Engine dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelosSidecarDispatchOutcome {
    /// The exact pending digest is now immutable until its Engine result is known.
    Dispatched,
    /// The exact digest was already dispatched, such as after a process restart.
    AlreadyDispatched,
    /// The exact digest was already accepted.
    AlreadyAccepted,
}

/// Result of removing one exact unaccepted sidecar digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelosSidecarRemoveOutcome {
    /// The exact pending or dispatched primary record and index entry were removed.
    RemovedPending,
    /// No record exists for the supplied block hash.
    AlreadyAbsent,
}

/// Durable canonical-coverage checkpoint advanced atomically with finalized fork pruning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosFinalizedCoverage {
    /// Finalized canonical block covered by accepted sidecars.
    pub block_number: u64,
    /// Exact canonical hash at `block_number`.
    pub block_hash: B256,
}

/// Visibility returned after an atomic finalized-coverage/pruning transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelosSidecarPruneOutcome {
    /// Effective durable coverage after this call.
    ///
    /// This can trail the requested Engine finality while Reth still holds the corresponding
    /// canonical blocks only in memory.
    pub finalized: TelosFinalizedCoverage,
    /// Fork records removed, including orphan descendants above finality.
    pub removed_records: u64,
    /// Integrity-framed bytes removed from the primary table.
    pub removed_bytes: u64,
    /// Newly finalized canonical records retained in this advancement.
    pub retained_canonical_records: u64,
}

/// Object-safe sidecar persistence interface used by Engine API ingestion and replay.
pub trait TelosSidecarStore: Send + Sync {
    /// Exact chain accepted by this store.
    fn chain_identity(&self) -> TelosChainIdentity;

    /// Validates the complete Engine forkchoice tuple in one consistent sidecar snapshot.
    ///
    /// The head must be nonzero. Nonzero safe and finalized hashes, plus the head, must resolve to
    /// accepted sidecars or the trusted execution anchor. The selected chain must extend durable
    /// finality, and safe/finalized ancestry must be consistent before the Engine can mutate its
    /// canonical state.
    fn validate_forkchoice_state(
        &self,
        anchor: &TelosExecutionAnchor,
        head_block_hash: B256,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> Result<(), TelosSidecarError>;

    /// Atomically validates continuity and finality, persists all indexes, and freezes the exact
    /// candidate for Engine dispatch.
    ///
    /// This is the only safe ingress operation for `engine_newPayload`: no observable state may
    /// contain a dispatched child whose validated parent was concurrently removed. Exact retries
    /// are idempotent, while a different digest can never replace a dispatched or accepted record.
    fn validate_and_mark_dispatched(
        &self,
        anchor: &TelosExecutionAnchor,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError>;

    /// Atomically writes a pending candidate and its number/hash index.
    ///
    /// A conflicting candidate may replace only a pending record. Accepted records are immutable.
    fn put_pending(
        &self,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarPutOutcome, TelosSidecarError>;

    /// Atomically freezes only the exact pending digest before calling the Engine.
    fn mark_dispatched(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError>;

    /// Atomically promotes only the exact pending digest to accepted.
    fn mark_accepted(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarAcceptOutcome, TelosSidecarError>;

    /// Atomically removes only the exact unaccepted digest after an `INVALID` result.
    fn remove_pending(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarRemoveOutcome, TelosSidecarError>;

    /// Advances coverage no further than Reth's durably persisted canonical tip and atomically
    /// prunes every noncanonical fork rooted at or below that height, including descendants.
    ///
    /// A `VALID` forkchoice update can precede Reth's asynchronous block persistence. In that
    /// window this operation succeeds without advancing beyond the last committed `Finish` stage
    /// checkpoint. The next forkchoice retry/update resumes from the durable marker.
    fn finalize_and_prune(
        &self,
        anchor: &TelosExecutionAnchor,
        finalized_hash: B256,
    ) -> Result<TelosSidecarPruneOutcome, TelosSidecarError>;

    /// Returns the last finalized coverage marker committed with pruning.
    fn finalized_coverage(&self) -> Result<Option<TelosFinalizedCoverage>, TelosSidecarError>;

    /// Reads one lifecycle-tagged record by its exact block hash.
    fn get_record_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosStoredSidecar>, TelosSidecarError>;

    /// Reads every lifecycle-tagged fork record at a height in deterministic hash order.
    fn get_records_by_number(
        &self,
        block_number: u64,
    ) -> Result<Vec<TelosStoredSidecar>, TelosSidecarError>;

    /// Reads only a pending candidate, for compare-and-set Engine lifecycle transitions.
    fn get_pending_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosExecutionSidecar>, TelosSidecarError> {
        Ok(self
            .get_record_by_hash(block_hash)?
            .filter(|record| record.state == TelosSidecarState::Pending)
            .map(TelosStoredSidecar::into_sidecar))
    }

    /// Reads only an exact digest already dispatched to the Engine.
    fn get_dispatched_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosExecutionSidecar>, TelosSidecarError> {
        Ok(self
            .get_record_by_hash(block_hash)?
            .filter(|record| record.state == TelosSidecarState::Dispatched)
            .map(TelosStoredSidecar::into_sidecar))
    }

    /// Reads metadata permitted only on Engine execution paths.
    fn get_engine_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosExecutionSidecar>, TelosSidecarError> {
        Ok(self
            .get_record_by_hash(block_hash)?
            .filter(|record| record.state != TelosSidecarState::Pending)
            .map(TelosStoredSidecar::into_sidecar))
    }

    /// Reads only Engine-accepted metadata, for stored replay and public RPC.
    fn get_accepted_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosExecutionSidecar>, TelosSidecarError> {
        Ok(self
            .get_record_by_hash(block_hash)?
            .filter(|record| record.state == TelosSidecarState::Accepted)
            .map(TelosStoredSidecar::into_sidecar))
    }

    /// Reads accepted fork records at a height in deterministic block-hash order.
    fn get_accepted_by_number(
        &self,
        block_number: u64,
    ) -> Result<Vec<TelosExecutionSidecar>, TelosSidecarError> {
        Ok(self
            .get_records_by_number(block_number)?
            .into_iter()
            .filter(|record| record.state == TelosSidecarState::Accepted)
            .map(TelosStoredSidecar::into_sidecar)
            .collect())
    }
}

/// Validates a candidate against a dispatched/accepted parent or the trusted snapshot anchor.
///
/// This read-only helper is intended for diagnostics. Engine ingress must use
/// [`TelosSidecarStore::validate_and_mark_dispatched`] so validation and persistence cannot race.
/// A missing non-anchor parent is always rejected.
pub fn validate_sidecar_continuity(
    store: &dyn TelosSidecarStore,
    anchor: &TelosExecutionAnchor,
    child: &TelosExecutionSidecar,
) -> Result<(), TelosSidecarError> {
    validate_sidecar_continuity_with_visibility(store, anchor, child, false)
}

/// Validates replay/RPC continuity using only Engine-accepted parent metadata.
pub fn validate_accepted_sidecar_continuity(
    store: &dyn TelosSidecarStore,
    anchor: &TelosExecutionAnchor,
    child: &TelosExecutionSidecar,
) -> Result<(), TelosSidecarError> {
    validate_sidecar_continuity_with_visibility(store, anchor, child, true)
}

fn validate_sidecar_continuity_with_visibility(
    store: &dyn TelosSidecarStore,
    anchor: &TelosExecutionAnchor,
    child: &TelosExecutionSidecar,
    accepted_only: bool,
) -> Result<(), TelosSidecarError> {
    let chain = store.chain_identity();
    anchor.validate_for_chain(chain)?;
    ensure_chain(chain, child.envelope.chain)?;

    let child_execution = child.envelope.extra_fields.execution.as_ref().ok_or_else(|| {
        TelosSidecarError::Validation("execution metadata is missing".to_string())
    })?;
    let parent = if accepted_only {
        store.get_accepted_by_hash(child.envelope.parent_hash)?
    } else {
        store.get_engine_by_hash(child.envelope.parent_hash)?
    };
    let expected = if let Some(parent) = parent {
        let expected_number = parent.envelope.block_number.checked_add(1).ok_or(
            TelosSidecarError::ParentBlockNumberOverflow {
                parent_block_hash: parent.envelope.block_hash,
            },
        )?;
        if child.envelope.block_number != expected_number {
            return Err(TelosSidecarError::NonSequentialBlock {
                parent_block_hash: parent.envelope.block_hash,
                expected: expected_number,
                actual: child.envelope.block_number,
            })
        }
        let execution = parent.envelope.extra_fields.execution.as_ref().ok_or_else(|| {
            TelosSidecarError::Validation("parent execution metadata is missing".to_string())
        })?;
        let gas_price = execution
            .gas_price_changes
            .last()
            .map_or(execution.starting_gas_price, |change| change.value);
        let revision = execution
            .revision_changes
            .last()
            .map_or(execution.starting_revision, |change| change.value);
        (gas_price, revision)
    } else if child.envelope.parent_hash == anchor.parent_block_hash &&
        child.envelope.block_number == anchor.parent_block_number + 1
    {
        (anchor.starting_gas_price, anchor.starting_revision)
    } else {
        return Err(TelosSidecarError::MissingParentSidecar {
            block_number: child.envelope.block_number,
            block_hash: child.envelope.block_hash,
            parent_hash: child.envelope.parent_hash,
        })
    };

    if child_execution.starting_gas_price != expected.0 {
        return Err(TelosSidecarError::GasPriceContinuity {
            block_hash: child.envelope.block_hash,
            expected: expected.0,
            actual: child_execution.starting_gas_price,
        })
    }
    if child_execution.starting_revision != expected.1 {
        return Err(TelosSidecarError::RevisionContinuity {
            block_hash: child.envelope.block_hash,
            expected: expected.1,
            actual: child_execution.starting_revision,
        })
    }
    Ok(())
}

/// Lock-based in-memory implementation for focused tests and ephemeral development nodes.
#[derive(Debug)]
pub struct InMemoryTelosSidecarStore {
    chain: TelosChainIdentity,
    state: RwLock<InMemoryState>,
}

impl InMemoryTelosSidecarStore {
    /// Creates an empty store bound to one exact chain.
    pub fn new(chain: TelosChainIdentity) -> Self {
        Self { chain, state: RwLock::new(InMemoryState::default()) }
    }

    /// Records one canonical block as durably persisted by an embedding development harness.
    ///
    /// Production nodes use [`ProviderTelosSidecarStore`], whose persistence proof is read from
    /// Reth's database in the same transaction as finality advancement.
    pub fn note_persisted_canonical_block(
        &self,
        block_number: u64,
        block_hash: B256,
    ) -> Result<(), TelosSidecarError> {
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;
        state.persisted_canonical.insert(block_number, block_hash);
        Ok(())
    }
}

impl TelosSidecarStore for InMemoryTelosSidecarStore {
    fn chain_identity(&self) -> TelosChainIdentity {
        self.chain
    }

    fn validate_forkchoice_state(
        &self,
        anchor: &TelosExecutionAnchor,
        head_block_hash: B256,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> Result<(), TelosSidecarError> {
        anchor.validate_for_chain(self.chain)?;
        let state = self.state.read().map_err(|_| TelosSidecarError::LockPoisoned)?;
        validate_forkchoice_state_in_memory(
            &state,
            anchor,
            head_block_hash,
            safe_block_hash,
            finalized_block_hash,
        )
    }

    fn validate_and_mark_dispatched(
        &self,
        anchor: &TelosExecutionAnchor,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
        anchor.validate_for_chain(self.chain)?;
        ensure_chain(self.chain, sidecar.envelope.chain)?;
        let block_hash = sidecar.envelope.block_hash;
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;
        let existing = state.by_hash.get(&block_hash).cloned();
        let existing_outcome = existing
            .as_ref()
            .map(|record| {
                validate_in_memory_index(&state, block_hash, record)?;
                compare_existing_for_pending(record, sidecar)
            })
            .transpose()?;

        if existing_outcome == Some(TelosSidecarPutOutcome::AlreadyAccepted) {
            return Ok(TelosSidecarDispatchOutcome::AlreadyAccepted)
        }

        validate_sidecar_ingress_in_memory(&state, self.chain, anchor, sidecar)?;
        if existing_outcome == Some(TelosSidecarPutOutcome::AlreadyDispatched) {
            return Ok(TelosSidecarDispatchOutcome::AlreadyDispatched)
        }

        // Validate every fallible index condition before mutating the lock-protected state. The
        // primary and both indexes then become visible together when this write lock is released.
        write_dispatched_in_memory(&mut state, sidecar, existing.as_ref())?;
        Ok(TelosSidecarDispatchOutcome::Dispatched)
    }

    fn put_pending(
        &self,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarPutOutcome, TelosSidecarError> {
        ensure_chain(self.chain, sidecar.envelope.chain)?;
        let block_hash = sidecar.envelope.block_hash;
        let block_number = sidecar.envelope.block_number;
        let parent_hash = sidecar.envelope.parent_hash;
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;

        if let Some(existing) = state.by_hash.get(&block_hash) {
            validate_in_memory_index(&state, block_hash, existing)?;
            let outcome = compare_existing_for_pending(existing, sidecar)?;
            if outcome != TelosSidecarPutOutcome::ReplacedPending {
                return Ok(outcome)
            }
            ensure_candidate_above_finalized_coverage(sidecar, state.finalized)?;

            let old_number = existing.sidecar.envelope.block_number;
            let old_parent = existing.sidecar.envelope.parent_hash;
            if old_number != block_number &&
                state
                    .by_number
                    .get(&block_number)
                    .is_some_and(|hashes| hashes.contains(&block_hash))
            {
                return Err(TelosSidecarError::CorruptIndex {
                    block_number,
                    block_hash,
                    indexed_hash: Some(block_hash),
                })
            }
            if old_parent != parent_hash &&
                state
                    .by_parent
                    .get(&parent_hash)
                    .is_some_and(|hashes| hashes.contains(&block_hash))
            {
                return Err(TelosSidecarError::CorruptParentIndex {
                    parent_hash,
                    block_hash,
                    indexed_hash: Some(block_hash),
                })
            }

            if old_number != block_number {
                let remove_height = state.by_number.get_mut(&old_number).ok_or(
                    TelosSidecarError::CorruptIndex {
                        block_number: old_number,
                        block_hash,
                        indexed_hash: None,
                    },
                )?;
                remove_height.remove(&block_hash);
                if remove_height.is_empty() {
                    state.by_number.remove(&old_number);
                }
                state.by_number.entry(block_number).or_default().insert(block_hash);
            }
            if old_parent != parent_hash {
                let old_children = state.by_parent.get_mut(&old_parent).ok_or(
                    TelosSidecarError::CorruptParentIndex {
                        parent_hash: old_parent,
                        block_hash,
                        indexed_hash: None,
                    },
                )?;
                old_children.remove(&block_hash);
                if old_children.is_empty() {
                    state.by_parent.remove(&old_parent);
                }
                state.by_parent.entry(parent_hash).or_default().insert(block_hash);
            }
            state.by_hash.insert(block_hash, TelosStoredSidecar::pending(sidecar.clone()));
            return Ok(outcome)
        }

        ensure_candidate_above_finalized_coverage(sidecar, state.finalized)?;

        if state.by_number.get(&block_number).is_some_and(|hashes| hashes.contains(&block_hash)) {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        if state.by_parent.get(&parent_hash).is_some_and(|hashes| hashes.contains(&block_hash)) {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        state.by_hash.insert(block_hash, TelosStoredSidecar::pending(sidecar.clone()));
        state.by_number.entry(block_number).or_default().insert(block_hash);
        state.by_parent.entry(parent_hash).or_default().insert(block_hash);
        Ok(TelosSidecarPutOutcome::InsertedPending)
    }

    fn mark_dispatched(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;
        let existing = state
            .by_hash
            .get(&block_hash)
            .ok_or(TelosSidecarError::MissingCandidate { block_hash, digest })?;
        validate_in_memory_index(&state, block_hash, existing)?;
        ensure_candidate_digest(existing, block_hash, digest)?;
        match existing.state {
            TelosSidecarState::Pending => {
                state.by_hash.get_mut(&block_hash).expect("record checked above").state =
                    TelosSidecarState::Dispatched;
                Ok(TelosSidecarDispatchOutcome::Dispatched)
            }
            TelosSidecarState::Dispatched => Ok(TelosSidecarDispatchOutcome::AlreadyDispatched),
            TelosSidecarState::Accepted => Ok(TelosSidecarDispatchOutcome::AlreadyAccepted),
        }
    }

    fn mark_accepted(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarAcceptOutcome, TelosSidecarError> {
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;
        let existing = state
            .by_hash
            .get(&block_hash)
            .ok_or(TelosSidecarError::MissingCandidate { block_hash, digest })?;
        validate_in_memory_index(&state, block_hash, existing)?;
        ensure_candidate_digest(existing, block_hash, digest)?;
        match existing.state {
            TelosSidecarState::Pending => {
                return Err(TelosSidecarError::CandidateNotDispatched { block_hash, digest })
            }
            TelosSidecarState::Dispatched => {}
            TelosSidecarState::Accepted => return Ok(TelosSidecarAcceptOutcome::AlreadyAccepted),
        }
        state.by_hash.get_mut(&block_hash).expect("record checked above").state =
            TelosSidecarState::Accepted;
        Ok(TelosSidecarAcceptOutcome::Accepted)
    }

    fn remove_pending(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarRemoveOutcome, TelosSidecarError> {
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;
        let Some(existing) = state.by_hash.get(&block_hash) else {
            return Ok(TelosSidecarRemoveOutcome::AlreadyAbsent)
        };
        validate_in_memory_index(&state, block_hash, existing)?;
        ensure_candidate_digest(existing, block_hash, digest)?;
        ensure_unaccepted(existing)?;
        let removals = collect_unaccepted_subtree_in_memory(&state, block_hash)?;

        for removal_hash in removals {
            let record = state
                .by_hash
                .remove(&removal_hash)
                .expect("subtree primary validated before mutation");
            let block_number = record.sidecar.envelope.block_number;
            let parent_hash = record.sidecar.envelope.parent_hash;
            let hashes = state
                .by_number
                .get_mut(&block_number)
                .expect("subtree number index validated before mutation");
            hashes.remove(&removal_hash);
            if hashes.is_empty() {
                state.by_number.remove(&block_number);
            }
            let children = state
                .by_parent
                .get_mut(&parent_hash)
                .expect("subtree parent index validated before mutation");
            children.remove(&removal_hash);
            if children.is_empty() {
                state.by_parent.remove(&parent_hash);
            }
        }
        Ok(TelosSidecarRemoveOutcome::RemovedPending)
    }

    fn finalize_and_prune(
        &self,
        anchor: &TelosExecutionAnchor,
        finalized_hash: B256,
    ) -> Result<TelosSidecarPruneOutcome, TelosSidecarError> {
        anchor.validate_for_chain(self.chain)?;
        let mut state = self.state.write().map_err(|_| TelosSidecarError::LockPoisoned)?;
        finalize_and_prune_in_memory(&mut state, anchor, finalized_hash)
    }

    fn finalized_coverage(&self) -> Result<Option<TelosFinalizedCoverage>, TelosSidecarError> {
        Ok(self.state.read().map_err(|_| TelosSidecarError::LockPoisoned)?.finalized)
    }

    fn get_record_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosStoredSidecar>, TelosSidecarError> {
        let state = self.state.read().map_err(|_| TelosSidecarError::LockPoisoned)?;
        let record = state.by_hash.get(&block_hash).cloned();
        if let Some(record) = &record {
            validate_in_memory_index(&state, block_hash, record)?;
        }
        Ok(record)
    }

    fn get_records_by_number(
        &self,
        block_number: u64,
    ) -> Result<Vec<TelosStoredSidecar>, TelosSidecarError> {
        let state = self.state.read().map_err(|_| TelosSidecarError::LockPoisoned)?;
        let Some(hashes) = state.by_number.get(&block_number) else { return Ok(Vec::new()) };
        hashes
            .iter()
            .map(|hash| {
                let record = state
                    .by_hash
                    .get(hash)
                    .cloned()
                    .ok_or(TelosSidecarError::MissingPrimary { block_number, block_hash: *hash })?;
                if record.sidecar.envelope.block_number != block_number ||
                    record.sidecar.envelope.block_hash != *hash
                {
                    return Err(TelosSidecarError::CorruptIndex {
                        block_number,
                        block_hash: *hash,
                        indexed_hash: Some(record.sidecar.envelope.block_hash),
                    })
                }
                validate_in_memory_index(&state, *hash, &record)?;
                Ok(record)
            })
            .collect()
    }
}

/// Reth database implementation using one transaction for the primary and number/hash index.
#[derive(Debug)]
pub struct DatabaseTelosSidecarStore<DB> {
    db: Arc<DB>,
    chain: TelosChainIdentity,
}

impl<DB> DatabaseTelosSidecarStore<DB> {
    /// Wraps a database after [`TelosSidecarTables`] have been created and tracked.
    pub const fn new(db: Arc<DB>, chain: TelosChainIdentity) -> Self {
        Self { db, chain }
    }

    /// Returns the underlying database handle.
    pub const fn database(&self) -> &Arc<DB> {
        &self.db
    }
}

impl<DB: Database> TelosSidecarStore for DatabaseTelosSidecarStore<DB> {
    fn chain_identity(&self) -> TelosChainIdentity {
        self.chain
    }

    fn validate_forkchoice_state(
        &self,
        anchor: &TelosExecutionAnchor,
        head_block_hash: B256,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> Result<(), TelosSidecarError> {
        let tx = self.db.tx().map_err(database_error)?;
        validate_forkchoice_state_with_transaction(
            &tx,
            self.chain,
            anchor,
            head_block_hash,
            safe_block_hash,
            finalized_block_hash,
        )?;
        tx.commit().map_err(database_error)
    }

    fn validate_and_mark_dispatched(
        &self,
        anchor: &TelosExecutionAnchor,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
        let tx = self.db.tx_mut().map_err(database_error)?;
        let outcome =
            validate_and_mark_dispatched_with_transaction(&tx, self.chain, anchor, sidecar)?;
        tx.commit().map_err(database_error)?;
        Ok(outcome)
    }

    fn put_pending(
        &self,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarPutOutcome, TelosSidecarError> {
        let tx = self.db.tx_mut().map_err(database_error)?;
        let outcome = put_pending_with_transaction(&tx, self.chain, sidecar)?;
        tx.commit().map_err(database_error)?;
        Ok(outcome)
    }

    fn mark_dispatched(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
        let tx = self.db.tx_mut().map_err(database_error)?;
        let outcome = mark_dispatched_with_transaction(&tx, self.chain, block_hash, digest)?;
        tx.commit().map_err(database_error)?;
        Ok(outcome)
    }

    fn mark_accepted(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarAcceptOutcome, TelosSidecarError> {
        let tx = self.db.tx_mut().map_err(database_error)?;
        let outcome = mark_accepted_with_transaction(&tx, self.chain, block_hash, digest)?;
        tx.commit().map_err(database_error)?;
        Ok(outcome)
    }

    fn remove_pending(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarRemoveOutcome, TelosSidecarError> {
        let tx = self.db.tx_mut().map_err(database_error)?;
        let outcome = remove_pending_with_transaction(&tx, self.chain, block_hash, digest)?;
        tx.commit().map_err(database_error)?;
        Ok(outcome)
    }

    fn finalize_and_prune(
        &self,
        anchor: &TelosExecutionAnchor,
        finalized_hash: B256,
    ) -> Result<TelosSidecarPruneOutcome, TelosSidecarError> {
        anchor.validate_for_chain(self.chain)?;
        let tx = self.db.tx_mut().map_err(database_error)?;
        let outcome = finalize_and_prune_with_transaction(&tx, self.chain, anchor, finalized_hash)?;
        tx.commit().map_err(database_error)?;
        Ok(outcome)
    }

    fn finalized_coverage(&self) -> Result<Option<TelosFinalizedCoverage>, TelosSidecarError> {
        let tx = self.db.tx().map_err(database_error)?;
        let marker = finalized_coverage_from_transaction(&tx)?;
        tx.commit().map_err(database_error)?;
        Ok(marker)
    }

    fn get_record_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosStoredSidecar>, TelosSidecarError> {
        let tx = self.db.tx().map_err(database_error)?;
        let sidecar = get_record_by_hash_from_transaction(&tx, self.chain, block_hash)?;
        tx.commit().map_err(database_error)?;
        Ok(sidecar)
    }

    fn get_records_by_number(
        &self,
        block_number: u64,
    ) -> Result<Vec<TelosStoredSidecar>, TelosSidecarError> {
        let tx = self.db.tx().map_err(database_error)?;
        let sidecars = get_records_by_number_from_transaction(&tx, self.chain, block_number)?;
        tx.commit().map_err(database_error)?;
        Ok(sidecars)
    }
}

/// Provider-factory implementation that can directly wrap `ctx.node.provider().clone()`.
///
/// The node database must register [`TelosSidecarTables`] before the provider factory is shared.
#[derive(Debug)]
pub struct ProviderTelosSidecarStore<F> {
    factory: F,
    chain: TelosChainIdentity,
}

impl<F> ProviderTelosSidecarStore<F> {
    /// Wraps a node provider after the custom sidecar tables have been created and tracked.
    pub const fn new(factory: F, chain: TelosChainIdentity) -> Self {
        Self { factory, chain }
    }

    /// Returns the node provider factory used to open transactions.
    pub const fn provider_factory(&self) -> &F {
        &self.factory
    }
}

impl<F: DatabaseProviderFactory> TelosSidecarStore for ProviderTelosSidecarStore<F> {
    fn chain_identity(&self) -> TelosChainIdentity {
        self.chain
    }

    fn validate_forkchoice_state(
        &self,
        anchor: &TelosExecutionAnchor,
        head_block_hash: B256,
        safe_block_hash: B256,
        finalized_block_hash: B256,
    ) -> Result<(), TelosSidecarError> {
        let provider = self.factory.database_provider_ro().map_err(provider_error)?;
        validate_forkchoice_state_with_transaction(
            provider.tx_ref(),
            self.chain,
            anchor,
            head_block_hash,
            safe_block_hash,
            finalized_block_hash,
        )?;
        provider.commit().map_err(provider_error)
    }

    fn validate_and_mark_dispatched(
        &self,
        anchor: &TelosExecutionAnchor,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
        let provider = self.factory.database_provider_rw().map_err(provider_error)?;
        let outcome = validate_and_mark_dispatched_with_transaction(
            provider.tx_ref(),
            self.chain,
            anchor,
            sidecar,
        )?;
        provider.commit().map_err(provider_error)?;
        Ok(outcome)
    }

    fn put_pending(
        &self,
        sidecar: &TelosExecutionSidecar,
    ) -> Result<TelosSidecarPutOutcome, TelosSidecarError> {
        let provider = self.factory.database_provider_rw().map_err(provider_error)?;
        let outcome = put_pending_with_transaction(provider.tx_ref(), self.chain, sidecar)?;
        provider.commit().map_err(provider_error)?;
        Ok(outcome)
    }

    fn mark_dispatched(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
        let provider = self.factory.database_provider_rw().map_err(provider_error)?;
        let outcome =
            mark_dispatched_with_transaction(provider.tx_ref(), self.chain, block_hash, digest)?;
        provider.commit().map_err(provider_error)?;
        Ok(outcome)
    }

    fn mark_accepted(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarAcceptOutcome, TelosSidecarError> {
        let provider = self.factory.database_provider_rw().map_err(provider_error)?;
        let outcome =
            mark_accepted_with_transaction(provider.tx_ref(), self.chain, block_hash, digest)?;
        provider.commit().map_err(provider_error)?;
        Ok(outcome)
    }

    fn remove_pending(
        &self,
        block_hash: B256,
        digest: B256,
    ) -> Result<TelosSidecarRemoveOutcome, TelosSidecarError> {
        let provider = self.factory.database_provider_rw().map_err(provider_error)?;
        let outcome =
            remove_pending_with_transaction(provider.tx_ref(), self.chain, block_hash, digest)?;
        provider.commit().map_err(provider_error)?;
        Ok(outcome)
    }

    fn finalize_and_prune(
        &self,
        anchor: &TelosExecutionAnchor,
        finalized_hash: B256,
    ) -> Result<TelosSidecarPruneOutcome, TelosSidecarError> {
        anchor.validate_for_chain(self.chain)?;
        let provider = self.factory.database_provider_rw().map_err(provider_error)?;
        let outcome = finalize_and_prune_with_transaction(
            provider.tx_ref(),
            self.chain,
            anchor,
            finalized_hash,
        )?;
        provider.commit().map_err(provider_error)?;
        Ok(outcome)
    }

    fn finalized_coverage(&self) -> Result<Option<TelosFinalizedCoverage>, TelosSidecarError> {
        let provider = self.factory.database_provider_ro().map_err(provider_error)?;
        let marker = finalized_coverage_from_transaction(provider.tx_ref())?;
        provider.commit().map_err(provider_error)?;
        Ok(marker)
    }

    fn get_record_by_hash(
        &self,
        block_hash: B256,
    ) -> Result<Option<TelosStoredSidecar>, TelosSidecarError> {
        let provider = self.factory.database_provider_ro().map_err(provider_error)?;
        let sidecar =
            get_record_by_hash_from_transaction(provider.tx_ref(), self.chain, block_hash)?;
        provider.commit().map_err(provider_error)?;
        Ok(sidecar)
    }

    fn get_records_by_number(
        &self,
        block_number: u64,
    ) -> Result<Vec<TelosStoredSidecar>, TelosSidecarError> {
        let provider = self.factory.database_provider_ro().map_err(provider_error)?;
        let sidecars =
            get_records_by_number_from_transaction(provider.tx_ref(), self.chain, block_number)?;
        provider.commit().map_err(provider_error)?;
        Ok(sidecars)
    }
}

/// Complete custom table set that must be created before opening the database-backed store.
#[derive(Clone, Copy, Debug)]
pub struct TelosSidecarTables;

impl TableSet for TelosSidecarTables {
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(
            [
                TelosSidecarTableInfo::Primary,
                TelosSidecarTableInfo::NumberHash,
                TelosSidecarTableInfo::ParentHash,
                TelosSidecarTableInfo::FinalizedCoverage,
            ]
            .into_iter()
            .map(|table| Box::new(table) as Box<dyn TableInfo>),
        )
    }
}

/// Primary table mapping block hash to an integrity-framed canonical envelope.
#[derive(Debug)]
pub struct TelosExecutionSidecars;

impl Table for TelosExecutionSidecars {
    const NAME: &'static str = "TelosExecutionSidecars";
    const DUPSORT: bool = false;

    type Key = B256;
    type Value = Bytes;
}

/// Fork-safe secondary index mapping `(block number, block hash)` to the primary key.
#[derive(Debug)]
pub struct TelosExecutionSidecarsByNumberHash;

impl Table for TelosExecutionSidecarsByNumberHash {
    const NAME: &'static str = "TelosExecutionSidecarsByNumberHash";
    const DUPSORT: bool = false;

    type Key = TelosSidecarNumberHashKey;
    type Value = B256;
}

/// Parent/child index used for atomic invalid-subtree cleanup.
#[derive(Debug)]
pub struct TelosExecutionSidecarsByParentHash;

impl Table for TelosExecutionSidecarsByParentHash {
    const NAME: &'static str = "TelosExecutionSidecarsByParentHash";
    const DUPSORT: bool = false;

    type Key = TelosSidecarParentHashKey;
    type Value = B256;
}

/// Single-row finalized coverage marker, keyed by finalized block number.
#[derive(Debug)]
pub struct TelosSidecarFinalizedCoverage;

impl Table for TelosSidecarFinalizedCoverage {
    const NAME: &'static str = "TelosSidecarFinalizedCoverage";
    const DUPSORT: bool = false;

    type Key = u64;
    type Value = B256;
}

/// Lexicographically encoded key used to scan all sidecars at one block height.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosSidecarNumberHashKey {
    /// Block number, encoded big-endian first.
    pub block_number: u64,
    /// Block hash, encoded after the number.
    pub block_hash: B256,
}

impl TelosSidecarNumberHashKey {
    /// Creates a number/hash composite key.
    pub const fn new(block_number: u64, block_hash: B256) -> Self {
        Self { block_number, block_hash }
    }
}

impl Encode for TelosSidecarNumberHashKey {
    type Encoded = [u8; 40];

    fn encode(self) -> Self::Encoded {
        let mut encoded = [0; 40];
        encoded[..8].copy_from_slice(&self.block_number.to_be_bytes());
        encoded[8..].copy_from_slice(self.block_hash.as_slice());
        encoded
    }
}

impl Decode for TelosSidecarNumberHashKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 40 {
            return Err(DatabaseError::Decode)
        }
        let block_number =
            u64::from_be_bytes(value[..8].try_into().map_err(|_| DatabaseError::Decode)?);
        let block_hash = B256::from_slice(&value[8..]);
        Ok(Self { block_number, block_hash })
    }
}

/// Lexicographically encoded `(parent hash, child hash)` relationship.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosSidecarParentHashKey {
    /// Exact parent block hash.
    pub parent_hash: B256,
    /// Exact child block hash.
    pub block_hash: B256,
}

impl TelosSidecarParentHashKey {
    /// Creates a parent/child composite key.
    pub const fn new(parent_hash: B256, block_hash: B256) -> Self {
        Self { parent_hash, block_hash }
    }
}

impl Encode for TelosSidecarParentHashKey {
    type Encoded = [u8; 64];

    fn encode(self) -> Self::Encoded {
        let mut encoded = [0; 64];
        encoded[..32].copy_from_slice(self.parent_hash.as_slice());
        encoded[32..].copy_from_slice(self.block_hash.as_slice());
        encoded
    }
}

impl Decode for TelosSidecarParentHashKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 64 {
            return Err(DatabaseError::Decode)
        }
        Ok(Self {
            parent_hash: B256::from_slice(&value[..32]),
            block_hash: B256::from_slice(&value[32..]),
        })
    }
}

/// Validation, integrity, or persistence error. Every variant is fail-closed.
#[derive(Debug, Error)]
pub enum TelosSidecarError {
    /// The canonical envelope used an unsupported schema version.
    #[error("unsupported Telos sidecar version {actual}; expected {expected}")]
    UnsupportedVersion {
        /// Required version.
        expected: u8,
        /// Observed version.
        actual: u8,
    },
    /// A durable record used an unsupported lifecycle-aware framing version.
    #[error("unsupported Telos sidecar record version {actual}; expected {expected}")]
    UnsupportedRecordVersion {
        /// Required record version.
        expected: u8,
        /// Observed record version.
        actual: u8,
    },
    /// A durable record contains an unknown lifecycle-state discriminant.
    #[error("invalid Telos sidecar lifecycle state {0}")]
    InvalidRecordState(u8),
    /// A trusted snapshot anchor used an unsupported schema version.
    #[error("unsupported Telos execution-anchor version {actual}; expected {expected}")]
    UnsupportedAnchorVersion {
        /// Required version.
        expected: u8,
        /// Observed version.
        actual: u8,
    },
    /// The anchor's snapshot head has no representable child height.
    #[error("Telos execution anchor parent block number cannot be incremented")]
    AnchorBlockNumberOverflow,
    /// A durable parent sidecar has no representable child height.
    #[error("Telos sidecar parent {parent_block_hash} block number cannot be incremented")]
    ParentBlockNumberOverflow {
        /// Parent whose height overflowed.
        parent_block_hash: B256,
    },
    /// A candidate is not the immediate child of its durable parent.
    #[error(
        "non-sequential Telos sidecar after parent {parent_block_hash}: expected block {expected}, got {actual}"
    )]
    NonSequentialBlock {
        /// Exact parent hash.
        parent_block_hash: B256,
        /// Required child height.
        expected: u64,
        /// Candidate height.
        actual: u64,
    },
    /// No durable parent record or matching trusted snapshot anchor exists.
    #[error("missing Telos parent sidecar {parent_hash} for block {block_number} ({block_hash})")]
    MissingParentSidecar {
        /// Candidate block height.
        block_number: u64,
        /// Candidate block hash.
        block_hash: B256,
        /// Required parent hash.
        parent_hash: B256,
    },
    /// The candidate's native gas price does not inherit from its exact parent.
    #[error(
        "Telos gas-price continuity mismatch for block {block_hash}: expected {expected}, got {actual}"
    )]
    GasPriceContinuity {
        /// Candidate block hash.
        block_hash: B256,
        /// Required starting value.
        expected: U256,
        /// Supplied starting value.
        actual: U256,
    },
    /// The candidate's native revision does not inherit from its exact parent.
    #[error(
        "Telos revision continuity mismatch for block {block_hash}: expected {expected}, got {actual}"
    )]
    RevisionContinuity {
        /// Candidate block hash.
        block_hash: B256,
        /// Required starting revision.
        expected: u64,
        /// Supplied starting revision.
        actual: u64,
    },
    /// A transaction count cannot be represented on this platform.
    #[error("transaction count {0} cannot be represented on this platform")]
    TransactionCountOverflow(u64),
    /// Payload-bound extension validation failed.
    #[error("invalid Telos execution sidecar: {0}")]
    Validation(String),
    /// Canonical serialization failed.
    #[error("failed to encode canonical Telos sidecar: {0}")]
    Encoding(String),
    /// Canonical deserialization failed.
    #[error("failed to decode canonical Telos sidecar: {0}")]
    Decoding(String),
    /// The envelope exceeded its hard byte limit.
    #[error("Telos sidecar is too large: {actual} bytes exceeds {maximum}")]
    TooLarge {
        /// Actual canonical byte length.
        actual: usize,
        /// Hard maximum canonical byte length.
        maximum: usize,
    },
    /// The supplied bytes decode but are not the unique canonical representation.
    #[error("Telos sidecar bytes are not canonical")]
    NonCanonical,
    /// The record framing is malformed.
    #[error("corrupt Telos sidecar record: {0}")]
    CorruptRecord(&'static str),
    /// The persisted digest does not match the canonical bytes.
    #[error("Telos sidecar digest mismatch: stored {stored_digest}, actual {actual_digest}")]
    DigestMismatch {
        /// Digest stored in the record header.
        stored_digest: B256,
        /// Digest recomputed from canonical bytes.
        actual_digest: B256,
    },
    /// The sidecar belongs to a different chain.
    #[error("Telos sidecar chain mismatch: expected {expected:?}, got {actual:?}")]
    ChainMismatch {
        /// Store chain identity.
        expected: TelosChainIdentity,
        /// Sidecar chain identity.
        actual: TelosChainIdentity,
    },
    /// A compare-and-set transition referenced a block hash with no current candidate.
    #[error("missing Telos sidecar candidate {block_hash} for digest {digest}")]
    MissingCandidate {
        /// Requested candidate block hash.
        block_hash: B256,
        /// Requested candidate digest.
        digest: B256,
    },
    /// A delayed lifecycle transition no longer matches the current pending candidate.
    #[error(
        "Telos sidecar candidate digest mismatch for block {block_hash}: requested {requested_digest}, current {current_digest}"
    )]
    CandidateDigestMismatch {
        /// Candidate block hash.
        block_hash: B256,
        /// Digest captured by the Engine request before dispatch.
        requested_digest: B256,
        /// Current durable candidate digest.
        current_digest: B256,
    },
    /// A conflicting digest attempted to replace metadata already dispatched to the Engine.
    #[error(
        "Telos sidecar {block_hash} is already dispatched: in-flight digest {dispatched_digest}, requested {requested_digest}"
    )]
    CandidateInFlight {
        /// Candidate block hash.
        block_hash: B256,
        /// Immutable digest previously dispatched to the Engine.
        dispatched_digest: B256,
        /// Conflicting incoming digest.
        requested_digest: B256,
    },
    /// A VALID transition was attempted for metadata never dispatched to the Engine.
    #[error("Telos sidecar {block_hash} digest {digest} was not dispatched to the Engine")]
    CandidateNotDispatched {
        /// Candidate block hash.
        block_hash: B256,
        /// Candidate digest.
        digest: B256,
    },
    /// An operation attempted to replace or delete an accepted record.
    #[error(
        "accepted Telos sidecar {block_hash} is immutable: accepted digest {accepted_digest}, requested {requested_digest}"
    )]
    AcceptedImmutable {
        /// Accepted block hash.
        block_hash: B256,
        /// Immutable accepted digest.
        accepted_digest: B256,
        /// Digest requested by the rejected operation.
        requested_digest: B256,
    },
    /// A primary lookup decoded a different block hash.
    #[error("Telos sidecar primary key mismatch: expected {expected}, got {actual}")]
    PrimaryKeyMismatch {
        /// Lookup key.
        expected: B256,
        /// Hash committed by the record.
        actual: B256,
    },
    /// A number/hash index entry is missing or inconsistent.
    #[error(
        "corrupt Telos sidecar index at block {block_number} hash {block_hash}: indexed value {indexed_hash:?}"
    )]
    CorruptIndex {
        /// Indexed block number.
        block_number: u64,
        /// Hash committed by the index key.
        block_hash: B256,
        /// Hash stored as the index value, or `None` when absent.
        indexed_hash: Option<B256>,
    },
    /// A parent/child index entry is missing or inconsistent.
    #[error(
        "corrupt Telos sidecar parent index at parent {parent_hash} child {block_hash}: indexed value {indexed_hash:?}"
    )]
    CorruptParentIndex {
        /// Indexed parent hash.
        parent_hash: B256,
        /// Indexed child hash.
        block_hash: B256,
        /// Hash stored as the index value, or `None` when absent.
        indexed_hash: Option<B256>,
    },
    /// A parent/child index entry has no primary record.
    #[error("Telos sidecar parent {parent_hash} references missing child primary {block_hash}")]
    MissingChildPrimary {
        /// Indexed parent hash.
        parent_hash: B256,
        /// Missing child hash.
        block_hash: B256,
    },
    /// Invalid-subtree cleanup encountered an accepted descendant and aborted atomically.
    #[error(
        "cannot remove invalid Telos sidecar {invalid_root}: descendant {accepted_descendant} is already accepted"
    )]
    AcceptedDescendant {
        /// INVALID root candidate.
        invalid_root: B256,
        /// Accepted descendant that violates the contiguous-prefix invariant.
        accepted_descendant: B256,
    },
    /// Corrupt parent indexes formed a cycle.
    #[error("cyclic Telos sidecar parent index encountered at {0}")]
    CyclicParentIndex(B256),
    /// The Engine supplied the null hash as the forkchoice head.
    #[error("Telos forkchoice head block hash cannot be zero")]
    ZeroForkchoiceHead,
    /// A non-anchor forkchoice hash has no durable sidecar.
    #[error("Telos forkchoice {role} sidecar {block_hash} is missing")]
    ForkchoiceSidecarMissing {
        /// Forkchoice field or ancestry position being resolved.
        role: &'static str,
        /// Missing block hash.
        block_hash: B256,
    },
    /// A forkchoice hash references metadata that Engine validation has not accepted.
    #[error("Telos forkchoice {role} sidecar {block_hash} is {state:?}, not accepted")]
    ForkchoiceSidecarNotAccepted {
        /// Forkchoice field or ancestry position being resolved.
        role: &'static str,
        /// Referenced block hash.
        block_hash: B256,
        /// Durable lifecycle state.
        state: TelosSidecarState,
    },
    /// A sidecar encountered while walking forkchoice ancestry has an unexpected height.
    #[error(
        "Telos forkchoice ancestry gap at {block_hash}: expected block {expected_number}, got {actual_number}"
    )]
    ForkchoiceChainGap {
        /// Sidecar at the discontinuity.
        block_hash: B256,
        /// Required height.
        expected_number: u64,
        /// Sidecar height.
        actual_number: u64,
    },
    /// A requested safe, finalized, or durable-finality block is not an ancestor of its
    /// descendant.
    #[error(
        "Telos forkchoice block {expected_ancestor} is not an ancestor of {descendant_hash}; reached {actual_ancestor}"
    )]
    ForkchoiceAncestryMismatch {
        /// Descendant whose ancestry was walked.
        descendant_hash: B256,
        /// Hash required at the ancestor height.
        expected_ancestor: B256,
        /// Hash actually reached at that height.
        actual_ancestor: B256,
    },
    /// The Engine supplied the null hash as a finalized block.
    #[error("Telos finalized block hash cannot be zero")]
    ZeroFinalizedHash,
    /// The durable finalized-coverage table did not contain exactly one coherent row.
    #[error("corrupt Telos finalized-coverage table contains {entries} rows")]
    CorruptFinalizedCoverage {
        /// Observed number of marker rows.
        entries: usize,
    },
    /// A durable coverage marker predates the configured execution anchor.
    #[error(
        "Telos finalized coverage block {marker_number} is below execution anchor {anchor_number}"
    )]
    FinalizedCoverageBeforeAnchor {
        /// Durable marker height.
        marker_number: u64,
        /// Trusted anchor height.
        anchor_number: u64,
    },
    /// A marker at the anchor height did not bind the exact trusted anchor hash.
    #[error(
        "Telos finalized coverage at anchor height binds {marker_hash}, expected {anchor_hash}"
    )]
    FinalizedCoverageAnchorMismatch {
        /// Hash stored in finalized coverage.
        marker_hash: B256,
        /// Trusted anchor hash.
        anchor_hash: B256,
    },
    /// Existing coverage is ahead of Reth's durable canonical commit point.
    #[error(
        "Telos finalized coverage block {coverage_number} is above Reth persisted canonical tip {persisted_number}"
    )]
    FinalizedCoverageAbovePersistedTip {
        /// Existing sidecar coverage height.
        coverage_number: u64,
        /// Durable Reth `Finish` checkpoint height.
        persisted_number: u64,
    },
    /// The exact hash selected for coverage was not present in Reth's committed canonical index.
    #[error(
        "Reth persisted canonical proof is missing for Telos block {block_number} ({block_hash}); header index maps it to {indexed_number:?}"
    )]
    PersistedCanonicalBlockMissing {
        /// Coverage height being committed.
        block_number: u64,
        /// Exact accepted sidecar hash required at that height.
        block_hash: B256,
        /// Height recorded by Reth's hash/number index, if any.
        indexed_number: Option<u64>,
    },
    /// Finality referenced a block for which no sidecar record exists.
    #[error("finalized Telos sidecar {block_hash} is missing")]
    FinalizedSidecarMissing {
        /// Finalized block hash.
        block_hash: B256,
    },
    /// Finality referenced metadata not accepted by Engine validation.
    #[error("finalized Telos sidecar {block_hash} is {state:?}, not accepted")]
    FinalizedSidecarNotAccepted {
        /// Finalized block hash.
        block_hash: B256,
        /// Durable lifecycle state.
        state: TelosSidecarState,
    },
    /// A forkchoice update attempted to move finalized coverage backwards.
    #[error("Telos finalized coverage regression from block {current_number} to {new_number}")]
    FinalizedRegression {
        /// Current durable finalized height.
        current_number: u64,
        /// Requested finalized height.
        new_number: u64,
    },
    /// A forkchoice update supplied a different finalized hash at an already covered height.
    #[error(
        "Telos finalized coverage conflict at block {block_number}: current {current_hash}, requested {new_hash}"
    )]
    FinalizedConflict {
        /// Conflicting height.
        block_number: u64,
        /// Durable finalized hash.
        current_hash: B256,
        /// Requested finalized hash.
        new_hash: B256,
    },
    /// The accepted chain between the old and new coverage markers was not contiguous.
    #[error(
        "Telos finalized chain gap at {block_hash}: expected block {expected_number}, got {actual_number}"
    )]
    FinalizedChainGap {
        /// Sidecar at the discontinuity.
        block_hash: B256,
        /// Required height.
        expected_number: u64,
        /// Sidecar height.
        actual_number: u64,
    },
    /// The new finalized ancestry did not reach the prior durable marker.
    #[error(
        "Telos finalized ancestry reached {actual_hash}, expected prior marker {expected_hash}"
    )]
    FinalizedAncestryMismatch {
        /// Required prior marker hash.
        expected_hash: B256,
        /// Hash reached after walking the new finalized ancestry.
        actual_hash: B256,
    },
    /// Pruning encountered a record protected as part of the finalized canonical segment.
    #[error("refusing to prune canonical Telos sidecar {block_hash}")]
    CanonicalPruneAttempt {
        /// Protected canonical block hash.
        block_hash: B256,
    },
    /// A new candidate was submitted at or below durable finality.
    #[error(
        "Telos sidecar candidate block {block_number} ({block_hash}) is at or below finalized coverage block {finalized_number} ({finalized_hash})"
    )]
    CandidateBelowFinalizedCoverage {
        /// Candidate height.
        block_number: u64,
        /// Candidate hash.
        block_hash: B256,
        /// Durable finalized height.
        finalized_number: u64,
        /// Durable finalized hash.
        finalized_hash: B256,
    },
    /// A direct child of the durable coverage boundary referenced a different parent.
    #[error(
        "Telos sidecar block {block_number} ({block_hash}) extends {actual_parent}, but durable coverage requires parent {expected_parent}"
    )]
    CandidateCoverageParentMismatch {
        /// Candidate height.
        block_number: u64,
        /// Candidate hash.
        block_hash: B256,
        /// Parent required by the anchor or finalized marker.
        expected_parent: B256,
        /// Parent committed by the candidate.
        actual_parent: B256,
    },
    /// Prune visibility counters exceeded their fixed-width representation.
    #[error("Telos sidecar prune visibility counter overflow")]
    PruneCounterOverflow,
    /// A secondary index entry has no primary record.
    #[error("Telos sidecar index at block {block_number} references missing primary {block_hash}")]
    MissingPrimary {
        /// Indexed block number.
        block_number: u64,
        /// Missing primary key.
        block_hash: B256,
    },
    /// An in-memory lock was poisoned by a prior panic.
    #[error("Telos in-memory sidecar store lock is poisoned")]
    LockPoisoned,
    /// Reth database operation failed.
    #[error("Telos sidecar database error: {0}")]
    Database(String),
}

#[derive(Debug, Default)]
struct InMemoryState {
    by_hash: HashMap<B256, TelosStoredSidecar>,
    by_number: BTreeMap<u64, BTreeSet<B256>>,
    by_parent: HashMap<B256, BTreeSet<B256>>,
    finalized: Option<TelosFinalizedCoverage>,
    persisted_canonical: BTreeMap<u64, B256>,
}

#[derive(Clone, Copy, Debug)]
enum TelosSidecarTableInfo {
    Primary,
    NumberHash,
    ParentHash,
    FinalizedCoverage,
}

impl TableInfo for TelosSidecarTableInfo {
    fn name(&self) -> &'static str {
        match self {
            Self::Primary => TelosExecutionSidecars::NAME,
            Self::NumberHash => TelosExecutionSidecarsByNumberHash::NAME,
            Self::ParentHash => TelosExecutionSidecarsByParentHash::NAME,
            Self::FinalizedCoverage => TelosSidecarFinalizedCoverage::NAME,
        }
    }

    fn is_dupsort(&self) -> bool {
        false
    }
}

fn validate_envelope(envelope: &TelosExecutionSidecarEnvelope) -> Result<(), TelosSidecarError> {
    if envelope.version != TELOS_EXECUTION_SIDECAR_VERSION {
        return Err(TelosSidecarError::UnsupportedVersion {
            expected: TELOS_EXECUTION_SIDECAR_VERSION,
            actual: envelope.version,
        })
    }
    let transaction_count = usize::try_from(envelope.transaction_count)
        .map_err(|_| TelosSidecarError::TransactionCountOverflow(envelope.transaction_count))?;
    let execution = envelope.extra_fields.execution.as_ref().ok_or_else(|| {
        TelosSidecarError::Validation("execution metadata is missing".to_string())
    })?;
    validate_extra_fields_for_payload(
        &envelope.extra_fields,
        transaction_count,
        envelope.gas_used,
        execution.execution_base_fee,
        envelope.block_hash,
        envelope.parent_hash,
    )
    .map_err(|error| TelosSidecarError::Validation(error.to_string()))?;

    // Keep the two independently versioned protocol layers explicit in the canonical record.
    if execution.version != TELOS_EXECUTION_METADATA_VERSION {
        return Err(TelosSidecarError::Validation(format!(
            "execution metadata version is {}, expected {}",
            execution.version, TELOS_EXECUTION_METADATA_VERSION
        )))
    }
    Ok(())
}

fn canonicalize_extra_fields(
    fields: &mut TelosEngineApiExtraFields,
) -> Result<(), TelosSidecarError> {
    if let Some(accounts) = &mut fields.statediffs_account {
        accounts.sort_by_key(|row| row.address);
    }
    if let Some(storage) = &mut fields.statediffs_accountstate {
        storage.sort_by_key(|row| (row.address, row.key));
    }
    if let Some(creates) = &mut fields.new_addresses_using_create {
        creates.sort_unstable();
    }
    if let Some(wallets) = &mut fields.new_addresses_using_openwallet {
        wallets.sort_unstable();
    }
    if let Some(receipts) = &mut fields.receipts {
        for receipt in receipts {
            receipt.tx_type = TelosReceiptType::Number(canonical_receipt_type(&receipt.tx_type)?);
        }
    }
    Ok(())
}

fn canonical_receipt_type(receipt_type: &TelosReceiptType) -> Result<u8, TelosSidecarError> {
    let value = match receipt_type {
        TelosReceiptType::Number(value) => *value,
        TelosReceiptType::Name(name) => match name.as_str() {
            "Legacy" | "legacy" | "0x0" | "0" => 0,
            "Eip2930" | "eip2930" | "0x1" | "1" => 1,
            "Eip1559" | "eip1559" | "0x2" | "2" => 2,
            "Eip4844" | "eip4844" | "0x3" | "3" => 3,
            "Eip7702" | "eip7702" | "0x4" | "4" => 4,
            _ => {
                return Err(TelosSidecarError::Validation(format!(
                    "unsupported receipt transaction type `{name}`"
                )))
            }
        },
    };
    Ok(value)
}

const fn check_size(actual: usize) -> Result<(), TelosSidecarError> {
    if actual > MAX_TELOS_EXECUTION_SIDECAR_BYTES {
        return Err(TelosSidecarError::TooLarge {
            actual,
            maximum: MAX_TELOS_EXECUTION_SIDECAR_BYTES,
        })
    }
    Ok(())
}

fn ensure_chain(
    expected: TelosChainIdentity,
    actual: TelosChainIdentity,
) -> Result<(), TelosSidecarError> {
    if expected != actual {
        return Err(TelosSidecarError::ChainMismatch { expected, actual })
    }
    Ok(())
}

fn compare_existing_for_pending(
    existing: &TelosStoredSidecar,
    incoming: &TelosExecutionSidecar,
) -> Result<TelosSidecarPutOutcome, TelosSidecarError> {
    if existing.sidecar.digest == incoming.digest &&
        existing.sidecar.canonical_bytes == incoming.canonical_bytes
    {
        return Ok(match existing.state {
            TelosSidecarState::Pending => TelosSidecarPutOutcome::AlreadyPending,
            TelosSidecarState::Dispatched => TelosSidecarPutOutcome::AlreadyDispatched,
            TelosSidecarState::Accepted => TelosSidecarPutOutcome::AlreadyAccepted,
        })
    }
    if existing.state == TelosSidecarState::Pending {
        return Ok(TelosSidecarPutOutcome::ReplacedPending)
    }

    match existing.state {
        TelosSidecarState::Pending => unreachable!("pending replacement returned above"),
        TelosSidecarState::Dispatched => Err(TelosSidecarError::CandidateInFlight {
            block_hash: incoming.envelope.block_hash,
            dispatched_digest: existing.sidecar.digest,
            requested_digest: incoming.digest,
        }),
        TelosSidecarState::Accepted => Err(TelosSidecarError::AcceptedImmutable {
            block_hash: incoming.envelope.block_hash,
            accepted_digest: existing.sidecar.digest,
            requested_digest: incoming.digest,
        }),
    }
}

fn ensure_candidate_digest(
    existing: &TelosStoredSidecar,
    block_hash: B256,
    requested_digest: B256,
) -> Result<(), TelosSidecarError> {
    if existing.sidecar.digest != requested_digest {
        return Err(TelosSidecarError::CandidateDigestMismatch {
            block_hash,
            requested_digest,
            current_digest: existing.sidecar.digest,
        })
    }
    Ok(())
}

fn ensure_unaccepted(existing: &TelosStoredSidecar) -> Result<(), TelosSidecarError> {
    if existing.state == TelosSidecarState::Accepted {
        return Err(TelosSidecarError::AcceptedImmutable {
            block_hash: existing.sidecar.envelope.block_hash,
            accepted_digest: existing.sidecar.digest,
            requested_digest: existing.sidecar.digest,
        })
    }
    Ok(())
}

const fn ensure_candidate_above_finalized_coverage(
    sidecar: &TelosExecutionSidecar,
    finalized: Option<TelosFinalizedCoverage>,
) -> Result<(), TelosSidecarError> {
    if let Some(finalized) = finalized &&
        sidecar.envelope.block_number <= finalized.block_number
    {
        return Err(TelosSidecarError::CandidateBelowFinalizedCoverage {
            block_number: sidecar.envelope.block_number,
            block_hash: sidecar.envelope.block_hash,
            finalized_number: finalized.block_number,
            finalized_hash: finalized.block_hash,
        })
    }
    Ok(())
}

fn ensure_candidate_extends_coverage(
    sidecar: &TelosExecutionSidecar,
    coverage: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    ensure_candidate_above_finalized_coverage(sidecar, Some(coverage))?;
    if coverage.block_number.checked_add(1) == Some(sidecar.envelope.block_number) &&
        sidecar.envelope.parent_hash != coverage.block_hash
    {
        return Err(TelosSidecarError::CandidateCoverageParentMismatch {
            block_number: sidecar.envelope.block_number,
            block_hash: sidecar.envelope.block_hash,
            expected_parent: coverage.block_hash,
            actual_parent: sidecar.envelope.parent_hash,
        })
    }
    Ok(())
}

fn validate_sidecar_against_parent(
    anchor: &TelosExecutionAnchor,
    child: &TelosExecutionSidecar,
    parent: Option<&TelosExecutionSidecar>,
) -> Result<(), TelosSidecarError> {
    let expected = if let Some(parent) = parent {
        let expected_number = parent.envelope.block_number.checked_add(1).ok_or(
            TelosSidecarError::ParentBlockNumberOverflow {
                parent_block_hash: parent.envelope.block_hash,
            },
        )?;
        if child.envelope.block_number != expected_number {
            return Err(TelosSidecarError::NonSequentialBlock {
                parent_block_hash: parent.envelope.block_hash,
                expected: expected_number,
                actual: child.envelope.block_number,
            })
        }
        terminal_execution_context(parent)?
    } else if child.envelope.parent_hash == anchor.parent_block_hash &&
        child.envelope.block_number == anchor.parent_block_number + 1
    {
        (anchor.starting_gas_price, anchor.starting_revision)
    } else {
        return Err(TelosSidecarError::MissingParentSidecar {
            block_number: child.envelope.block_number,
            block_hash: child.envelope.block_hash,
            parent_hash: child.envelope.parent_hash,
        })
    };
    validate_starting_execution_context(child, expected)
}

fn validate_sidecar_ingress_in_memory(
    state: &InMemoryState,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    child: &TelosExecutionSidecar,
) -> Result<(), TelosSidecarError> {
    anchor.validate_for_chain(chain)?;
    ensure_chain(chain, child.envelope.chain)?;
    let coverage = current_coverage_in_memory(state, anchor)?;
    ensure_candidate_extends_coverage(child, coverage)?;
    let parent = state.by_hash.get(&child.envelope.parent_hash);
    if let Some(parent) = parent {
        validate_in_memory_index(state, child.envelope.parent_hash, parent)?;
    }
    let visible_parent = parent
        .filter(|record| record.state != TelosSidecarState::Pending)
        .map(|record| &record.sidecar);
    validate_sidecar_against_parent(anchor, child, visible_parent)
}

fn write_dispatched_in_memory(
    state: &mut InMemoryState,
    sidecar: &TelosExecutionSidecar,
    existing: Option<&TelosStoredSidecar>,
) -> Result<(), TelosSidecarError> {
    let block_hash = sidecar.envelope.block_hash;
    let block_number = sidecar.envelope.block_number;
    let parent_hash = sidecar.envelope.parent_hash;
    if let Some(existing) = existing {
        debug_assert_eq!(existing.state, TelosSidecarState::Pending);
        validate_in_memory_index(state, block_hash, existing)?;
        let old_number = existing.sidecar.envelope.block_number;
        let old_parent = existing.sidecar.envelope.parent_hash;
        if old_number != block_number &&
            state.by_number.get(&block_number).is_some_and(|hashes| hashes.contains(&block_hash))
        {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        if old_parent != parent_hash &&
            state.by_parent.get(&parent_hash).is_some_and(|hashes| hashes.contains(&block_hash))
        {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }

        // Every operation below is infallible after the complete index preflight above.
        if old_number != block_number {
            let old_height = state.by_number.get_mut(&old_number).expect("index preflight");
            let removed = old_height.remove(&block_hash);
            debug_assert!(removed);
            if old_height.is_empty() {
                state.by_number.remove(&old_number);
            }
            state.by_number.entry(block_number).or_default().insert(block_hash);
        }
        if old_parent != parent_hash {
            let old_children = state.by_parent.get_mut(&old_parent).expect("index preflight");
            let removed = old_children.remove(&block_hash);
            debug_assert!(removed);
            if old_children.is_empty() {
                state.by_parent.remove(&old_parent);
            }
            state.by_parent.entry(parent_hash).or_default().insert(block_hash);
        }
    } else {
        if state.by_number.get(&block_number).is_some_and(|hashes| hashes.contains(&block_hash)) {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        if state.by_parent.get(&parent_hash).is_some_and(|hashes| hashes.contains(&block_hash)) {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        state.by_number.entry(block_number).or_default().insert(block_hash);
        state.by_parent.entry(parent_hash).or_default().insert(block_hash);
    }

    let previous = state.by_hash.insert(
        block_hash,
        TelosStoredSidecar { sidecar: sidecar.clone(), state: TelosSidecarState::Dispatched },
    );
    debug_assert_eq!(previous.as_ref(), existing);
    Ok(())
}

fn validate_in_memory_index(
    state: &InMemoryState,
    block_hash: B256,
    record: &TelosStoredSidecar,
) -> Result<(), TelosSidecarError> {
    if record.sidecar.envelope.block_hash != block_hash {
        return Err(TelosSidecarError::PrimaryKeyMismatch {
            expected: block_hash,
            actual: record.sidecar.envelope.block_hash,
        })
    }
    let block_number = record.sidecar.envelope.block_number;
    if !state.by_number.get(&block_number).is_some_and(|hashes| hashes.contains(&block_hash)) {
        return Err(TelosSidecarError::CorruptIndex { block_number, block_hash, indexed_hash: None })
    }
    let parent_hash = record.sidecar.envelope.parent_hash;
    if !state.by_parent.get(&parent_hash).is_some_and(|hashes| hashes.contains(&block_hash)) {
        return Err(TelosSidecarError::CorruptParentIndex {
            parent_hash,
            block_hash,
            indexed_hash: None,
        })
    }
    Ok(())
}

fn collect_unaccepted_subtree_in_memory(
    state: &InMemoryState,
    root: B256,
) -> Result<Vec<B256>, TelosSidecarError> {
    let mut stack = vec![(root, root)];
    let mut seen = BTreeSet::new();
    let mut removals = Vec::new();
    while let Some((parent_hash, block_hash)) = stack.pop() {
        if !seen.insert(block_hash) {
            return Err(TelosSidecarError::CyclicParentIndex(block_hash))
        }
        let record = state
            .by_hash
            .get(&block_hash)
            .ok_or(TelosSidecarError::MissingChildPrimary { parent_hash, block_hash })?;
        if block_hash != root && record.sidecar.envelope.parent_hash != parent_hash {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        validate_in_memory_index(state, block_hash, record)?;
        if record.state == TelosSidecarState::Accepted {
            return Err(TelosSidecarError::AcceptedDescendant {
                invalid_root: root,
                accepted_descendant: block_hash,
            })
        }
        removals.push(block_hash);
        if let Some(children) = state.by_parent.get(&block_hash) {
            stack.extend(children.iter().rev().map(|child| (block_hash, *child)));
        }
    }
    Ok(removals)
}

fn validate_forkchoice_state_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
    head_block_hash: B256,
    safe_block_hash: B256,
    finalized_block_hash: B256,
) -> Result<(), TelosSidecarError> {
    if head_block_hash == B256::ZERO {
        return Err(TelosSidecarError::ZeroForkchoiceHead)
    }

    let current_finalized = current_coverage_in_memory(state, anchor)?;
    let head = resolve_forkchoice_block_in_memory(state, anchor, "head", head_block_hash)?;
    let requested_finalized = if finalized_block_hash == B256::ZERO {
        current_finalized
    } else {
        let requested =
            resolve_forkchoice_block_in_memory(state, anchor, "finalized", finalized_block_hash)?;
        validate_coverage_advancement(current_finalized, requested)?;
        validate_forkchoice_ancestry_in_memory(state, anchor, current_finalized, requested)?;
        requested
    };

    validate_forkchoice_ancestry_in_memory(state, anchor, requested_finalized, head)?;
    if safe_block_hash != B256::ZERO {
        let safe = resolve_forkchoice_block_in_memory(state, anchor, "safe", safe_block_hash)?;
        validate_forkchoice_ancestry_in_memory(state, anchor, requested_finalized, safe)?;
        validate_forkchoice_ancestry_in_memory(state, anchor, safe, head)?;
    }
    Ok(())
}

fn resolve_forkchoice_block_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
    role: &'static str,
    block_hash: B256,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    if block_hash == anchor.parent_block_hash {
        return Ok(anchor_coverage(anchor))
    }
    let record = state
        .by_hash
        .get(&block_hash)
        .ok_or(TelosSidecarError::ForkchoiceSidecarMissing { role, block_hash })?;
    validate_in_memory_index(state, block_hash, record)?;
    validate_forkchoice_record(role, block_hash, record)
}

fn validate_forkchoice_ancestry_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
    ancestor: TelosFinalizedCoverage,
    descendant: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    let mut current = descendant;
    while current.block_number > ancestor.block_number {
        let child = state.by_hash.get(&current.block_hash).ok_or(
            TelosSidecarError::ForkchoiceSidecarMissing {
                role: "ancestry",
                block_hash: current.block_hash,
            },
        )?;
        validate_in_memory_index(state, current.block_hash, child)?;
        let actual_number = child.sidecar.envelope.block_number;
        if actual_number != current.block_number {
            return Err(TelosSidecarError::ForkchoiceChainGap {
                block_hash: current.block_hash,
                expected_number: current.block_number,
                actual_number,
            })
        }
        if child.state != TelosSidecarState::Accepted {
            return Err(TelosSidecarError::ForkchoiceSidecarNotAccepted {
                role: "ancestry",
                block_hash: current.block_hash,
                state: child.state,
            })
        }

        let expected_parent_number = current.block_number - 1;
        let parent_hash = child.sidecar.envelope.parent_hash;
        let parent = if parent_hash == anchor.parent_block_hash {
            validate_sidecar_against_parent(anchor, &child.sidecar, None)?;
            anchor_coverage(anchor)
        } else {
            let parent_record = state.by_hash.get(&parent_hash).ok_or(
                TelosSidecarError::ForkchoiceSidecarMissing {
                    role: "ancestry",
                    block_hash: parent_hash,
                },
            )?;
            validate_in_memory_index(state, parent_hash, parent_record)?;
            let parent = validate_forkchoice_record("ancestry", parent_hash, parent_record)?;
            validate_sidecar_against_parent(anchor, &child.sidecar, Some(&parent_record.sidecar))?;
            parent
        };
        if parent.block_number != expected_parent_number {
            return Err(TelosSidecarError::ForkchoiceChainGap {
                block_hash: parent.block_hash,
                expected_number: expected_parent_number,
                actual_number: parent.block_number,
            })
        }
        current = parent;
    }

    validate_forkchoice_ancestor_match(ancestor, descendant, current)
}

fn validate_forkchoice_record(
    role: &'static str,
    block_hash: B256,
    record: &TelosStoredSidecar,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    if record.state != TelosSidecarState::Accepted {
        return Err(TelosSidecarError::ForkchoiceSidecarNotAccepted {
            role,
            block_hash,
            state: record.state,
        })
    }
    Ok(TelosFinalizedCoverage { block_number: record.sidecar.envelope.block_number, block_hash })
}

fn validate_forkchoice_ancestor_match(
    ancestor: TelosFinalizedCoverage,
    descendant: TelosFinalizedCoverage,
    actual: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    if actual != ancestor {
        return Err(TelosSidecarError::ForkchoiceAncestryMismatch {
            descendant_hash: descendant.block_hash,
            expected_ancestor: ancestor.block_hash,
            actual_ancestor: actual.block_hash,
        })
    }
    Ok(())
}

fn finalize_and_prune_in_memory(
    state: &mut InMemoryState,
    anchor: &TelosExecutionAnchor,
    finalized_hash: B256,
) -> Result<TelosSidecarPruneOutcome, TelosSidecarError> {
    if finalized_hash == B256::ZERO {
        return Err(TelosSidecarError::ZeroFinalizedHash)
    }

    let had_marker = state.finalized.is_some();
    let current = current_coverage_in_memory(state, anchor)?;
    let requested = resolve_finalized_in_memory(state, anchor, finalized_hash)?;
    validate_coverage_advancement(current, requested)?;
    let mut canonical = canonical_segment_in_memory(state, current, requested)?;
    let persisted_number = state
        .persisted_canonical
        .last_key_value()
        .map_or(anchor.parent_block_number, |(number, _)| {
            (*number).max(anchor.parent_block_number)
        });
    let finalized = persistence_bounded_coverage(current, requested, persisted_number, &canonical)?;
    ensure_persisted_canonical_in_memory(state, anchor, finalized)?;
    canonical.retain(|number, _| *number <= finalized.block_number);
    if current == finalized {
        if !had_marker {
            state.finalized = Some(finalized);
        }
        return Ok(TelosSidecarPruneOutcome {
            finalized,
            removed_records: 0,
            removed_bytes: 0,
            retained_canonical_records: 0,
        })
    }

    validate_canonical_execution_context_in_memory(state, anchor, current, &canonical)?;
    let mut protected = canonical.values().copied().collect::<BTreeSet<_>>();
    if current.block_number > anchor.parent_block_number {
        protected.insert(current.block_hash);
    }
    let mut removals = BTreeMap::new();
    for (number, canonical_hash) in &canonical {
        let hashes =
            state.by_number.get(number).cloned().ok_or(TelosSidecarError::MissingPrimary {
                block_number: *number,
                block_hash: *canonical_hash,
            })?;
        for hash in hashes {
            if hash != *canonical_hash && !removals.contains_key(&hash) {
                collect_prunable_subtree_in_memory(state, hash, &protected, &mut removals)?;
            }
        }
    }

    let removed_records =
        u64::try_from(removals.len()).map_err(|_| TelosSidecarError::PruneCounterOverflow)?;
    let removed_bytes = removals.values().try_fold(0u64, |total, record| {
        let bytes = u64::try_from(record.encode_record().len())
            .map_err(|_| TelosSidecarError::PruneCounterOverflow)?;
        total.checked_add(bytes).ok_or(TelosSidecarError::PruneCounterOverflow)
    })?;
    let retained_canonical_records =
        u64::try_from(canonical.len()).map_err(|_| TelosSidecarError::PruneCounterOverflow)?;

    for (block_hash, expected) in removals {
        let record =
            state.by_hash.remove(&block_hash).expect("prune primary validated before mutation");
        debug_assert_eq!(record, expected);
        let block_number = record.sidecar.envelope.block_number;
        let parent_hash = record.sidecar.envelope.parent_hash;
        let hashes = state
            .by_number
            .get_mut(&block_number)
            .expect("prune number index validated before mutation");
        let removed = hashes.remove(&block_hash);
        debug_assert!(removed);
        if hashes.is_empty() {
            state.by_number.remove(&block_number);
        }
        let children = state
            .by_parent
            .get_mut(&parent_hash)
            .expect("prune parent index validated before mutation");
        let removed = children.remove(&block_hash);
        debug_assert!(removed);
        if children.is_empty() {
            state.by_parent.remove(&parent_hash);
        }
    }
    state.finalized = Some(finalized);

    Ok(TelosSidecarPruneOutcome {
        finalized,
        removed_records,
        removed_bytes,
        retained_canonical_records,
    })
}

fn current_coverage_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    let Some(marker) = state.finalized else { return Ok(anchor_coverage(anchor)) };
    validate_coverage_marker(marker, anchor)?;
    if marker.block_number > anchor.parent_block_number {
        let record = state
            .by_hash
            .get(&marker.block_hash)
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash: marker.block_hash })?;
        validate_in_memory_index(state, marker.block_hash, record)?;
        validate_finalized_record(marker, record)?;
    }
    Ok(marker)
}

fn resolve_finalized_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
    block_hash: B256,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    if block_hash == anchor.parent_block_hash {
        return Ok(anchor_coverage(anchor))
    }
    let record = state
        .by_hash
        .get(&block_hash)
        .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash })?;
    validate_in_memory_index(state, block_hash, record)?;
    let coverage =
        TelosFinalizedCoverage { block_number: record.sidecar.envelope.block_number, block_hash };
    validate_finalized_record(coverage, record)?;
    Ok(coverage)
}

fn ensure_persisted_canonical_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
    finalized: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    if finalized == anchor_coverage(anchor) {
        return Ok(())
    }
    let indexed_hash = state.persisted_canonical.get(&finalized.block_number).copied();
    if indexed_hash != Some(finalized.block_hash) {
        return Err(TelosSidecarError::PersistedCanonicalBlockMissing {
            block_number: finalized.block_number,
            block_hash: finalized.block_hash,
            indexed_number: None,
        })
    }
    Ok(())
}

fn canonical_segment_in_memory(
    state: &InMemoryState,
    current: TelosFinalizedCoverage,
    finalized: TelosFinalizedCoverage,
) -> Result<BTreeMap<u64, B256>, TelosSidecarError> {
    let mut expected_number = finalized.block_number;
    let mut block_hash = finalized.block_hash;
    let mut seen = BTreeSet::new();
    let mut canonical = BTreeMap::new();
    while expected_number > current.block_number {
        if !seen.insert(block_hash) {
            return Err(TelosSidecarError::CyclicParentIndex(block_hash))
        }
        let record = state
            .by_hash
            .get(&block_hash)
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash })?;
        validate_in_memory_index(state, block_hash, record)?;
        validate_finalized_record(
            TelosFinalizedCoverage { block_number: expected_number, block_hash },
            record,
        )?;
        canonical.insert(expected_number, block_hash);
        block_hash = record.sidecar.envelope.parent_hash;
        expected_number -= 1;
    }
    if block_hash != current.block_hash {
        return Err(TelosSidecarError::FinalizedAncestryMismatch {
            expected_hash: current.block_hash,
            actual_hash: block_hash,
        })
    }
    Ok(canonical)
}

fn validate_canonical_execution_context_in_memory(
    state: &InMemoryState,
    anchor: &TelosExecutionAnchor,
    current: TelosFinalizedCoverage,
    canonical: &BTreeMap<u64, B256>,
) -> Result<(), TelosSidecarError> {
    let mut expected = if current.block_number == anchor.parent_block_number {
        (anchor.starting_gas_price, anchor.starting_revision)
    } else {
        let current_record = state
            .by_hash
            .get(&current.block_hash)
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash: current.block_hash })?;
        terminal_execution_context(&current_record.sidecar)?
    };
    for block_hash in canonical.values() {
        let record = state
            .by_hash
            .get(block_hash)
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash: *block_hash })?;
        validate_starting_execution_context(&record.sidecar, expected)?;
        expected = terminal_execution_context(&record.sidecar)?;
    }
    Ok(())
}

fn collect_prunable_subtree_in_memory(
    state: &InMemoryState,
    root: B256,
    protected: &BTreeSet<B256>,
    removals: &mut BTreeMap<B256, TelosStoredSidecar>,
) -> Result<(), TelosSidecarError> {
    let mut stack = vec![(root, root)];
    let mut seen = BTreeSet::new();
    while let Some((parent_hash, block_hash)) = stack.pop() {
        if !seen.insert(block_hash) {
            return Err(TelosSidecarError::CyclicParentIndex(block_hash))
        }
        let record = state
            .by_hash
            .get(&block_hash)
            .ok_or(TelosSidecarError::MissingChildPrimary { parent_hash, block_hash })?;
        if block_hash != root && record.sidecar.envelope.parent_hash != parent_hash {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        validate_in_memory_index(state, block_hash, record)?;
        if protected.contains(&block_hash) {
            return Err(TelosSidecarError::CanonicalPruneAttempt { block_hash })
        }
        if removals.contains_key(&block_hash) {
            continue
        }
        removals.insert(block_hash, record.clone());
        if let Some(children) = state.by_parent.get(&block_hash) {
            stack.extend(children.iter().rev().map(|child| (block_hash, *child)));
        }
    }
    Ok(())
}

const fn anchor_coverage(anchor: &TelosExecutionAnchor) -> TelosFinalizedCoverage {
    TelosFinalizedCoverage {
        block_number: anchor.parent_block_number,
        block_hash: anchor.parent_block_hash,
    }
}

fn validate_coverage_marker(
    marker: TelosFinalizedCoverage,
    anchor: &TelosExecutionAnchor,
) -> Result<(), TelosSidecarError> {
    if marker.block_number < anchor.parent_block_number {
        return Err(TelosSidecarError::FinalizedCoverageBeforeAnchor {
            marker_number: marker.block_number,
            anchor_number: anchor.parent_block_number,
        })
    }
    if marker.block_number == anchor.parent_block_number &&
        marker.block_hash != anchor.parent_block_hash
    {
        return Err(TelosSidecarError::FinalizedCoverageAnchorMismatch {
            marker_hash: marker.block_hash,
            anchor_hash: anchor.parent_block_hash,
        })
    }
    Ok(())
}

fn validate_finalized_record(
    expected: TelosFinalizedCoverage,
    record: &TelosStoredSidecar,
) -> Result<(), TelosSidecarError> {
    let actual_number = record.sidecar.envelope.block_number;
    if actual_number != expected.block_number {
        return Err(TelosSidecarError::FinalizedChainGap {
            block_hash: expected.block_hash,
            expected_number: expected.block_number,
            actual_number,
        })
    }
    if record.state != TelosSidecarState::Accepted {
        return Err(TelosSidecarError::FinalizedSidecarNotAccepted {
            block_hash: expected.block_hash,
            state: record.state,
        })
    }
    Ok(())
}

fn validate_coverage_advancement(
    current: TelosFinalizedCoverage,
    finalized: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    if finalized.block_number < current.block_number {
        return Err(TelosSidecarError::FinalizedRegression {
            current_number: current.block_number,
            new_number: finalized.block_number,
        })
    }
    if finalized.block_number == current.block_number && finalized.block_hash != current.block_hash
    {
        return Err(TelosSidecarError::FinalizedConflict {
            block_number: current.block_number,
            current_hash: current.block_hash,
            new_hash: finalized.block_hash,
        })
    }
    Ok(())
}

fn persistence_bounded_coverage(
    current: TelosFinalizedCoverage,
    requested: TelosFinalizedCoverage,
    persisted_number: u64,
    requested_segment: &BTreeMap<u64, B256>,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    if persisted_number < current.block_number {
        return Err(TelosSidecarError::FinalizedCoverageAbovePersistedTip {
            coverage_number: current.block_number,
            persisted_number,
        })
    }
    let target_number = requested.block_number.min(persisted_number);
    if target_number == current.block_number {
        return Ok(current)
    }
    let block_hash = requested_segment.get(&target_number).copied().ok_or(
        TelosSidecarError::FinalizedChainGap {
            block_hash: requested.block_hash,
            expected_number: target_number,
            actual_number: requested.block_number,
        },
    )?;
    Ok(TelosFinalizedCoverage { block_number: target_number, block_hash })
}

fn validate_starting_execution_context(
    sidecar: &TelosExecutionSidecar,
    expected: (U256, u64),
) -> Result<(), TelosSidecarError> {
    let execution = sidecar.envelope.extra_fields.execution.as_ref().ok_or_else(|| {
        TelosSidecarError::Validation("execution metadata is missing".to_string())
    })?;
    if execution.starting_gas_price != expected.0 {
        return Err(TelosSidecarError::GasPriceContinuity {
            block_hash: sidecar.envelope.block_hash,
            expected: expected.0,
            actual: execution.starting_gas_price,
        })
    }
    if execution.starting_revision != expected.1 {
        return Err(TelosSidecarError::RevisionContinuity {
            block_hash: sidecar.envelope.block_hash,
            expected: expected.1,
            actual: execution.starting_revision,
        })
    }
    Ok(())
}

fn terminal_execution_context(
    sidecar: &TelosExecutionSidecar,
) -> Result<(U256, u64), TelosSidecarError> {
    let execution = sidecar.envelope.extra_fields.execution.as_ref().ok_or_else(|| {
        TelosSidecarError::Validation("execution metadata is missing".to_string())
    })?;
    let gas_price = execution
        .gas_price_changes
        .last()
        .map_or(execution.starting_gas_price, |change| change.value);
    let revision = execution
        .revision_changes
        .last()
        .map_or(execution.starting_revision, |change| change.value);
    Ok((gas_price, revision))
}

fn validate_and_mark_dispatched_with_transaction<TX: DbTx + DbTxMut>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    sidecar: &TelosExecutionSidecar,
) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
    anchor.validate_for_chain(chain)?;
    ensure_chain(chain, sidecar.envelope.chain)?;
    let block_hash = sidecar.envelope.block_hash;

    if let Some(existing) = get_record_by_hash_from_transaction(tx, chain, block_hash)? {
        let outcome = compare_existing_for_pending(&existing, sidecar)?;
        if outcome == TelosSidecarPutOutcome::AlreadyAccepted {
            return Ok(TelosSidecarDispatchOutcome::AlreadyAccepted)
        }
    }

    let stored_coverage = finalized_coverage_from_transaction(tx)?;
    let coverage = current_coverage_from_transaction(tx, chain, anchor, stored_coverage)?;
    ensure_candidate_extends_coverage(sidecar, coverage)?;
    let parent = get_record_by_hash_from_transaction(tx, chain, sidecar.envelope.parent_hash)?;
    let visible_parent = parent
        .as_ref()
        .filter(|record| record.state != TelosSidecarState::Pending)
        .map(|record| &record.sidecar);
    validate_sidecar_against_parent(anchor, sidecar, visible_parent)?;

    // Both helpers operate on this same write transaction. Any validation, index, or compare-and-
    // set failure drops the transaction and exposes neither a pending row nor partial indexes.
    put_pending_with_transaction(tx, chain, sidecar)?;
    mark_dispatched_with_transaction(tx, chain, block_hash, sidecar.digest)
}

fn put_pending_with_transaction<TX: DbTx + DbTxMut>(
    tx: &TX,
    chain: TelosChainIdentity,
    sidecar: &TelosExecutionSidecar,
) -> Result<TelosSidecarPutOutcome, TelosSidecarError> {
    ensure_chain(chain, sidecar.envelope.chain)?;
    let block_hash = sidecar.envelope.block_hash;
    let block_number = sidecar.envelope.block_number;
    let parent_hash = sidecar.envelope.parent_hash;
    let index_key = TelosSidecarNumberHashKey::new(block_number, block_hash);
    let parent_index_key = TelosSidecarParentHashKey::new(parent_hash, block_hash);

    if let Some(existing_record) =
        tx.get::<TelosExecutionSidecars>(block_hash).map_err(database_error)?
    {
        let existing = TelosStoredSidecar::decode_record(&existing_record, chain)?;
        validate_record_index(tx, block_hash, &existing)?;
        let outcome = compare_existing_for_pending(&existing, sidecar)?;
        if outcome != TelosSidecarPutOutcome::ReplacedPending {
            return Ok(outcome)
        }
        ensure_candidate_above_finalized_coverage(
            sidecar,
            finalized_coverage_from_transaction(tx)?,
        )?;

        let old_number = existing.sidecar.envelope.block_number;
        let old_parent = existing.sidecar.envelope.parent_hash;
        if old_number != block_number {
            match tx.get::<TelosExecutionSidecarsByNumberHash>(index_key).map_err(database_error)? {
                None => {}
                indexed_hash => {
                    return Err(TelosSidecarError::CorruptIndex {
                        block_number,
                        block_hash,
                        indexed_hash,
                    })
                }
            }
            let old_index_key = TelosSidecarNumberHashKey::new(old_number, block_hash);
            if !tx
                .delete::<TelosExecutionSidecarsByNumberHash>(old_index_key, None)
                .map_err(database_error)?
            {
                return Err(TelosSidecarError::CorruptIndex {
                    block_number: old_number,
                    block_hash,
                    indexed_hash: None,
                })
            }
            tx.put::<TelosExecutionSidecarsByNumberHash>(index_key, block_hash)
                .map_err(database_error)?;
        }
        if old_parent != parent_hash {
            match tx
                .get::<TelosExecutionSidecarsByParentHash>(parent_index_key)
                .map_err(database_error)?
            {
                None => {}
                indexed_hash => {
                    return Err(TelosSidecarError::CorruptParentIndex {
                        parent_hash,
                        block_hash,
                        indexed_hash,
                    })
                }
            }
            let old_parent_key = TelosSidecarParentHashKey::new(old_parent, block_hash);
            if !tx
                .delete::<TelosExecutionSidecarsByParentHash>(old_parent_key, None)
                .map_err(database_error)?
            {
                return Err(TelosSidecarError::CorruptParentIndex {
                    parent_hash: old_parent,
                    block_hash,
                    indexed_hash: None,
                })
            }
            tx.put::<TelosExecutionSidecarsByParentHash>(parent_index_key, block_hash)
                .map_err(database_error)?;
        }

        tx.put::<TelosExecutionSidecars>(
            block_hash,
            TelosStoredSidecar::pending(sidecar.clone()).encode_record(),
        )
        .map_err(database_error)?;
        return Ok(outcome)
    }

    ensure_candidate_above_finalized_coverage(sidecar, finalized_coverage_from_transaction(tx)?)?;

    if let Some(indexed_hash) =
        tx.get::<TelosExecutionSidecarsByNumberHash>(index_key).map_err(database_error)?
    {
        return Err(TelosSidecarError::CorruptIndex {
            block_number,
            block_hash,
            indexed_hash: Some(indexed_hash),
        })
    }
    if let Some(indexed_hash) =
        tx.get::<TelosExecutionSidecarsByParentHash>(parent_index_key).map_err(database_error)?
    {
        return Err(TelosSidecarError::CorruptParentIndex {
            parent_hash,
            block_hash,
            indexed_hash: Some(indexed_hash),
        })
    }

    tx.put::<TelosExecutionSidecars>(
        block_hash,
        TelosStoredSidecar::pending(sidecar.clone()).encode_record(),
    )
    .map_err(database_error)?;
    tx.put::<TelosExecutionSidecarsByNumberHash>(index_key, block_hash).map_err(database_error)?;
    tx.put::<TelosExecutionSidecarsByParentHash>(parent_index_key, block_hash)
        .map_err(database_error)?;
    Ok(TelosSidecarPutOutcome::InsertedPending)
}

fn mark_accepted_with_transaction<TX: DbTx + DbTxMut>(
    tx: &TX,
    chain: TelosChainIdentity,
    block_hash: B256,
    digest: B256,
) -> Result<TelosSidecarAcceptOutcome, TelosSidecarError> {
    let record = tx
        .get::<TelosExecutionSidecars>(block_hash)
        .map_err(database_error)?
        .ok_or(TelosSidecarError::MissingCandidate { block_hash, digest })?;
    let mut existing = TelosStoredSidecar::decode_record(&record, chain)?;
    validate_record_index(tx, block_hash, &existing)?;
    ensure_candidate_digest(&existing, block_hash, digest)?;
    match existing.state {
        TelosSidecarState::Pending => {
            return Err(TelosSidecarError::CandidateNotDispatched { block_hash, digest })
        }
        TelosSidecarState::Dispatched => {}
        TelosSidecarState::Accepted => return Ok(TelosSidecarAcceptOutcome::AlreadyAccepted),
    }

    existing.state = TelosSidecarState::Accepted;
    tx.put::<TelosExecutionSidecars>(block_hash, existing.encode_record())
        .map_err(database_error)?;
    Ok(TelosSidecarAcceptOutcome::Accepted)
}

fn mark_dispatched_with_transaction<TX: DbTx + DbTxMut>(
    tx: &TX,
    chain: TelosChainIdentity,
    block_hash: B256,
    digest: B256,
) -> Result<TelosSidecarDispatchOutcome, TelosSidecarError> {
    let record = tx
        .get::<TelosExecutionSidecars>(block_hash)
        .map_err(database_error)?
        .ok_or(TelosSidecarError::MissingCandidate { block_hash, digest })?;
    let mut existing = TelosStoredSidecar::decode_record(&record, chain)?;
    validate_record_index(tx, block_hash, &existing)?;
    ensure_candidate_digest(&existing, block_hash, digest)?;
    match existing.state {
        TelosSidecarState::Pending => {
            existing.state = TelosSidecarState::Dispatched;
            tx.put::<TelosExecutionSidecars>(block_hash, existing.encode_record())
                .map_err(database_error)?;
            Ok(TelosSidecarDispatchOutcome::Dispatched)
        }
        TelosSidecarState::Dispatched => Ok(TelosSidecarDispatchOutcome::AlreadyDispatched),
        TelosSidecarState::Accepted => Ok(TelosSidecarDispatchOutcome::AlreadyAccepted),
    }
}

fn remove_pending_with_transaction<TX: DbTx + DbTxMut>(
    tx: &TX,
    chain: TelosChainIdentity,
    block_hash: B256,
    digest: B256,
) -> Result<TelosSidecarRemoveOutcome, TelosSidecarError> {
    let Some(record) = tx.get::<TelosExecutionSidecars>(block_hash).map_err(database_error)? else {
        return Ok(TelosSidecarRemoveOutcome::AlreadyAbsent)
    };
    let existing = TelosStoredSidecar::decode_record(&record, chain)?;
    validate_record_index(tx, block_hash, &existing)?;
    ensure_candidate_digest(&existing, block_hash, digest)?;
    ensure_unaccepted(&existing)?;

    let removals = collect_unaccepted_subtree_from_transaction(tx, chain, block_hash)?;
    for (removal_hash, record) in removals {
        let block_number = record.sidecar.envelope.block_number;
        let parent_hash = record.sidecar.envelope.parent_hash;
        let index_key = TelosSidecarNumberHashKey::new(block_number, removal_hash);
        let parent_key = TelosSidecarParentHashKey::new(parent_hash, removal_hash);
        if !tx.delete::<TelosExecutionSidecars>(removal_hash, None).map_err(database_error)? {
            return Err(TelosSidecarError::MissingChildPrimary {
                parent_hash,
                block_hash: removal_hash,
            })
        }
        if !tx
            .delete::<TelosExecutionSidecarsByNumberHash>(index_key, None)
            .map_err(database_error)?
        {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash: removal_hash,
                indexed_hash: None,
            })
        }
        if !tx
            .delete::<TelosExecutionSidecarsByParentHash>(parent_key, None)
            .map_err(database_error)?
        {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash: removal_hash,
                indexed_hash: None,
            })
        }
    }
    Ok(TelosSidecarRemoveOutcome::RemovedPending)
}

fn collect_unaccepted_subtree_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    root: B256,
) -> Result<Vec<(B256, TelosStoredSidecar)>, TelosSidecarError> {
    let mut stack = vec![(root, root)];
    let mut seen = BTreeSet::new();
    let mut removals = Vec::new();
    while let Some((parent_hash, block_hash)) = stack.pop() {
        if !seen.insert(block_hash) {
            return Err(TelosSidecarError::CyclicParentIndex(block_hash))
        }
        let encoded = tx
            .get::<TelosExecutionSidecars>(block_hash)
            .map_err(database_error)?
            .ok_or(TelosSidecarError::MissingChildPrimary { parent_hash, block_hash })?;
        let record = TelosStoredSidecar::decode_record(&encoded, chain)?;
        if block_hash != root && record.sidecar.envelope.parent_hash != parent_hash {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        validate_record_index(tx, block_hash, &record)?;
        if record.state == TelosSidecarState::Accepted {
            return Err(TelosSidecarError::AcceptedDescendant {
                invalid_root: root,
                accepted_descendant: block_hash,
            })
        }
        removals.push((block_hash, record));
        let children = child_hashes_from_transaction(tx, block_hash)?;
        stack.extend(children.into_iter().rev().map(|child| (block_hash, child)));
    }
    Ok(removals)
}

fn validate_forkchoice_state_with_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    head_block_hash: B256,
    safe_block_hash: B256,
    finalized_block_hash: B256,
) -> Result<(), TelosSidecarError> {
    anchor.validate_for_chain(chain)?;
    if head_block_hash == B256::ZERO {
        return Err(TelosSidecarError::ZeroForkchoiceHead)
    }

    let current_finalized = current_coverage_from_transaction(
        tx,
        chain,
        anchor,
        finalized_coverage_from_transaction(tx)?,
    )?;
    let head =
        resolve_forkchoice_block_from_transaction(tx, chain, anchor, "head", head_block_hash)?;
    let requested_finalized = if finalized_block_hash == B256::ZERO {
        current_finalized
    } else {
        let requested = resolve_forkchoice_block_from_transaction(
            tx,
            chain,
            anchor,
            "finalized",
            finalized_block_hash,
        )?;
        validate_coverage_advancement(current_finalized, requested)?;
        validate_forkchoice_ancestry_from_transaction(
            tx,
            chain,
            anchor,
            current_finalized,
            requested,
        )?;
        requested
    };

    validate_forkchoice_ancestry_from_transaction(tx, chain, anchor, requested_finalized, head)?;
    if safe_block_hash != B256::ZERO {
        let safe =
            resolve_forkchoice_block_from_transaction(tx, chain, anchor, "safe", safe_block_hash)?;
        validate_forkchoice_ancestry_from_transaction(
            tx,
            chain,
            anchor,
            requested_finalized,
            safe,
        )?;
        validate_forkchoice_ancestry_from_transaction(tx, chain, anchor, safe, head)?;
    }
    Ok(())
}

fn resolve_forkchoice_block_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    role: &'static str,
    block_hash: B256,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    if block_hash == anchor.parent_block_hash {
        return Ok(anchor_coverage(anchor))
    }
    let record = get_record_by_hash_from_transaction(tx, chain, block_hash)?
        .ok_or(TelosSidecarError::ForkchoiceSidecarMissing { role, block_hash })?;
    validate_forkchoice_record(role, block_hash, &record)
}

fn validate_forkchoice_ancestry_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    ancestor: TelosFinalizedCoverage,
    descendant: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    let mut current = descendant;
    while current.block_number > ancestor.block_number {
        let child = get_record_by_hash_from_transaction(tx, chain, current.block_hash)?.ok_or(
            TelosSidecarError::ForkchoiceSidecarMissing {
                role: "ancestry",
                block_hash: current.block_hash,
            },
        )?;
        let actual_number = child.sidecar.envelope.block_number;
        if actual_number != current.block_number {
            return Err(TelosSidecarError::ForkchoiceChainGap {
                block_hash: current.block_hash,
                expected_number: current.block_number,
                actual_number,
            })
        }
        if child.state != TelosSidecarState::Accepted {
            return Err(TelosSidecarError::ForkchoiceSidecarNotAccepted {
                role: "ancestry",
                block_hash: current.block_hash,
                state: child.state,
            })
        }

        let expected_parent_number = current.block_number - 1;
        let parent_hash = child.sidecar.envelope.parent_hash;
        let parent = if parent_hash == anchor.parent_block_hash {
            validate_sidecar_against_parent(anchor, &child.sidecar, None)?;
            anchor_coverage(anchor)
        } else {
            let parent_record = get_record_by_hash_from_transaction(tx, chain, parent_hash)?
                .ok_or(TelosSidecarError::ForkchoiceSidecarMissing {
                    role: "ancestry",
                    block_hash: parent_hash,
                })?;
            let parent = validate_forkchoice_record("ancestry", parent_hash, &parent_record)?;
            validate_sidecar_against_parent(anchor, &child.sidecar, Some(&parent_record.sidecar))?;
            parent
        };
        if parent.block_number != expected_parent_number {
            return Err(TelosSidecarError::ForkchoiceChainGap {
                block_hash: parent.block_hash,
                expected_number: expected_parent_number,
                actual_number: parent.block_number,
            })
        }
        current = parent;
    }

    validate_forkchoice_ancestor_match(ancestor, descendant, current)
}

fn finalize_and_prune_with_transaction<TX: DbTx + DbTxMut>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    finalized_hash: B256,
) -> Result<TelosSidecarPruneOutcome, TelosSidecarError> {
    anchor.validate_for_chain(chain)?;
    if finalized_hash == B256::ZERO {
        return Err(TelosSidecarError::ZeroFinalizedHash)
    }

    let stored_marker = finalized_coverage_from_transaction(tx)?;
    let current = current_coverage_from_transaction(tx, chain, anchor, stored_marker)?;
    let requested = resolve_finalized_from_transaction(tx, chain, anchor, finalized_hash)?;
    validate_coverage_advancement(current, requested)?;
    let mut canonical = canonical_segment_from_transaction(tx, chain, current, requested)?;
    let persisted_number = persisted_canonical_tip_from_transaction(tx, anchor)?;
    let finalized = persistence_bounded_coverage(current, requested, persisted_number, &canonical)?;
    ensure_persisted_canonical_from_transaction(tx, anchor, finalized)?;
    canonical.retain(|number, _| *number <= finalized.block_number);
    if current == finalized {
        if stored_marker.is_none() {
            write_finalized_coverage(tx, finalized)?;
        }
        return Ok(TelosSidecarPruneOutcome {
            finalized,
            removed_records: 0,
            removed_bytes: 0,
            retained_canonical_records: 0,
        })
    }

    validate_canonical_execution_context_from_transaction(tx, chain, anchor, current, &canonical)?;
    let mut protected = canonical.values().copied().collect::<BTreeSet<_>>();
    if current.block_number > anchor.parent_block_number {
        protected.insert(current.block_hash);
    }
    let mut removals = BTreeMap::new();
    for (number, canonical_hash) in &canonical {
        for record in get_records_by_number_from_transaction(tx, chain, *number)? {
            let block_hash = record.sidecar.envelope.block_hash;
            if block_hash != *canonical_hash && !removals.contains_key(&block_hash) {
                collect_prunable_subtree_from_transaction(
                    tx,
                    chain,
                    block_hash,
                    &protected,
                    &mut removals,
                )?;
            }
        }
    }

    let removed_records =
        u64::try_from(removals.len()).map_err(|_| TelosSidecarError::PruneCounterOverflow)?;
    let removed_bytes = removals.values().try_fold(0u64, |total, record| {
        let bytes = u64::try_from(record.encode_record().len())
            .map_err(|_| TelosSidecarError::PruneCounterOverflow)?;
        total.checked_add(bytes).ok_or(TelosSidecarError::PruneCounterOverflow)
    })?;
    let retained_canonical_records =
        u64::try_from(canonical.len()).map_err(|_| TelosSidecarError::PruneCounterOverflow)?;

    for (block_hash, record) in removals {
        let block_number = record.sidecar.envelope.block_number;
        let parent_hash = record.sidecar.envelope.parent_hash;
        if !tx.delete::<TelosExecutionSidecars>(block_hash, None).map_err(database_error)? {
            return Err(TelosSidecarError::MissingChildPrimary { parent_hash, block_hash })
        }
        if !tx
            .delete::<TelosExecutionSidecarsByNumberHash>(
                TelosSidecarNumberHashKey::new(block_number, block_hash),
                None,
            )
            .map_err(database_error)?
        {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash,
                indexed_hash: None,
            })
        }
        if !tx
            .delete::<TelosExecutionSidecarsByParentHash>(
                TelosSidecarParentHashKey::new(parent_hash, block_hash),
                None,
            )
            .map_err(database_error)?
        {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: None,
            })
        }
    }
    write_finalized_coverage(tx, finalized)?;

    Ok(TelosSidecarPruneOutcome {
        finalized,
        removed_records,
        removed_bytes,
        retained_canonical_records,
    })
}

pub(crate) fn finalized_coverage_from_transaction<TX: DbTx>(
    tx: &TX,
) -> Result<Option<TelosFinalizedCoverage>, TelosSidecarError> {
    let entries = tx.entries::<TelosSidecarFinalizedCoverage>().map_err(database_error)?;
    match entries {
        0 => Ok(None),
        1 => {
            let mut cursor =
                tx.cursor_read::<TelosSidecarFinalizedCoverage>().map_err(database_error)?;
            let (block_number, block_hash) = cursor
                .first()
                .map_err(database_error)?
                .ok_or(TelosSidecarError::CorruptFinalizedCoverage { entries })?;
            Ok(Some(TelosFinalizedCoverage { block_number, block_hash }))
        }
        entries => Err(TelosSidecarError::CorruptFinalizedCoverage { entries }),
    }
}

fn persisted_canonical_tip_from_transaction<TX: DbTx>(
    tx: &TX,
    anchor: &TelosExecutionAnchor,
) -> Result<u64, TelosSidecarError> {
    // Reth updates Finish only after staging the complete canonical block/state batch. This read
    // and the sidecar marker write share one MDBX writer transaction, so they serialize with both
    // persistence commits and unwinds. A crash can therefore leave coverage behind, never ahead.
    Ok(tx
        .get::<tables::StageCheckpoints>(StageId::Finish.to_string())
        .map_err(database_error)?
        .map_or(anchor.parent_block_number, |checkpoint| checkpoint.block_number))
}

fn ensure_persisted_canonical_from_transaction<TX: DbTx>(
    tx: &TX,
    anchor: &TelosExecutionAnchor,
    finalized: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    if finalized == anchor_coverage(anchor) {
        return Ok(())
    }
    let indexed_number =
        tx.get::<tables::HeaderNumbers>(finalized.block_hash).map_err(database_error)?;
    if indexed_number != Some(finalized.block_number) {
        return Err(TelosSidecarError::PersistedCanonicalBlockMissing {
            block_number: finalized.block_number,
            block_hash: finalized.block_hash,
            indexed_number,
        })
    }
    Ok(())
}

fn current_coverage_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    marker: Option<TelosFinalizedCoverage>,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    let Some(marker) = marker else { return Ok(anchor_coverage(anchor)) };
    validate_coverage_marker(marker, anchor)?;
    if marker.block_number > anchor.parent_block_number {
        let record = get_record_by_hash_from_transaction(tx, chain, marker.block_hash)?
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash: marker.block_hash })?;
        validate_finalized_record(marker, &record)?;
    }
    Ok(marker)
}

fn resolve_finalized_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    block_hash: B256,
) -> Result<TelosFinalizedCoverage, TelosSidecarError> {
    if block_hash == anchor.parent_block_hash {
        return Ok(anchor_coverage(anchor))
    }
    let record = get_record_by_hash_from_transaction(tx, chain, block_hash)?
        .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash })?;
    let coverage =
        TelosFinalizedCoverage { block_number: record.sidecar.envelope.block_number, block_hash };
    validate_finalized_record(coverage, &record)?;
    Ok(coverage)
}

fn canonical_segment_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    current: TelosFinalizedCoverage,
    finalized: TelosFinalizedCoverage,
) -> Result<BTreeMap<u64, B256>, TelosSidecarError> {
    let mut expected_number = finalized.block_number;
    let mut block_hash = finalized.block_hash;
    let mut seen = BTreeSet::new();
    let mut canonical = BTreeMap::new();
    while expected_number > current.block_number {
        if !seen.insert(block_hash) {
            return Err(TelosSidecarError::CyclicParentIndex(block_hash))
        }
        let record = get_record_by_hash_from_transaction(tx, chain, block_hash)?
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash })?;
        validate_finalized_record(
            TelosFinalizedCoverage { block_number: expected_number, block_hash },
            &record,
        )?;
        canonical.insert(expected_number, block_hash);
        block_hash = record.sidecar.envelope.parent_hash;
        expected_number -= 1;
    }
    if block_hash != current.block_hash {
        return Err(TelosSidecarError::FinalizedAncestryMismatch {
            expected_hash: current.block_hash,
            actual_hash: block_hash,
        })
    }
    Ok(canonical)
}

fn validate_canonical_execution_context_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    anchor: &TelosExecutionAnchor,
    current: TelosFinalizedCoverage,
    canonical: &BTreeMap<u64, B256>,
) -> Result<(), TelosSidecarError> {
    let mut expected = if current.block_number == anchor.parent_block_number {
        (anchor.starting_gas_price, anchor.starting_revision)
    } else {
        let current_record = get_record_by_hash_from_transaction(tx, chain, current.block_hash)?
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash: current.block_hash })?;
        terminal_execution_context(&current_record.sidecar)?
    };
    for block_hash in canonical.values() {
        let record = get_record_by_hash_from_transaction(tx, chain, *block_hash)?
            .ok_or(TelosSidecarError::FinalizedSidecarMissing { block_hash: *block_hash })?;
        validate_starting_execution_context(&record.sidecar, expected)?;
        expected = terminal_execution_context(&record.sidecar)?;
    }
    Ok(())
}

fn collect_prunable_subtree_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    root: B256,
    protected: &BTreeSet<B256>,
    removals: &mut BTreeMap<B256, TelosStoredSidecar>,
) -> Result<(), TelosSidecarError> {
    let mut stack = vec![(root, root)];
    let mut seen = BTreeSet::new();
    while let Some((parent_hash, block_hash)) = stack.pop() {
        if !seen.insert(block_hash) {
            return Err(TelosSidecarError::CyclicParentIndex(block_hash))
        }
        let encoded = tx
            .get::<TelosExecutionSidecars>(block_hash)
            .map_err(database_error)?
            .ok_or(TelosSidecarError::MissingChildPrimary { parent_hash, block_hash })?;
        let record = TelosStoredSidecar::decode_record(&encoded, chain)?;
        if block_hash != root && record.sidecar.envelope.parent_hash != parent_hash {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        validate_record_index(tx, block_hash, &record)?;
        if protected.contains(&block_hash) {
            return Err(TelosSidecarError::CanonicalPruneAttempt { block_hash })
        }
        if removals.contains_key(&block_hash) {
            continue
        }
        removals.insert(block_hash, record);
        let children = child_hashes_from_transaction(tx, block_hash)?;
        stack.extend(children.into_iter().rev().map(|child| (block_hash, child)));
    }
    Ok(())
}

fn write_finalized_coverage<TX: DbTxMut>(
    tx: &TX,
    finalized: TelosFinalizedCoverage,
) -> Result<(), TelosSidecarError> {
    tx.clear::<TelosSidecarFinalizedCoverage>().map_err(database_error)?;
    tx.put::<TelosSidecarFinalizedCoverage>(finalized.block_number, finalized.block_hash)
        .map_err(database_error)
}

fn child_hashes_from_transaction<TX: DbTx>(
    tx: &TX,
    parent_hash: B256,
) -> Result<Vec<B256>, TelosSidecarError> {
    let mut cursor =
        tx.cursor_read::<TelosExecutionSidecarsByParentHash>().map_err(database_error)?;
    let start = TelosSidecarParentHashKey::new(parent_hash, B256::ZERO);
    let end = TelosSidecarParentHashKey::new(parent_hash, B256::repeat_byte(u8::MAX));
    let mut children = Vec::new();
    for row in cursor.walk_range(start..=end).map_err(database_error)? {
        let (key, block_hash) = row.map_err(database_error)?;
        if key.parent_hash != parent_hash || key.block_hash != block_hash {
            return Err(TelosSidecarError::CorruptParentIndex {
                parent_hash,
                block_hash: key.block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        children.push(block_hash);
    }
    Ok(children)
}

fn validate_record_index<TX: DbTx>(
    tx: &TX,
    block_hash: B256,
    record: &TelosStoredSidecar,
) -> Result<(), TelosSidecarError> {
    if record.sidecar.envelope.block_hash != block_hash {
        return Err(TelosSidecarError::PrimaryKeyMismatch {
            expected: block_hash,
            actual: record.sidecar.envelope.block_hash,
        })
    }
    let block_number = record.sidecar.envelope.block_number;
    let index_key = TelosSidecarNumberHashKey::new(block_number, block_hash);
    match tx.get::<TelosExecutionSidecarsByNumberHash>(index_key).map_err(database_error)? {
        Some(indexed_hash) if indexed_hash == block_hash => {}
        indexed_hash => {
            return Err(TelosSidecarError::CorruptIndex { block_number, block_hash, indexed_hash })
        }
    }
    let parent_hash = record.sidecar.envelope.parent_hash;
    let parent_key = TelosSidecarParentHashKey::new(parent_hash, block_hash);
    match tx.get::<TelosExecutionSidecarsByParentHash>(parent_key).map_err(database_error)? {
        Some(indexed_hash) if indexed_hash == block_hash => Ok(()),
        indexed_hash => {
            Err(TelosSidecarError::CorruptParentIndex { parent_hash, block_hash, indexed_hash })
        }
    }
}

pub(crate) fn get_record_by_hash_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    block_hash: B256,
) -> Result<Option<TelosStoredSidecar>, TelosSidecarError> {
    let record = tx.get::<TelosExecutionSidecars>(block_hash).map_err(database_error)?;
    let record =
        record.map(|record| TelosStoredSidecar::decode_record(&record, chain)).transpose()?;
    if let Some(record) = &record {
        validate_record_index(tx, block_hash, record)?;
    }
    Ok(record)
}

fn get_records_by_number_from_transaction<TX: DbTx>(
    tx: &TX,
    chain: TelosChainIdentity,
    block_number: u64,
) -> Result<Vec<TelosStoredSidecar>, TelosSidecarError> {
    let mut cursor =
        tx.cursor_read::<TelosExecutionSidecarsByNumberHash>().map_err(database_error)?;
    let start = TelosSidecarNumberHashKey::new(block_number, B256::ZERO);
    let end = TelosSidecarNumberHashKey::new(block_number, B256::repeat_byte(u8::MAX));
    let mut sidecars = Vec::new();
    for row in cursor.walk_range(start..=end).map_err(database_error)? {
        let (key, block_hash) = row.map_err(database_error)?;
        if key.block_hash != block_hash {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash: key.block_hash,
                indexed_hash: Some(block_hash),
            })
        }
        let record = tx
            .get::<TelosExecutionSidecars>(block_hash)
            .map_err(database_error)?
            .ok_or(TelosSidecarError::MissingPrimary { block_number, block_hash })?;
        let record = TelosStoredSidecar::decode_record(&record, chain)?;
        if record.sidecar.envelope.block_number != block_number ||
            record.sidecar.envelope.block_hash != block_hash
        {
            return Err(TelosSidecarError::CorruptIndex {
                block_number,
                block_hash,
                indexed_hash: Some(record.sidecar.envelope.block_hash),
            })
        }
        let parent_hash = record.sidecar.envelope.parent_hash;
        let parent_key = TelosSidecarParentHashKey::new(parent_hash, block_hash);
        match tx.get::<TelosExecutionSidecarsByParentHash>(parent_key).map_err(database_error)? {
            Some(indexed_hash) if indexed_hash == block_hash => {}
            indexed_hash => {
                return Err(TelosSidecarError::CorruptParentIndex {
                    parent_hash,
                    block_hash,
                    indexed_hash,
                })
            }
        }
        sidecars.push(record);
    }
    Ok(sidecars)
}

fn database_error(error: DatabaseError) -> TelosSidecarError {
    TelosSidecarError::Database(error.to_string())
}

fn provider_error(error: reth_provider::ProviderError) -> TelosSidecarError {
    TelosSidecarError::Database(error.to_string())
}

impl fmt::Display for TelosSidecarPutOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsertedPending => f.write_str("inserted pending"),
            Self::AlreadyPending => f.write_str("already pending"),
            Self::ReplacedPending => f.write_str("replaced pending"),
            Self::AlreadyDispatched => f.write_str("already dispatched"),
            Self::AlreadyAccepted => f.write_str("already accepted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use reth_db::{init_db, mdbx::DatabaseArguments, open_db};
    use reth_telos_rpc_engine_api::structs::{
        TelosAccountStateTableRow, TelosAccountTableRow, TelosExecutionMetadataV3,
        TelosExtraFieldReceipt,
    };

    fn chain() -> TelosChainIdentity {
        TelosChainIdentity { chain_id: 40, genesis_hash: B256::repeat_byte(0x40) }
    }

    fn anchor(parent_block_number: u64, parent_block_hash: B256) -> TelosExecutionAnchor {
        TelosExecutionAnchor {
            version: TELOS_EXECUTION_ANCHOR_VERSION,
            chain: chain(),
            parent_block_number,
            parent_block_hash,
            starting_gas_price: U256::from(7),
            starting_revision: 1,
        }
    }

    fn fields(block_hash: B256, parent_hash: B256, balance: u64) -> TelosEngineApiExtraFields {
        TelosEngineApiExtraFields {
            statediffs_account: Some(vec![
                TelosAccountTableRow {
                    address: Address::repeat_byte(0x22),
                    balance: U256::from(balance),
                    ..Default::default()
                },
                TelosAccountTableRow {
                    address: Address::repeat_byte(0x11),
                    balance: U256::from(1),
                    ..Default::default()
                },
            ]),
            statediffs_accountstate: Some(vec![
                TelosAccountStateTableRow {
                    address: Address::repeat_byte(0x22),
                    key: U256::from(2),
                    value: U256::from(3),
                    ..Default::default()
                },
                TelosAccountStateTableRow {
                    address: Address::repeat_byte(0x11),
                    key: U256::from(1),
                    value: U256::from(2),
                    ..Default::default()
                },
            ]),
            revision_changes: None,
            gasprice_changes: None,
            execution: Some(TelosExecutionMetadataV3 {
                version: TELOS_EXECUTION_METADATA_VERSION,
                block_hash,
                parent_hash,
                transaction_count: 2,
                execution_base_fee: U256::from(7),
                starting_gas_price: U256::from(7),
                starting_revision: 1,
                gas_price_changes: Vec::new(),
                revision_changes: Vec::new(),
            }),
            new_addresses_using_create: Some(vec![(0, U256::from(0x22)), (0, U256::from(0x11))]),
            new_addresses_using_openwallet: Some(Vec::new()),
            receipts: Some(vec![
                TelosExtraFieldReceipt {
                    tx_type: TelosReceiptType::Name("Legacy".to_string()),
                    success: true,
                    cumulative_gas_used: 21,
                    logs: Vec::new(),
                },
                TelosExtraFieldReceipt {
                    tx_type: TelosReceiptType::Number(0),
                    success: true,
                    cumulative_gas_used: 42,
                    logs: Vec::new(),
                },
            ]),
        }
    }

    fn sidecar(block_hash: B256, parent_hash: B256, block_number: u64) -> TelosExecutionSidecar {
        TelosExecutionSidecar::new(
            chain(),
            block_number,
            block_hash,
            parent_hash,
            2,
            42,
            fields(block_hash, parent_hash, 2),
        )
        .unwrap()
    }

    fn accept(store: &dyn TelosSidecarStore, sidecar: &TelosExecutionSidecar) {
        let block_hash = sidecar.envelope().block_hash;
        store.put_pending(sidecar).unwrap();
        store.mark_dispatched(block_hash, sidecar.digest()).unwrap();
        store.mark_accepted(block_hash, sidecar.digest()).unwrap();
    }

    #[test]
    fn forkchoice_preflight_validates_the_full_accepted_tuple_and_ancestry() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let block7_hash = B256::repeat_byte(0x71);
        let block8_hash = B256::repeat_byte(0x72);
        let block9_hash = B256::repeat_byte(0x73);
        let fork8_hash = B256::repeat_byte(0x82);
        let pending8_hash = B256::repeat_byte(0x92);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let block7 = sidecar(block7_hash, snapshot_hash, 7);
        let block8 = sidecar(block8_hash, block7_hash, 8);
        let block9 = sidecar(block9_hash, block8_hash, 9);
        let fork8 = sidecar(fork8_hash, block7_hash, 8);
        let pending8 = sidecar(pending8_hash, block7_hash, 8);
        for candidate in [&block7, &block8, &block9, &fork8] {
            accept(&store, candidate);
        }
        store.put_pending(&pending8).unwrap();

        store
            .validate_forkchoice_state(&execution_anchor, block9_hash, block8_hash, block7_hash)
            .unwrap();
        store
            .validate_forkchoice_state(&execution_anchor, block9_hash, B256::ZERO, B256::ZERO)
            .unwrap();
        store
            .validate_forkchoice_state(
                &execution_anchor,
                snapshot_hash,
                snapshot_hash,
                snapshot_hash,
            )
            .unwrap();

        assert!(matches!(
            store.validate_forkchoice_state(&execution_anchor, B256::ZERO, B256::ZERO, B256::ZERO,),
            Err(TelosSidecarError::ZeroForkchoiceHead)
        ));
        assert!(matches!(
            store.validate_forkchoice_state(
                &execution_anchor,
                block9_hash,
                B256::repeat_byte(0xff),
                block7_hash,
            ),
            Err(TelosSidecarError::ForkchoiceSidecarMissing { role: "safe", .. })
        ));
        assert!(matches!(
            store.validate_forkchoice_state(
                &execution_anchor,
                block9_hash,
                pending8_hash,
                block7_hash,
            ),
            Err(TelosSidecarError::ForkchoiceSidecarNotAccepted {
                role: "safe",
                state: TelosSidecarState::Pending,
                ..
            })
        ));
        assert!(matches!(
            store.validate_forkchoice_state(
                &execution_anchor,
                block9_hash,
                fork8_hash,
                block7_hash,
            ),
            Err(TelosSidecarError::ForkchoiceAncestryMismatch { .. })
        ));
        assert!(matches!(
            store.validate_forkchoice_state(
                &execution_anchor,
                block9_hash,
                block8_hash,
                fork8_hash,
            ),
            Err(TelosSidecarError::ForkchoiceAncestryMismatch { .. })
        ));
    }

    #[test]
    fn forkchoice_preflight_rejects_finality_regression_before_engine_dispatch() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let block7_hash = B256::repeat_byte(0x71);
        let block8_hash = B256::repeat_byte(0x72);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let block7 = sidecar(block7_hash, snapshot_hash, 7);
        let block8 = sidecar(block8_hash, block7_hash, 8);
        accept(&store, &block7);
        accept(&store, &block8);
        store.note_persisted_canonical_block(7, block7_hash).unwrap();
        store.note_persisted_canonical_block(8, block8_hash).unwrap();
        store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();

        assert!(matches!(
            store.validate_forkchoice_state(
                &execution_anchor,
                block8_hash,
                block8_hash,
                block7_hash,
            ),
            Err(TelosSidecarError::FinalizedRegression { current_number: 8, new_number: 7 })
        ));
    }

    #[test]
    fn canonicalizes_semantically_unordered_fields_and_receipt_spelling() {
        let block_hash = B256::repeat_byte(0xaa);
        let parent_hash = B256::repeat_byte(0xbb);
        let first = sidecar(block_hash, parent_hash, 7);
        let mut reordered_fields = fields(block_hash, parent_hash, 2);
        reordered_fields.statediffs_account.as_mut().unwrap().reverse();
        reordered_fields.statediffs_accountstate.as_mut().unwrap().reverse();
        reordered_fields.new_addresses_using_create.as_mut().unwrap().reverse();
        reordered_fields.receipts.as_mut().unwrap()[0].tx_type = TelosReceiptType::Number(0);
        let reordered = TelosExecutionSidecar::new(
            chain(),
            7,
            block_hash,
            parent_hash,
            2,
            42,
            reordered_fields,
        )
        .unwrap();

        assert_eq!(first.digest(), reordered.digest());
        assert_eq!(first.canonical_bytes(), reordered.canonical_bytes());
    }

    #[test]
    fn pending_conflict_replaces_until_exact_digest_is_accepted() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let block_hash = B256::repeat_byte(0xaa);
        let parent_hash = B256::repeat_byte(0xbb);
        let first = sidecar(block_hash, parent_hash, 7);
        assert_eq!(store.put_pending(&first).unwrap(), TelosSidecarPutOutcome::InsertedPending);
        assert_eq!(store.put_pending(&first).unwrap(), TelosSidecarPutOutcome::AlreadyPending);

        let conflict = TelosExecutionSidecar::new(
            chain(),
            7,
            block_hash,
            parent_hash,
            2,
            42,
            fields(block_hash, parent_hash, 99),
        )
        .unwrap();
        assert_eq!(store.put_pending(&conflict).unwrap(), TelosSidecarPutOutcome::ReplacedPending);
        assert_eq!(store.get_pending_by_hash(block_hash).unwrap(), Some(conflict.clone()));
        assert!(matches!(
            store.mark_accepted(block_hash, conflict.digest()),
            Err(TelosSidecarError::CandidateNotDispatched { .. })
        ));
        store.mark_dispatched(block_hash, conflict.digest()).unwrap();
        assert_eq!(
            store.mark_accepted(block_hash, conflict.digest()).unwrap(),
            TelosSidecarAcceptOutcome::Accepted
        );
        assert_eq!(store.put_pending(&conflict).unwrap(), TelosSidecarPutOutcome::AlreadyAccepted);
        assert!(matches!(
            store.put_pending(&first),
            Err(TelosSidecarError::AcceptedImmutable { .. })
        ));
    }

    #[test]
    fn delayed_engine_results_cannot_mutate_a_replacement_candidate() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let block_hash = B256::repeat_byte(0xaa);
        let parent_hash = B256::repeat_byte(0xbb);
        let first = sidecar(block_hash, parent_hash, 7);
        let replacement = TelosExecutionSidecar::new(
            chain(),
            7,
            block_hash,
            parent_hash,
            2,
            42,
            fields(block_hash, parent_hash, 99),
        )
        .unwrap();
        store.put_pending(&first).unwrap();
        store.put_pending(&replacement).unwrap();
        store.mark_dispatched(block_hash, replacement.digest()).unwrap();

        assert!(matches!(
            store.remove_pending(block_hash, first.digest()),
            Err(TelosSidecarError::CandidateDigestMismatch { .. })
        ));
        assert!(matches!(
            store.mark_accepted(block_hash, first.digest()),
            Err(TelosSidecarError::CandidateDigestMismatch { .. })
        ));
        assert_eq!(store.get_dispatched_by_hash(block_hash).unwrap(), Some(replacement.clone()));
        assert_eq!(store.get_accepted_by_hash(block_hash).unwrap(), None);

        store.mark_accepted(block_hash, replacement.digest()).unwrap();
        assert!(matches!(
            store.remove_pending(block_hash, replacement.digest()),
            Err(TelosSidecarError::AcceptedImmutable { .. })
        ));
        assert_eq!(store.get_accepted_by_hash(block_hash).unwrap(), Some(replacement));
    }

    #[test]
    fn dispatched_digest_is_immutable_across_crash_or_known_payload_retry() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let block_hash = B256::repeat_byte(0xaa);
        let parent_hash = B256::repeat_byte(0xbb);
        let dispatched = sidecar(block_hash, parent_hash, 7);
        let poison = TelosExecutionSidecar::new(
            chain(),
            7,
            block_hash,
            parent_hash,
            2,
            42,
            fields(block_hash, parent_hash, 99),
        )
        .unwrap();
        store.put_pending(&dispatched).unwrap();
        store.mark_dispatched(block_hash, dispatched.digest()).unwrap();

        assert!(matches!(
            store.put_pending(&poison),
            Err(TelosSidecarError::CandidateInFlight { .. })
        ));
        assert_eq!(
            store.put_pending(&dispatched).unwrap(),
            TelosSidecarPutOutcome::AlreadyDispatched
        );
        assert_eq!(
            store.mark_dispatched(block_hash, dispatched.digest()).unwrap(),
            TelosSidecarDispatchOutcome::AlreadyDispatched
        );
        store.mark_accepted(block_hash, dispatched.digest()).unwrap();
        assert_eq!(store.get_accepted_by_hash(block_hash).unwrap(), Some(dispatched));
    }

    #[test]
    fn invalid_parent_atomically_removes_all_unaccepted_descendants() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let root_hash = B256::repeat_byte(0x71);
        let child_hash = B256::repeat_byte(0x72);
        let grandchild_hash = B256::repeat_byte(0x73);
        let root = sidecar(root_hash, B256::repeat_byte(0x70), 7);
        let child = sidecar(child_hash, root_hash, 8);
        let grandchild = sidecar(grandchild_hash, child_hash, 9);
        for candidate in [&root, &child, &grandchild] {
            store.put_pending(candidate).unwrap();
            store.mark_dispatched(candidate.envelope().block_hash, candidate.digest()).unwrap();
        }

        assert_eq!(
            store.remove_pending(root_hash, root.digest()).unwrap(),
            TelosSidecarRemoveOutcome::RemovedPending
        );
        for hash in [root_hash, child_hash, grandchild_hash] {
            assert_eq!(store.get_record_by_hash(hash).unwrap(), None);
        }
        for number in 7..=9 {
            assert!(store.get_records_by_number(number).unwrap().is_empty());
        }

        let corrected = TelosExecutionSidecar::new(
            chain(),
            7,
            root_hash,
            B256::repeat_byte(0x70),
            2,
            42,
            fields(root_hash, B256::repeat_byte(0x70), 99),
        )
        .unwrap();
        assert_eq!(store.put_pending(&corrected).unwrap(), TelosSidecarPutOutcome::InsertedPending);
    }

    #[test]
    fn invalid_subtree_cleanup_aborts_before_touching_an_accepted_descendant() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let root_hash = B256::repeat_byte(0x81);
        let child_hash = B256::repeat_byte(0x82);
        let root = sidecar(root_hash, B256::repeat_byte(0x80), 7);
        let child = sidecar(child_hash, root_hash, 8);
        for candidate in [&root, &child] {
            store.put_pending(candidate).unwrap();
            store.mark_dispatched(candidate.envelope().block_hash, candidate.digest()).unwrap();
        }
        // The low-level store refuses to compound an already-corrupt accepted-prefix invariant.
        store.mark_accepted(child_hash, child.digest()).unwrap();

        assert!(matches!(
            store.remove_pending(root_hash, root.digest()),
            Err(TelosSidecarError::AcceptedDescendant {
                invalid_root,
                accepted_descendant,
            }) if invalid_root == root_hash && accepted_descendant == child_hash
        ));
        assert!(store.get_record_by_hash(root_hash).unwrap().is_some());
        assert!(store.get_record_by_hash(child_hash).unwrap().is_some());
    }

    #[test]
    fn finalized_pruning_retains_canonical_segment_and_removes_complete_fork_subtrees() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let canonical7_hash = B256::repeat_byte(0x71);
        let canonical8_hash = B256::repeat_byte(0x72);
        let fork7_hash = B256::repeat_byte(0x81);
        let fork8_hash = B256::repeat_byte(0x82);
        let fork9_hash = B256::repeat_byte(0x83);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let canonical7 = sidecar(canonical7_hash, snapshot_hash, 7);
        let canonical8 = sidecar(canonical8_hash, canonical7_hash, 8);
        let fork7 = sidecar(fork7_hash, snapshot_hash, 7);
        let fork8 = sidecar(fork8_hash, fork7_hash, 8);
        let fork9 = sidecar(fork9_hash, fork8_hash, 9);
        accept(&store, &canonical7);
        accept(&store, &canonical8);
        accept(&store, &fork7);
        store.put_pending(&fork8).unwrap();
        store.mark_dispatched(fork8_hash, fork8.digest()).unwrap();
        store.put_pending(&fork9).unwrap();
        store.note_persisted_canonical_block(7, canonical7_hash).unwrap();
        store.note_persisted_canonical_block(8, canonical8_hash).unwrap();

        let outcome = store.finalize_and_prune(&execution_anchor, canonical8_hash).unwrap();
        assert_eq!(
            outcome.finalized,
            TelosFinalizedCoverage { block_number: 8, block_hash: canonical8_hash }
        );
        assert_eq!(outcome.removed_records, 3);
        assert!(outcome.removed_bytes > 0);
        assert_eq!(outcome.retained_canonical_records, 2);
        assert_eq!(store.get_accepted_by_hash(canonical7_hash).unwrap(), Some(canonical7));
        assert_eq!(store.get_accepted_by_hash(canonical8_hash).unwrap(), Some(canonical8));
        for hash in [fork7_hash, fork8_hash, fork9_hash] {
            assert_eq!(store.get_record_by_hash(hash).unwrap(), None);
        }
        assert_eq!(store.finalized_coverage().unwrap(), Some(outcome.finalized));

        let repeated = store.finalize_and_prune(&execution_anchor, canonical8_hash).unwrap();
        assert_eq!(repeated.removed_records, 0);
        assert_eq!(repeated.removed_bytes, 0);
        assert_eq!(repeated.retained_canonical_records, 0);
        assert!(matches!(
            store.finalize_and_prune(&execution_anchor, snapshot_hash),
            Err(TelosSidecarError::FinalizedRegression { current_number: 8, new_number: 6 })
        ));

        let late_fork = sidecar(B256::repeat_byte(0x91), snapshot_hash, 7);
        assert!(matches!(
            store.put_pending(&late_fork),
            Err(TelosSidecarError::CandidateBelowFinalizedCoverage {
                block_number: 7,
                finalized_number: 8,
                ..
            })
        ));
    }

    #[test]
    fn in_memory_finality_never_advances_past_simulated_persistence() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let block7_hash = B256::repeat_byte(0x71);
        let block8_hash = B256::repeat_byte(0x72);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let block7 = sidecar(block7_hash, snapshot_hash, 7);
        let block8 = sidecar(block8_hash, block7_hash, 8);
        accept(&store, &block7);
        accept(&store, &block8);

        // Engine finality can arrive while both canonical blocks are still only in Reth's tree.
        let deferred = store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();
        assert_eq!(deferred.finalized, anchor_coverage(&execution_anchor));
        assert_eq!(store.finalized_coverage().unwrap(), Some(anchor_coverage(&execution_anchor)));
        assert!(store.get_accepted_by_hash(block7_hash).unwrap().is_some());
        assert!(store.get_accepted_by_hash(block8_hash).unwrap().is_some());

        // Each later attempt advances only through the exact durable canonical prefix.
        store.note_persisted_canonical_block(7, B256::repeat_byte(0x77)).unwrap();
        assert!(matches!(
            store.finalize_and_prune(&execution_anchor, block8_hash),
            Err(TelosSidecarError::PersistedCanonicalBlockMissing {
                block_number: 7,
                block_hash,
                ..
            }) if block_hash == block7_hash
        ));
        assert_eq!(store.finalized_coverage().unwrap(), Some(anchor_coverage(&execution_anchor)));
        store.note_persisted_canonical_block(7, block7_hash).unwrap();
        let through7 = store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();
        assert_eq!(
            through7.finalized,
            TelosFinalizedCoverage { block_number: 7, block_hash: block7_hash }
        );
        assert!(store.get_accepted_by_hash(block8_hash).unwrap().is_some());

        store.note_persisted_canonical_block(8, block8_hash).unwrap();
        let through8 = store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();
        assert_eq!(
            through8.finalized,
            TelosFinalizedCoverage { block_number: 8, block_hash: block8_hash }
        );
    }

    #[test]
    fn finalized_pruning_failure_leaves_records_and_marker_untouched() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let canonical7_hash = B256::repeat_byte(0x71);
        let disconnected9_hash = B256::repeat_byte(0x99);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let canonical7 = sidecar(canonical7_hash, snapshot_hash, 7);
        let disconnected9 = sidecar(disconnected9_hash, B256::repeat_byte(0x98), 9);
        accept(&store, &canonical7);
        accept(&store, &disconnected9);

        assert!(matches!(
            store.finalize_and_prune(&execution_anchor, disconnected9_hash),
            Err(TelosSidecarError::FinalizedSidecarMissing { .. })
        ));
        assert_eq!(store.finalized_coverage().unwrap(), None);
        assert!(store.get_record_by_hash(canonical7_hash).unwrap().is_some());
        assert!(store.get_record_by_hash(disconnected9_hash).unwrap().is_some());
    }

    #[test]
    fn number_index_preserves_competing_forks_in_hash_order() {
        let store: Box<dyn TelosSidecarStore> = Box::new(InMemoryTelosSidecarStore::new(chain()));
        let parent_hash = B256::repeat_byte(0xbb);
        let higher_hash = B256::repeat_byte(0x22);
        let lower_hash = B256::repeat_byte(0x11);
        store.put_pending(&sidecar(higher_hash, parent_hash, 7)).unwrap();
        store.put_pending(&sidecar(lower_hash, parent_hash, 7)).unwrap();

        let sidecars = store.get_records_by_number(7).unwrap();
        assert_eq!(sidecars.len(), 2);
        assert_eq!(sidecars[0].sidecar().envelope().block_hash, lower_hash);
        assert_eq!(sidecars[1].sidecar().envelope().block_hash, higher_hash);
    }

    #[test]
    fn atomic_ingress_commits_dispatched_state_and_freezes_the_exact_digest() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let block_hash = B256::repeat_byte(0x71);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let candidate = sidecar(block_hash, snapshot_hash, 7);

        assert_eq!(
            store.validate_and_mark_dispatched(&execution_anchor, &candidate).unwrap(),
            TelosSidecarDispatchOutcome::Dispatched
        );
        assert_eq!(store.get_pending_by_hash(block_hash).unwrap(), None);
        assert_eq!(store.get_dispatched_by_hash(block_hash).unwrap(), Some(candidate.clone()));
        assert_eq!(
            store.validate_and_mark_dispatched(&execution_anchor, &candidate).unwrap(),
            TelosSidecarDispatchOutcome::AlreadyDispatched
        );

        let conflict = TelosExecutionSidecar::new(
            chain(),
            7,
            block_hash,
            snapshot_hash,
            2,
            42,
            fields(block_hash, snapshot_hash, 99),
        )
        .unwrap();
        assert!(matches!(
            store.validate_and_mark_dispatched(&execution_anchor, &conflict),
            Err(TelosSidecarError::CandidateInFlight { .. })
        ));
        assert_eq!(store.get_dispatched_by_hash(block_hash).unwrap(), Some(candidate));
    }

    #[test]
    fn failed_atomic_ingress_preserves_pending_candidate_and_indexes() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let block_hash = B256::repeat_byte(0x71);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let original = sidecar(block_hash, snapshot_hash, 7);
        store.put_pending(&original).unwrap();

        let mut wrong_fields = fields(block_hash, snapshot_hash, 2);
        wrong_fields.execution.as_mut().unwrap().starting_revision = 9;
        let invalid =
            TelosExecutionSidecar::new(chain(), 7, block_hash, snapshot_hash, 2, 42, wrong_fields)
                .unwrap();
        assert!(matches!(
            store.validate_and_mark_dispatched(&execution_anchor, &invalid),
            Err(TelosSidecarError::RevisionContinuity { expected: 1, actual: 9, .. })
        ));

        let record = store.get_record_by_hash(block_hash).unwrap().unwrap();
        assert_eq!(record.state(), TelosSidecarState::Pending);
        assert_eq!(record.sidecar(), &original);
        assert_eq!(store.get_records_by_number(7).unwrap().len(), 1);
    }

    #[test]
    fn atomic_ingress_requires_a_direct_child_to_extend_durable_coverage() {
        let snapshot_hash = B256::repeat_byte(0x60);
        let finalized_hash = B256::repeat_byte(0x71);
        let child_hash = B256::repeat_byte(0x72);
        let wrong_parent = B256::repeat_byte(0x81);
        let execution_anchor = anchor(6, snapshot_hash);
        let store = InMemoryTelosSidecarStore::new(chain());
        let finalized = sidecar(finalized_hash, snapshot_hash, 7);
        store.validate_and_mark_dispatched(&execution_anchor, &finalized).unwrap();
        store.mark_accepted(finalized_hash, finalized.digest()).unwrap();
        store.state.write().unwrap().finalized =
            Some(TelosFinalizedCoverage { block_number: 7, block_hash: finalized_hash });

        let child = sidecar(child_hash, wrong_parent, 8);
        assert!(matches!(
            store.validate_and_mark_dispatched(&execution_anchor, &child),
            Err(TelosSidecarError::CandidateCoverageParentMismatch {
                expected_parent,
                actual_parent,
                ..
            }) if expected_parent == finalized_hash && actual_parent == wrong_parent
        ));
        assert_eq!(store.get_record_by_hash(child_hash).unwrap(), None);
        assert!(store.get_records_by_number(8).unwrap().is_empty());
    }

    #[test]
    fn atomic_ingress_and_invalidation_never_leave_an_orphaned_dispatched_child() {
        use std::{sync::Barrier, thread};

        for round in 0..16u8 {
            let snapshot_hash = B256::repeat_byte(0x60);
            let parent_hash = B256::repeat_byte(0x70 + round);
            let child_hash = B256::repeat_byte(0x90 + round);
            let execution_anchor = anchor(6, snapshot_hash);
            let store = Arc::new(InMemoryTelosSidecarStore::new(chain()));
            let parent = sidecar(parent_hash, snapshot_hash, 7);
            let child = sidecar(child_hash, parent_hash, 8);
            store.validate_and_mark_dispatched(&execution_anchor, &parent).unwrap();

            let barrier = Arc::new(Barrier::new(2));
            let removal_store = Arc::clone(&store);
            let removal_barrier = Arc::clone(&barrier);
            let removal = thread::spawn(move || {
                removal_barrier.wait();
                removal_store.remove_pending(parent_hash, parent.digest())
            });
            let ingress_store = Arc::clone(&store);
            let ingress_barrier = Arc::clone(&barrier);
            let ingress = thread::spawn(move || {
                ingress_barrier.wait();
                ingress_store.validate_and_mark_dispatched(&execution_anchor, &child)
            });

            assert_eq!(removal.join().unwrap().unwrap(), TelosSidecarRemoveOutcome::RemovedPending);
            match ingress.join().unwrap() {
                Ok(TelosSidecarDispatchOutcome::Dispatched) |
                Err(TelosSidecarError::MissingParentSidecar { .. }) => {}
                outcome => panic!("unexpected atomic ingress race outcome: {outcome:?}"),
            }
            assert_eq!(store.get_record_by_hash(parent_hash).unwrap(), None);
            assert_eq!(store.get_record_by_hash(child_hash).unwrap(), None);
            assert!(store.get_records_by_number(8).unwrap().is_empty());
        }
    }

    #[test]
    fn continuity_requires_anchor_then_inherits_exact_parent_child_context() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let snapshot_hash = B256::repeat_byte(0x99);
        let first_hash = B256::repeat_byte(0xaa);
        let second_hash = B256::repeat_byte(0xbb);
        let anchor = anchor(6, snapshot_hash);

        let mut first_fields = fields(first_hash, snapshot_hash, 2);
        let first_execution = first_fields.execution.as_mut().unwrap();
        first_execution.gas_price_changes =
            vec![reth_telos_rpc_engine_api::structs::TelosExecutionChange {
                boundary: 2,
                value: U256::from(9),
            }];
        first_execution.revision_changes =
            vec![reth_telos_rpc_engine_api::structs::TelosExecutionChange {
                boundary: 2,
                value: 2,
            }];
        let first =
            TelosExecutionSidecar::new(chain(), 7, first_hash, snapshot_hash, 2, 42, first_fields)
                .unwrap();
        validate_sidecar_continuity(&store, &anchor, &first).unwrap();
        store.put_pending(&first).unwrap();
        store.mark_dispatched(first_hash, first.digest()).unwrap();

        let mut second_fields = fields(second_hash, first_hash, 2);
        let second_execution = second_fields.execution.as_mut().unwrap();
        second_execution.starting_gas_price = U256::from(9);
        second_execution.starting_revision = 2;
        let second = TelosExecutionSidecar::new(
            chain(),
            8,
            second_hash,
            first_hash,
            2,
            42,
            second_fields.clone(),
        )
        .unwrap();
        validate_sidecar_continuity(&store, &anchor, &second).unwrap();
        assert!(matches!(
            validate_accepted_sidecar_continuity(&store, &anchor, &second),
            Err(TelosSidecarError::MissingParentSidecar { .. })
        ));
        store.mark_accepted(first_hash, first.digest()).unwrap();
        validate_accepted_sidecar_continuity(&store, &anchor, &second).unwrap();

        second_fields.execution.as_mut().unwrap().starting_revision = 1;
        let wrong =
            TelosExecutionSidecar::new(chain(), 8, second_hash, first_hash, 2, 42, second_fields)
                .unwrap();
        assert!(matches!(
            validate_sidecar_continuity(&store, &anchor, &wrong),
            Err(TelosSidecarError::RevisionContinuity { expected: 2, actual: 1, .. })
        ));
    }

    #[test]
    fn continuity_rejects_a_missing_non_anchor_parent() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let child = sidecar(B256::repeat_byte(0xaa), B256::repeat_byte(0xbb), 8);
        let error =
            validate_sidecar_continuity(&store, &anchor(6, B256::repeat_byte(0x99)), &child)
                .unwrap_err();
        assert!(matches!(error, TelosSidecarError::MissingParentSidecar { .. }));
    }

    #[test]
    fn hash_lookup_fails_closed_when_height_index_is_missing() {
        let store = InMemoryTelosSidecarStore::new(chain());
        let block_hash = B256::repeat_byte(0xaa);
        let sidecar = sidecar(block_hash, B256::repeat_byte(0xbb), 7);
        store.put_pending(&sidecar).unwrap();
        store.state.write().unwrap().by_number.clear();

        assert!(matches!(
            store.get_record_by_hash(block_hash),
            Err(TelosSidecarError::CorruptIndex {
                block_number: 7,
                block_hash: actual_hash,
                indexed_hash: None,
            }) if actual_hash == block_hash
        ));
    }

    #[test]
    fn integrity_frame_rejects_tampering_and_cross_chain_replay() {
        let sidecar = sidecar(B256::repeat_byte(0xaa), B256::repeat_byte(0xbb), 7);
        let stored = TelosStoredSidecar::pending(sidecar);
        let record = stored.encode_record();
        let decoded = TelosStoredSidecar::decode_record(&record, chain()).unwrap();
        assert_eq!(decoded, stored);

        let mut tampered = record.to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(
            TelosStoredSidecar::decode_record(&tampered, chain()),
            Err(TelosSidecarError::DigestMismatch { .. })
        ));

        let other_chain =
            TelosChainIdentity { chain_id: 41, genesis_hash: B256::repeat_byte(0x41) };
        assert!(matches!(
            TelosStoredSidecar::decode_record(&record, other_chain),
            Err(TelosSidecarError::ChainMismatch { .. })
        ));
    }

    #[test]
    fn decoder_rejects_noncanonical_valid_json() {
        let block_hash = B256::repeat_byte(0xaa);
        let parent_hash = B256::repeat_byte(0xbb);
        let envelope = TelosExecutionSidecarEnvelope {
            version: TELOS_EXECUTION_SIDECAR_VERSION,
            chain: chain(),
            block_number: 7,
            block_hash,
            parent_hash,
            transaction_count: 2,
            gas_used: 42,
            extra_fields: fields(block_hash, parent_hash, 2),
        };
        let noncanonical = serde_json::to_vec(&envelope).unwrap();
        assert!(matches!(
            TelosExecutionSidecar::from_canonical_bytes(&noncanonical),
            Err(TelosSidecarError::NonCanonical)
        ));
    }

    #[test]
    fn envelope_binding_mismatch_is_rejected() {
        let block_hash = B256::repeat_byte(0xaa);
        let parent_hash = B256::repeat_byte(0xbb);
        let result = TelosExecutionSidecar::new(
            chain(),
            7,
            B256::repeat_byte(0xcc),
            parent_hash,
            2,
            42,
            fields(block_hash, parent_hash, 2),
        );
        assert!(matches!(result, Err(TelosSidecarError::Validation(_))));
    }

    #[test]
    fn number_hash_key_encoding_preserves_database_sort_order() {
        let earlier = TelosSidecarNumberHashKey::new(6, B256::repeat_byte(0xff));
        let lower_hash = TelosSidecarNumberHashKey::new(7, B256::repeat_byte(0x11));
        let higher_hash = TelosSidecarNumberHashKey::new(7, B256::repeat_byte(0x22));
        assert!(earlier.encode() < lower_hash.encode());
        assert!(lower_hash.encode() < higher_hash.encode());
        assert_eq!(TelosSidecarNumberHashKey::decode(&higher_hash.encode()).unwrap(), higher_hash);
    }

    #[test]
    fn parent_hash_key_encoding_groups_children_deterministically() {
        let parent = B256::repeat_byte(0x11);
        let lower = TelosSidecarParentHashKey::new(parent, B256::repeat_byte(0x22));
        let higher = TelosSidecarParentHashKey::new(parent, B256::repeat_byte(0x33));
        let next_parent =
            TelosSidecarParentHashKey::new(B256::repeat_byte(0x12), B256::repeat_byte(0x01));
        assert!(lower.encode() < higher.encode());
        assert!(higher.encode() < next_parent.encode());
        assert_eq!(TelosSidecarParentHashKey::decode(&higher.encode()).unwrap(), higher);
    }

    #[test]
    fn mdbx_sidecar_cursors_work_with_database_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db");
        let parent_hash = B256::repeat_byte(0x60);
        let block_hash = B256::repeat_byte(0x71);
        let pending = sidecar(block_hash, parent_hash, 7);

        let mut database =
            Arc::new(init_db(&database_path, DatabaseArguments::default()).unwrap().with_metrics());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(database, chain());

        assert_eq!(store.put_pending(&pending).unwrap(), TelosSidecarPutOutcome::InsertedPending);
        assert_eq!(store.get_records_by_number(7).unwrap().len(), 1);
        assert_eq!(store.finalized_coverage().unwrap(), None);
        assert_eq!(
            store.remove_pending(block_hash, pending.digest()).unwrap(),
            TelosSidecarRemoveOutcome::RemovedPending
        );
        assert!(store.get_records_by_number(7).unwrap().is_empty());
    }

    #[test]
    fn mdbx_forkchoice_preflight_uses_one_accepted_ancestry_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db");
        let snapshot_hash = B256::repeat_byte(0x60);
        let block7_hash = B256::repeat_byte(0x71);
        let block8_hash = B256::repeat_byte(0x72);
        let pending7_hash = B256::repeat_byte(0x81);
        let execution_anchor = anchor(6, snapshot_hash);
        let block7 = sidecar(block7_hash, snapshot_hash, 7);
        let block8 = sidecar(block8_hash, block7_hash, 8);
        let pending7 = sidecar(pending7_hash, snapshot_hash, 7);

        let mut database = Arc::new(init_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(database, chain());
        accept(&store, &block7);
        accept(&store, &block8);
        store.put_pending(&pending7).unwrap();

        store
            .validate_forkchoice_state(&execution_anchor, block8_hash, block7_hash, snapshot_hash)
            .unwrap();
        assert!(matches!(
            store.validate_forkchoice_state(
                &execution_anchor,
                block8_hash,
                pending7_hash,
                snapshot_hash,
            ),
            Err(TelosSidecarError::ForkchoiceSidecarNotAccepted {
                role: "safe",
                state: TelosSidecarState::Pending,
                ..
            })
        ));
    }

    #[test]
    fn mdbx_atomic_ingress_failure_preserves_the_original_pending_record() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db");
        let snapshot_hash = B256::repeat_byte(0x60);
        let block_hash = B256::repeat_byte(0x71);
        let execution_anchor = anchor(6, snapshot_hash);
        let original = sidecar(block_hash, snapshot_hash, 7);

        let mut database = Arc::new(init_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(Arc::clone(&database), chain());
        store.put_pending(&original).unwrap();

        let mut wrong_fields = fields(block_hash, snapshot_hash, 2);
        wrong_fields.execution.as_mut().unwrap().starting_revision = 9;
        let invalid =
            TelosExecutionSidecar::new(chain(), 7, block_hash, snapshot_hash, 2, 42, wrong_fields)
                .unwrap();
        assert!(matches!(
            store.validate_and_mark_dispatched(&execution_anchor, &invalid),
            Err(TelosSidecarError::RevisionContinuity { expected: 1, actual: 9, .. })
        ));
        drop(store);
        drop(database);

        let mut database = Arc::new(open_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(database, chain());
        let record = store.get_record_by_hash(block_hash).unwrap().unwrap();
        assert_eq!(record.state(), TelosSidecarState::Pending);
        assert_eq!(record.sidecar(), &original);
        assert_eq!(store.get_records_by_number(7).unwrap().len(), 1);
        assert_eq!(
            store.validate_and_mark_dispatched(&execution_anchor, &original).unwrap(),
            TelosSidecarDispatchOutcome::Dispatched
        );
        assert_eq!(
            store.validate_and_mark_dispatched(&execution_anchor, &original).unwrap(),
            TelosSidecarDispatchOutcome::AlreadyDispatched
        );
    }

    #[test]
    fn mdbx_atomic_ingress_racing_invalidation_persists_no_orphan() {
        use std::{sync::Barrier, thread};

        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db");
        let snapshot_hash = B256::repeat_byte(0x60);
        let parent_hash = B256::repeat_byte(0x71);
        let child_hash = B256::repeat_byte(0x72);
        let execution_anchor = anchor(6, snapshot_hash);
        let parent = sidecar(parent_hash, snapshot_hash, 7);
        let child = sidecar(child_hash, parent_hash, 8);

        let mut database = Arc::new(init_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = Arc::new(DatabaseTelosSidecarStore::new(Arc::clone(&database), chain()));
        store.validate_and_mark_dispatched(&execution_anchor, &parent).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let removal_store = Arc::clone(&store);
        let removal_barrier = Arc::clone(&barrier);
        let removal = thread::spawn(move || {
            removal_barrier.wait();
            removal_store.remove_pending(parent_hash, parent.digest())
        });
        let ingress_store = Arc::clone(&store);
        let ingress_barrier = Arc::clone(&barrier);
        let ingress = thread::spawn(move || {
            ingress_barrier.wait();
            ingress_store.validate_and_mark_dispatched(&execution_anchor, &child)
        });

        assert_eq!(removal.join().unwrap().unwrap(), TelosSidecarRemoveOutcome::RemovedPending);
        match ingress.join().unwrap() {
            Ok(TelosSidecarDispatchOutcome::Dispatched) |
            Err(TelosSidecarError::MissingParentSidecar { .. }) => {}
            outcome => panic!("unexpected MDBX ingress race outcome: {outcome:?}"),
        }
        drop(store);
        drop(database);

        let mut database = Arc::new(open_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(database, chain());
        assert_eq!(store.get_record_by_hash(parent_hash).unwrap(), None);
        assert_eq!(store.get_record_by_hash(child_hash).unwrap(), None);
        assert!(store.get_records_by_number(8).unwrap().is_empty());
    }

    #[test]
    fn mdbx_restart_preserves_lifecycle_and_accepted_immutability() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db");
        let block_hash = B256::repeat_byte(0xaa);
        let fork_hash = B256::repeat_byte(0xab);
        let fork_child_hash = B256::repeat_byte(0xac);
        let parent_hash = B256::repeat_byte(0xbb);
        let primary = sidecar(block_hash, parent_hash, 7);
        let fork = sidecar(fork_hash, parent_hash, 7);
        let fork_child = sidecar(fork_child_hash, fork_hash, 8);

        let mut database = Arc::new(init_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(Arc::clone(&database), chain());
        assert_eq!(store.put_pending(&primary).unwrap(), TelosSidecarPutOutcome::InsertedPending);
        assert_eq!(store.put_pending(&fork).unwrap(), TelosSidecarPutOutcome::InsertedPending);
        assert_eq!(
            store.put_pending(&fork_child).unwrap(),
            TelosSidecarPutOutcome::InsertedPending
        );
        store.mark_dispatched(block_hash, primary.digest()).unwrap();
        store.mark_dispatched(fork_hash, fork.digest()).unwrap();
        store.mark_dispatched(fork_child_hash, fork_child.digest()).unwrap();
        store.mark_accepted(block_hash, primary.digest()).unwrap();
        drop(store);
        drop(database);

        let mut database = Arc::new(open_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(Arc::clone(&database), chain());

        assert_eq!(store.get_accepted_by_hash(block_hash).unwrap(), Some(primary.clone()));
        assert_eq!(store.get_dispatched_by_hash(fork_hash).unwrap(), Some(fork.clone()));
        assert_eq!(store.get_dispatched_by_hash(fork_child_hash).unwrap(), Some(fork_child));
        let at_height = store.get_records_by_number(7).unwrap();
        assert_eq!(at_height.len(), 2);
        assert_eq!(at_height[0].sidecar().envelope().block_hash, block_hash);
        assert_eq!(at_height[0].state(), TelosSidecarState::Accepted);
        assert_eq!(at_height[1].sidecar().envelope().block_hash, fork_hash);
        assert_eq!(at_height[1].state(), TelosSidecarState::Dispatched);
        assert_eq!(store.put_pending(&primary).unwrap(), TelosSidecarPutOutcome::AlreadyAccepted);

        let fork_poison = TelosExecutionSidecar::new(
            chain(),
            7,
            fork_hash,
            parent_hash,
            2,
            42,
            fields(fork_hash, parent_hash, 99),
        )
        .unwrap();
        assert!(matches!(
            store.put_pending(&fork_poison),
            Err(TelosSidecarError::CandidateInFlight { .. })
        ));
        assert_eq!(store.put_pending(&fork).unwrap(), TelosSidecarPutOutcome::AlreadyDispatched);

        let conflict = TelosExecutionSidecar::new(
            chain(),
            7,
            block_hash,
            parent_hash,
            2,
            42,
            fields(block_hash, parent_hash, 99),
        )
        .unwrap();
        assert!(matches!(
            store.put_pending(&conflict),
            Err(TelosSidecarError::AcceptedImmutable { .. })
        ));
        let tx = database.tx_mut().unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::Finish.to_string(),
            reth_stages_types::StageCheckpoint::new(7),
        )
        .unwrap();
        tx.put::<tables::HeaderNumbers>(block_hash, 7).unwrap();
        tx.commit().unwrap();
        let prune = store.finalize_and_prune(&anchor(6, parent_hash), block_hash).unwrap();
        assert_eq!(prune.removed_records, 2);
        assert!(prune.removed_bytes > 0);
        assert_eq!(prune.retained_canonical_records, 1);
        assert_eq!(store.finalized_coverage().unwrap(), Some(prune.finalized));
        drop(store);
        drop(database);

        // A second reopen proves the rejected conflict and pending deletion were atomic.
        let mut database = Arc::new(open_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(database, chain());
        assert_eq!(store.get_accepted_by_hash(block_hash).unwrap(), Some(primary));
        assert_eq!(store.get_record_by_hash(fork_hash).unwrap(), None);
        assert_eq!(store.get_record_by_hash(fork_child_hash).unwrap(), None);
        assert_eq!(store.get_records_by_number(7).unwrap().len(), 1);
        assert!(store.get_records_by_number(8).unwrap().is_empty());
        assert_eq!(
            store.finalized_coverage().unwrap(),
            Some(TelosFinalizedCoverage { block_number: 7, block_hash })
        );
        assert!(matches!(
            store.put_pending(&fork),
            Err(TelosSidecarError::CandidateBelowFinalizedCoverage { .. })
        ));
    }

    #[test]
    fn mdbx_restart_recovers_when_engine_finality_precedes_block_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("db");
        let snapshot_hash = B256::repeat_byte(0x60);
        let block7_hash = B256::repeat_byte(0x71);
        let block8_hash = B256::repeat_byte(0x72);
        let execution_anchor = anchor(6, snapshot_hash);
        let block7 = sidecar(block7_hash, snapshot_hash, 7);
        let block8 = sidecar(block8_hash, block7_hash, 8);

        let mut database = Arc::new(init_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(Arc::clone(&database), chain());
        accept(&store, &block7);
        accept(&store, &block8);

        // Simulate a crash immediately after FCU VALID but before Reth saves either block.
        let deferred = store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();
        assert_eq!(deferred.finalized, anchor_coverage(&execution_anchor));
        drop(store);
        drop(database);

        let mut database = Arc::new(open_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(Arc::clone(&database), chain());
        assert_eq!(store.finalized_coverage().unwrap(), Some(anchor_coverage(&execution_anchor)));
        assert_eq!(store.get_accepted_by_hash(block7_hash).unwrap(), Some(block7));
        assert_eq!(store.get_accepted_by_hash(block8_hash).unwrap(), Some(block8.clone()));

        // Reth's Finish checkpoint and hash index are the atomic durability proof. Only block 7
        // is visible after this simulated persistence commit, so finality clamps there.
        let tx = database.tx_mut().unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::Finish.to_string(),
            reth_stages_types::StageCheckpoint::new(7),
        )
        .unwrap();
        tx.put::<tables::HeaderNumbers>(block7_hash, 7).unwrap();
        tx.commit().unwrap();
        let through7 = store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();
        assert_eq!(
            through7.finalized,
            TelosFinalizedCoverage { block_number: 7, block_hash: block7_hash }
        );
        drop(store);
        drop(database);

        let mut database = Arc::new(open_db(&database_path, DatabaseArguments::default()).unwrap());
        database.create_tables_for::<TelosSidecarTables>().unwrap();
        let store = DatabaseTelosSidecarStore::new(Arc::clone(&database), chain());
        assert_eq!(
            store.finalized_coverage().unwrap(),
            Some(TelosFinalizedCoverage { block_number: 7, block_hash: block7_hash })
        );
        assert_eq!(store.get_accepted_by_hash(block8_hash).unwrap(), Some(block8));

        let tx = database.tx_mut().unwrap();
        tx.put::<tables::StageCheckpoints>(
            StageId::Finish.to_string(),
            reth_stages_types::StageCheckpoint::new(8),
        )
        .unwrap();
        tx.put::<tables::HeaderNumbers>(block8_hash, 8).unwrap();
        tx.commit().unwrap();
        let through8 = store.finalize_and_prune(&execution_anchor, block8_hash).unwrap();
        assert_eq!(
            through8.finalized,
            TelosFinalizedCoverage { block_number: 8, block_hash: block8_hash }
        );
    }
}

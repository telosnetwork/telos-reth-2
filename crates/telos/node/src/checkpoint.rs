//! Telos checkpoint manifest and audit records.
//!
//! Historical Telos headers expose the empty trie root even though the EVM state is non-empty.
//! A checkpoint therefore binds two different roots: the placeholder in the canonical header and
//! an independently pinned root computed from the exported accounts, storage, and bytecode.

use crate::{
    chainspec::{TELOS_MAINNET, TELOS_TESTNET},
    sidecar::{TelosChainIdentity, TelosExecutionAnchor},
};
use alloy_consensus::{Header, EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
use alloy_primitives::{b256, hex, Bloom, B256, U256};
use alloy_rlp::Decodable;
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_primitives_traits::SealedHeader;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

/// Current checkpoint manifest schema.
pub const TELOS_CHECKPOINT_MANIFEST_VERSION: u8 = 2;
/// Current completed-import audit schema.
pub const TELOS_CHECKPOINT_AUDIT_VERSION: u8 = 2;
/// Prefix accepted by [`crate::chainspec::TelosChainSpecParser`] for checkpoint manifests.
pub const TELOS_CHECKPOINT_CHAIN_PREFIX: &str = "telos-checkpoint:";

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const TELOS_MAINNET_NATIVE_CHAIN_ID: B256 =
    b256!("4667b205c6838ef70ff7988f6e8257e8be0e1284a2f59699054a018f743b1d11");
const TELOS_TESTNET_NATIVE_CHAIN_ID: B256 =
    b256!("1eaa0824707c8c16bd25145493bf062aecddfeb56c736f6ba6397f3195f33c9f");

/// Native-chain boundary cryptographically cross-checked against the sparse EVM checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosNativeCheckpointAnchor {
    /// Exact Antelope chain ID reported by both SHIP and nodeos HTTP.
    pub chain_id: B256,
    /// Native block embedded in the sparse EVM anchor header's `extraData`.
    pub block_number: u32,
    /// Exact native anchor block ID.
    pub block_id: B256,
    /// First native child consumed by the companion after bootstrap.
    pub first_child_block_number: u32,
    /// Exact first native child block ID.
    pub first_child_block_id: B256,
    /// Expected EVM hash produced for that first native child.
    pub evm_first_child_block_hash: B256,
    /// Native gas price effective before transaction zero of the first child.
    pub starting_gas_price: U256,
    /// Native revision effective before transaction zero of the first child.
    pub starting_revision: u64,
}

/// Trusted inputs for importing a nonzero Telos checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosCheckpointManifest {
    /// Manifest schema version.
    pub version: u8,
    /// Identity of the public Telos chain from canonical block zero.
    pub canonical_chain: TelosChainIdentity,
    /// Execution boundary and DB-local checkpoint identity.
    ///
    /// The checkpoint header is Reth's first available block, so its hash is also the genesis hash
    /// of this sparse database. `canonical_chain.genesis_hash` retains the public block-zero
    /// identity and prevents a checkpoint from being relabeled as another network.
    pub execution_anchor: TelosExecutionAnchor,
    /// Canonical anchor header encoded as one complete RLP item.
    pub header_rlp: String,
    /// SHA-256 of the decoded RLP bytes.
    pub header_rlp_sha256: B256,
    /// SHA-256 of the exact JSONL state dump bytes.
    pub state_dump_sha256: B256,
    /// SHA-256 of the exact export metadata that linked the state dump to its source copy.
    pub export_metadata_sha256: B256,
    /// SHA-256 of the authenticated native/EVM anchor cross-binding evidence.
    pub native_anchor_attestation_sha256: B256,
    /// Exact irreversible native boundary used by the companion on its first request.
    pub native_anchor: TelosNativeCheckpointAnchor,
    /// SHA-256 of the verified MDBX-copy manifest consumed by the exporter.
    pub backup_manifest_sha256: B256,
    /// SHA-256 of the copied `mdbx.dat` consumed by the exporter.
    pub backup_mdbx_sha256: B256,
    /// Real Ethereum trie root independently pinned for the exported state.
    pub actual_state_root: B256,
}

impl TelosCheckpointManifest {
    /// Loads a size-bounded manifest from a regular file.
    pub fn load(path: &Path) -> eyre::Result<(Self, B256)> {
        let metadata = reth_fs_util::metadata(path)?;
        if !metadata.is_file() {
            eyre::bail!("Telos checkpoint manifest is not a regular file: {}", path.display());
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            eyre::bail!(
                "Telos checkpoint manifest exceeds {MAX_MANIFEST_BYTES} bytes: {}",
                path.display()
            );
        }

        let bytes = reth_fs_util::read(path)?;
        let file_sha256 = sha256(&bytes);
        let manifest = serde_json::from_slice(&bytes).map_err(|error| {
            eyre::eyre!("invalid Telos checkpoint manifest {}: {error}", path.display())
        })?;
        Ok((manifest, file_sha256))
    }

    /// Validates all chain, anchor, header, and root bindings and returns the canonical header.
    pub fn validate(&self) -> eyre::Result<Header> {
        if self.version != TELOS_CHECKPOINT_MANIFEST_VERSION {
            eyre::bail!(
                "unsupported Telos checkpoint manifest version {}; expected {}",
                self.version,
                TELOS_CHECKPOINT_MANIFEST_VERSION
            );
        }

        let canonical_spec = match self.canonical_chain.chain_id {
            40 => TELOS_MAINNET.clone(),
            41 => TELOS_TESTNET.clone(),
            chain_id => {
                eyre::bail!("unsupported Telos checkpoint chain ID {chain_id}");
            }
        };
        if self.canonical_chain.genesis_hash != canonical_spec.genesis_hash() {
            eyre::bail!(
                "canonical Telos genesis mismatch for chain {}: manifest {}, expected {}",
                self.canonical_chain.chain_id,
                self.canonical_chain.genesis_hash,
                canonical_spec.genesis_hash()
            );
        }

        self.execution_anchor
            .validate_for_chain(self.execution_anchor.chain)
            .map_err(|error| eyre::eyre!("invalid Telos execution anchor: {error}"))?;
        if self.execution_anchor.chain.chain_id != self.canonical_chain.chain_id {
            eyre::bail!(
                "checkpoint chain ID {} does not match canonical chain ID {}",
                self.execution_anchor.chain.chain_id,
                self.canonical_chain.chain_id
            );
        }
        if self.execution_anchor.parent_block_number == 0 {
            eyre::bail!("Telos checkpoint anchor must be a nonzero block");
        }
        if self.execution_anchor.chain.genesis_hash != self.execution_anchor.parent_block_hash {
            eyre::bail!(
                "sparse checkpoint database genesis {} must equal anchor hash {}",
                self.execution_anchor.chain.genesis_hash,
                self.execution_anchor.parent_block_hash
            );
        }
        if self.actual_state_root == EMPTY_ROOT_HASH || self.actual_state_root == B256::ZERO {
            eyre::bail!(
                "Telos checkpoint actual state root must be a nonzero, non-empty trie root"
            );
        }
        if [
            self.header_rlp_sha256,
            self.state_dump_sha256,
            self.export_metadata_sha256,
            self.native_anchor_attestation_sha256,
            self.backup_manifest_sha256,
            self.backup_mdbx_sha256,
        ]
        .contains(&B256::ZERO)
        {
            eyre::bail!("Telos checkpoint manifest contains a missing SHA-256 provenance digest");
        }

        let header_bytes = decode_header_rlp(&self.header_rlp)?;
        let header_digest = sha256(&header_bytes);
        if header_digest != self.header_rlp_sha256 {
            eyre::bail!(
                "Telos checkpoint header SHA-256 mismatch: manifest {}, decoded bytes {}",
                self.header_rlp_sha256,
                header_digest
            );
        }
        let mut input = header_bytes.as_slice();
        let header = Header::decode(&mut input)
            .map_err(|error| eyre::eyre!("invalid Telos checkpoint header RLP: {error}"))?;
        if !input.is_empty() {
            eyre::bail!("Telos checkpoint header file contains trailing bytes");
        }

        let header_hash = header.hash_slow();
        if header_hash != self.execution_anchor.parent_block_hash {
            eyre::bail!(
                "Telos checkpoint header hash mismatch: header {}, anchor {}",
                header_hash,
                self.execution_anchor.parent_block_hash
            );
        }
        if header.number != self.execution_anchor.parent_block_number {
            eyre::bail!(
                "Telos checkpoint header number {} does not match anchor {}",
                header.number,
                self.execution_anchor.parent_block_number
            );
        }
        validate_sparse_anchor_header(&header)?;
        self.validate_native_anchor(&header)?;

        Ok(header)
    }

    fn validate_native_anchor(&self, header: &Header) -> eyre::Result<()> {
        let expected_chain_id = match self.canonical_chain.chain_id {
            40 => TELOS_MAINNET_NATIVE_CHAIN_ID,
            41 => TELOS_TESTNET_NATIVE_CHAIN_ID,
            _ => unreachable!("canonical chain validated above"),
        };
        if self.native_anchor.chain_id != expected_chain_id {
            eyre::bail!(
                "native Telos chain mismatch: manifest {}, expected {}",
                self.native_anchor.chain_id,
                expected_chain_id
            );
        }
        if self.native_anchor.block_number == 0 ||
            self.native_anchor.first_child_block_number !=
                self.native_anchor.block_number.checked_add(1).ok_or_else(|| {
                    eyre::eyre!("native checkpoint block number has no representable child")
                })?
        {
            eyre::bail!("native checkpoint first child is not the exact anchor successor");
        }
        let embedded_block_id = B256::try_from(header.extra_data.as_ref()).map_err(|_| {
            eyre::eyre!("Telos checkpoint header extraData is not an exact native block ID")
        })?;
        if embedded_block_id != self.native_anchor.block_id {
            eyre::bail!(
                "checkpoint header native block ID {} does not match attested {}",
                embedded_block_id,
                self.native_anchor.block_id
            );
        }
        for (label, number, id) in [
            ("anchor", self.native_anchor.block_number, self.native_anchor.block_id),
            (
                "first child",
                self.native_anchor.first_child_block_number,
                self.native_anchor.first_child_block_id,
            ),
        ] {
            let encoded_number = u32::from_be_bytes(
                id.as_slice()[..4].try_into().expect("B256 always contains four prefix bytes"),
            );
            if encoded_number != number {
                eyre::bail!("native {label} ID encodes block {encoded_number}, expected {number}");
            }
        }
        if self.native_anchor.first_child_block_id == self.native_anchor.block_id ||
            self.native_anchor.evm_first_child_block_hash == B256::ZERO ||
            self.native_anchor.evm_first_child_block_hash ==
                self.execution_anchor.parent_block_hash
        {
            eyre::bail!("native checkpoint first-child binding is missing");
        }
        if self.native_anchor.starting_gas_price != self.execution_anchor.starting_gas_price ||
            self.native_anchor.starting_revision != self.execution_anchor.starting_revision
        {
            eyre::bail!(
                "native checkpoint first-child execution context does not match the execution anchor"
            );
        }
        Ok(())
    }

    /// Builds the sparse chain specification used by both bootstrap and subsequent node starts.
    pub fn checkpoint_chain_spec(&self) -> eyre::Result<Arc<ChainSpec>> {
        let header = self.validate()?;
        let mut spec = match self.canonical_chain.chain_id {
            40 => TELOS_MAINNET.as_ref().clone(),
            41 => TELOS_TESTNET.as_ref().clone(),
            _ => unreachable!("validated above"),
        };

        // The public chain identity remains pinned in the manifest. Reth's storage model treats
        // the first available sparse header as this database's genesis, allowing static files to
        // begin at the checkpoint instead of materializing hundreds of millions of dummy blocks.
        spec.genesis.number = Some(header.number);
        spec.genesis.parent_hash = Some(header.parent_hash);
        spec.genesis.timestamp = header.timestamp;
        spec.genesis_header = SealedHeader::new(header, self.execution_anchor.parent_block_hash);
        Ok(Arc::new(spec))
    }

    /// Verifies the SHA-256 of the exact state dump without loading it into memory.
    pub fn verify_state_dump(&self, path: &Path) -> eyre::Result<()> {
        let metadata = reth_fs_util::metadata(path)?;
        if !metadata.is_file() {
            eyre::bail!("Telos state dump is not a regular file: {}", path.display());
        }

        let actual = sha256_reader(BufReader::new(File::open(path)?))?;
        if actual != self.state_dump_sha256 {
            eyre::bail!(
                "Telos state dump SHA-256 mismatch for {}: manifest {}, file {}",
                path.display(),
                self.state_dump_sha256,
                actual
            );
        }
        Ok(())
    }
}

/// Verifies that the anchor can be represented by Reth's empty checkpoint body.
///
/// Requiring a finalized, transaction-free EVM block prevents the sparse database from serving an
/// empty body whose transaction or receipt roots contradict its canonical anchor header.
pub fn validate_sparse_anchor_header(header: &Header) -> eyre::Result<()> {
    if header.number == 0 {
        eyre::bail!("Telos checkpoint anchor must be a nonzero block");
    }
    if header.state_root != EMPTY_ROOT_HASH {
        eyre::bail!(
            "Telos checkpoint canonical header must carry EMPTY_ROOT_HASH, got {}",
            header.state_root
        );
    }
    if header.transactions_root != EMPTY_ROOT_HASH ||
        header.receipts_root != EMPTY_ROOT_HASH ||
        header.ommers_hash != EMPTY_OMMER_ROOT_HASH ||
        header.gas_used != 0 ||
        header.logs_bloom != Bloom::ZERO
    {
        eyre::bail!(
            "Telos sparse checkpoint anchor must have an empty transaction, receipt, and ommer body"
        );
    }
    if header.base_fee_per_gas.is_some() ||
        header.withdrawals_root.is_some() ||
        header.blob_gas_used.is_some() ||
        header.excess_blob_gas.is_some() ||
        header.parent_beacon_block_root.is_some() ||
        header.requests_hash.is_some() ||
        header.block_access_list_hash.is_some() ||
        header.slot_number.is_some()
    {
        eyre::bail!(
            "Telos sparse checkpoint anchor contains unsupported post-Berlin header fields"
        );
    }
    Ok(())
}

/// Durable evidence written only after the entire state was imported and its trie root verified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelosCheckpointAudit {
    /// Audit schema version.
    pub version: u8,
    /// SHA-256 of the exact trusted manifest bytes.
    pub manifest_sha256: B256,
    /// Public Telos block-zero identity.
    pub canonical_chain: TelosChainIdentity,
    /// Sparse database identity and execution boundary.
    pub execution_anchor: TelosExecutionAnchor,
    /// SHA-256 of the exact imported JSONL bytes.
    pub state_dump_sha256: B256,
    /// SHA-256 of the exact export metadata used to construct the trusted manifest.
    pub export_metadata_sha256: B256,
    /// SHA-256 of the native/EVM anchor cross-binding evidence.
    pub native_anchor_attestation_sha256: B256,
    /// Native checkpoint boundary consumed by the companion.
    pub native_anchor: TelosNativeCheckpointAnchor,
    /// SHA-256 of the source MDBX-copy manifest.
    pub backup_manifest_sha256: B256,
    /// SHA-256 of the copied source `mdbx.dat`.
    pub backup_mdbx_sha256: B256,
    /// Root recomputed from the imported state.
    pub computed_state_root: B256,
}

impl TelosCheckpointAudit {
    /// Creates a completed audit record after a verified import.
    pub fn completed(
        manifest: &TelosCheckpointManifest,
        manifest_sha256: B256,
        computed_state_root: B256,
    ) -> eyre::Result<Self> {
        if computed_state_root != manifest.actual_state_root {
            eyre::bail!(
                "recomputed checkpoint state root {computed_state_root} does not match manifest {}",
                manifest.actual_state_root
            );
        }
        Ok(Self {
            version: TELOS_CHECKPOINT_AUDIT_VERSION,
            manifest_sha256,
            canonical_chain: manifest.canonical_chain,
            execution_anchor: manifest.execution_anchor,
            state_dump_sha256: manifest.state_dump_sha256,
            export_metadata_sha256: manifest.export_metadata_sha256,
            native_anchor_attestation_sha256: manifest.native_anchor_attestation_sha256,
            native_anchor: manifest.native_anchor,
            backup_manifest_sha256: manifest.backup_manifest_sha256,
            backup_mdbx_sha256: manifest.backup_mdbx_sha256,
            computed_state_root,
        })
    }

    /// Loads and verifies the deterministic completion record next to a manifest.
    pub fn load_completed(manifest_path: &Path) -> eyre::Result<TelosCheckpointManifest> {
        Ok(Self::load_completed_with_sha256(manifest_path)?.0)
    }

    /// Loads and verifies the deterministic completion record, returning the exact manifest
    /// digest from the same bounded read used for audit validation.
    pub fn load_completed_with_sha256(
        manifest_path: &Path,
    ) -> eyre::Result<(TelosCheckpointManifest, B256)> {
        let (manifest, manifest_sha256) = TelosCheckpointManifest::load(manifest_path)?;
        manifest.validate()?;

        let audit_path = checkpoint_audit_path(manifest_path);
        let metadata = reth_fs_util::metadata(&audit_path).map_err(|error| {
            eyre::eyre!(
                "missing completed Telos checkpoint audit {}: {error}",
                audit_path.display()
            )
        })?;
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            eyre::bail!("invalid completed Telos checkpoint audit {}", audit_path.display());
        }
        let actual: Self = reth_fs_util::read_json_file(&audit_path)?;
        let expected = Self::completed(&manifest, manifest_sha256, manifest.actual_state_root)?;
        if actual != expected {
            eyre::bail!(
                "completed Telos checkpoint audit {} does not match manifest {}",
                audit_path.display(),
                manifest_path.display()
            );
        }
        Ok((manifest, manifest_sha256))
    }
}

/// Returns the manifest path from a `telos-checkpoint:<path>` chain selector.
pub fn checkpoint_manifest_path(selector: &str) -> Option<PathBuf> {
    selector
        .strip_prefix(TELOS_CHECKPOINT_CHAIN_PREFIX)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

/// Deterministic completion-marker path required by the checkpoint chain parser.
pub fn checkpoint_audit_path(manifest_path: &Path) -> PathBuf {
    manifest_path.with_extension("audit.json")
}

/// Deterministic execution-anchor artifact written alongside a checkpoint manifest.
pub fn checkpoint_execution_anchor_path(manifest_path: &Path) -> PathBuf {
    manifest_path.with_extension("anchor.json")
}

fn decode_header_rlp(encoded: &str) -> eyre::Result<Vec<u8>> {
    let encoded = encoded.strip_prefix("0x").unwrap_or(encoded);
    if encoded.len() > MAX_HEADER_BYTES * 2 {
        eyre::bail!("Telos checkpoint header exceeds {MAX_HEADER_BYTES} decoded bytes");
    }
    if !encoded.len().is_multiple_of(2) {
        eyre::bail!("Telos checkpoint header RLP hex has an odd length");
    }
    hex::decode(encoded).map_err(|error| eyre::eyre!("invalid checkpoint header RLP hex: {error}"))
}

fn sha256(bytes: &[u8]) -> B256 {
    B256::from(<[u8; 32]>::from(Sha256::digest(bytes)))
}

fn sha256_reader(mut reader: impl Read) -> eyre::Result<B256> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break
        }
        hasher.update(&buffer[..read]);
    }
    Ok(B256::from(<[u8; 32]>::from(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::TELOS_EXECUTION_ANCHOR_VERSION;
    use alloy_rlp::Encodable;

    fn manifest() -> TelosCheckpointManifest {
        let mut header = Header { number: 7, state_root: EMPTY_ROOT_HASH, ..Default::default() };
        let native_block_number = 43u32;
        let mut native_block_bytes = [0x11; 32];
        native_block_bytes[..4].copy_from_slice(&native_block_number.to_be_bytes());
        let native_block_id = B256::from(native_block_bytes);
        let mut native_child_bytes = [0x22; 32];
        native_child_bytes[..4].copy_from_slice(&(native_block_number + 1).to_be_bytes());
        let native_child_id = B256::from(native_child_bytes);
        header.extra_data = native_block_id.to_vec().into();
        // Distinguish this checkpoint hash from the empty default header.
        header.timestamp = 1_700_000_000;
        let header_hash = header.hash_slow();
        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);

        TelosCheckpointManifest {
            version: TELOS_CHECKPOINT_MANIFEST_VERSION,
            canonical_chain: TelosChainIdentity {
                chain_id: 40,
                genesis_hash: TELOS_MAINNET.genesis_hash(),
            },
            execution_anchor: TelosExecutionAnchor {
                version: TELOS_EXECUTION_ANCHOR_VERSION,
                chain: TelosChainIdentity { chain_id: 40, genesis_hash: header_hash },
                parent_block_number: header.number,
                parent_block_hash: header_hash,
                starting_gas_price: U256::from(7),
                starting_revision: 1,
            },
            header_rlp: format!("0x{}", hex::encode(&header_rlp)),
            header_rlp_sha256: sha256(&header_rlp),
            state_dump_sha256: B256::repeat_byte(0x55),
            export_metadata_sha256: B256::repeat_byte(0x56),
            native_anchor_attestation_sha256: B256::repeat_byte(0x59),
            native_anchor: TelosNativeCheckpointAnchor {
                chain_id: TELOS_MAINNET_NATIVE_CHAIN_ID,
                block_number: native_block_number,
                block_id: native_block_id,
                first_child_block_number: native_block_number + 1,
                first_child_block_id: native_child_id,
                evm_first_child_block_hash: B256::repeat_byte(0x77),
                starting_gas_price: U256::from(7),
                starting_revision: 1,
            },
            backup_manifest_sha256: B256::repeat_byte(0x57),
            backup_mdbx_sha256: B256::repeat_byte(0x58),
            actual_state_root: B256::repeat_byte(0x66),
        }
    }

    #[test]
    fn exact_checkpoint_builds_sparse_chain_spec() {
        let manifest = manifest();
        let header = manifest.validate().unwrap();
        let spec = manifest.checkpoint_chain_spec().unwrap();

        assert_eq!(header.number, 7);
        assert_eq!(spec.chain().id(), 40);
        assert_eq!(spec.genesis().number, Some(7));
        assert_eq!(spec.genesis_hash(), manifest.execution_anchor.parent_block_hash);
        assert_eq!(spec.genesis_header().state_root, EMPTY_ROOT_HASH);
    }

    #[test]
    fn checkpoint_rejects_cross_chain_and_root_relabeling() {
        let mut wrong_chain = manifest();
        wrong_chain.canonical_chain.genesis_hash = TELOS_TESTNET.genesis_hash();
        assert!(wrong_chain.validate().unwrap_err().to_string().contains("genesis mismatch"));

        let mut empty_actual_root = manifest();
        empty_actual_root.actual_state_root = EMPTY_ROOT_HASH;
        assert!(empty_actual_root
            .validate()
            .unwrap_err()
            .to_string()
            .contains("actual state root"));

        let mut wrong_header_digest = manifest();
        wrong_header_digest.header_rlp_sha256 = B256::ZERO;
        assert!(wrong_header_digest.validate().unwrap_err().to_string().contains("SHA-256"));
    }

    #[test]
    fn checkpoint_rejects_native_anchor_substitution() {
        let mut wrong_chain = manifest();
        wrong_chain.native_anchor.chain_id = TELOS_TESTNET_NATIVE_CHAIN_ID;
        assert!(wrong_chain.validate().unwrap_err().to_string().contains("native Telos chain"));

        let mut wrong_id = manifest();
        wrong_id.native_anchor.block_id = B256::repeat_byte(0x44);
        assert!(wrong_id.validate().unwrap_err().to_string().contains("native block ID"));

        let mut wrong_number = manifest();
        wrong_number.native_anchor.block_number += 1;
        wrong_number.native_anchor.first_child_block_number += 1;
        assert!(wrong_number.validate().unwrap_err().to_string().contains("encodes block"));

        let mut skipped_child = manifest();
        skipped_child.native_anchor.first_child_block_number += 1;
        assert!(skipped_child
            .validate()
            .unwrap_err()
            .to_string()
            .contains("exact anchor successor"));

        let mut wrong_child_id = manifest();
        wrong_child_id.native_anchor.first_child_block_id = B256::repeat_byte(0x22);
        assert!(wrong_child_id.validate().unwrap_err().to_string().contains("encodes block"));

        let mut parent_reused_as_child = manifest();
        parent_reused_as_child.native_anchor.evm_first_child_block_hash =
            parent_reused_as_child.execution_anchor.parent_block_hash;
        assert!(parent_reused_as_child
            .validate()
            .unwrap_err()
            .to_string()
            .contains("first-child binding"));
    }

    #[test]
    fn checkpoint_rejects_native_digest_and_context_substitution() {
        let mut missing_attestation = manifest();
        missing_attestation.native_anchor_attestation_sha256 = B256::ZERO;
        assert!(missing_attestation.validate().unwrap_err().to_string().contains("SHA-256"));

        let mut wrong_gas_price = manifest();
        wrong_gas_price.native_anchor.starting_gas_price = U256::from(8);
        assert!(wrong_gas_price.validate().unwrap_err().to_string().contains("execution context"));

        let mut wrong_revision = manifest();
        wrong_revision.native_anchor.starting_revision = 2;
        assert!(wrong_revision.validate().unwrap_err().to_string().contains("execution context"));
    }

    #[test]
    fn sparse_anchor_rejects_a_missing_body() {
        let mut manifest = manifest();
        let mut header = manifest.validate().unwrap();
        header.transactions_root = B256::repeat_byte(0x77);
        let header_hash = header.hash_slow();
        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        manifest.header_rlp = format!("0x{}", hex::encode(&header_rlp));
        manifest.header_rlp_sha256 = sha256(&header_rlp);
        manifest.execution_anchor.parent_block_hash = header_hash;
        manifest.execution_anchor.chain.genesis_hash = header_hash;

        assert!(manifest.validate().unwrap_err().to_string().contains("empty transaction"));
    }

    #[test]
    fn completed_audit_is_bound_to_the_recomputed_root() {
        let manifest = manifest();
        let manifest_sha256 = B256::repeat_byte(0x99);
        let audit =
            TelosCheckpointAudit::completed(&manifest, manifest_sha256, manifest.actual_state_root)
                .unwrap();

        assert_eq!(audit.manifest_sha256, manifest_sha256);
        assert_eq!(audit.computed_state_root, manifest.actual_state_root);
        assert!(TelosCheckpointAudit::completed(
            &manifest,
            manifest_sha256,
            B256::repeat_byte(0x77),
        )
        .unwrap_err()
        .to_string()
        .contains("recomputed checkpoint state root"));
    }
}

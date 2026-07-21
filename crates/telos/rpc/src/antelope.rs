//! Antelope (EOSIO) transaction signing utilities.
//!
//! Implements the minimum protocol surface needed to construct, sign, and
//! submit an `eosio.evm::raw` action wrapping an Ethereum transaction.
//!
//! References:
//! - EOSIO/Leap `fc` signature encoding (`SIG_K1_` base58check with ripemd160("K1") checksum)
//! - `transaction::sig_digest` = sha256(`chain_id` || `packed_trx` || `cfa_hash`) where `cfa_hash`
//!   is 32 zero bytes when there are no context-free actions.

use alloy_primitives::B256;
use ripemd::Ripemd160;
use secp256k1::{ecdsa::RecoveryId, Message, Secp256k1, SecretKey};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

// --- Errors --------------------------------------------------------------

/// Errors produced by the Antelope signing / submission helpers.
#[derive(Debug, thiserror::Error)]
pub enum AntelopeError {
    /// The WIF-encoded private key was malformed or failed the base58check checksum.
    #[error("invalid WIF key: {0}")]
    InvalidWif(&'static str),
    /// An Antelope account/permission/action name did not fit the allowed encoding.
    #[error("invalid name: {0}")]
    InvalidName(&'static str),
    /// Failed to hex-decode a field from a nodeos response (`chain_id`, `block_id`, ...).
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    /// secp256k1 signing failed (for example, could not find a canonical signature).
    #[error("signing error: {0}")]
    Signing(&'static str),
    /// Underlying HTTP transport error while talking to nodeos.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// Failed to (de)serialize a nodeos JSON payload.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Nodeos returned a non-2xx HTTP status; carries the status code and response body.
    #[error("nodeos error {status}: {body}")]
    Nodeos {
        /// HTTP status code returned by nodeos.
        status: u16,
        /// Raw response body returned by nodeos (typically JSON).
        body: String,
    },
    /// A nodeos response exceeded the configured in-memory safety limit.
    #[error("nodeos response exceeded {limit} bytes")]
    ResponseTooLarge {
        /// Maximum accepted response size.
        limit: usize,
    },
    /// The gas-price cache lock was poisoned by a previous panic.
    #[error("gas-price cache is unavailable")]
    CacheUnavailable,
    /// The `last_irreversible_block_id` from `get_info` was not a 32-byte hex blob.
    #[error("bad block_id hex in get_info response")]
    BadBlockId,
}

// --- Name encoding -------------------------------------------------------

/// Encode an EOSIO-style name into a u64.
///
/// The first 12 characters allow `.`, `1`-`5`, and `a`-`z`; the four-bit thirteenth position is
/// restricted to `.`, `1`-`5`, and `a`-`j`.
pub fn name_to_u64(name: &str) -> Result<u64, AntelopeError> {
    let bytes = name.as_bytes();
    if bytes.len() > 13 {
        return Err(AntelopeError::InvalidName("name longer than 13 chars"));
    }
    let mut value: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let sym = char_to_symbol(b)?;
        if i < 12 {
            value |= ((sym as u64) & 0x1F) << (64 - 5 * (i + 1));
        } else {
            // Antelope names reserve only four bits for the thirteenth symbol. Reject values
            // that cannot be represented instead of silently aliasing them through truncation.
            if sym > 0x0F {
                return Err(AntelopeError::InvalidName(
                    "13th character must be '.', '1'-'5', or 'a'-'j'",
                ));
            }
            value |= sym as u64;
        }
    }
    Ok(value)
}

const fn char_to_symbol(c: u8) -> Result<u8, AntelopeError> {
    match c {
        b'.' => Ok(0),
        b'1'..=b'5' => Ok(c - b'1' + 1),
        b'a'..=b'z' => Ok(c - b'a' + 6),
        _ => Err(AntelopeError::InvalidName("invalid name character")),
    }
}

// --- Varint ---------------------------------------------------------------

/// Append an Antelope-style LEB128 variable-length u32 to `out`.
pub fn write_varuint32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

// --- WIF key decoding -----------------------------------------------------

/// Decode an Antelope WIF (version 0x80) private key to a secp256k1 `SecretKey`.
pub fn wif_to_secret_key(wif: &str) -> Result<SecretKey, AntelopeError> {
    let decoded = bs58::decode(wif)
        .into_vec()
        .map_err(|_| AntelopeError::InvalidWif("base58 decode failed"))?;
    if decoded.len() < 5 {
        return Err(AntelopeError::InvalidWif("too short"));
    }
    let (payload, checksum) = decoded.split_at(decoded.len() - 4);
    // Classic WIF checksum: sha256(sha256(payload))[:4]
    let h1 = Sha256::digest(payload);
    let h2 = Sha256::digest(h1);
    if &h2[..4] != checksum {
        return Err(AntelopeError::InvalidWif("checksum mismatch"));
    }
    if payload.is_empty() || payload[0] != 0x80 {
        return Err(AntelopeError::InvalidWif("version byte must be 0x80"));
    }
    // 32-byte private key, optionally followed by 0x01 compression flag
    let priv_bytes = match payload.len() {
        33 => &payload[1..33],
        34 if payload[33] == 0x01 => &payload[1..33],
        _ => return Err(AntelopeError::InvalidWif("unexpected length")),
    };
    SecretKey::from_slice(priv_bytes)
        .map_err(|_| AntelopeError::InvalidWif("invalid secp256k1 key"))
}

// --- K1 signature encoding ------------------------------------------------

/// Encode a recoverable secp256k1 signature as an Antelope `SIG_K1_...` string.
///
/// fc's `compact_signature` layout is `[header(1) || r(32) || s(32)]` where
/// header = 27 + 4 + parity (so 31/32 for recid 0/1).
pub fn sig_k1_encode(rec_id: i32, rs: &[u8; 64]) -> String {
    let mut data = [0u8; 65];
    data[0] = 27 + 4 + rec_id as u8;
    data[1..].copy_from_slice(rs);

    // Checksum = ripemd160(sig || "K1")[:4]
    let mut hasher = Ripemd160::new();
    hasher.update(data);
    hasher.update(b"K1");
    let checksum = hasher.finalize();

    let mut out = Vec::with_capacity(65 + 4);
    out.extend_from_slice(&data);
    out.extend_from_slice(&checksum[..4]);
    format!("SIG_K1_{}", bs58::encode(out).into_string())
}

// --- Canonical signing ----------------------------------------------------

/// Sign a 32-byte digest with K1 canonical ECDSA.
///
/// Uses RFC6979 with extra entropy; increments a 32-byte nonce counter until
/// the resulting signature has minimally encoded positive `r` and `s` scalars (fc
/// canonicality), giving a stable canonical signature within 100 tries.
pub fn sign_k1_canonical(sk: &SecretKey, digest: &[u8; 32]) -> Result<String, AntelopeError> {
    let secp = Secp256k1::signing_only();
    let msg = Message::from_digest(*digest);
    let n_order_minus_1_over_2: [u8; 32] = [
        0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFE, 0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B,
        0x20, 0xA0,
    ];
    let _ = n_order_minus_1_over_2; // low-S is enforced by secp256k1 crate by default

    let mut noncedata = [0u8; 32];
    for attempt in 0u32..100 {
        noncedata[..4].copy_from_slice(&attempt.to_le_bytes());
        let sig = secp.sign_ecdsa_recoverable_with_noncedata(&msg, sk, &noncedata);
        let (rec_id, compact) = sig.serialize_compact();
        // secp256k1 normalizes to low-S automatically. fc additionally requires minimally
        // encoded positive integers for both scalars.
        if is_fc_canonical_scalar(&compact[..32]) && is_fc_canonical_scalar(&compact[32..]) {
            let rec_id_i32: i32 = match rec_id {
                RecoveryId::Zero => 0,
                RecoveryId::One => 1,
                RecoveryId::Two => 2,
                RecoveryId::Three => 3,
            };
            return Ok(sig_k1_encode(rec_id_i32, &compact));
        }
    }
    Err(AntelopeError::Signing("no canonical signature after 100 tries"))
}

/// Returns whether a 32-byte scalar satisfies fc's compact-signature canonicality predicate.
///
/// The scalar must be positive when interpreted as a signed integer and may only begin with a
/// zero byte when that byte is needed to keep the next byte positive.
fn is_fc_canonical_scalar(scalar: &[u8]) -> bool {
    debug_assert_eq!(scalar.len(), 32);
    scalar[0] & 0x80 == 0 && !(scalar[0] == 0 && scalar[1] & 0x80 == 0)
}

/// Re-export of [`secp256k1::SecretKey`] under a Telos-specific alias, for tests / callers.
pub use secp256k1::SecretKey as Secp256k1SecretKey;

// --- Action data serialization for eosio.evm::raw -------------------------

/// Serialize the action data for `eosio.evm::raw(name ram_payer, bytes tx, bool estimate_gas,
/// optional<checksum160> sender)`.
pub fn serialize_raw_action_data(
    ram_payer: u64,
    tx_bytes: &[u8],
    estimate_gas: bool,
    sender: Option<[u8; 20]>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 5 + tx_bytes.len() + 1 + 1 + 20);
    out.extend_from_slice(&ram_payer.to_le_bytes());
    write_varuint32(&mut out, tx_bytes.len() as u32);
    out.extend_from_slice(tx_bytes);
    out.push(if estimate_gas { 1 } else { 0 });
    match sender {
        None => out.push(0),
        Some(bytes) => {
            out.push(1);
            out.extend_from_slice(&bytes);
        }
    }
    out
}

// --- Packed transaction ---------------------------------------------------

/// An Antelope action in its binary-packable form.
#[derive(Debug, Clone)]
pub struct PackedAction {
    /// Contract account (name-encoded as u64) the action targets.
    pub account: u64,
    /// Action name (name-encoded as u64).
    pub name: u64,
    /// Authorization list as `(actor, permission)` pairs, both name-encoded.
    pub authorization: Vec<(u64, u64)>, // (actor, permission)
    /// Action data, serialized using the contract's ABI.
    pub data: Vec<u8>,
}

/// An Antelope transaction in its binary-packable form (no context-free actions, no
/// transaction-extensions).
#[derive(Debug, Clone)]
pub struct PackedTransaction {
    /// Absolute expiration timestamp, seconds since the Unix epoch.
    pub expiration: u32,
    /// Low 16 bits of the TAPOS reference block number.
    pub ref_block_num: u16,
    /// Bytes 8..12 of the TAPOS reference block id, interpreted little-endian.
    pub ref_block_prefix: u32,
    /// Maximum net usage, in 8-byte words; 0 means "let nodeos choose".
    pub max_net_usage_words: u32,
    /// Maximum CPU usage in milliseconds; 0 means "let nodeos choose".
    pub max_cpu_usage_ms: u8,
    /// Optional deferred-delay, in seconds.
    pub delay_sec: u32,
    /// Ordered list of actions to execute atomically.
    pub actions: Vec<PackedAction>,
}

impl PackedTransaction {
    /// Serialize `self` using Antelope's `packed_transaction` binary format (no context-free
    /// actions, no transaction extensions).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.expiration.to_le_bytes());
        out.extend_from_slice(&self.ref_block_num.to_le_bytes());
        out.extend_from_slice(&self.ref_block_prefix.to_le_bytes());
        write_varuint32(&mut out, self.max_net_usage_words);
        out.push(self.max_cpu_usage_ms);
        write_varuint32(&mut out, self.delay_sec);
        // context_free_actions: empty
        write_varuint32(&mut out, 0);
        // actions
        write_varuint32(&mut out, self.actions.len() as u32);
        for a in &self.actions {
            out.extend_from_slice(&a.account.to_le_bytes());
            out.extend_from_slice(&a.name.to_le_bytes());
            write_varuint32(&mut out, a.authorization.len() as u32);
            for (actor, perm) in &a.authorization {
                out.extend_from_slice(&actor.to_le_bytes());
                out.extend_from_slice(&perm.to_le_bytes());
            }
            write_varuint32(&mut out, a.data.len() as u32);
            out.extend_from_slice(&a.data);
        }
        // transaction_extensions: empty
        write_varuint32(&mut out, 0);
        out
    }
}

// --- TAPOS helpers --------------------------------------------------------

/// Extract `ref_block_num` from a block height (low 16 bits).
pub const fn ref_block_num(block_num: u32) -> u16 {
    (block_num & 0xFFFF) as u16
}

/// Extract `ref_block_prefix` from a 32-byte block id (bytes 8..12, little-endian).
pub fn ref_block_prefix(block_id: &B256) -> u32 {
    let slice = &block_id.as_slice()[8..12];
    u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]])
}

/// Return a Unix-epoch timestamp `seconds` in the future.
pub fn now_plus(seconds: u32) -> u32 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as u32;
    now + seconds
}

// --- sig_digest -----------------------------------------------------------

/// Compute the signable digest: sha256(`chain_id` || `packed_trx` || `cfa_hash`),
/// where `cfa_hash` is 32 zero bytes when there are no context-free actions.
pub fn sig_digest(chain_id: &B256, packed_trx: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(chain_id.as_slice());
    hasher.update(packed_trx);
    hasher.update([0u8; 32]);
    let out = hasher.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

// --- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn wif_for_secret(secret: [u8; 32]) -> String {
        let mut payload = Vec::with_capacity(37);
        payload.push(0x80);
        payload.extend_from_slice(&secret);
        let first = Sha256::digest(&payload);
        let second = Sha256::digest(first);
        payload.extend_from_slice(&second[..4]);
        bs58::encode(payload).into_string()
    }

    #[test]
    fn name_encoding_known_vectors() {
        // "eosio" canonical u64: top 5*5=25 bits carry 'e','o','s','i','o', rest zero.
        // 'e'=10, 'o'=20, 's'=24, 'i'=14, 'o'=20 → 01010 10100 11000 01110 10100 ...0
        // = 0x5530_EA00_0000_0000
        assert_eq!(name_to_u64("eosio").unwrap(), 0x5530_EA00_0000_0000);

        // "eosio.evm" = "eosio" prefix + '.','e','v','m' → 0x5530_EA01_5B90_0000
        // (computed by appending bits 00000 01010 11011 10010 after the eosio prefix)
        assert_eq!(name_to_u64("eosio.evm").unwrap(), 0x5530_EA01_5B90_0000);

        // "rpc.evm": 7 chars, no 13th char → low 4 bits are zero.
        let v = name_to_u64("rpc.evm").unwrap();
        assert_eq!(v & 0xF, 0, "no 13th char → low 4 bits are zero");

        // Rejects invalid chars
        assert!(name_to_u64("EOSIO").is_err(), "uppercase not allowed");
        assert!(name_to_u64("eosio6").is_err(), "digit > 5 not allowed");
        assert!(
            name_to_u64("abcdefghijklk").is_err(),
            "13th character values above four bits must not be truncated"
        );
        assert!(name_to_u64("abcdefghijklj").is_ok(), "'j' is valid in position 13");
    }

    #[test]
    fn varuint32_roundtrip() {
        for v in [0u32, 1, 0x7F, 0x80, 0x3FFF, 0x4000, 0xFFFF, 0xFFFFFFFF] {
            let mut buf = Vec::new();
            write_varuint32(&mut buf, v);
            // re-decode
            let mut val: u32 = 0;
            let mut shift = 0;
            for (i, b) in buf.iter().enumerate() {
                val |= ((b & 0x7F) as u32) << shift;
                if b & 0x80 == 0 {
                    assert_eq!(i + 1, buf.len());
                    break;
                }
                shift += 7;
            }
            assert_eq!(val, v);
        }
    }

    #[test]
    fn wif_decode_known_vector() {
        let expected = [7u8; 32];
        let wif = wif_for_secret(expected);
        let sk = wif_to_secret_key(&wif).expect("should decode");
        assert_eq!(sk.secret_bytes(), expected);
    }

    #[test]
    fn sig_k1_format() {
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let sig = sign_k1_canonical(&sk, &[0u8; 32]).unwrap();
        assert!(sig.starts_with("SIG_K1_"));
        assert!(sig.len() > 90);
    }

    #[test]
    fn fc_compact_signature_scalar_canonicality() {
        let mut scalar = [0u8; 32];

        scalar[0] = 1;
        assert!(is_fc_canonical_scalar(&scalar));

        scalar[0] = 0;
        scalar[1] = 0x80;
        assert!(is_fc_canonical_scalar(&scalar), "a leading zero is required here");

        scalar[1] = 0x7f;
        assert!(!is_fc_canonical_scalar(&scalar), "redundant leading zero is rejected");

        scalar[0] = 0x80;
        scalar[1] = 0;
        assert!(!is_fc_canonical_scalar(&scalar), "negative signed encoding is rejected");
    }
}

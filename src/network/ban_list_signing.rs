//! Ban list cryptographic signing
//!
//! Provides functions to sign and verify ban lists for authenticity.
//!
//! S-003: bincode 1.x is unmaintained. Consider migrating to bincode 2.x with
//! Limit or a canonical serde format (e.g. serde_json with sorted keys).
//! Lower urgency since data is internal before signing.

use crate::module::traits::ModuleError;
use crate::network::protocol::BanListMessage;
use blvm_secp256k1::ecdsa::{
    ecdsa_sig_parse_compact, ecdsa_sig_verify, ecdsa_sign_compact_rfc6979, ge_from_pubkey_bytes,
    ge_to_compressed, pubkey_from_secret,
};
use blvm_secp256k1::scalar::Scalar;
use sha2::{Digest, Sha256};

fn hash_ban_list(ban_list: &BanListMessage) -> Result<[u8; 32], ModuleError> {
    let serialized = bincode::serialize(ban_list)
        .map_err(|e| ModuleError::CryptoError(format!("Serialization failed: {e}")))?;
    Ok(Sha256::digest(&serialized).into())
}

/// Sign a ban list with a raw 32-byte private key.
/// Returns the signature as 64 compact bytes.
pub fn sign_ban_list(
    ban_list: &BanListMessage,
    private_key: &[u8; 32],
) -> Result<Vec<u8>, ModuleError> {
    let hash = hash_ban_list(ban_list)?;
    ecdsa_sign_compact_rfc6979(&hash, private_key)
        .map(|s| s.to_vec())
        .ok_or_else(|| ModuleError::CryptoError("ECDSA signing failed".to_string()))
}

/// Verify a ban list signature against a compressed public key (33 bytes).
pub fn verify_ban_list_signature(
    ban_list: &BanListMessage,
    signature: &[u8],
    public_key: &[u8; 33],
) -> Result<bool, ModuleError> {
    if signature.len() != 64 {
        return Ok(false);
    }
    let hash = hash_ban_list(ban_list)?;
    let compact: &[u8; 64] = signature.try_into().unwrap();
    let (sigr, sigs) = match ecdsa_sig_parse_compact(compact) {
        Some(p) => p,
        None => return Ok(false),
    };
    let pk = match ge_from_pubkey_bytes(public_key) {
        Some(p) => p,
        None => return Ok(false),
    };
    let mut msg = Scalar::zero();
    let _ = msg.set_b32(&hash);
    Ok(ecdsa_sig_verify(&sigr, &sigs, &pk, &msg))
}

/// Extended ban list message with signature.
#[derive(Debug, Clone)]
pub struct SignedBanListMessage {
    pub ban_list: BanListMessage,
    pub signature: Vec<u8>,
    /// Compressed public key (33 bytes).
    pub public_key: [u8; 33],
}

impl SignedBanListMessage {
    /// Create a signed ban list message from a raw 32-byte private key.
    pub fn new(ban_list: BanListMessage, private_key: &[u8; 32]) -> Result<Self, ModuleError> {
        let mut sec = Scalar::zero();
        if sec.set_b32(private_key) || sec.is_zero() {
            return Err(ModuleError::CryptoError("Invalid secret key".to_string()));
        }
        let public_key = ge_to_compressed(&pubkey_from_secret(&sec));
        let signature = sign_ban_list(&ban_list, private_key)?;
        Ok(Self {
            ban_list,
            signature,
            public_key,
        })
    }

    /// Verify the signature.
    pub fn verify(&self) -> Result<bool, ModuleError> {
        verify_ban_list_signature(&self.ban_list, &self.signature, &self.public_key)
    }
}

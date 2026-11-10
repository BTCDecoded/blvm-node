//! Module signature verification
//!
//! Provides cryptographic signature verification for module manifests and binaries
//! using blvm-secp256k1 (pure-Rust, no C FFI).

use crate::module::traits::ModuleError;
use blvm_secp256k1::ecdsa::{
    ecdsa_sig_parse_compact, ecdsa_sig_parse_der, ecdsa_sig_verify, ge_from_pubkey_bytes,
};
use blvm_secp256k1::scalar::Scalar;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Module signature verifier.
pub struct ModuleSigner;

impl ModuleSigner {
    pub fn new() -> Self {
        Self
    }

    /// Verify module manifest signatures against a threshold (e.g. 2-of-3).
    pub fn verify_manifest(
        &self,
        manifest_content: &[u8],
        signatures: &[(String, String)],
        public_keys: &HashMap<String, String>,
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        let hash: [u8; 32] = Sha256::digest(manifest_content).into();
        self.verify_threshold(&hash, signatures, public_keys, threshold)
    }

    /// Verify binary signatures (same logic as manifest).
    pub fn verify_binary(
        &self,
        binary_content: &[u8],
        signatures: &[(String, String)],
        public_keys: &HashMap<String, String>,
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        self.verify_manifest(binary_content, signatures, public_keys, threshold)
    }

    /// Verify a single signature against a message.
    pub fn verify_single_signature(
        &self,
        message: &[u8],
        signature_hex: &str,
        public_key_hex: &str,
    ) -> Result<bool, ModuleError> {
        let hash: [u8; 32] = Sha256::digest(message).into();
        let sig_bytes = hex::decode(signature_hex)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid signature hex: {e}")))?;
        let pubkey_bytes = hex::decode(public_key_hex)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey hex: {e}")))?;
        Ok(verify_one(&hash, &sig_bytes, &pubkey_bytes))
    }

    /// Verify payment address signatures.
    pub fn verify_payment_addresses(
        &self,
        author_address: &str,
        commons_address: &str,
        price_sats: u64,
        signature_hex: &str,
        public_keys: &HashMap<String, String>,
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        let msg = format!("{author_address}||{commons_address}||{price_sats}");
        let hash: [u8; 32] = Sha256::digest(msg.as_bytes()).into();
        let sig_bytes = hex::decode(signature_hex)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid signature hex: {e}")))?;
        let mut valid = 0usize;
        for pubkey_hex in public_keys.values() {
            let pubkey_bytes = match hex::decode(pubkey_hex) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if verify_one(&hash, &sig_bytes, &pubkey_bytes) {
                valid += 1;
            }
        }
        Ok(valid >= threshold.0)
    }

    fn verify_threshold(
        &self,
        hash: &[u8; 32],
        signatures: &[(String, String)],
        public_keys: &HashMap<String, String>,
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        let mut valid_signatures = 0;
        let mut verified_signers = HashSet::new();
        for (maintainer, sig_hex) in signatures {
            if verified_signers.contains(maintainer) {
                continue;
            }
            let sig_bytes = hex::decode(sig_hex)
                .map_err(|e| ModuleError::CryptoError(format!("Invalid signature hex: {e}")))?;
            if let Some(pubkey_hex) = public_keys.get(maintainer) {
                let pubkey_bytes = hex::decode(pubkey_hex)
                    .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey hex: {e}")))?;
                if verify_one(hash, &sig_bytes, &pubkey_bytes) {
                    valid_signatures += 1;
                    verified_signers.insert(maintainer.clone());
                }
            }
        }
        Ok(valid_signatures >= threshold.0)
    }
}

/// Verify a single compact (64-byte) or DER signature against `hash` and `pubkey`.
fn verify_one(hash: &[u8; 32], sig_bytes: &[u8], pubkey_bytes: &[u8]) -> bool {
    let pk = match ge_from_pubkey_bytes(pubkey_bytes) {
        Some(p) => p,
        None => return false,
    };
    let mut msg = Scalar::zero();
    let _ = msg.set_b32(hash);
    let parsed = if sig_bytes.len() == 64 {
        let compact: &[u8; 64] = match sig_bytes.try_into() {
            Ok(a) => a,
            Err(_) => return false,
        };
        ecdsa_sig_parse_compact(compact)
    } else {
        ecdsa_sig_parse_der(sig_bytes)
    };
    match parsed {
        Some((sigr, sigs)) => ecdsa_sig_verify(&sigr, &sigs, &pk, &msg),
        None => false,
    }
}

impl Default for ModuleSigner {
    fn default() -> Self {
        Self::new()
    }
}

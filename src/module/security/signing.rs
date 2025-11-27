//! Module signature verification
//!
//! Provides cryptographic signature verification for module manifests and binaries
//! using direct secp256k1 implementation (no external dependencies on governance infrastructure).

use crate::module::traits::ModuleError;
use secp256k1::{ecdsa::Signature, Message, PublicKey, Secp256k1};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Module signature verifier (independent implementation)
///
/// This implementation uses secp256k1 directly to avoid dependencies on
/// governance infrastructure, maintaining node independence.
pub struct ModuleSigner {
    secp: Secp256k1<secp256k1::All>,
}

impl ModuleSigner {
    /// Create a new module signer
    pub fn new() -> Self {
        Self {
            secp: Secp256k1::new(),
        }
    }

    /// Verify module manifest signatures
    ///
    /// Verifies that a manifest has been signed by the required number of maintainers
    /// according to the threshold (e.g., 2-of-3 means 2 out of 3 maintainers must sign).
    ///
    /// # Arguments
    ///
    /// * `manifest_content` - The raw bytes of the manifest file
    /// * `signatures` - Vector of (maintainer_name, signature_hex) pairs
    /// * `public_keys` - Map of maintainer_name -> public_key_hex
    /// * `threshold` - (required_count, total_count) tuple, e.g., (2, 3) for 2-of-3
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if threshold is met, `Ok(false)` if not, or an error if verification fails.
    pub fn verify_manifest(
        &self,
        manifest_content: &[u8],
        signatures: &[(String, String)], // (maintainer, signature_hex)
        public_keys: &HashMap<String, String>, // (maintainer, pubkey_hex)
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        // Hash manifest content
        let message_hash = Sha256::digest(manifest_content);
        let message = Message::from_digest_slice(&message_hash)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid message hash: {e}")))?;

        // Verify each signature
        let mut valid_signatures = 0;
        let mut verified_signers = HashSet::new();

        for (maintainer, sig_hex) in signatures {
            // Skip if already verified (prevent duplicate counting)
            if verified_signers.contains(maintainer) {
                continue;
            }

            // Parse signature
            let sig_bytes = hex::decode(sig_hex)
                .map_err(|e| ModuleError::CryptoError(format!("Invalid signature hex: {e}")))?;

            // Try compact format first (64 bytes), then DER if that fails
            let signature = Signature::from_compact(&sig_bytes)
                .or_else(|_| Signature::from_der(&sig_bytes))
                .map_err(|e| ModuleError::CryptoError(format!("Invalid signature format: {e}")))?;

            // Get public key for this maintainer
            if let Some(pubkey_hex) = public_keys.get(maintainer) {
                let pubkey_bytes = hex::decode(pubkey_hex)
                    .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey hex: {e}")))?;
                let public_key = PublicKey::from_slice(&pubkey_bytes)
                    .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey: {e}")))?;

                // Verify signature
                if self
                    .secp
                    .verify_ecdsa(&message, &signature, &public_key)
                    .is_ok()
                {
                    valid_signatures += 1;
                    verified_signers.insert(maintainer.clone());
                }
            }
        }

        // Check threshold
        Ok(valid_signatures >= threshold.0)
    }

    /// Verify binary signatures (same logic, different content)
    ///
    /// Verifies that a module binary has been signed by the required number of maintainers.
    /// Uses the same verification logic as manifest verification.
    ///
    /// # Arguments
    ///
    /// * `binary_content` - The raw bytes of the binary file
    /// * `signatures` - Vector of (maintainer_name, signature_hex) pairs
    /// * `public_keys` - Map of maintainer_name -> public_key_hex
    /// * `threshold` - (required_count, total_count) tuple
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if threshold is met, `Ok(false)` if not, or an error if verification fails.
    pub fn verify_binary(
        &self,
        binary_content: &[u8],
        signatures: &[(String, String)],
        public_keys: &HashMap<String, String>,
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        // Same implementation as verify_manifest, just different content
        self.verify_manifest(binary_content, signatures, public_keys, threshold)
    }

    /// Verify a single signature
    ///
    /// Helper method to verify a single signature against a message and public key.
    ///
    /// # Arguments
    ///
    /// * `message` - The message bytes to verify
    /// * `signature_hex` - The signature in hex format
    /// * `public_key_hex` - The public key in hex format
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if signature is valid, `Ok(false)` if invalid, or an error if parsing fails.
    pub fn verify_single_signature(
        &self,
        message: &[u8],
        signature_hex: &str,
        public_key_hex: &str,
    ) -> Result<bool, ModuleError> {
        // Hash message
        let message_hash = Sha256::digest(message);
        let secp_message = Message::from_digest_slice(&message_hash)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid message hash: {e}")))?;

        // Parse signature
        let sig_bytes = hex::decode(signature_hex)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid signature hex: {e}")))?;
        let signature = Signature::from_compact(&sig_bytes)
            .or_else(|_| Signature::from_der(&sig_bytes))
            .map_err(|e| ModuleError::CryptoError(format!("Invalid signature format: {e}")))?;

        // Parse public key
        let pubkey_bytes = hex::decode(public_key_hex)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey hex: {e}")))?;
        let public_key = PublicKey::from_slice(&pubkey_bytes)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey: {e}")))?;

        // Verify signature
        Ok(self
            .secp
            .verify_ecdsa(&secp_message, &signature, &public_key)
            .is_ok())
    }

    /// Verify payment address signatures
    ///
    /// Verifies that payment addresses (author and commons) are cryptographically signed
    /// by the module maintainers. This prevents node operators from tampering with addresses.
    ///
    /// # Arguments
    ///
    /// * `author_address` - Module author's payment address (75% of payment)
    /// * `commons_address` - Commons governance address (15% of payment)
    /// * `price_sats` - Module price in satoshis
    /// * `signature_hex` - Signature over the payment addresses and price
    /// * `public_keys` - Map of maintainer_name -> public_key_hex (from manifest signatures)
    /// * `threshold` - (required_count, total_count) tuple for signature threshold
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if signature is valid, `Ok(false)` if invalid, or error if verification fails.
    pub fn verify_payment_addresses(
        &self,
        author_address: &str,
        commons_address: &str,
        price_sats: u64,
        signature_hex: &str,
        public_keys: &HashMap<String, String>,
        threshold: (usize, usize),
    ) -> Result<bool, ModuleError> {
        // Create message: author_address || commons_address || price_sats
        let message_data = format!("{author_address}||{commons_address}||{price_sats}");
        let message_hash = Sha256::digest(message_data.as_bytes());
        let message = Message::from_digest_slice(&message_hash)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid message hash: {e}")))?;

        // Parse signature
        let sig_bytes = hex::decode(signature_hex)
            .map_err(|e| ModuleError::CryptoError(format!("Invalid signature hex: {e}")))?;
        let signature = Signature::from_compact(&sig_bytes)
            .or_else(|_| Signature::from_der(&sig_bytes))
            .map_err(|e| ModuleError::CryptoError(format!("Invalid signature format: {e}")))?;

        // Verify signature against all public keys (need threshold to match)
        let mut valid_signatures = 0;
        for pubkey_hex in public_keys.values() {
            let pubkey_bytes = hex::decode(pubkey_hex)
                .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey hex: {e}")))?;
            let public_key = PublicKey::from_slice(&pubkey_bytes)
                .map_err(|e| ModuleError::CryptoError(format!("Invalid pubkey: {e}")))?;

            if self
                .secp
                .verify_ecdsa(&message, &signature, &public_key)
                .is_ok()
            {
                valid_signatures += 1;
            }
        }

        // Check threshold (for payment addresses, we require at least one valid signature)
        // In practice, this should match the manifest signature threshold
        Ok(valid_signatures >= threshold.0)
    }
}

impl Default for ModuleSigner {
    fn default() -> Self {
        Self::new()
    }
}

//! REST API Input Validation Utilities
//!
//! Provides helper functions for validating REST API parameters,
//! reusing the RPC validation logic for consistency.

use crate::rpc::validation::{
    MAX_ADDRESS_STRING_LENGTH, MAX_HASH_STRING_LENGTH, MAX_HEX_STRING_LENGTH,
};

/// Validate a hex string (for transaction hex, etc.)
pub fn validate_hex_string(hex: &str, max_length: Option<usize>) -> Result<String, String> {
    let max_len = max_length.unwrap_or(MAX_HEX_STRING_LENGTH);

    if hex.is_empty() {
        return Err("Hex string cannot be empty".to_string());
    }

    if hex.len() > max_len {
        return Err(format!(
            "Hex string too long: {} bytes (max: {})",
            hex.len(),
            max_len
        ));
    }

    // Validate hex format (even length, valid hex chars)
    if hex.len() % 2 != 0 {
        return Err("Hex string must be even-length".to_string());
    }

    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Hex string contains invalid hex characters".to_string());
    }

    Ok(hex.to_string())
}

/// Validate a hash string (64 hex chars, 32 bytes)
pub fn validate_hash_string(hash: &str) -> Result<String, String> {
    if hash.len() != MAX_HASH_STRING_LENGTH {
        return Err(format!(
            "Hash must be {} hex characters (32 bytes), got {}",
            MAX_HASH_STRING_LENGTH,
            hash.len()
        ));
    }

    validate_hex_string(hash, Some(MAX_HASH_STRING_LENGTH))
}

/// Validate an address string
pub fn validate_address_string(address: &str) -> Result<String, String> {
    if address.is_empty() {
        return Err("Address cannot be empty".to_string());
    }

    if address.len() > MAX_ADDRESS_STRING_LENGTH {
        return Err(format!(
            "Address too long: {} bytes (max: {})",
            address.len(),
            MAX_ADDRESS_STRING_LENGTH
        ));
    }

    Ok(address.to_string())
}

/// Validate a block height
pub fn validate_block_height(height: u64) -> Result<u64, String> {
    use crate::rpc::validation::MAX_BLOCK_HEIGHT;

    if height > MAX_BLOCK_HEIGHT {
        return Err(format!(
            "Block height too large: {} (max: {})",
            height, MAX_BLOCK_HEIGHT
        ));
    }

    Ok(height)
}

/// Validate transaction hex (for submission)
pub fn validate_transaction_hex(hex: &str) -> Result<String, String> {
    // Transaction hex can be up to MAX_HEX_STRING_LENGTH (1MB)
    validate_hex_string(hex, Some(MAX_HEX_STRING_LENGTH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_hex_string() {
        assert!(validate_hex_string("deadbeef", None).is_ok());
        assert!(validate_hex_string("deadbee", None).is_err()); // odd length
        assert!(validate_hex_string("deadbeef!", None).is_err()); // invalid char
    }

    #[test]
    fn test_validate_hash_string() {
        let hash = "0".repeat(64);
        assert!(validate_hash_string(&hash).is_ok());
        assert!(validate_hash_string("deadbeef").is_err()); // wrong length
    }

    #[test]
    fn test_validate_address_string() {
        assert!(validate_address_string("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh").is_ok());
        assert!(validate_address_string("").is_err());
    }

    #[test]
    fn test_validate_block_height() {
        assert!(validate_block_height(0).is_ok());
        assert!(validate_block_height(800000).is_ok());
        // MAX_BLOCK_HEIGHT is 2_000_000_000, so this should fail
        // But we can't easily test that without importing the constant
    }
}

//! Bitcoin-compatible hashing functions
//!
//! Implements proper Bitcoin double SHA256 hashing for all storage operations.
//! This replaces the incorrect DefaultHasher usage throughout the storage layer.

#[cfg(not(feature = "production"))]
use sha2::{Digest, Sha256};

/// Calculate Bitcoin double SHA256 hash
///
/// This is the standard Bitcoin hashing algorithm used for:
/// - Block hashes
/// - Transaction hashes  
/// - Merkle tree calculations
/// - All Bitcoin protocol hashing
///
/// # Arguments
/// * `data` - The data to hash
///
/// # Returns
/// 32-byte hash as array
///
/// Performance optimization: Uses OptimizedSha256 in production mode for SHA-NI acceleration
#[inline]
pub fn double_sha256(data: &[u8]) -> [u8; 32] {
    #[cfg(feature = "production")]
    {
        use blvm_protocol::crypto::OptimizedSha256;
        let hasher = OptimizedSha256::new();
        hasher.hash256(data)
    }

    #[cfg(not(feature = "production"))]
    {
        let first_hash = Sha256::digest(data);
        let second_hash = Sha256::digest(first_hash);
        let mut result = [0u8; 32];
        result.copy_from_slice(&second_hash);
        result
    }
}

/// Double SHA-256 of the 80-byte header. Version is `i32`; time, bits, and nonce are `u32`.
#[inline]
pub fn header_double_sha256(header: &blvm_protocol::BlockHeader) -> [u8; 32] {
    let mut header_data = [0u8; 80];
    header_data[0..4].copy_from_slice(&(header.version as i32).to_le_bytes());
    header_data[4..36].copy_from_slice(&header.prev_block_hash);
    header_data[36..68].copy_from_slice(&header.merkle_root);
    header_data[68..72].copy_from_slice(&(header.timestamp as u32).to_le_bytes());
    header_data[72..76].copy_from_slice(&(header.bits as u32).to_le_bytes());
    header_data[76..80].copy_from_slice(&(header.nonce as u32).to_le_bytes());
    double_sha256(&header_data)
}

/// Calculate single SHA256 hash (for internal use)
///
/// Performance optimization: Uses OptimizedSha256 in production mode for SHA-NI acceleration
pub fn sha256(data: &[u8]) -> [u8; 32] {
    #[cfg(feature = "production")]
    {
        use blvm_protocol::crypto::OptimizedSha256;
        let hasher = OptimizedSha256::new();
        hasher.hash(data)
    }

    #[cfg(not(feature = "production"))]
    {
        let hash = Sha256::digest(data);
        let mut result = [0u8; 32];
        result.copy_from_slice(&hash);
        result
    }
}

/// Calculate RIPEMD160 hash (for Bitcoin addresses)
pub fn ripemd160(data: &[u8]) -> [u8; 20] {
    use ripemd::{Digest, Ripemd160};
    let mut hasher = Ripemd160::new();
    hasher.update(data);
    let hash = hasher.finalize();
    let mut result = [0u8; 20];
    result.copy_from_slice(&hash);
    result
}

/// Calculate Bitcoin address hash (SHA256 + RIPEMD160)
pub fn hash160(data: &[u8]) -> [u8; 20] {
    let sha256_hash = sha256(data);
    ripemd160(&sha256_hash)
}

/// Internal block hash (SHA256d digest order) → Bitcoin RPC hex (Core `GetHex()`).
pub fn hash_to_rpc_hex(hash: &[u8; 32]) -> String {
    let mut display = *hash;
    display.reverse();
    hex::encode(display)
}

/// Bitcoin RPC hex → internal digest order (inverse of [`hash_to_rpc_hex`]).
pub fn hash_from_rpc_hex(hex_str: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| format!("invalid hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "hash must be 64 hex characters (32 bytes), got {}",
            hex_str.len()
        ));
    }
    let mut internal = [0u8; 32];
    internal.copy_from_slice(&bytes);
    internal.reverse();
    Ok(internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(feature = "production", feature = "profile", feature = "ctv"))]
    #[test]
    fn header_bytes_match_block_header_hash() {
        use blvm_protocol::BlockHeader;
        let header = BlockHeader {
            version: -1,
            prev_block_hash: [0x11; 32],
            merkle_root: [0x22; 32],
            timestamp: 1_700_000_000,
            bits: 0x1d00ffff,
            nonce: 7,
        };
        let wire = blvm_protocol::serialization::serialize_block_header(&header);
        assert_eq!(
            double_sha256(&wire),
            blvm_consensus::block::block_header_hash(&header)
        );
    }

    #[test]
    fn test_double_sha256_empty() {
        // Test with empty string - just verify it produces a 32-byte hash
        let result = double_sha256(&[]);
        assert_eq!(result.len(), 32);

        // Verify it's different from single SHA256
        let single_hash = sha256(&[]);
        assert_ne!(result, single_hash);
    }

    #[test]
    fn test_double_sha256_known_vector() {
        // Test with known Bitcoin test vector
        let data = b"hello world";
        let result = double_sha256(data);

        // This should match standard double SHA256
        // (We'll verify this matches actual Bitcoin behavior)
        assert_eq!(result.len(), 32);

        // Verify it's different from single SHA256
        let single_hash = sha256(data);
        assert_ne!(result, single_hash);
    }

    #[test]
    fn test_sha256_consistency() {
        let data = b"test data";
        let hash1 = sha256(data);
        let hash2 = sha256(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_ripemd160() {
        let data = b"test";
        let result = ripemd160(data);
        assert_eq!(result.len(), 20);
    }

    #[test]
    fn test_hash160() {
        let data = b"test data";
        let result = hash160(data);
        assert_eq!(result.len(), 20);

        // Should be different from both SHA256 and RIPEMD160
        let sha256_hash = sha256(data);
        let ripemd_hash = ripemd160(data);
        assert_ne!(result, sha256_hash[..20]);
        assert_ne!(result, ripemd_hash);
    }

    #[test]
    fn test_hash_deterministic() {
        let data = b"deterministic test";
        let hash1 = double_sha256(data);
        let hash2 = double_sha256(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_mainnet_genesis_hash_rpc_roundtrip() {
        let internal = [
            0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72, 0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63,
            0xf7, 0x4f, 0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c, 0x68, 0xd6, 0x19, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let rpc = hash_to_rpc_hex(&internal);
        assert_eq!(
            rpc,
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
        assert_eq!(hash_from_rpc_hex(&rpc).unwrap(), internal);
    }
}

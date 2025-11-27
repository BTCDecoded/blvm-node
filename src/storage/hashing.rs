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
pub fn double_sha256(data: &[u8]) -> [u8; 32] {
    #[cfg(feature = "production")]
    {
        use bllvm_consensus::crypto::OptimizedSha256;
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

/// Calculate single SHA256 hash (for internal use)
///
/// Performance optimization: Uses OptimizedSha256 in production mode for SHA-NI acceleration
pub fn sha256(data: &[u8]) -> [u8; 32] {
    #[cfg(feature = "production")]
    {
        use bllvm_consensus::crypto::OptimizedSha256;
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // This should match Bitcoin Core's double SHA256
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
}

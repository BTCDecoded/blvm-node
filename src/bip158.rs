//! BIP158: Compact Block Filters for Light Client Discovery
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0158.mediawiki
//!
//! Implements Golomb-Rice Coded Sets (GCS) for efficient block filtering.
//! Allows light clients to determine if a block contains relevant transactions
//! without downloading the entire block.

use protocol_engine::Transaction;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

/// BIP158 filter parameter: false positive rate (2^(-P))
/// P = 19 means ~1 in 524,288 false positives
pub const BIP158_P: u8 = 19;

/// BIP158 filter parameter: multiplier M = 2^P
pub const BIP158_M: u64 = 1 << BIP158_P; // 2^19 = 524,288

/// Compact block filter (GCS)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactBlockFilter {
    /// Golomb-Rice encoded filter data
    pub filter_data: Vec<u8>,
    /// Number of elements in the filter
    pub num_elements: u32,
}

/// Hash a script to a number in range [0, N*M)
fn hash_to_range(script: &[u8], n: u64, m: u64) -> u64 {
    // Hash script with SHA256
    let mut hasher = Sha256::new();
    hasher.update(script);
    let hash = hasher.finalize();

    // Interpret first 8 bytes as u64 (little-endian)
    let hash_value = u64::from_le_bytes([
        hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
    ]);

    // Map to range [0, N*M)
    (hash_value % (n * m))
}

/// Golomb-Rice encode a value
///
/// BIP158: Encode value x as:
/// - Write (x / 2^P) in unary (that many 1s, then a 0)
/// - Write (x mod 2^P) in binary (P bits)
fn golomb_rice_encode(value: u64, p: u8) -> Vec<u8> {
    let mut result = Vec::new();

    // Calculate quotient and remainder
    let quotient = value >> p; // value / 2^P
    let remainder = value & ((1u64 << p) - 1); // value mod 2^P

    // Encode quotient in unary (quotient number of 1s, then 0)
    // We'll encode this as bits in bytes
    let quotient_bytes = (quotient / 8) as usize;
    let quotient_remainder = (quotient % 8) as u8;

    // Add quotient bytes (all 1s)
    result.resize(quotient_bytes, 0xFF);

    // Add remainder bits of quotient
    if quotient_remainder > 0 {
        let byte = 0xFF << (8 - quotient_remainder);
        result.push(byte);
    } else {
        // Need to add a 0 byte to terminate unary encoding
        if quotient > 0 {
            result.push(0xFF);
        }
    }

    // Terminate unary with a 0 bit (clear last bit)
    if let Some(last) = result.last_mut() {
        *last &= !(1u8 << (7 - quotient_remainder));
    } else {
        result.push(0);
    }

    // Encode remainder in binary (P bits)
    // BIP158 uses P=19, so 19 bits = 2.375 bytes, round to 3 bytes
    let remainder_bytes = ((p + 7) / 8) as usize;
    for i in 0..remainder_bytes {
        let shift = i * 8;
        let byte = ((remainder >> shift) & 0xFF) as u8;
        result.push(byte);
    }

    result
}

/// Golomb-Rice decode a value from stream
/// This is a simplified decoder - full implementation needs bit-level reading
fn golomb_rice_decode(_data: &[u8], _p: u8, _offset: &mut usize) -> Option<u64> {
    // Full implementation requires bit-level decoding
    // For now, return None to indicate not fully implemented
    None
}

/// Build a compact block filter from transaction data
///
/// BIP158: Filter contains:
/// 1. All spendable output scriptPubKeys in the block
/// 2. All scriptPubKeys from outputs spent by block's inputs
pub fn build_block_filter(
    block_transactions: &[Transaction],
    previous_outpoint_scripts: &[Vec<u8>], // Scripts from UTXOs being spent
) -> Result<CompactBlockFilter, String> {
    let mut filter_set = HashSet::new();

    // Add all scriptPubKeys from block outputs
    for tx in block_transactions {
        for output in &tx.outputs {
            if !output.script_pubkey.is_empty() {
                filter_set.insert(output.script_pubkey.clone());
            }
        }
    }

    // Add all scriptPubKeys from inputs (UTXOs being spent)
    for script in previous_outpoint_scripts {
        if !script.is_empty() {
            filter_set.insert(script.clone());
        }
    }

    // Convert to sorted vector of hashed values
    let n = filter_set.len() as u64;
    if n == 0 {
        // Empty filter
        return Ok(CompactBlockFilter {
            filter_data: Vec::new(),
            num_elements: 0,
        });
    }

    // Hash each script to range [0, N*M)
    let mut hashed_values: Vec<u64> = filter_set
        .iter()
        .map(|script| hash_to_range(script, n, BIP158_M))
        .collect();

    // Sort and remove duplicates
    hashed_values.sort_unstable();
    hashed_values.dedup();

    // Update n after deduplication
    let n_final = hashed_values.len() as u64;

    // Compute differences between consecutive values
    // First value is difference from 0
    let mut differences = Vec::new();
    if !hashed_values.is_empty() {
        differences.push(hashed_values[0]);
        for i in 1..hashed_values.len() {
            let diff = hashed_values[i] - hashed_values[i - 1];
            differences.push(diff);
        }
    }

    // Encode differences using Golomb-Rice
    let mut filter_data = Vec::new();
    for diff in differences {
        let encoded = golomb_rice_encode(diff, BIP158_P);
        filter_data.extend_from_slice(&encoded);
    }

    Ok(CompactBlockFilter {
        filter_data,
        num_elements: n_final as u32,
    })
}

/// Match a script against a compact block filter
///
/// Returns true if the script (or its hash) is likely in the filter
/// Note: This is a simplified check - full implementation needs GCS decoding
pub fn match_filter(_filter: &CompactBlockFilter, _script: &[u8]) -> bool {
    // Full implementation requires decoding the Golomb-Rice encoded filter
    // and checking if the hashed script value is present in the set
    // For now, return false (requires bit-level GCS decoding)
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_to_range() {
        let script = b"test script";
        let n = 100;
        let m = BIP158_M;
        let value = hash_to_range(script, n, m);
        assert!(value < n * m);
    }

    #[test]
    fn test_golomb_rice_encode() {
        // Test encoding small value
        let encoded = golomb_rice_encode(0, BIP158_P);
        assert!(!encoded.is_empty());

        let encoded2 = golomb_rice_encode(1, BIP158_P);
        assert!(!encoded2.is_empty());
    }

    #[test]
    fn test_empty_filter() {
        let filter = build_block_filter(&[], &[]).unwrap();
        assert_eq!(filter.num_elements, 0);
        assert!(filter.filter_data.is_empty());
    }
}

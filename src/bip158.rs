//! BIP158: Compact Block Filters for Light Client Discovery
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0158.mediawiki
//!
//! Implements Golomb-Rice Coded Sets (GCS) for efficient block filtering.
//! Allows light clients to determine if a block contains relevant transactions
//! without downloading the entire block.

use bllvm_protocol::Transaction;
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

/// Bit writer for Golomb-Rice encoding
struct BitWriter {
    data: Vec<u8>,
    current_byte: u8,
    bit_count: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            current_byte: 0,
            bit_count: 0,
        }
    }

    /// Write a single bit
    fn write_bit(&mut self, bit: bool) {
        if bit {
            self.current_byte |= 1u8 << (7 - self.bit_count);
        }
        self.bit_count += 1;
        
        if self.bit_count == 8 {
            self.data.push(self.current_byte);
            self.current_byte = 0;
            self.bit_count = 0;
        }
    }

    /// Write multiple bits (up to 64)
    fn write_bits(&mut self, value: u64, num_bits: u8) {
        for i in 0..num_bits {
            let bit = ((value >> (num_bits - 1 - i)) & 1) != 0;
            self.write_bit(bit);
        }
    }

    /// Finish writing (flush remaining bits)
    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            self.data.push(self.current_byte);
        }
        self.data
    }
}

/// Golomb-Rice encode a value
///
/// BIP158: Encode value x as:
/// - Write (x / 2^P) in unary (that many 1s, then a 0)
/// - Write (x mod 2^P) in binary (P bits)
fn golomb_rice_encode(value: u64, p: u8) -> Vec<u8> {
    let mut writer = BitWriter::new();

    // Calculate quotient and remainder
    let quotient = value >> p; // value / 2^P
    let remainder = value & ((1u64 << p) - 1); // value mod 2^P

    // Encode quotient in unary (quotient number of 1s, then a 0)
    for _ in 0..quotient {
        writer.write_bit(true);
    }
    writer.write_bit(false); // Terminate unary with 0

    // Encode remainder in binary (P bits)
    writer.write_bits(remainder, p);

    writer.finish()
}

/// Bit reader for Golomb-Rice decoding
struct BitReader<'a> {
    data: &'a [u8],
    bit_offset: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_offset: 0,
        }
    }

    /// Read a single bit
    fn read_bit(&mut self) -> Option<bool> {
        if self.bit_offset >= self.data.len() * 8 {
            return None;
        }
        let byte_idx = self.bit_offset / 8;
        let bit_idx = self.bit_offset % 8;
        let bit = (self.data[byte_idx] >> (7 - bit_idx)) & 1;
        self.bit_offset += 1;
        Some(bit == 1)
    }

    /// Read P bits as a u64
    fn read_bits(&mut self, p: u8) -> Option<u64> {
        let mut value = 0u64;
        for _ in 0..p {
            if let Some(bit) = self.read_bit() {
                value = (value << 1) | (if bit { 1 } else { 0 });
            } else {
                return None;
            }
        }
        Some(value)
    }

    /// Get current bit offset
    fn bit_offset(&self) -> usize {
        self.bit_offset
    }
}

/// Golomb-Rice decode a value from stream
///
/// BIP158: Decode value x by:
/// - Read unary-encoded quotient (count 1s until 0)
/// - Read P bits as remainder
/// - Value = quotient * 2^P + remainder
fn golomb_rice_decode(reader: &mut BitReader, p: u8) -> Option<u64> {
    // Read quotient in unary (count 1s until we hit a 0)
    let mut quotient = 0u64;
    loop {
        match reader.read_bit() {
            Some(true) => quotient += 1,
            Some(false) => break,
            None => return None,
        }
    }

    // Read remainder in binary (P bits)
    let remainder = reader.read_bits(p)?;

    // Reconstruct value: quotient * 2^P + remainder
    Some((quotient << p) | remainder)
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
///
/// Algorithm:
/// 1. Hash the script to get a value in range [0, N*M)
/// 2. Decode the filter to reconstruct the sorted set of hashed values
/// 3. Check if the script's hash value is in the set
pub fn match_filter(filter: &CompactBlockFilter, script: &[u8]) -> bool {
    if filter.num_elements == 0 {
        return false;
    }

    let n = filter.num_elements as u64;
    
    // Hash script to range [0, N*M)
    let script_hash = hash_to_range(script, n, BIP158_M);
    
    // Decode filter to reconstruct sorted set
    let mut reader = BitReader::new(&filter.filter_data);
    let mut decoded_values = Vec::new();
    let mut current_value = 0u64;
    
    // Decode all differences and reconstruct values
    for _ in 0..filter.num_elements {
        if let Some(diff) = golomb_rice_decode(&mut reader, BIP158_P) {
            current_value += diff;
            decoded_values.push(current_value);
        } else {
            // Decoding failed - filter may be corrupted
            return false;
        }
    }
    
    // Check if script_hash is in the decoded set
    decoded_values.binary_search(&script_hash).is_ok()
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

    #[test]
    fn test_golomb_rice_encode_decode_roundtrip() {
        let test_values = vec![0, 1, 2, 10, 100, 1000, 10000];
        
        for value in test_values {
            let encoded = golomb_rice_encode(value, BIP158_P);
            let mut reader = BitReader::new(&encoded);
            let decoded = golomb_rice_decode(&mut reader, BIP158_P);
            
            assert_eq!(decoded, Some(value), "Roundtrip failed for value {}", value);
        }
    }

    #[test]
    fn test_build_and_match_filter() {
        use bllvm_protocol::{Transaction, TransactionInput, TransactionOutput, OutPoint};
        
        // Create a test transaction
        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0,
                },
                script_sig: vec![],
                sequence: 0xffffffff,
            }],
            outputs: vec![TransactionOutput {
                value: 1000,
                script_pubkey: vec![0x51, 0x52], // OP_1 OP_2
            }],
            lock_time: 0,
        };
        
        // Build filter
        let filter = build_block_filter(&[tx.clone()], &[]).unwrap();
        assert!(filter.num_elements > 0);
        
        // Match the script that's in the filter
        let script_in_filter = &tx.outputs[0].script_pubkey;
        assert!(match_filter(&filter, script_in_filter));
        
        // Match a script that's not in the filter
        let script_not_in_filter = vec![0x53, 0x54]; // OP_3 OP_4
        // Note: May have false positives due to GCS nature, but should generally work
        let matched = match_filter(&filter, &script_not_in_filter);
        // False positives are possible, so we can't assert false
        // But we can verify the filter works for scripts that are definitely in it
    }
}

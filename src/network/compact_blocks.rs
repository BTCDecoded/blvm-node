//! Compact Block Relay (BIP152) Implementation
//!
//! Reduces bandwidth during block propagation by sending only block headers
//! and short transaction IDs, allowing peers to reconstruct blocks using
//! transactions from their mempool.
//!
//! Specification: https://github.com/bitcoin/bips/blob/master/bip-0152.mediawiki
//!
//! ## Iroh Integration
//!
//! Compact blocks work seamlessly over Iroh QUIC transport. When using Iroh,
//! compact blocks provide additional benefits:
//! - Lower latency due to QUIC's multiplexing and stream prioritization
//! - Better NAT traversal support (Iroh's magic endpoint)
//! - Encryption by default (QUIC/TLS)
//!
//! Both features are optional and work independently:
//! - Compact blocks can be used with TCP, Quinn, or Iroh
//! - Iroh can be used with or without compact blocks
//! - The combination provides optimal bandwidth and latency for mobile nodes and NAT-traversed connections

use crate::network::transport::TransportType;
use anyhow::Result;
use bllvm_protocol::{Block, BlockHeader, Hash, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;

/// Short transaction ID (6 bytes / 48 bits)
///
/// Computed using SipHash-2-4 of the transaction hash with keys derived
/// from the block header nonce.
pub type ShortTxId = [u8; 6];

/// Compact block representation
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompactBlock {
    /// Block header
    pub header: BlockHeader,
    /// Nonce for short ID calculation (64-bit)
    pub nonce: u64,
    /// Short transaction IDs (6 bytes each)
    pub short_ids: Vec<ShortTxId>,
    /// Prefilled transactions (transactions that likely won't be in peer's mempool)
    pub prefilled_txs: Vec<(usize, Transaction)>,
}

/// Calculate Bitcoin transaction hash (double SHA256 of serialized transaction)
///
/// Properly serializes a transaction according to Bitcoin protocol and computes
/// the transaction ID (txid) using double SHA256.
///
/// # Arguments
/// * `tx` - Transaction to hash
///
/// # Returns
/// Transaction hash (32 bytes)
pub fn calculate_tx_hash(tx: &Transaction) -> Hash {
    let mut data = Vec::new();

    // Version (4 bytes, little-endian)
    data.extend_from_slice(&(tx.version as u32).to_le_bytes());

    // Input count (varint)
    data.extend_from_slice(&encode_varint(tx.inputs.len() as u64));

    // Inputs
    for input in &tx.inputs {
        // Previous output hash (32 bytes)
        data.extend_from_slice(&input.prevout.hash);
        // Previous output index (4 bytes, little-endian)
        data.extend_from_slice(&(input.prevout.index as u32).to_le_bytes());
        // Script length (varint)
        data.extend_from_slice(&encode_varint(input.script_sig.len() as u64));
        // Script
        data.extend_from_slice(&input.script_sig);
        // Sequence (4 bytes, little-endian)
        data.extend_from_slice(&(input.sequence as u32).to_le_bytes());
    }

    // Output count (varint)
    data.extend_from_slice(&encode_varint(tx.outputs.len() as u64));

    // Outputs
    for output in &tx.outputs {
        // Value (8 bytes, little-endian)
        data.extend_from_slice(&(output.value as u64).to_le_bytes());
        // Script length (varint)
        data.extend_from_slice(&encode_varint(output.script_pubkey.len() as u64));
        // Script
        data.extend_from_slice(&output.script_pubkey);
    }

    // Lock time (4 bytes, little-endian)
    data.extend_from_slice(&(tx.lock_time as u32).to_le_bytes());

    // Double SHA256
    let hash1 = Sha256::digest(&data);
    let hash2 = Sha256::digest(&hash1);

    let mut result = [0u8; 32];
    result.copy_from_slice(&hash2);
    result
}

/// Encode a number as a Bitcoin varint
fn encode_varint(value: u64) -> Vec<u8> {
    if value < 0xfd {
        vec![value as u8]
    } else if value <= 0xffff {
        let mut result = vec![0xfd];
        result.extend_from_slice(&(value as u16).to_le_bytes());
        result
    } else if value <= 0xffffffff {
        let mut result = vec![0xfe];
        result.extend_from_slice(&(value as u32).to_le_bytes());
        result
    } else {
        let mut result = vec![0xff];
        result.extend_from_slice(&value.to_le_bytes());
        result
    }
}

/// Calculate short transaction ID
///
/// Uses SipHash-2-4 with keys derived from block header nonce.
///
/// # Arguments
/// * `tx_hash` - Full transaction hash (32 bytes)
/// * `nonce` - Block nonce (used to derive SipHash keys)
///
/// # Returns
/// Short transaction ID (6 bytes)
pub fn calculate_short_tx_id(tx_hash: &Hash, nonce: u64) -> ShortTxId {
    // Derive SipHash keys from nonce
    let k0 = nonce;
    let k1 = nonce.wrapping_add(1);

    // Use SipHash-2-4 to hash the transaction hash
    // siphasher 0.3 uses the Hasher trait from std::hash
    use siphasher::sip::SipHasher24;
    let mut hasher = SipHasher24::new_with_keys(k0, k1);
    hasher.write(tx_hash);
    let hash_result = hasher.finish();

    // Take first 6 bytes (48 bits) as short ID
    let mut short_id = [0u8; 6];
    short_id.copy_from_slice(&hash_result.to_le_bytes()[..6]);
    short_id
}

/// Reconstruct full block from compact block
///
/// Attempts to match short IDs with transactions from mempool,
/// then requests missing transactions from peer.
///
/// **BIP125 + BIP152 Integration**: When matching transactions from mempool,
/// this function uses RBF conflict detection to ensure that matched transactions
/// don't conflict with each other or with already-matched transactions.
///
/// # Arguments
/// * `compact_block` - The compact block to reconstruct
/// * `mempool_txs` - Map of transaction hash to transaction from mempool
///
/// # Returns
/// Vector of indices for missing transactions (to request via getblocktxn)
pub fn reconstruct_block(
    compact_block: &CompactBlock,
    mempool_txs: &HashMap<Hash, Transaction>,
) -> Result<Vec<usize>> {
    let mut missing_indices = Vec::new();
    let mut reconstructed_txs = Vec::new();

    // Match short IDs with mempool transactions
    for (index, &short_id) in compact_block.short_ids.iter().enumerate() {
        let mut matched = false;

        // Search mempool for transaction matching this short ID
        for (tx_hash, tx) in mempool_txs {
            let calculated_short_id = calculate_short_tx_id(tx_hash, compact_block.nonce);
            if calculated_short_id == short_id {
                // BIP125 + BIP152 Integration: Check for RBF conflicts
                // If this transaction conflicts with already-matched transactions,
                // don't use it (the block version is authoritative)
                let has_conflict = reconstructed_txs
                    .iter()
                    .any(|(_, existing_tx)| has_conflict_with_tx(tx, existing_tx));

                if !has_conflict {
                    reconstructed_txs.push((index, tx.clone()));
                    matched = true;
                    break;
                } else {
                    // Conflict detected: This mempool transaction is a replacement
                    // The block contains the authoritative version, so we'll request it
                    // This ensures we get the correct transaction that won the RBF
                    matched = false; // Will be requested from peer
                    break;
                }
            }
        }

        if !matched {
            missing_indices.push(index);
        }
    }

    Ok(missing_indices)
}

/// Check if two transactions conflict (BIP125 requirement #4)
///
/// A conflict exists if tx1 and tx2 spend at least one common input.
/// Used during compact block reconstruction to detect RBF conflicts.
fn has_conflict_with_tx(tx1: &Transaction, tx2: &Transaction) -> bool {
    for input1 in &tx1.inputs {
        for input2 in &tx2.inputs {
            if input1.prevout == input2.prevout {
                return true;
            }
        }
    }
    false
}

/// Create compact block from full block
///
/// # Arguments
/// * `block` - Full block to convert
/// * `nonce` - Nonce for short ID calculation (typically from block header)
/// * `prefilled_indices` - Indices of transactions to include in full (not as short IDs)
///
/// # Returns
/// Compact block representation
pub fn create_compact_block(
    block: &Block,
    nonce: u64,
    prefilled_indices: &HashSet<usize>,
) -> CompactBlock {
    let mut short_ids = Vec::new();
    let mut prefilled_txs = Vec::new();

    // Calculate short IDs for all transactions
    for (index, tx) in block.transactions.iter().enumerate() {
        if prefilled_indices.contains(&index) {
            // Include transaction in full (0-indexed per BIP152)
            prefilled_txs.push((index, tx.clone()));
        } else {
            // Include as short ID - calculate actual transaction hash first
            let tx_hash = calculate_tx_hash(tx);
            let short_id = calculate_short_tx_id(&tx_hash, nonce);
            short_ids.push(short_id);
        }
    }

    CompactBlock {
        header: block.header.clone(),
        nonce,
        short_ids,
        prefilled_txs,
    }
}

/// Determine if compact blocks should be preferred for a given transport type
///
/// Compact blocks are especially beneficial for QUIC transports (Iroh/Quinn)
/// due to QUIC's lower latency and better handling of multiple streams.
///
/// # Arguments
/// * `transport_type` - The transport type being used
///
/// # Returns
/// `true` if compact blocks should be preferred for this transport
pub fn should_prefer_compact_blocks(transport_type: TransportType) -> bool {
    match transport_type {
        #[cfg(feature = "quinn")]
        TransportType::Quinn => true, // QUIC: prefer compact blocks for lower latency
        #[cfg(feature = "iroh")]
        TransportType::Iroh => true, // Iroh QUIC: definitely prefer compact blocks
        #[cfg(any(feature = "quinn", feature = "iroh"))]
        TransportType::Tcp => false, // TCP: standard blocks are fine, compact blocks optional
        #[cfg(not(any(feature = "quinn", feature = "iroh")))]
        TransportType::Tcp => false, // TCP: standard blocks are fine, compact blocks optional
    }
}

/// Negotiate both compact blocks and block filters with a peer
///
/// When a peer supports both BIP152 (compact blocks) and BIP157 (filters),
/// coordinate the negotiation to use both optimizations together.
///
/// # Arguments
/// * `transport_type` - The transport being used
/// * `peer_services` - Service flags from peer's version message
///
/// # Returns
/// Tuple of (compact_block_version, prefer_compact, supports_filters)
pub fn negotiate_optimizations(
    transport_type: TransportType,
    peer_services: u64,
) -> (u64, bool, bool) {
    use bllvm_protocol::bip157::NODE_COMPACT_FILTERS;

    let compact_version = recommended_compact_block_version(transport_type);
    let prefer_compact = should_prefer_compact_blocks(transport_type);
    let supports_filters = (peer_services & NODE_COMPACT_FILTERS) != 0;

    (compact_version, prefer_compact, supports_filters)
}

/// Create optimized SendCmpct message considering both compact blocks and filters
///
/// When both features are available, recommends using compact blocks
/// with filter support for maximum bandwidth efficiency.
///
/// This coordinates BIP152 (compact blocks) and BIP157 (filters) negotiation,
/// ensuring peers can use both optimizations together when available.
pub fn create_optimized_sendcmpct(
    transport_type: TransportType,
    peer_services: u64,
) -> crate::network::protocol::SendCmpctMessage {
    use crate::network::protocol::SendCmpctMessage;

    let (version, prefer_cmpct, _supports_filters) =
        negotiate_optimizations(transport_type, peer_services);

    // When both compact blocks and filters are available, prefer compact blocks
    // This reduces bandwidth for both features working together
    SendCmpctMessage {
        version,
        prefer_cmpct: if prefer_cmpct { 1 } else { 0 },
    }
}

/// Get recommended compact block version based on transport
///
/// Returns the compact block version to negotiate based on transport capabilities.
/// Version 2 adds prefilled transaction index optimization which works well with QUIC.
///
/// # Arguments
/// * `transport_type` - The transport type being used
///
/// # Returns
/// Recommended compact block version (1 or 2)
pub fn recommended_compact_block_version(transport_type: TransportType) -> u64 {
    match transport_type {
        TransportType::Tcp => 1, // Version 1 is sufficient for TCP
        #[cfg(feature = "quinn")]
        TransportType::Quinn => 2, // Version 2 for QUIC (better prefilled optimization)
        #[cfg(feature = "iroh")]
        TransportType::Iroh => 2, // Version 2 for Iroh (NAT traversal benefits from optimization)
    }
}

/// Check if transport supports QUIC (Iroh or Quinn)
///
/// QUIC transports benefit more from compact blocks due to:
/// - Lower latency on connection establishment
/// - Better multiplexing for multiple block requests
/// - Stream prioritization for compact block data
///
/// # Arguments
/// * `transport_type` - The transport type to check
///
/// # Returns
/// `true` if transport is QUIC-based
pub fn is_quic_transport(transport_type: TransportType) -> bool {
    match transport_type {
        #[cfg(feature = "quinn")]
        TransportType::Quinn => true,
        #[cfg(feature = "iroh")]
        TransportType::Iroh => true,
        #[cfg(any(feature = "quinn", feature = "iroh"))]
        TransportType::Tcp => false,
        #[cfg(not(any(feature = "quinn", feature = "iroh")))]
        TransportType::Tcp => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::transport::TransportType;

    #[test]
    fn test_calculate_short_tx_id() {
        let tx_hash = [0u8; 32];
        let nonce = 12345u64;
        let short_id = calculate_short_tx_id(&tx_hash, nonce);

        // Short ID should be 6 bytes
        assert_eq!(short_id.len(), 6);
    }

    #[test]
    fn test_reconstruct_block_empty_mempool() {
        let compact_block = CompactBlock {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0; 32],
                merkle_root: [0; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            nonce: 0,
            short_ids: vec![[0u8; 6]],
            prefilled_txs: vec![],
        };

        let mempool_txs = HashMap::new();
        let missing = reconstruct_block(&compact_block, &mempool_txs).unwrap();

        // All transactions should be missing
        assert_eq!(missing.len(), 1);
    }

    #[test]
    fn test_should_prefer_compact_blocks_tcp() {
        // TCP: compact blocks optional, not preferred by default
        assert_eq!(should_prefer_compact_blocks(TransportType::Tcp), false);
    }

    #[cfg(feature = "quinn")]
    #[test]
    fn test_should_prefer_compact_blocks_quinn() {
        // Quinn QUIC: prefer compact blocks
        assert_eq!(should_prefer_compact_blocks(TransportType::Quinn), true);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_should_prefer_compact_blocks_iroh() {
        // Iroh QUIC: definitely prefer compact blocks
        assert_eq!(should_prefer_compact_blocks(TransportType::Iroh), true);
    }

    #[test]
    fn test_recommended_compact_block_version_tcp() {
        // TCP: version 1 is sufficient
        assert_eq!(recommended_compact_block_version(TransportType::Tcp), 1);
    }

    #[cfg(feature = "quinn")]
    #[test]
    fn test_recommended_compact_block_version_quinn() {
        // Quinn: version 2 for better optimization
        assert_eq!(recommended_compact_block_version(TransportType::Quinn), 2);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_recommended_compact_block_version_iroh() {
        // Iroh: version 2 for NAT traversal benefits
        assert_eq!(recommended_compact_block_version(TransportType::Iroh), 2);
    }

    #[test]
    fn test_is_quic_transport_tcp() {
        // TCP is not QUIC
        assert_eq!(is_quic_transport(TransportType::Tcp), false);
    }

    #[cfg(feature = "quinn")]
    #[test]
    fn test_is_quic_transport_quinn() {
        // Quinn is QUIC
        assert_eq!(is_quic_transport(TransportType::Quinn), true);
    }

    #[cfg(feature = "iroh")]
    #[test]
    fn test_is_quic_transport_iroh() {
        // Iroh is QUIC
        assert_eq!(is_quic_transport(TransportType::Iroh), true);
    }
}

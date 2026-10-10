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
use blvm_protocol::{Block, BlockHeader, Hash, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;

pub use blvm_protocol::bip152::{CompactBlock, ShortTxId};

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
        data.extend_from_slice(&input.prevout.index.to_le_bytes());
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
    let hash2 = Sha256::digest(hash1);

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

/// SipHash-2-4 keys for a compact-block short id.
///
/// The keys are the first two little-endian 64-bit integers of
/// SHA256(80-byte header || compact-block nonce).
fn short_id_keys(header: &BlockHeader, nonce: u64) -> (u64, u64) {
    let mut data = blvm_protocol::serialization::serialize_block_header(header);
    data.extend_from_slice(&nonce.to_le_bytes());
    let hash = Sha256::digest(&data);
    let k0 = u64::from_le_bytes(hash[0..8].try_into().expect("sha256 prefix"));
    let k1 = u64::from_le_bytes(hash[8..16].try_into().expect("sha256 prefix"));
    (k0, k1)
}

/// Calculate a compact-block short transaction id.
///
/// `wtxid` is the witness transaction id. A transaction with no witness uses
/// the transaction id. The SipHash keys come from `header` and `nonce`.
pub fn calculate_short_tx_id(header: &BlockHeader, wtxid: &Hash, nonce: u64) -> ShortTxId {
    let (k0, k1) = short_id_keys(header, nonce);
    use siphasher::sip::SipHasher24;
    let mut hasher = SipHasher24::new_with_keys(k0, k1);
    hasher.write(wtxid);
    let hash_result = hasher.finish();
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
    let prefilled: HashSet<usize> = compact_block
        .prefilled_txs
        .iter()
        .map(|(index, _, _)| *index)
        .collect();
    let mut next_block_index = 0usize;

    // Short ids are only the transactions that were not prefilled, in block order.
    for &short_id in &compact_block.short_ids {
        while prefilled.contains(&next_block_index) {
            next_block_index = next_block_index.saturating_add(1);
        }
        let block_index = next_block_index;
        next_block_index = next_block_index.saturating_add(1);

        // More than one pool transaction with this short id is ambiguous.
        // The block transaction has to be requested rather than guessed.
        let mut matches = Vec::new();
        for (tx_hash, tx) in mempool_txs {
            let calculated_short_id =
                calculate_short_tx_id(&compact_block.header, tx_hash, compact_block.nonce);
            if calculated_short_id == short_id {
                matches.push(tx);
            }
        }

        if let [tx] = matches.as_slice() {
            let conflicts_match = reconstructed_txs
                .iter()
                .any(|(_, existing_tx)| has_conflict_with_tx(tx, existing_tx));
            let conflicts_prefilled = compact_block
                .prefilled_txs
                .iter()
                .any(|(_, existing_tx, _)| has_conflict_with_tx(tx, existing_tx));
            if conflicts_match || conflicts_prefilled {
                missing_indices.push(block_index);
            } else {
                reconstructed_txs.push((block_index, (*tx).clone()));
            }
        } else {
            missing_indices.push(block_index);
        }
    }

    Ok(missing_indices)
}

/// One slot of a compact block while `getblocktxn` / `blocktxn` are in flight.
#[derive(Debug, Clone)]
pub struct CompactAssembly {
    pub header: BlockHeader,
    pub nonce: u64,
    /// `None` until a short-id match or a `blocktxn` fills the position.
    pub slots: Vec<Option<(Transaction, Option<Vec<blvm_protocol::segwit::Witness>>)>>,
    pub missing: Vec<usize>,
}

/// Place prefilled transactions and unique, non-conflicting pool matches.
///
/// `mempool_txs` is keyed by witness transaction id. Missing positions stay empty.
pub fn begin_compact_assembly(
    compact_block: &CompactBlock,
    mempool_txs: &HashMap<Hash, (Transaction, Option<Vec<blvm_protocol::segwit::Witness>>)>,
) -> Result<CompactAssembly> {
    let tx_only: HashMap<Hash, Transaction> = mempool_txs
        .iter()
        .map(|(hash, (tx, _))| (*hash, tx.clone()))
        .collect();
    let missing = reconstruct_block(compact_block, &tx_only)?;
    let n = compact_block.short_ids.len() + compact_block.prefilled_txs.len();
    let mut slots = vec![None; n];
    for (index, tx, witness) in &compact_block.prefilled_txs {
        if *index >= n {
            anyhow::bail!("prefilled compact-block index {index} is past {n}");
        }
        slots[*index] = Some((tx.clone(), witness.clone()));
    }
    let prefilled: HashSet<usize> = compact_block
        .prefilled_txs
        .iter()
        .map(|(index, _, _)| *index)
        .collect();
    let mut next_block_index = 0usize;
    for &short_id in &compact_block.short_ids {
        while prefilled.contains(&next_block_index) {
            next_block_index = next_block_index.saturating_add(1);
        }
        let block_index = next_block_index;
        next_block_index = next_block_index.saturating_add(1);
        if missing.contains(&block_index) {
            continue;
        }
        let mut found = None;
        for (tx_hash, (tx, witness)) in mempool_txs {
            let calculated =
                calculate_short_tx_id(&compact_block.header, tx_hash, compact_block.nonce);
            if calculated == short_id {
                found = Some((tx.clone(), witness.clone()));
                break;
            }
        }
        if let Some(filled) = found {
            if block_index < slots.len() {
                slots[block_index] = Some(filled);
            }
        }
    }
    Ok(CompactAssembly {
        header: compact_block.header.clone(),
        nonce: compact_block.nonce,
        slots,
        missing,
    })
}

/// Fill `missing` positions from a `blocktxn`, keeping each witness stack.
pub fn apply_blocktxn(
    assembly: &mut CompactAssembly,
    transactions: &[Transaction],
    witnesses: Option<&[Vec<blvm_protocol::segwit::Witness>]>,
) -> Result<()> {
    if transactions.len() != assembly.missing.len() {
        anyhow::bail!(
            "blocktxn count {} does not match {} missing positions",
            transactions.len(),
            assembly.missing.len()
        );
    }
    for (i, &index) in assembly.missing.iter().enumerate() {
        if index >= assembly.slots.len() {
            anyhow::bail!("blocktxn index {index} is past the compact block");
        }
        let witness = witnesses.and_then(|all| all.get(i).cloned());
        assembly.slots[index] = Some((transactions[i].clone(), witness));
    }
    assembly.missing.clear();
    Ok(())
}

/// Transactions and witness stacks in block order, after every slot is filled.
pub fn completed_compact_block(
    assembly: &CompactAssembly,
) -> Result<(Block, Vec<Vec<blvm_protocol::segwit::Witness>>)> {
    let mut transactions = Vec::with_capacity(assembly.slots.len());
    let mut witnesses = Vec::with_capacity(assembly.slots.len());
    for (index, slot) in assembly.slots.iter().enumerate() {
        let Some((tx, witness)) = slot else {
            anyhow::bail!("compact block position {index} is still missing");
        };
        let stacks = witness
            .clone()
            .unwrap_or_else(|| tx.inputs.iter().map(|_| Vec::new()).collect());
        witnesses.push(stacks);
        transactions.push(tx.clone());
    }
    Ok((
        Block {
            header: assembly.header.clone(),
            transactions: transactions.into_boxed_slice(),
        },
        witnesses,
    ))
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
    create_compact_block_with_witnesses(block, nonce, prefilled_indices, &[])
}

/// Create a compact block. `witnesses` is one optional stack list per transaction.
/// A missing or empty stack list uses the transaction id, which equals the witness id.
pub fn create_compact_block_with_witnesses(
    block: &Block,
    nonce: u64,
    prefilled_indices: &HashSet<usize>,
    witnesses: &[Option<Vec<blvm_protocol::segwit::Witness>>],
) -> CompactBlock {
    let mut short_ids = Vec::new();
    let mut prefilled_txs = Vec::new();

    for (index, tx) in block.transactions.iter().enumerate() {
        if prefilled_indices.contains(&index) {
            let witness = witnesses
                .get(index)
                .and_then(|stacks| stacks.clone())
                .filter(|stacks| stacks.iter().any(|stack| !stack.is_empty()));
            prefilled_txs.push((index, tx.clone(), witness));
        } else {
            let witness = witnesses.get(index).and_then(|stacks| stacks.as_deref());
            let wtxid = crate::network::txhash::calculate_wtxid(tx, witness);
            let short_id = calculate_short_tx_id(&block.header, &wtxid, nonce);
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
    use blvm_protocol::bip157::NODE_COMPACT_FILTERS;

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
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        let short_id = calculate_short_tx_id(&header, &tx_hash, nonce);

        // Short ID should be 6 bytes
        assert_eq!(short_id.len(), 6);
    }

    #[test]
    fn short_id_uses_header_keys_and_witness_hash() {
        use blvm_protocol::opcodes::OP_1;

        let wtxid = [1u8; 32];
        let nonce = 7u64;
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [2u8; 32],
            merkle_root: [3u8; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        let other_header = BlockHeader {
            merkle_root: [4u8; 32],
            ..header.clone()
        };
        assert_ne!(
            calculate_short_tx_id(&header, &wtxid, nonce),
            calculate_short_tx_id(&other_header, &wtxid, nonce)
        );

        let tx = Transaction {
            version: 1,
            inputs: vec![blvm_protocol::TransactionInput {
                prevout: blvm_protocol::OutPoint {
                    hash: [5u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: blvm_protocol::constants::SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![blvm_protocol::TransactionOutput {
                value: 50_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let block = Block {
            header: header.clone(),
            transactions: vec![tx].into_boxed_slice(),
        };
        let bare = create_compact_block_with_witnesses(&block, nonce, &HashSet::new(), &[]);
        let with_witness = create_compact_block_with_witnesses(
            &block,
            nonce,
            &HashSet::new(),
            &[Some(vec![vec![vec![OP_1]]])],
        );
        assert_ne!(bare.short_ids, with_witness.short_ids);
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
    fn missing_short_id_keeps_the_block_index_after_a_prefilled_coinbase() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

        let tx = |prev: u8, value: i64| Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [prev; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        let block = Block {
            header,
            transactions: vec![tx(1, 50_000), tx(2, 40_000)].into_boxed_slice(),
        };
        let compact = create_compact_block(&block, 7, &HashSet::from([0]));
        assert_eq!(compact.prefilled_txs[0].0, 0);
        assert_eq!(compact.short_ids.len(), 1);

        let missing = reconstruct_block(&compact, &HashMap::new()).unwrap();
        assert_eq!(missing, vec![1]);
    }

    #[test]
    fn prefilled_witness_survives_wire_conversion() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

        let tx = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0; 32],
                merkle_root: [0; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: vec![tx].into_boxed_slice(),
        };
        let witness = vec![vec![vec![OP_1]]];
        let compact = create_compact_block_with_witnesses(
            &block,
            7,
            &HashSet::from([0]),
            &[Some(witness.clone())],
        );
        assert_eq!(compact.prefilled_txs[0].2, Some(witness.clone()));

        let message = blvm_protocol::network::CmpctBlockMessage::try_from(compact).unwrap();
        assert_eq!(message.prefilled_txs[0].witness, Some(witness.clone()));

        let back = blvm_protocol::bip152::CompactBlock::from(&message);
        assert_eq!(back.prefilled_txs[0].2, Some(witness));
    }

    #[test]
    fn ambiguous_short_id_is_requested() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

        let header = BlockHeader {
            version: 0,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        let nonce = 0u64;
        let mut first = [0u8; 32];
        first[24] = 0x99;
        first[25] = 0x13;
        first[26] = 0x4b;
        let mut second = [0u8; 32];
        second[24] = 0xd7;
        second[25] = 0x08;
        second[26] = 0x79;
        let shared = calculate_short_tx_id(&header, &first, nonce);
        assert_eq!(shared, calculate_short_tx_id(&header, &second, nonce));

        let unique = [9u8; 32];
        let unique_id = calculate_short_tx_id(&header, &unique, nonce);
        assert_ne!(unique_id, shared);

        let tx = |version: u64| Transaction {
            version,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [version as u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let mut pool = HashMap::new();
        pool.insert(first, tx(1));
        pool.insert(second, tx(2));
        pool.insert(unique, tx(3));
        let compact = CompactBlock {
            header,
            nonce,
            short_ids: vec![shared, unique_id],
            prefilled_txs: vec![],
        };
        let missing = reconstruct_block(&compact, &pool).unwrap();
        assert_eq!(missing, vec![0]);
    }

    #[test]
    fn test_should_prefer_compact_blocks_tcp() {
        // TCP: compact blocks optional, not preferred by default
        assert!(!should_prefer_compact_blocks(TransportType::Tcp));
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
        assert!(!is_quic_transport(TransportType::Tcp));
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

    #[test]
    fn blocktxn_witness_fills_the_short_id_after_a_prefilled_coinbase() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

        let tx = |prev: u8| Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [prev; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0; 32],
            merkle_root: [0; 32],
            timestamp: 0,
            bits: 0,
            nonce: 0,
        };
        let block = Block {
            header,
            transactions: vec![tx(1), tx(2)].into_boxed_slice(),
        };
        let compact = create_compact_block(&block, 7, &HashSet::from([0]));
        let mut assembly = begin_compact_assembly(&compact, &HashMap::new()).unwrap();
        assert_eq!(assembly.missing, vec![1]);
        let witness = vec![vec![vec![OP_1]]];
        apply_blocktxn(&mut assembly, &[tx(2)], Some(&[witness.clone()])).unwrap();
        let (reconstructed, witnesses) = completed_compact_block(&assembly).unwrap();
        assert_eq!(witnesses[1], witness);
        assert_eq!(
            reconstructed.transactions[1].outputs[0].script_pubkey,
            vec![OP_1]
        );
    }
}

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
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, BlockHeader, Hash, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::time::{Duration, Instant};

pub use blvm_protocol::bip152::{CompactBlock, ShortTxId};

/// A compact block with more transactions than this is fetched whole.
pub const MAX_COMPACT_BLOCK_TXS: usize = 100_000;
/// More holes than this, and the block is fetched whole.
pub const MAX_COMPACT_MISSING: usize = 128;
/// Assemblies retained while `getblocktxn` is in flight.
pub const MAX_PENDING_COMPACT: usize = 4;
/// Bytes retained across those assemblies.
pub const MAX_PENDING_COMPACT_BYTES: usize = 8 * 1024 * 1024;
/// An assembly older than this is dropped and the block is fetched whole.
pub const COMPACT_ASSEMBLY_DEADLINE: Duration = Duration::from_secs(60);

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
    let mut by_short: HashMap<ShortTxId, Vec<&Transaction>> = HashMap::new();
    for (tx_hash, tx) in mempool_txs {
        let short_id = calculate_short_tx_id(&compact_block.header, tx_hash, compact_block.nonce);
        by_short.entry(short_id).or_default().push(tx);
    }

    // Short ids are only the transactions that were not prefilled, in block order.
    for &short_id in &compact_block.short_ids {
        while prefilled.contains(&next_block_index) {
            next_block_index = next_block_index.saturating_add(1);
        }
        let block_index = next_block_index;
        next_block_index = next_block_index.saturating_add(1);

        // More than one pool transaction with this short id is ambiguous.
        // The block transaction has to be requested rather than guessed.
        let matches = by_short.get(&short_id).map(Vec::as_slice).unwrap_or(&[]);

        if let [tx] = matches {
            let tx = *tx;
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
    pub slots: Vec<Option<(Transaction, Option<Vec<Witness>>)>>,
    pub missing: Vec<usize>,
    /// When this assembly was created. Older than [`COMPACT_ASSEMBLY_DEADLINE`] is dropped.
    pub started: Instant,
    /// `6 * short_ids.len()`, counted in [`assembly_retained_bytes`].
    pub short_id_bytes: usize,
}

impl CompactAssembly {
    pub fn expired_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= COMPACT_ASSEMBLY_DEADLINE
    }
}

/// Serialized transaction bytes in `slots`, plus one slot's size times capacity,
/// plus the short-id bytes recorded on the assembly.
pub fn assembly_retained_bytes(assembly: &CompactAssembly) -> usize {
    let tx_bytes = assembly
        .slots
        .iter()
        .flatten()
        .map(|(tx, witness)| serialized_slot_bytes(tx, witness.as_deref()))
        .sum::<usize>();
    let slot_bytes = assembly
        .slots
        .capacity()
        .saturating_mul(std::mem::size_of::<
            Option<(Transaction, Option<Vec<Witness>>)>,
        >());
    tx_bytes
        .saturating_add(slot_bytes)
        .saturating_add(assembly.short_id_bytes)
}

fn serialized_slot_bytes(tx: &Transaction, witness: Option<&[Witness]>) -> usize {
    let body = blvm_protocol::serialization::transaction::serialize_transaction(tx).len();
    let witness_bytes = witness
        .map(|stacks| {
            stacks
                .iter()
                .map(|stack| stack.iter().map(|item| item.len()).sum::<usize>())
                .sum::<usize>()
        })
        .unwrap_or(0);
    body.saturating_add(witness_bytes)
}

/// What the pending table should do with one compact block.
pub struct CompactBudget {
    /// Assemblies already held, not counting this block's hash.
    pub assemblies: usize,
    /// Retained bytes of those assemblies.
    pub retained_bytes: usize,
    /// This block hash is already in the table.
    pub same_hash: bool,
}

pub enum CompactDecision {
    Hold(CompactAssembly),
    /// Fetch this block whole. Do not insert an assembly.
    FullBlock,
    /// A reconstruction of this hash is already held and the replacement
    /// does not fit. Leave that assembly in place.
    KeepCurrent,
}

/// Caps from the compact-block budget. A `FullBlock` result must be fetched with `getdata`.
pub fn decide_compact_block(
    compact: &CompactBlock,
    matches: &HashMap<Hash, (Transaction, Option<Vec<Witness>>)>,
    budget: CompactBudget,
) -> Result<CompactDecision> {
    let total = compact
        .short_ids
        .len()
        .saturating_add(compact.prefilled_txs.len());
    if total > MAX_COMPACT_BLOCK_TXS {
        return Ok(CompactDecision::FullBlock);
    }
    let assembly = begin_compact_assembly(compact, matches)?;
    if assembly.missing.len() > MAX_COMPACT_MISSING
        || assembly
            .missing
            .iter()
            .any(|index| u16::try_from(*index).is_err())
    {
        return Ok(CompactDecision::FullBlock);
    }
    let adding = if budget.same_hash { 0 } else { 1 };
    if budget.assemblies.saturating_add(adding) > MAX_PENDING_COMPACT {
        return Ok(CompactDecision::FullBlock);
    }
    let next_bytes = budget
        .retained_bytes
        .saturating_add(assembly_retained_bytes(&assembly));
    if next_bytes > MAX_PENDING_COMPACT_BYTES {
        if budget.same_hash {
            return Ok(CompactDecision::KeepCurrent);
        }
        return Ok(CompactDecision::FullBlock);
    }
    Ok(CompactDecision::Hold(assembly))
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
    // First pool row for a short id wins, matching the previous scan. Ambiguous
    // ids are already in `missing` from `reconstruct_block` and are not filled.
    let mut by_short: HashMap<ShortTxId, &(Transaction, Option<Vec<Witness>>)> = HashMap::new();
    for (tx_hash, pair) in mempool_txs {
        let short_id = calculate_short_tx_id(&compact_block.header, tx_hash, compact_block.nonce);
        by_short.entry(short_id).or_insert(pair);
    }
    let missing_set: HashSet<usize> = missing.iter().copied().collect();
    let mut next_block_index = 0usize;
    for &short_id in &compact_block.short_ids {
        while prefilled.contains(&next_block_index) {
            next_block_index = next_block_index.saturating_add(1);
        }
        let block_index = next_block_index;
        next_block_index = next_block_index.saturating_add(1);
        if missing_set.contains(&block_index) {
            continue;
        }
        let found = by_short.get(&short_id).map(|pair| (*pair).clone());
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
        started: Instant::now(),
        short_id_bytes: 6usize.saturating_mul(compact_block.short_ids.len()),
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
#[path = "compact_blocks_tests.rs"]
mod tests;

//! Block processing and validation integration
//!
//! Handles parsing blocks from wire format, storing witnesses, and validating
//! blocks with proper witness data and median time-past.

use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use crate::utils::current_timestamp;
use anyhow::Result;
use blvm_protocol::serialization::deserialize_block_with_witnesses;
use blvm_protocol::validation::ProtocolValidationContext;
use blvm_protocol::{
    BitcoinProtocolEngine, Block, BlockHeader, ProtocolVersion, UtxoSet, ValidationResult,
    segwit::Witness,
};
use std::sync::Arc;

/// Parse a block from Bitcoin wire format and extract witness data
// CRITICAL FIX: witnesses is now Vec<Vec<Witness>> (one Vec per transaction, each containing one Witness per input)
pub fn parse_block_from_wire(data: &[u8]) -> Result<(Block, Vec<Vec<Witness>>)> {
    let (block, witnesses) = deserialize_block_with_witnesses(data)
        .map_err(|e| anyhow::anyhow!("Failed to parse block from wire format: {}", e))?;

    // Do not compare against serialize_block_with_witnesses length: that re-encode is not
    // byte-identical to the P2P block payload for segwit mainnet blocks (lossy round-trip).

    Ok((block, witnesses))
}

/// Empty witness stacks (one per input) for pre-SegWit blocks.
fn empty_witnesses_for_block(block: &Block) -> Vec<Vec<Witness>> {
    block
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
        .collect()
}

fn load_witnesses_from_store(
    blockstore: &BlockStore,
    block: &Block,
    height: u64,
    segwit_active: bool,
) -> Result<Vec<Vec<Witness>>> {
    let block_hash = blockstore.get_block_hash(block);
    match blockstore.get_witness(&block_hash)? {
        Some(w) => {
            if w.len() != block.transactions.len() {
                anyhow::bail!(
                    "witness count {} != transaction count {} at height {height}",
                    w.len(),
                    block.transactions.len()
                );
            }
            Ok(w)
        }
        None if !segwit_active => Ok(empty_witnesses_for_block(block)),
        None => anyhow::bail!("missing witness data for block at height {height}"),
    }
}

/// Load witnesses for validation using consensus network fork activation (height-based).
pub fn load_witnesses_for_block_network(
    blockstore: &BlockStore,
    block: &Block,
    height: u64,
    network: blvm_protocol::types::Network,
) -> Result<Vec<Vec<Witness>>> {
    use blvm_consensus::activation::IsForkActive;
    use blvm_consensus::block::BlockValidationContext;
    use blvm_consensus::types::ForkId;

    let ctx = BlockValidationContext::for_network(network);
    let segwit_active = ctx.is_fork_active(ForkId::SegWit, height);
    load_witnesses_from_store(blockstore, block, height, segwit_active)
}

/// Load witnesses for validation. Pre-SegWit: synthesize empty stacks when not stored.
/// Post-SegWit: absent blockstore entry is an error (fail closed). Stored all-empty stacks
/// are valid (e.g. coinbase-only blocks).
pub fn load_witnesses_for_block(
    blockstore: &BlockStore,
    block: &Block,
    height: u64,
    protocol_version: ProtocolVersion,
) -> Result<Vec<Vec<Witness>>> {
    load_witnesses_for_block_network(
        blockstore,
        block,
        height,
        protocol_version.consensus_network(),
    )
}

/// Store a block with its witnesses and update recent headers
pub fn store_block_with_context(
    blockstore: &BlockStore,
    block: &Block,
    witnesses: &[Vec<Witness>], // CRITICAL FIX: Changed from &[Witness] to &[Vec<Witness>]
    height: u64,
) -> Result<()> {
    blockstore.store_block_with_witness(block, witnesses, height)?;
    let block_hash = blockstore.get_block_hash(block);
    blockstore.store_height(height, &block_hash)?;
    Ok(())
}

/// Store a block with its witnesses, update recent headers, and index transactions
/// This is the preferred method when Storage is available for transaction indexing
pub fn store_block_with_context_and_index(
    blockstore: &BlockStore,
    storage: Option<&Arc<Storage>>,
    block: &Block,
    witnesses: &[Vec<Witness>], // CRITICAL FIX: Changed from &[Witness] to &[Vec<Witness>]
    height: u64,
) -> Result<()> {
    // Store block
    store_block_with_context(blockstore, block, witnesses, height)?;

    // Index transactions if storage is available
    if let Some(storage) = storage {
        let block_hash = blockstore.get_block_hash(block);
        if let Err(e) = storage.index_block(block, &block_hash, height) {
            // Log error but don't fail block storage if indexing fails
            tracing::warn!("Failed to index block transactions: {}", e);
        }
        #[cfg(feature = "production")]
        if let Ok(Some(undo)) = blockstore.get_undo_log(&block_hash) {
            for tx in block.transactions.iter() {
                let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
                for input in tx.inputs.iter() {
                    let Some(entry) = undo.entries.iter().find(|e| e.outpoint == input.prevout) else {
                        continue;
                    };
                    let Some(prev) = &entry.previous_utxo else {
                        continue;
                    };
                    let _ = storage.transactions().note_spend(
                        prev.script_pubkey.as_ref(),
                        prev.value,
                        &tx_hash,
                    );
                }
            }
        }
        if let Err(e) = storage.chain().record_connected_block(
            &block_hash,
            height,
            &block.header.prev_block_hash,
        ) {
            tracing::warn!("Failed to record block in chain index: {}", e);
        }
    }

    Ok(())
}

/// Headers whose median is the time bound for a block at `height`.
///
/// The bound is the median of the ancestors, so the window ends at `height - 1`.
/// The tip's `recent_headers` window is a different chain once this block is not
/// the next block after the tip.
pub(crate) fn mtp_headers_before(blockstore: &BlockStore, height: u64) -> Option<Vec<BlockHeader>> {
    if height == 0 {
        return None;
    }
    blockstore
        .headers_back_from(height - 1, 11)
        .ok()
        .filter(|headers| !headers.is_empty())
}

/// Retrieve witnesses and headers for block validation
// CRITICAL FIX: witnesses is now Vec<Vec<Witness>> (one Vec per transaction, each containing one Witness per input)
pub fn prepare_block_validation_context(
    blockstore: &BlockStore,
    block: &Block,
    current_height: u64,
    protocol_version: ProtocolVersion,
) -> Result<(Vec<Vec<Witness>>, Option<Vec<BlockHeader>>)> {
    let witnesses = load_witnesses_for_block(blockstore, block, current_height, protocol_version)?;

    let recent_headers = mtp_headers_before(blockstore, current_height);

    Ok((witnesses, recent_headers))
}

/// Reject when H08 fails and a parent header is available at `height - 1`.
fn reject_if_bad_prev_hash(
    blockstore: &BlockStore,
    block: &Block,
    height: u64,
) -> Result<Option<ValidationResult>> {
    if height == 0 {
        return Ok(None);
    }
    if let Some(parent) = blockstore.get_header_at_height(height - 1)? {
        if !blvm_consensus::block::validate_prev_block_hash(&block.header, &parent) {
            return Ok(Some(ValidationResult::Invalid(
                "Block prev_block_hash does not match parent header hash".to_string(),
            )));
        }
    }
    Ok(None)
}

/// Validate a block using protocol validation with proper witness data and headers
///
/// This function uses the protocol engine's `validate_and_connect_block()` method,
/// which ensures protocol-specific validation (size limits, feature flags) is always applied.
pub fn validate_block_with_context(
    blockstore: &BlockStore,
    protocol: &BitcoinProtocolEngine,
    block: &Block,
    witnesses: &[Vec<Witness>], // CRITICAL FIX: Changed from &[Witness] to &[Vec<Witness>]
    utxo_set: &mut UtxoSet,
    height: u64,
) -> Result<ValidationResult> {
    if let Some(invalid) = reject_if_bad_prev_hash(blockstore, block, height)? {
        return Ok(invalid);
    }

    let recent_headers = mtp_headers_before(blockstore, height);

    // Compute median time-past (BIP113) from recent headers, if available
    let median_time_past = recent_headers
        .as_ref()
        .map(|headers| blvm_protocol::bip113::get_median_time_past(headers))
        .unwrap_or(0);

    // Use the node's time utility as the single source of network time.
    // This can later be upgraded to adjusted network time without touching consensus.
    let network_time = current_timestamp();

    // Create protocol validation context and populate time fields
    let mut context = ProtocolValidationContext::new(protocol.get_protocol_version(), height)?;
    context
        .context_data
        .insert("median_time_past".to_string(), median_time_past.to_string());
    context
        .context_data
        .insert("network_time".to_string(), network_time.to_string());

    // Validate block with protocol validation — move UTXO set in, no clone
    let owned_utxo = std::mem::take(utxo_set);
    let (result, new_utxo_set) = protocol.validate_and_connect_block_with_difficulty(
        block,
        witnesses,
        &owned_utxo,
        height,
        recent_headers.as_deref(),
        &context,
        Some(blockstore.difficulty_ancestor_lookup()),
    )?;

    *utxo_set = new_utxo_set;
    Ok(result)
}

/// Protocol-only validation (no UTXO connect). Use for side-chain blocks stored
/// without becoming the active tip; full connect runs during reorg.
#[cfg(feature = "production")]
pub fn validate_block_protocol_only(
    blockstore: &BlockStore,
    protocol: &BitcoinProtocolEngine,
    block: &Block,
    witnesses: &[Vec<Witness>],
    height: u64,
    utxo_set: &UtxoSet,
) -> Result<ValidationResult> {
    let recent_headers = mtp_headers_before(blockstore, height);

    let median_time_past = recent_headers
        .as_ref()
        .map(|headers| blvm_protocol::bip113::get_median_time_past(headers))
        .unwrap_or(0);
    let network_time = current_timestamp();

    let mut context = ProtocolValidationContext::new(protocol.get_protocol_version(), height)?;
    context
        .context_data
        .insert("median_time_past".to_string(), median_time_past.to_string());
    context
        .context_data
        .insert("network_time".to_string(), network_time.to_string());

    protocol
        .validate_block_with_protocol_and_witnesses(block, witnesses, utxo_set, height, &context)
        .map_err(Into::into)
}

/// Validate and connect a block, returning the consensus undo log for persistence.
#[cfg(feature = "production")]
pub fn validate_block_with_context_and_undo(
    blockstore: &BlockStore,
    protocol: &BitcoinProtocolEngine,
    block: &Block,
    witnesses: &[Vec<Witness>],
    utxo_set: &mut UtxoSet,
    height: u64,
) -> Result<(
    ValidationResult,
    UtxoSet,
    blvm_consensus::reorganization::BlockUndoLog,
)> {
    if let Some(invalid) = reject_if_bad_prev_hash(blockstore, block, height)? {
        return Ok((
            invalid,
            utxo_set.clone(),
            blvm_consensus::reorganization::BlockUndoLog::new(),
        ));
    }

    let recent_headers = mtp_headers_before(blockstore, height);

    let median_time_past = recent_headers
        .as_ref()
        .map(|headers| blvm_protocol::bip113::get_median_time_past(headers))
        .unwrap_or(0);
    let network_time = current_timestamp();

    let mut context = ProtocolValidationContext::new(protocol.get_protocol_version(), height)?;
    context
        .context_data
        .insert("median_time_past".to_string(), median_time_past.to_string());
    context
        .context_data
        .insert("network_time".to_string(), network_time.to_string());

    let protocol_result = protocol
        .validate_block_with_protocol_and_witnesses(block, witnesses, utxo_set, height, &context)?;
    if !matches!(protocol_result, ValidationResult::Valid) {
        return Ok((
            protocol_result,
            utxo_set.clone(),
            blvm_consensus::reorganization::BlockUndoLog::new(),
        ));
    }

    let mut consensus_ctx =
        protocol.connect_block_validation_context(recent_headers.as_deref(), network_time);
    consensus_ctx.sequence_prev_mtp = Some(blockstore.sequence_prev_mtp_lookup());
    consensus_ctx.difficulty_ancestor = Some(blockstore.difficulty_ancestor_lookup());
    let owned_utxo = std::mem::take(utxo_set);
    let (result, new_utxo_set, undo_log) =
        blvm_consensus::block::connect_block(block, witnesses, owned_utxo, height, &consensus_ctx)?;
    *utxo_set = new_utxo_set;
    Ok((result, utxo_set.clone(), undo_log))
}

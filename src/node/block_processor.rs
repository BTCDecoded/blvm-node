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

    // Validate that consensus serialization matches the original wire size
    // Uses consensus serialization through blvm_protocol::serialization re-exports.
    let include_witness = true;
    if !blvm_protocol::serialization::block::validate_block_serialized_size(
        &block,
        &witnesses,
        include_witness,
        data.len(),
    ) {
        anyhow::bail!("Block size mismatch: serialized block does not match wire size");
    }

    Ok((block, witnesses))
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

/// Retrieve witnesses and headers for block validation
// CRITICAL FIX: witnesses is now Vec<Vec<Witness>> (one Vec per transaction, each containing one Witness per input)
pub fn prepare_block_validation_context(
    blockstore: &BlockStore,
    block: &Block,
    _current_height: u64,
) -> Result<(Vec<Vec<Witness>>, Option<Vec<BlockHeader>>)> {
    // Get witnesses for this block
    let block_hash = blockstore.get_block_hash(block);
    let witnesses = blockstore.get_witness(&block_hash)?.unwrap_or_else(|| {
        block
            .transactions
            .iter()
            .map(|tx| tx.inputs.iter().map(|_| Vec::new()).collect())
            .collect()
    });

    // Get recent headers for median time-past (BIP113)
    let recent_headers = blockstore
        .get_recent_headers(11)
        .ok()
        .filter(|headers| !headers.is_empty());

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

    // Get recent headers for median time-past
    let recent_headers = blockstore
        .get_recent_headers(11)
        .ok()
        .filter(|headers| !headers.is_empty());

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
    let (result, new_utxo_set) = protocol.validate_and_connect_block(
        block,
        witnesses,
        &owned_utxo,
        height,
        recent_headers.as_deref(),
        &context,
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
    let recent_headers = blockstore
        .get_recent_headers(11)
        .ok()
        .filter(|headers| !headers.is_empty());

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

    let recent_headers = blockstore
        .get_recent_headers(11)
        .ok()
        .filter(|headers| !headers.is_empty());

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

    let consensus_ctx =
        protocol.connect_block_validation_context(recent_headers.as_deref(), network_time);
    let owned_utxo = std::mem::take(utxo_set);
    let (result, new_utxo_set, undo_log) =
        blvm_consensus::block::connect_block(block, witnesses, owned_utxo, height, &consensus_ctx)?;
    *utxo_set = new_utxo_set;
    Ok((result, utxo_set.clone(), undo_log))
}

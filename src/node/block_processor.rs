//! Block processing and validation integration
//!
//! Handles parsing blocks from wire format, storing witnesses, and validating
//! blocks with proper witness data and median time-past.

use crate::storage::blockstore::BlockStore;
use anyhow::Result;
use bllvm_protocol::block::connect_block;
use bllvm_protocol::serialization::deserialize_block_with_witnesses;
use bllvm_protocol::types::Network;
use bllvm_protocol::{
    segwit::Witness, Block, BlockHeader, ProtocolVersion, UtxoSet, ValidationResult,
};

/// Convert ProtocolVersion to Network type
pub fn protocol_version_to_network(version: ProtocolVersion) -> Network {
    match version {
        ProtocolVersion::BitcoinV1 => Network::Mainnet,
        ProtocolVersion::Testnet3 => Network::Testnet,
        ProtocolVersion::Regtest => Network::Regtest,
    }
}

/// Parse a block from Bitcoin wire format and extract witness data
pub fn parse_block_from_wire(data: &[u8]) -> Result<(Block, Vec<Witness>)> {
    deserialize_block_with_witnesses(data)
        .map_err(|e| anyhow::anyhow!("Failed to parse block from wire format: {}", e))
}

/// Store a block with its witnesses and update recent headers
pub fn store_block_with_context(
    blockstore: &BlockStore,
    block: &Block,
    witnesses: &[Witness],
    height: u64,
) -> Result<()> {
    // Store block
    blockstore.store_block(block)?;

    // Store witnesses if present
    if !witnesses.is_empty() {
        let block_hash = blockstore.get_block_hash(block);
        blockstore.store_witness(&block_hash, witnesses)?;
    }

    // Store header for median time-past calculation
    blockstore.store_recent_header(height, &block.header)?;

    // Update height index
    let block_hash = blockstore.get_block_hash(block);
    blockstore.store_height(height, &block_hash)?;

    Ok(())
}

/// Retrieve witnesses and headers for block validation
pub fn prepare_block_validation_context(
    blockstore: &BlockStore,
    block: &Block,
    _current_height: u64,
) -> Result<(Vec<Witness>, Option<Vec<BlockHeader>>)> {
    // Get witnesses for this block
    let block_hash = blockstore.get_block_hash(block);
    let witnesses = blockstore
        .get_witness(&block_hash)?
        .unwrap_or_else(|| block.transactions.iter().map(|_| Vec::new()).collect());

    // Get recent headers for median time-past (BIP113)
    let recent_headers = blockstore
        .get_recent_headers(11)
        .ok()
        .filter(|headers| !headers.is_empty());

    Ok((witnesses, recent_headers))
}

/// Validate a block using connect_block with proper witness data and headers
pub fn validate_block_with_context(
    blockstore: &BlockStore,
    block: &Block,
    witnesses: &[Witness],
    utxo_set: &mut UtxoSet,
    height: u64,
    network: Network,
) -> Result<ValidationResult> {
    // Get recent headers for median time-past
    let recent_headers = blockstore
        .get_recent_headers(11)
        .ok()
        .filter(|headers| !headers.is_empty());

    // Validate block
    let (result, new_utxo_set) = connect_block(
        block,
        witnesses,
        utxo_set.clone(),
        height,
        recent_headers.as_deref(),
        network,
    )?;

    // Update UTXO set if valid
    if matches!(result, ValidationResult::Valid) {
        *utxo_set = new_utxo_set;
    }

    Ok(result)
}

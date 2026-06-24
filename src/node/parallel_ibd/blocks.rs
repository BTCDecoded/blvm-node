//! Block validation and storage for parallel IBD.

use crate::node::block_processor::{
    prepare_block_validation_context, store_block_with_context_and_index,
    validate_block_with_context,
};
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use anyhow::Result;
use blvm_protocol::{
    BitcoinProtocolEngine, Block, Hash, UtxoSet, ValidationResult, segwit::Witness,
};
use std::sync::Arc;
use tracing::debug;

/// Record block-index entries and chainwork for blocks flushed during parallel IBD.
pub(crate) fn index_ibd_flushed_blocks(
    storage: &Storage,
    block_hashes: &[Hash],
    chunk: &[(
        Block,
        Arc<Vec<Vec<Witness>>>,
        u64,
        blvm_consensus::reorganization::BlockUndoLog,
    )],
    heights: &[u64],
) -> Result<()> {
    debug_assert_eq!(block_hashes.len(), chunk.len());
    debug_assert_eq!(block_hashes.len(), heights.len());
    for i in 0..block_hashes.len() {
        let (block, _, _, _) = &chunk[i];
        storage
            .chain()
            .index_connected_block(&block_hashes[i], &block.header, heights[i])?;
    }
    Ok(())
}

/// Validate and store a block.
///
/// Prepares validation context, validates the block, and stores it if valid.
pub(crate) fn validate_and_store_block(
    blockstore: &BlockStore,
    storage: Option<&Arc<Storage>>,
    protocol: &BitcoinProtocolEngine,
    utxo_set: &mut UtxoSet,
    block: &Block,
    witnesses: &[Vec<Witness>],
    height: u64,
) -> Result<()> {
    let (stored_witnesses, _recent_headers) = prepare_block_validation_context(
        blockstore,
        block,
        height,
        protocol.get_protocol_version(),
    )?;

    let witnesses_to_use = if !witnesses.is_empty() {
        witnesses
    } else {
        &stored_witnesses
    };

    if witnesses_to_use.len() != block.transactions.len() {
        return Err(anyhow::anyhow!(
            "Witness count mismatch at height {}: {} witnesses for {} transactions",
            height,
            witnesses_to_use.len(),
            block.transactions.len()
        ));
    }

    let validation_result = validate_block_with_context(
        blockstore,
        protocol,
        block,
        witnesses_to_use,
        utxo_set,
        height,
    )?;

    if matches!(validation_result, ValidationResult::Valid) {
        store_block_with_context_and_index(blockstore, storage, block, witnesses_to_use, height)?;

        debug!("Validated and stored block at height {}", height);
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Block validation failed at height {}",
            height
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::BlockHeader;
    use tempfile::TempDir;

    #[test]
    fn index_ibd_flushed_blocks_records_each_height() {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        let genesis_header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 0,
            bits: 0x207fffff,
            nonce: 0,
        };
        storage.chain().initialize(&genesis_header).unwrap();
        let genesis_hash = storage.chain().get_tip_hash().unwrap().unwrap();

        let make_block = |prev: Hash, nonce: u8| Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: prev,
                merkle_root: [nonce; 32],
                timestamp: nonce as u64,
                bits: 0x207fffff,
                nonce: nonce as u64,
            },
            transactions: Box::new([]),
        };

        let block1 = make_block(genesis_hash, 1);
        let block2 = make_block([1u8; 32], 2);
        let hash1 = [1u8; 32];
        let hash2 = [2u8; 32];

        index_ibd_flushed_blocks(
            &storage,
            &[hash1, hash2],
            &[
                (
                    block1,
                    Arc::new(vec![]),
                    1,
                    blvm_consensus::reorganization::BlockUndoLog::new(),
                ),
                (
                    block2,
                    Arc::new(vec![]),
                    2,
                    blvm_consensus::reorganization::BlockUndoLog::new(),
                ),
            ],
            &[1, 2],
        )
        .unwrap();

        assert!(storage.chain().block_index().get(&hash1).unwrap().is_some());
        assert!(storage.chain().block_index().get(&hash2).unwrap().is_some());
        let tips = storage.chain().block_index().chain_tips().unwrap();
        assert_eq!(tips.len(), 1);
        assert_eq!(tips[0].0, hash2);
    }
}

//! Block validation and storage for parallel IBD.

use crate::node::block_processor::{
    prepare_block_validation_context, store_block_with_context_and_index,
    validate_block_with_context,
};
use crate::storage::blockstore::BlockStore;
use crate::storage::Storage;
use anyhow::Result;
use blvm_protocol::{segwit::Witness, BitcoinProtocolEngine, Block, UtxoSet, ValidationResult};
use std::sync::Arc;
use tracing::debug;

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
    let (stored_witnesses, _recent_headers) =
        prepare_block_validation_context(blockstore, block, height)?;

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

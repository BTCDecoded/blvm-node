//! Chain reorganization: disconnect/connect via `blvm-consensus` and persist results.

use crate::node::chain_selector::should_activate_over_active_tip;
use crate::node::event_publisher::EventPublisher;
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use crate::utils::current_timestamp;
use anyhow::{Context, Result};
use blvm_consensus::reorganization::{BlockUndoLog, reorganize_chain_with_witnesses};
use blvm_consensus::types::Network;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{BitcoinProtocolEngine, Block, Hash, UtxoSet};
use std::sync::Arc;
use tracing::{info, warn};

fn protocol_network(protocol: &BitcoinProtocolEngine) -> Network {
    protocol.get_protocol_version().consensus_network()
}

/// Walk parent links in the block index from `tip` toward genesis (ascending height).
pub fn collect_chain_to_tip(
    storage: &Storage,
    blockstore: &BlockStore,
    tip: &Hash,
) -> Result<Vec<Block>> {
    let index = storage.chain().block_index();
    let mut blocks = Vec::new();
    let mut current = *tip;
    let mut guard = 0u64;
    while guard < 10_000 {
        guard += 1;
        let Some(entry) = index.get(&current)? else {
            break;
        };
        let block = blockstore
            .get_block(&current)?
            .with_context(|| format!("missing block {current:?} for reorg"))?;
        blocks.push(block);
        if entry.height == 0 {
            break;
        }
        current = entry.prev_hash;
    }
    blocks.reverse();
    Ok(blocks)
}

fn witnesses_for_chain(
    blockstore: &BlockStore,
    storage: &Storage,
    chain: &[Block],
    protocol: &BitcoinProtocolEngine,
) -> Result<Vec<Vec<Vec<Witness>>>> {
    let protocol_version = protocol.get_protocol_version();
    let mut out = Vec::with_capacity(chain.len());
    for block in chain {
        let hash = blockstore.get_block_hash(block);
        let height = storage
            .chain()
            .block_index()
            .get(&hash)?
            .map(|e| e.height)
            .unwrap_or(0);
        let witnesses = crate::node::block_processor::load_witnesses_for_block(
            blockstore,
            block,
            height,
            protocol_version,
        )?;
        out.push(witnesses);
    }
    Ok(out)
}

fn refresh_active_height_index(
    storage: &Storage,
    blockstore: &BlockStore,
    tip: &Hash,
) -> Result<()> {
    let index = storage.chain().block_index();
    let mut current = *tip;
    let mut guard = 0u64;
    while guard < 10_000 {
        guard += 1;
        let Some(entry) = index.get(&current)? else {
            break;
        };
        blockstore.store_height(entry.height, &current)?;
        if entry.height == 0 {
            break;
        }
        current = entry.prev_hash;
    }
    Ok(())
}

fn block_height_in_index(storage: &Storage, blockstore: &BlockStore, block: &Block) -> u64 {
    let hash = blockstore.get_block_hash(block);
    storage
        .chain()
        .block_index()
        .get(&hash)
        .ok()
        .flatten()
        .map(|e| e.height)
        .unwrap_or(0)
}

/// Publish disconnect notifications (tip-first) and a single `ChainReorg` when a runtime is available.
pub fn publish_reorg_events(
    publisher: Option<&Arc<EventPublisher>>,
    storage: &Storage,
    blockstore: &BlockStore,
    old_tip: &Hash,
    new_tip: &Hash,
    disconnected_blocks: &[Block],
) {
    let Some(publisher) = publisher else {
        return;
    };

    let mut disconnects: Vec<(Hash, u64)> = Vec::with_capacity(disconnected_blocks.len());
    for block in disconnected_blocks.iter().rev() {
        let hash = blockstore.get_block_hash(block);
        let height = block_height_in_index(storage, blockstore, block);
        disconnects.push((hash, height));
    }
    let old_tip = *old_tip;
    let new_tip = *new_tip;
    let publisher = Arc::clone(publisher);

    let publish = async move {
        for (hash, height) in disconnects {
            publisher.publish_block_disconnected(&hash, height).await;
        }
        publisher.publish_chain_reorg(&old_tip, &new_tip).await;
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(publish);
    } else if let Ok(rt) = tokio::runtime::Runtime::new() {
        rt.block_on(publish);
    } else {
        warn!("No tokio runtime; skipping reorg event publish");
    }
}

/// Switch to `candidate_tip` when it carries more work than the active tip.
#[cfg(feature = "production")]
pub fn try_activate_heavier_fork(
    storage: &Storage,
    blockstore: &BlockStore,
    protocol: &BitcoinProtocolEngine,
    candidate_tip: &Hash,
    utxo_set: &mut UtxoSet,
    event_publisher: Option<&Arc<EventPublisher>>,
) -> Result<bool> {
    if !should_activate_over_active_tip(storage, candidate_tip)? {
        return Ok(false);
    }

    let (active_tip, active_height) = storage.chain().get_tip_hash_and_height()?;
    let current_chain = collect_chain_to_tip(storage, blockstore, &active_tip)?;
    let new_chain = collect_chain_to_tip(storage, blockstore, candidate_tip)?;
    if current_chain.is_empty() || new_chain.is_empty() {
        return Ok(false);
    }

    let new_witnesses = witnesses_for_chain(blockstore, storage, &new_chain, protocol)?;
    let network = protocol_network(protocol);
    let utxo_backup = utxo_set.clone();
    let owned_utxo = std::mem::take(utxo_set);

    let blockstore_ref = blockstore;
    let get_undo = |hash: &Hash| blockstore_ref.get_undo_log(hash).ok().flatten();
    let put_undo = |hash: &Hash, log: &BlockUndoLog| {
        blockstore_ref.store_undo_log(hash, log).map_err(|e| {
            blvm_consensus::error::ConsensusError::BlockValidation(e.to_string().into())
        })
    };

    let result = reorganize_chain_with_witnesses(
        &new_chain,
        &new_witnesses,
        None,
        &current_chain,
        owned_utxo,
        active_height,
        None::<fn(&Block) -> Option<Vec<Witness>>>,
        None::<fn(u64) -> Option<Vec<blvm_protocol::BlockHeader>>>,
        Some(get_undo),
        Some(put_undo),
        current_timestamp(),
        network,
    )
    .map_err(|e| {
        *utxo_set = utxo_backup;
        anyhow::anyhow!("reorganize_chain_with_witnesses: {e}")
    })?;

    *utxo_set = result.new_utxo_set;

    let new_tip_block = result
        .connected_blocks
        .last()
        .or(new_chain.last())
        .context("reorg produced no connected blocks")?;
    let new_tip_hash = blockstore.get_block_hash(new_tip_block);
    let new_height = result.new_height;

    storage
        .chain()
        .update_tip(&new_tip_hash, &new_tip_block.header, new_height)?;
    refresh_active_height_index(storage, blockstore, &new_tip_hash)?;
    storage.utxos().store_utxo_set(utxo_set)?;

    publish_reorg_events(
        event_publisher,
        storage,
        blockstore,
        &active_tip,
        &new_tip_hash,
        &result.disconnected_blocks,
    );

    info!(
        "Chain reorg: depth {}, new tip height {}, hash {:?}",
        result.reorganization_depth, new_height, new_tip_hash
    );
    Ok(true)
}

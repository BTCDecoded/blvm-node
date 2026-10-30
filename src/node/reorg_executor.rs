//! Chain reorganization: disconnect/connect via `blvm-consensus` and persist results.

use crate::node::chain_selector::should_activate_over_active_tip;
use crate::node::event_publisher::EventPublisher;
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use crate::storage::database::Tree;
use crate::storage::disk_utxo::outpoint_to_key;
use crate::storage::utxo_value_codec::{
    ValueCodec, decode_utxo_with_codec, encode_utxo_with_codec,
};
use crate::utils::current_timestamp;
use anyhow::{Context, Result};
use blvm_consensus::reorganization::{BlockUndoLog, reorganize_chain_with_witnesses};
use blvm_consensus::types::{BlockHeader, Network};
use blvm_protocol::segwit::Witness;
use blvm_protocol::{BitcoinProtocolEngine, Block, Hash, UtxoSet};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

fn protocol_network(protocol: &BitcoinProtocolEngine) -> Network {
    protocol.get_protocol_version().consensus_network()
}

fn load_reorg_block(blockstore: &BlockStore, hash: &Hash) -> Result<Block> {
    blockstore
        .get_block(hash)?
        .with_context(|| format!("missing block {hash:?} for reorg"))
}

/// Point-read the inputs a connect will spend. Coins the disconnect undo restores
/// are not in the snapshot; the reorg inserts those from the undo log.
pub(crate) fn read_snapshot_inputs(
    tree: &dyn Tree,
    codec: ValueCodec,
    blocks: &[Block],
    utxo_set: &mut UtxoSet,
) -> Result<usize> {
    let mut loaded = 0usize;
    for block in blocks {
        for tx in block.transactions.iter() {
            if blvm_consensus::transaction::is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                if utxo_set.contains_key(&input.prevout) {
                    continue;
                }
                let key = outpoint_to_key(&input.prevout);
                let Some(bytes) = tree.get(&key)? else {
                    continue;
                };
                let utxo = decode_utxo_with_codec(codec, &bytes)?;
                utxo_set.insert(input.prevout, Arc::new(utxo));
                loaded += 1;
            }
        }
    }
    Ok(loaded)
}

/// Where a reorg or connect reads coins when the in-memory set is empty.
///
/// An IBD export height means the coins live in the snapshot. The chainstate
/// table was never that set. Loading it and writing the reorg's fragment back
/// replaces the database with the coins this fork happened to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiskUtxoSource {
    Snapshot,
    ChainTable,
    MissingSnapshot,
}

pub(crate) fn disk_utxo_source(snapshot_open: bool, export_height_set: bool) -> DiskUtxoSource {
    if snapshot_open {
        DiskUtxoSource::Snapshot
    } else if export_height_set {
        DiskUtxoSource::MissingSnapshot
    } else {
        DiskUtxoSource::ChainTable
    }
}

/// Not an outpoint. Scanners that require a 40-byte key skip it.
pub(crate) const SNAPSHOT_REORG_JOURNAL_KEY: &[u8] = b"blvm-reorg-journal";
/// Running MuHash after this batch. Startup copies it into chainstate.
pub(crate) const SNAPSHOT_MUHASH_KEY: &[u8] = b"blvm-muhash-running";

pub(crate) fn encode_reorg_journal(height: u64, tip: &Hash) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(tip);
    bytes
}

pub(crate) fn parse_reorg_journal(bytes: &[u8]) -> Option<(u64, Hash)> {
    if bytes.len() != 40 {
        return None;
    }
    let mut height = [0u8; 8];
    height.copy_from_slice(&bytes[..8]);
    let mut tip = [0u8; 32];
    tip.copy_from_slice(&bytes[8..]);
    Some((u64::from_be_bytes(height), tip))
}

/// Last write wins. Disconnect is tip-first; connect is ancestor-to-tip.
/// `None` deletes the row. `Some` inserts it. One batch, so a crash does not
/// leave a half-applied fork. The journal and the MuHash bytes are in that
/// same batch. Startup finishes the tip move if the process dies before it.
pub(crate) fn commit_utxo_snapshot_delta(
    tree: &dyn Tree,
    codec: ValueCodec,
    disconnect_tip_first: &[&BlockUndoLog],
    connect_low_to_high: &[&BlockUndoLog],
    journal_tip: Option<(u64, Hash)>,
    mut muhash: Option<&mut blvm_muhash::MuHash3072>,
) -> Result<usize> {
    let mut net: HashMap<[u8; 40], Option<Vec<u8>>> = HashMap::new();
    for undo in disconnect_tip_first {
        for entry in &undo.entries {
            let key = outpoint_to_key(&entry.outpoint);
            if entry.new_utxo.is_some() {
                net.insert(key, None);
            }
            if let Some(previous) = &entry.previous_utxo {
                net.insert(key, Some(encode_utxo_with_codec(codec, previous)?));
            }
        }
    }
    for undo in connect_low_to_high {
        for entry in undo.entries.iter().rev() {
            let key = outpoint_to_key(&entry.outpoint);
            if entry.previous_utxo.is_some() {
                net.insert(key, None);
            }
            if let Some(created) = &entry.new_utxo {
                net.insert(key, Some(encode_utxo_with_codec(codec, created)?));
            }
        }
    }
    let count = net.len();
    if let Some(mh) = muhash.as_mut() {
        for (key, value) in &net {
            let outpoint = crate::storage::disk_utxo::key_to_outpoint(key);
            if let Some(old) = tree.get(key)? {
                let utxo = decode_utxo_with_codec(codec, &old)?;
                crate::storage::ibd_utxo_muhash::apply_snapshot_coin(mh, &outpoint, &utxo, false);
            }
            if let Some(bytes) = value {
                let utxo = decode_utxo_with_codec(codec, bytes)?;
                crate::storage::ibd_utxo_muhash::apply_snapshot_coin(mh, &outpoint, &utxo, true);
            }
        }
    }
    let mut batch = tree.batch()?;
    for (key, value) in &net {
        match value {
            None => batch.delete(key),
            Some(bytes) => batch.put(key, bytes),
        }
    }
    if let Some((height, tip)) = journal_tip {
        batch.put(
            SNAPSHOT_REORG_JOURNAL_KEY,
            &encode_reorg_journal(height, &tip),
        );
    }
    let muhash_bytes = muhash.map(|mh| mh.serialize_running_state());
    if let Some(bytes) = muhash_bytes.as_ref() {
        batch.put(SNAPSHOT_MUHASH_KEY, bytes);
    }
    batch.commit()?;
    Ok(count)
}

pub(crate) fn note_snapshot_height(storage: &Storage, height: u64) -> Result<()> {
    storage.chain().set_utxo_watermark(height)?;
    storage.chain().persist_engine_export_height(height)?;
    Ok(())
}

/// Record the snapshot height exactly, including a lower one.
///
/// [`note_snapshot_height`] only advances. Invalidate and a shorter reorg move
/// the tip down, and the next resume treats a higher watermark as a checkpoint
/// from blocks the store does not have.
pub(crate) fn rewind_snapshot_height(storage: &Storage, height: u64) -> Result<()> {
    storage.chain().force_set_ibd_utxo_watermark(height)?;
    storage.chain().force_set_engine_export_height(height)?;
    Ok(())
}

/// Finish a snapshot commit whose tip update did not reach disk.
///
/// The journal is written in the same batch as the coin delta. A crash after
/// that batch and before `update_tip` leaves the new coins under the old tip.
/// Replaying the reorg would then look up coins that are already spent.
pub(crate) fn recover_snapshot_commit(storage: &Storage, blockstore: &BlockStore) -> Result<()> {
    let Some((_, tree)) = storage.open_engine_resume_snapshot()? else {
        return Ok(());
    };
    if let Some(bytes) = tree.get(SNAPSHOT_MUHASH_KEY)? {
        if bytes.len() == blvm_muhash::MUHASH_RUNNING_STATE_BYTES {
            let mut running = [0u8; blvm_muhash::MUHASH_RUNNING_STATE_BYTES];
            running.copy_from_slice(&bytes);
            storage
                .chain()
                .persist_ibd_utxo_muhash_running_only(&running)?;
        }
        tree.remove(SNAPSHOT_MUHASH_KEY)?;
    }
    let Some(raw) = tree.get(SNAPSHOT_REORG_JOURNAL_KEY)? else {
        return Ok(());
    };
    let Some((height, hash)) = parse_reorg_journal(&raw) else {
        tree.remove(SNAPSHOT_REORG_JOURNAL_KEY)?;
        return Ok(());
    };
    let tip_hash = storage
        .chain()
        .get_tip_hash_and_height()
        .ok()
        .map(|(hash, _)| hash);
    if tip_hash != Some(hash) {
        let header = blockstore
            .get_header(&hash)?
            .with_context(|| format!("reorg journal tip {} has no header", hex::encode(hash)))?;
        storage.chain().update_tip(&hash, &header, height)?;
        refresh_active_height_index(storage, blockstore, &hash)?;
        info!("[REORG] finished committed snapshot journal at height {height}");
    }
    rewind_snapshot_height(storage, height)?;
    tree.remove(SNAPSHOT_REORG_JOURNAL_KEY)?;
    Ok(())
}

fn headers_on_chain(
    storage: &Storage,
    blockstore: &BlockStore,
    chain: &[Block],
) -> HashMap<u64, BlockHeader> {
    let index = storage.chain().block_index();
    let mut headers = HashMap::new();
    for block in chain {
        let hash = blockstore.get_block_hash(block);
        if let Ok(Some(entry)) = index.get(&hash) {
            headers.insert(entry.height, block.header.clone());
        }
    }
    headers
}

fn header_at(
    store: &BlockStore,
    overlay: &HashMap<u64, BlockHeader>,
    height: u64,
) -> Option<BlockHeader> {
    if let Some(header) = overlay.get(&height) {
        return Some(header.clone());
    }
    store.get_header_at_height(height).ok().flatten()
}

fn median_with_overlay(
    store: &BlockStore,
    overlay: &HashMap<u64, BlockHeader>,
    height: u64,
) -> Option<u64> {
    let start = height.saturating_sub(10);
    let mut headers = Vec::with_capacity(11);
    for h in start..=height {
        headers.push(header_at(store, overlay, h)?);
    }
    Some(blvm_protocol::bip113::get_median_time_past(&headers))
}

/// Height and parent for a reorg walk.
///
/// Side bodies stored before the chain index recorded them still have a header
/// and a height. The walk follows that header and writes the index row so the
/// next lookup does not miss it again.
fn fork_link(storage: &Storage, blockstore: &BlockStore, hash: &Hash) -> Result<(u64, Hash)> {
    if let Some(entry) = storage.chain().block_index().get(hash)? {
        return Ok((entry.height, entry.prev_hash));
    }
    let header = blockstore
        .get_header(hash)?
        .with_context(|| format!("missing index entry {hash:?}"))?;
    let height = blockstore
        .get_height_by_hash(hash)?
        .with_context(|| format!("missing index entry {hash:?}"))?;
    storage
        .chain()
        .record_connected_block(hash, height, &header.prev_block_hash)?;
    Ok((height, header.prev_block_hash))
}

/// Blocks from the common ancestor through each tip, oldest first.
///
/// The two slices start on the same block. Walking a fixed number of parents
/// from each tip separately shifts the windows when the tips differ in height,
/// and the ancestor search then reports that the chains do not meet.
pub fn collect_fork_chains(
    storage: &Storage,
    blockstore: &BlockStore,
    active_tip: &Hash,
    candidate_tip: &Hash,
) -> Result<(Vec<Block>, Vec<Block>)> {
    let (mut active_height, _) = fork_link(storage, blockstore, active_tip)
        .with_context(|| "active tip missing from block index")?;
    let (mut candidate_height, _) = fork_link(storage, blockstore, candidate_tip)
        .with_context(|| "candidate tip missing from block index")?;

    let mut active_hash = *active_tip;
    let mut candidate_hash = *candidate_tip;
    let mut active_above = Vec::new();
    let mut candidate_above = Vec::new();

    while active_height > candidate_height {
        active_above.push(load_reorg_block(blockstore, &active_hash)?);
        let (_, prev) = fork_link(storage, blockstore, &active_hash)?;
        active_hash = prev;
        active_height -= 1;
    }
    while candidate_height > active_height {
        candidate_above.push(load_reorg_block(blockstore, &candidate_hash)?);
        let (_, prev) = fork_link(storage, blockstore, &candidate_hash)?;
        candidate_hash = prev;
        candidate_height -= 1;
    }

    let mut active_fork = Vec::new();
    let mut candidate_fork = Vec::new();
    loop {
        if active_hash == candidate_hash {
            let ancestor = load_reorg_block(blockstore, &active_hash)?;
            active_fork.push(ancestor.clone());
            candidate_fork.push(ancestor);
            break;
        }
        active_fork.push(load_reorg_block(blockstore, &active_hash)?);
        candidate_fork.push(load_reorg_block(blockstore, &candidate_hash)?);
        if active_height == 0 {
            anyhow::bail!("chains do not share a common ancestor");
        }
        let (_, active_prev) = fork_link(storage, blockstore, &active_hash)?;
        let (_, candidate_prev) = fork_link(storage, blockstore, &candidate_hash)?;
        active_hash = active_prev;
        candidate_hash = candidate_prev;
        active_height -= 1;
        candidate_height -= 1;
    }

    active_fork.reverse();
    candidate_fork.reverse();
    active_above.reverse();
    candidate_above.reverse();
    active_fork.extend(active_above);
    candidate_fork.extend(candidate_above);
    Ok((active_fork, candidate_fork))
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
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current) {
            break;
        }
        let Some(entry) = index.get(&current)? else {
            break;
        };
        if blockstore.get_hash_by_height(entry.height)?.as_ref() == Some(&current) {
            break;
        }
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
    mempool: Option<&crate::node::mempool::MempoolManager>,
) -> Result<bool> {
    if !should_activate_over_active_tip(storage, candidate_tip)? {
        return Ok(false);
    }

    // The run loop does not keep the chain set in memory. IBD left it in the
    // snapshot tree. Loading that tree would copy every coin, and writing the
    // reorg's small map back with `store_utxo_set` would replace the database.
    let snapshot = if utxo_set.is_empty() {
        storage.open_engine_resume_snapshot()?
    } else {
        None
    };
    if utxo_set.is_empty() {
        let export_height_set = storage
            .chain()
            .get_engine_export_height()
            .ok()
            .flatten()
            .unwrap_or(0)
            > 0;
        match disk_utxo_source(snapshot.is_some(), export_height_set) {
            DiskUtxoSource::MissingSnapshot => {
                anyhow::bail!("reorg refused: IBD UTXO snapshot is empty");
            }
            DiskUtxoSource::ChainTable => {
                // No checkpoint: the table is the chain. The reorg has to see
                // those coins, and the set written back is the whole table.
                *utxo_set = storage.utxos().get_all_utxos()?;
            }
            DiskUtxoSource::Snapshot => {}
        }
    }

    let (active_tip, active_height) = storage.chain().get_tip_hash_and_height()?;
    let (current_chain, new_chain) =
        collect_fork_chains(storage, blockstore, &active_tip, candidate_tip)?;
    if current_chain.is_empty() || new_chain.is_empty() {
        return Ok(false);
    }

    let mut new_chain = new_chain;
    let mut new_witnesses = witnesses_for_chain(blockstore, storage, &new_chain, protocol)?;
    for (block, witnesses) in new_chain.iter_mut().zip(new_witnesses.iter_mut()) {
        let hash = blockstore.get_block_hash(block);
        let height = storage
            .chain()
            .block_index()
            .get(&hash)
            .ok()
            .flatten()
            .map(|e| e.height)
            .unwrap_or(0);
        let (restored, w) = crate::module::pipeline::try_rehydrate_block_for_consensus(
            height,
            hash,
            block.clone(),
            std::mem::take(witnesses),
        );
        *block = restored;
        *witnesses = w;
    }
    if let Some((name, tree)) = &snapshot {
        let loaded = read_snapshot_inputs(
            tree.as_ref(),
            storage.utxo_value_codec(),
            &new_chain[1..],
            utxo_set,
        )?;
        info!("[REORG] read {loaded} inputs from UTXO snapshot {name}");
    }
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

    let fork_headers = headers_on_chain(storage, blockstore, &new_chain);
    let mtp_store = blockstore.clone();
    let mtp_headers = fork_headers.clone();
    let difficulty_store = blockstore.clone();
    let difficulty_headers = fork_headers.clone();
    let header_store = blockstore.clone();
    let recent_headers_overlay = fork_headers;
    let get_headers = move |height: u64| -> Option<Vec<BlockHeader>> {
        if height == 0 {
            return Some(Vec::new());
        }
        let lo = height.saturating_sub(11);
        let mut headers = Vec::new();
        for h in lo..height {
            if let Some(header) = header_at(&header_store, &recent_headers_overlay, h) {
                headers.push(header);
            }
        }
        Some(headers)
    };
    let mut connect_context = move |_height: u64,
                                    recent_headers: Option<&[BlockHeader]>,
                                    network_time: u64,
                                    net: Network|
          -> blvm_consensus::block::BlockValidationContext {
        let mut ctx = blvm_consensus::block::block_validation_context_for_connect_ibd(
            recent_headers,
            network_time,
            net,
        );
        let mtp_store = mtp_store.clone();
        let mtp_headers = mtp_headers.clone();
        ctx.sequence_prev_mtp = Some(Arc::new(move |height| {
            median_with_overlay(&mtp_store, &mtp_headers, height)
        }));
        let difficulty_store = difficulty_store.clone();
        let difficulty_headers = difficulty_headers.clone();
        ctx.difficulty_ancestor = Some(Arc::new(move |height| {
            header_at(&difficulty_store, &difficulty_headers, height)
                .map(|header| (header.bits, header.timestamp))
        }));
        ctx
    };

    let result = reorganize_chain_with_witnesses(
        &new_chain,
        &new_witnesses,
        None,
        &current_chain,
        owned_utxo,
        active_height,
        None::<fn(&Block) -> Option<Vec<Witness>>>,
        Some(get_headers),
        Some(get_undo),
        Some(put_undo),
        current_timestamp(),
        network,
        Some(&mut connect_context),
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

    if let Some((name, tree)) = &snapshot {
        let codec = storage.utxo_value_codec();
        let mut disconnected = Vec::with_capacity(result.disconnected_blocks.len());
        for block in result.disconnected_blocks.iter().rev() {
            let hash = blockstore.get_block_hash(block);
            let undo = blockstore
                .get_undo_log(&hash)?
                .with_context(|| format!("missing undo for disconnected {}", hex::encode(hash)))?;
            disconnected.push(undo);
        }
        let disconnect_refs: Vec<&BlockUndoLog> = disconnected.iter().collect();
        let mut connect_refs = Vec::with_capacity(result.connected_blocks.len());
        for block in &result.connected_blocks {
            let hash = blockstore.get_block_hash(block);
            let undo = result
                .connected_block_undo_logs
                .get(&hash)
                .with_context(|| format!("missing undo for connected {}", hex::encode(hash)))?;
            connect_refs.push(undo);
        }
        let mut muhash = storage
            .chain()
            .get_ibd_utxo_muhash_running()
            .ok()
            .flatten()
            .map(|bytes| blvm_muhash::MuHash3072::deserialize_running_state(&bytes));
        let written = commit_utxo_snapshot_delta(
            tree.as_ref(),
            codec,
            &disconnect_refs,
            &connect_refs,
            Some((new_height, new_tip_hash)),
            muhash.as_mut(),
        )?;
        info!("[REORG] wrote {written} UTXO rows in snapshot {name}");
        if let Some(mh) = muhash.as_ref() {
            let bytes = mh.serialize_running_state();
            storage
                .chain()
                .persist_ibd_utxo_muhash_running_only(&bytes)?;
        }
    }

    storage
        .chain()
        .update_tip(&new_tip_hash, &new_tip_block.header, new_height)?;
    refresh_active_height_index(storage, blockstore, &new_tip_hash)?;
    if let Some((_, tree)) = &snapshot {
        // The delta above leaves the snapshot at `new_height`. The forward-only
        // setter would keep the old tip and the next start would treat that
        // checkpoint as ahead of the chain.
        rewind_snapshot_height(storage, new_height)?;
        let _ = tree.remove(SNAPSHOT_REORG_JOURNAL_KEY);
        let _ = tree.remove(SNAPSHOT_MUHASH_KEY);
        // The run loop must keep using the snapshot. The map here is only the
        // coins this reorg touched.
        *utxo_set = UtxoSet::default();
    } else {
        storage.utxos().store_utxo_set(utxo_set)?;
    }

    if let Some(pool) = mempool {
        let disconnected_witnesses =
            match witnesses_for_chain(blockstore, storage, &result.disconnected_blocks, protocol) {
                Ok(witnesses) => witnesses,
                Err(e) => {
                    warn!("reorg mempool readd could not load witnesses: {e}");
                    Vec::new()
                }
            };
        pool.apply_reorg(
            &result.disconnected_blocks,
            &disconnected_witnesses,
            &result.connected_blocks,
            utxo_set,
        );
    }

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

#[cfg(test)]
mod snapshot_delta_tests {
    use super::*;
    use blvm_protocol::types::{OutPoint, UTXO};
    use blvm_protocol::{Transaction, TransactionInput, TransactionOutput};

    fn coin(value: i64, height: u64) -> UTXO {
        UTXO {
            value,
            script_pubkey: vec![0x51].into(),
            height,
            is_coinbase: false,
        }
    }

    fn spend_undo(outpoint: OutPoint, previous: UTXO) -> BlockUndoLog {
        BlockUndoLog {
            entries: vec![blvm_consensus::reorganization::UndoEntry {
                outpoint,
                previous_utxo: Some(Arc::new(previous)),
                new_utxo: None,
            }],
        }
    }

    fn create_undo(outpoint: OutPoint, created: UTXO) -> BlockUndoLog {
        BlockUndoLog {
            entries: vec![blvm_consensus::reorganization::UndoEntry {
                outpoint,
                previous_utxo: None,
                new_utxo: Some(Arc::new(created)),
            }],
        }
    }

    #[test]
    fn a_shorter_snapshot_tip_rewinds_the_height_label() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage.chain().force_set_ibd_utxo_watermark(10).unwrap();
        storage.chain().force_set_engine_export_height(10).unwrap();
        note_snapshot_height(&storage, 4).unwrap();
        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(10));
        assert_eq!(
            storage.chain().get_engine_export_height().unwrap(),
            Some(10)
        );

        rewind_snapshot_height(&storage, 4).unwrap();

        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(4));
        assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(4));

        rewind_snapshot_height(&storage, 7).unwrap();

        assert_eq!(storage.chain().get_utxo_watermark().unwrap(), Some(7));
        assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(7));
    }

    #[test]
    fn a_snapshot_delta_updates_only_the_coins_the_fork_touches() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        let tree = storage.open_tree("ibd_utxos").unwrap();
        let codec = storage.utxo_value_codec();
        let kept = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let spent = OutPoint {
            hash: [2u8; 32],
            index: 0,
        };
        let created_by_old = OutPoint {
            hash: [3u8; 32],
            index: 0,
        };
        let created_by_new = OutPoint {
            hash: [4u8; 32],
            index: 0,
        };
        for (op, utxo) in [
            (kept, coin(10, 1)),
            (spent, coin(20, 4)),
            (created_by_old, coin(30, 9)),
        ] {
            let key = outpoint_to_key(&op);
            tree.insert(&key, &encode_utxo_with_codec(codec, &utxo).unwrap())
                .unwrap();
        }

        let disconnect = create_undo(created_by_old, coin(30, 9));
        let connect = BlockUndoLog {
            entries: vec![
                blvm_consensus::reorganization::UndoEntry {
                    outpoint: created_by_new,
                    previous_utxo: None,
                    new_utxo: Some(Arc::new(coin(40, 10))),
                },
                blvm_consensus::reorganization::UndoEntry {
                    outpoint: spent,
                    previous_utxo: Some(Arc::new(coin(20, 4))),
                    new_utxo: None,
                },
            ],
        };
        commit_utxo_snapshot_delta(
            tree.as_ref(),
            codec,
            &[&disconnect],
            &[&connect],
            None,
            None,
        )
        .unwrap();

        assert!(tree.get(&outpoint_to_key(&kept)).unwrap().is_some());
        assert!(tree.get(&outpoint_to_key(&spent)).unwrap().is_none());
        assert!(
            tree.get(&outpoint_to_key(&created_by_old))
                .unwrap()
                .is_none()
        );
        let born = tree
            .get(&outpoint_to_key(&created_by_new))
            .unwrap()
            .unwrap();
        let born = decode_utxo_with_codec(codec, &born).unwrap();
        assert_eq!(born.value, 40);
        assert_eq!(born.height, 10);
    }

    #[test]
    fn a_snapshot_read_loads_the_input_and_leaves_the_other_coin() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        let tree = storage.open_tree("ibd_utxos").unwrap();
        let codec = storage.utxo_value_codec();
        let spent = OutPoint {
            hash: [2u8; 32],
            index: 0,
        };
        let other = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        for (op, utxo) in [(spent, coin(20, 4)), (other, coin(10, 1))] {
            tree.insert(
                &outpoint_to_key(&op),
                &encode_utxo_with_codec(codec, &utxo).unwrap(),
            )
            .unwrap();
        }
        let block = Block {
            header: BlockHeader::default(),
            transactions: vec![Transaction {
                version: 1,
                inputs: vec![TransactionInput {
                    prevout: spent,
                    script_sig: vec![].into(),
                    sequence: 0xffff_ffff,
                }]
                .into(),
                outputs: vec![TransactionOutput {
                    value: 10,
                    script_pubkey: vec![0x52],
                }]
                .into(),
                lock_time: 0,
            }]
            .into(),
        };
        let mut set = UtxoSet::default();
        let n = read_snapshot_inputs(tree.as_ref(), codec, &[block], &mut set).unwrap();
        assert_eq!(n, 1);
        assert!(set.contains_key(&spent));
        assert!(!set.contains_key(&other));
        let _ = spend_undo(spent, coin(20, 4));
    }

    #[test]
    fn an_export_without_a_snapshot_is_not_the_chainstate_table() {
        assert_eq!(
            disk_utxo_source(false, true),
            DiskUtxoSource::MissingSnapshot
        );
        assert_eq!(disk_utxo_source(true, true), DiskUtxoSource::Snapshot);
        assert_eq!(disk_utxo_source(false, false), DiskUtxoSource::ChainTable);
    }

    #[test]
    fn a_committed_journal_finishes_the_tip_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        storage
            .chain()
            .set_ibd_utxo_canonical_tree("ibd_utxos")
            .unwrap();
        let tree = storage.open_tree("ibd_utxos").unwrap();
        let header = BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [5u8; 32],
            timestamp: 1_700_000_000,
            bits: 0x1d00ffff,
            nonce: 7,
        };
        let hash = blvm_consensus::block::block_header_hash(&header);
        storage.blocks().store_header(&hash, &header).unwrap();
        commit_utxo_snapshot_delta(
            tree.as_ref(),
            storage.utxo_value_codec(),
            &[],
            &[],
            Some((0, hash)),
            None,
        )
        .unwrap();
        recover_snapshot_commit(&storage, storage.blocks().as_ref()).unwrap();
        let (tip, height) = storage.chain().get_tip_hash_and_height().unwrap();
        assert_eq!(tip, hash);
        assert_eq!(height, 0);
        assert!(tree.get(SNAPSHOT_REORG_JOURNAL_KEY).unwrap().is_none());
        assert_eq!(storage.chain().get_engine_export_height().unwrap(), Some(0));
    }
}

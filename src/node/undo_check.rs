//! Detect undo data a reorg cannot use.
//!
//! Before the parallel-IBD undo fix, blocks connected by parallel IBD were stored with an
//! empty `BlockUndoLog`. A correct undo log is never empty: the coinbase always creates at
//! least one output, and every created output is recorded. Disconnecting a block whose undo
//! log is missing or empty restores nothing, so the UTXO set silently goes wrong.
//!
//! This module checks the most recent blocks at startup and logs what the operator has to
//! do. The reorg executor uses the same check to refuse a reorg that would disconnect such
//! a block.

use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use blvm_consensus::reorganization::BlockUndoLog;
use blvm_protocol::Hash;
use tracing::{error, info};

/// Blocks below the tip checked at startup (coinbase maturity: deeper reorgs are not
/// expected in practice).
pub const STARTUP_UNDO_CHECK_DEPTH: u64 = 100;

/// What the operator should do when undo data is unusable.
pub const RESYNC_ADVICE: &str = "This node was most likely synced by a blvm-node release whose \
parallel IBD stored empty undo data. Stop the node, move its data directory aside, and resync \
from scratch (blvm has no -reindex yet) to rebuild the undo data.";

/// True when this undo log cannot undo a block.
pub fn undo_log_unusable(log: Option<&BlockUndoLog>) -> bool {
    log.is_none_or(|l| l.entries.is_empty())
}

/// True when the stored undo log for `hash` is missing, unreadable, or empty.
pub fn block_undo_unusable(blockstore: &BlockStore, hash: &Hash) -> bool {
    match blockstore.get_undo_log(hash) {
        Ok(log) => undo_log_unusable(log.as_ref()),
        Err(_) => true,
    }
}

/// Heights in `[tip - depth + 1, tip]` (never genesis) on the active chain whose undo data
/// is unusable, newest first. Heights with no hash in the height index are skipped.
pub fn recent_heights_with_unusable_undo(
    blockstore: &BlockStore,
    tip_height: u64,
    depth: u64,
) -> Vec<u64> {
    if depth == 0 || tip_height == 0 {
        return Vec::new();
    }
    let lowest = tip_height.saturating_sub(depth - 1).max(1);
    (lowest..=tip_height)
        .rev()
        .filter(|&h| match blockstore.get_hash_by_height(h) {
            Ok(Some(hash)) => block_undo_unusable(blockstore, &hash),
            _ => false,
        })
        .collect()
}

/// Operator message for unusable undo data at `heights` among the last `depth` blocks.
pub fn startup_message(heights: &[u64], tip_height: u64, depth: u64) -> String {
    let newest = heights.iter().max().copied().unwrap_or(tip_height);
    let oldest = heights.iter().min().copied().unwrap_or(tip_height);
    format!(
        "Undo data is missing or empty for {} of the last {} blocks (heights {}..={}, tip {}). \
         blvm will refuse any reorg that disconnects one of these blocks instead of corrupting \
         the UTXO set, so this node can get stuck on a stale chain. {}",
        heights.len(),
        depth.min(tip_height),
        oldest,
        newest,
        tip_height,
        RESYNC_ADVICE
    )
}

/// Startup check: log an error telling the operator to resync if recent undo data is unusable.
/// Returns the affected heights (empty when all is well).
pub fn check_recent_undo_at_startup(storage: &Storage) -> Vec<u64> {
    let Ok((_, tip_height)) = storage.chain().get_tip_hash_and_height() else {
        return Vec::new();
    };
    let heights =
        recent_heights_with_unusable_undo(&storage.blocks(), tip_height, STARTUP_UNDO_CHECK_DEPTH);
    if heights.is_empty() {
        info!(
            "Undo data check: last {} blocks have usable undo data",
            STARTUP_UNDO_CHECK_DEPTH.min(tip_height)
        );
    } else {
        error!(
            "{}",
            startup_message(&heights, tip_height, STARTUP_UNDO_CHECK_DEPTH)
        );
    }
    heights
}

/// Refuse a reorg that would disconnect a block with unusable undo data.
/// `to_disconnect` is `(height, hash)` for each block leaving the active chain.
pub fn ensure_disconnect_undo_usable(
    blockstore: &BlockStore,
    to_disconnect: &[(u64, Hash)],
) -> anyhow::Result<()> {
    if let Some((height, hash)) = to_disconnect
        .iter()
        .find(|(_, hash)| block_undo_unusable(blockstore, hash))
    {
        anyhow::bail!(
            "Refusing reorg of depth {}: block at height {} ({}) has missing or empty undo \
             data, and disconnecting it would corrupt the UTXO set. {}",
            to_disconnect.len(),
            height,
            hex::encode(hash.iter().rev().copied().collect::<Vec<u8>>()),
            RESYNC_ADVICE
        );
    }
    Ok(())
}

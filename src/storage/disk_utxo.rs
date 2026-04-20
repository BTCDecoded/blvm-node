//! Disk-backed UTXO set with bounded in-memory cache
//!
//! Solves the OOM problem during IBD by keeping only a bounded subset of UTXOs
//! in memory and storing the complete set on disk (redb).
//!
//! ## Architecture
//!
//! ```text
//! ┌───────────────────────┐
//! │  In-Memory Cache      │  ← Bounded (e.g., 5M entries ≈ 2.5GB)
//! │  HashMap<OutPoint, U> │
//! └──────────┬────────────┘
//!            │ cache miss → load from disk
//! ┌──────────▼────────────┐
//! │  Disk Store (redb)    │  ← ALL UTXOs, unbounded
//! │  Tree: "ibd_utxos"    │
//! └───────────────────────┘
//! ```
//!
//! ## Performance optimizations (unified path)
//!
//! - **Incremental flush from block 1**: Always sync block changes to pending_writes;
//!   flush to disk when threshold reached. No bulk flush. No mode switch.
//! - **Flush without cache drop**: Cache stays warm; only pending_writes drains to disk.
//! - **Unified path**: Prefetch → validate → sync → evict runs every block.
//!   Early blocks: prefetch/sync/evict are fast (cache hit, small pending, no eviction).
//! - **Pending flush log**: append + sort/dedupe at flush (last write wins per key)
//! - **Fixed-size keys**: `[u8; 40]` avoids heap allocation per outpoint
//! - **Batch eviction**: Only evict when 10% over limit, clear 15% headroom
//! - **Cache size**: Auto-tuned by MemoryGuard from system RAM.

use crate::storage::database::Tree;
use anyhow::Result;

/// TidesDB max ops per transaction (TDB_MAX_TXN_OPS=100000). Batch splitting safety limit.
pub(crate) const MAX_BATCH_OPS: usize = 50_000;

/// Don't evict outputs created in the last N blocks (likely to be spent soon).
const EVICT_MIN_AGE_BLOCKS: u64 = 100;
/// Prefer evicting outputs older than this (creation height < current - N).
const EVICT_VERY_OLD_BLOCKS: u64 = 10_000;
/// Dust threshold (satoshis) — eviction sort prefers lowest value first (dust).
#[allow(dead_code)]
const EVICT_DUST_THRESHOLD: i64 = 546;
use blvm_protocol::transaction::is_coinbase;
use blvm_protocol::types::{Block, Hash, OutPoint, UTXO};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use tracing::debug;

/// Fixed-size outpoint key: 32 bytes txid + 8 bytes index (big-endian)
pub type OutPointKey = [u8; 40];

/// Pending/flushing value: UTXO kept in memory; serialized only when flushing to disk.
/// Some(arc)=insert (Arc avoids clone on get_pending), None=delete. Serialize deferred to flush.
type PendingValue = Option<Arc<UTXO>>;

/// Serialize an OutPoint to a fixed-size storage key.
/// Zero-allocation: returns a stack-allocated array instead of Vec.
#[inline]
pub fn outpoint_to_key(outpoint: &OutPoint) -> OutPointKey {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(&outpoint.hash);
    key[32..40].copy_from_slice(&(outpoint.index as u64).to_be_bytes());
    key
}

/// Convert storage key back to OutPoint for cache removal.
#[inline]
pub fn key_to_outpoint(key: &OutPointKey) -> OutPoint {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[..32]);
    let index = u64::from_be_bytes(key[32..40].try_into().unwrap()) as u32;
    OutPoint { hash, index }
}

/// Load UTXOs for given keys from disk. Used by prefetch overlap (spawn_blocking).
///
/// Uses Tree::get_many when available (RocksDB multi_get_cf = 1 batch call vs N get calls).
/// Fallback: sequential get. No par_iter (was causing 500+ concurrent get = lock contention).
///
/// Returns `(map, keys_sorted)` so callers can scan the same key set (e.g. in-flight UTXO
/// fallback) without cloning the request list.
pub(crate) fn load_keys_from_disk(
    disk: Arc<dyn Tree>,
    mut keys: Vec<OutPointKey>,
) -> Result<(FxHashMap<OutPointKey, UTXO>, Vec<OutPointKey>)> {
    if keys.is_empty() {
        return Ok((FxHashMap::default(), Vec::new()));
    }
    keys.sort_unstable();
    let mut key_refs: Vec<&[u8]> = Vec::with_capacity(keys.len());
    for k in &keys {
        key_refs.push(k.as_slice());
    }
    let values = disk.get_many(&key_refs)?;
    let mut result = FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
    #[cfg(feature = "rayon")]
    {
        use blvm_protocol::rayon::prelude::*;
        let rows: Vec<(OutPointKey, Option<Vec<u8>>)> = keys.iter().copied().zip(values).collect();
        for (k, utxo) in rows
            .into_par_iter()
            .filter_map(|(key, value)| {
                let data = value?;
                let utxo = bincode::deserialize::<UTXO>(&data).ok()?;
                Some((key, utxo))
            })
            .collect::<Vec<_>>()
        {
            result.insert(k, utxo);
        }
    }
    #[cfg(not(feature = "rayon"))]
    {
        for (key, value) in keys.iter().zip(values.into_iter()) {
            if let Some(data) = value {
                if let Ok(utxo) = bincode::deserialize::<UTXO>(&data) {
                    result.insert(*key, utxo);
                }
            }
        }
    }
    Ok((result, keys))
}

/// Reuses buffer for block input keys. Avoids per-block alloc in IBD v2 validation hot path.
#[inline]
pub fn block_input_keys_into(block: &Block, keys_out: &mut Vec<OutPointKey>) {
    let est: usize = block
        .transactions
        .iter()
        .filter(|tx| !is_coinbase(tx))
        .map(|tx| tx.inputs.len())
        .sum();
    keys_out.clear();
    keys_out.reserve(est);
    for tx in block.transactions.iter() {
        if is_coinbase(tx) {
            continue;
        }
        for input in tx.inputs.iter() {
            keys_out.push(outpoint_to_key(&input.prevout));
        }
    }
}

/// Collect and deduplicate outpoint keys from multiple blocks (for batched lookahead prefetch).
/// Reduces TidesDB round-trips by loading UTXOs for several blocks in one disk read batch.
pub(crate) fn block_input_keys_batch(blocks: &[&Block]) -> Vec<OutPointKey> {
    let est: usize = blocks
        .iter()
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !is_coinbase(tx))
                .map(|tx| tx.inputs.len())
                .sum::<usize>()
        })
        .sum();
    let mut seen = FxHashSet::with_capacity_and_hasher(est, Default::default());
    let mut keys = Vec::with_capacity(est);
    for block in blocks {
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_key(&input.prevout);
                if seen.insert(key) {
                    keys.push(key);
                }
            }
        }
    }
    keys
}

/// Same as `block_input_keys_batch` but reuses buffers. Avoids per-block allocations in hot path.
/// Caller provides cleared buffers; this clears and refills keys_out, reuses seen for dedup.
pub(crate) fn block_input_keys_batch_into(
    blocks: &[&Block],
    keys_out: &mut Vec<OutPointKey>,
    seen: &mut FxHashSet<OutPointKey>,
) {
    let est: usize = blocks
        .iter()
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !is_coinbase(tx))
                .map(|tx| tx.inputs.len())
                .sum::<usize>()
        })
        .sum();
    keys_out.clear();
    keys_out.reserve(est);
    seen.clear();
    for block in blocks {
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_key(&input.prevout);
                if seen.insert(key) {
                    keys_out.push(key);
                }
            }
        }
    }
}

/// Same as `block_input_keys_batch_into` but takes `Arc<Block>`. Avoids holding refs into
/// ready_buffer (fixes borrow conflicts with insert/remove_entry in validation loop).
pub(crate) fn block_input_keys_batch_into_arc(
    blocks: &[Arc<Block>],
    keys_out: &mut Vec<OutPointKey>,
    seen: &mut FxHashSet<OutPointKey>,
) {
    let est: usize = blocks
        .iter()
        .map(|b| {
            b.transactions
                .iter()
                .filter(|tx| !is_coinbase(tx))
                .map(|tx| tx.inputs.len())
                .sum::<usize>()
        })
        .sum();
    keys_out.clear();
    keys_out.reserve(est);
    seen.clear();
    for block in blocks {
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_key(&input.prevout);
                if seen.insert(key) {
                    keys_out.push(key);
                }
            }
        }
    }
}

/// Like `block_input_keys_into` but filters out intra-block spends.
///
/// Only skips prefetch when the input spends an output of a **non-coinbase** transaction that
/// appears **earlier in this block** (`tx_ids[j] == prevout.hash` for some `j` with `1 <= j < idx`).
/// Those UTXOs are not on disk yet; `connect_block_ibd`'s overlay supplies them after earlier txs.
///
/// Prevouts matching **coinbase** (`j == 0`) are never treated as prefetch-elidable here: BIP30
/// chain UTXOs can share a txid with this block's coinbase and must still load from disk.
///
/// Returns the number of keys filtered out (informational; log at tracing::debug level if needed).
/// Filter input keys using precomputed `tx_ids` (same length as `block.transactions`).
pub fn block_input_keys_into_filtered_with_tx_ids(
    block: &Block,
    tx_ids: &[Hash],
    keys_out: &mut Vec<OutPointKey>,
) -> usize {
    let est: usize = block
        .transactions
        .iter()
        .filter(|tx| !is_coinbase(tx))
        .map(|tx| tx.inputs.len())
        .sum();
    keys_out.clear();
    keys_out.reserve(est);

    let mut filtered = 0usize;
    for (spending_idx, tx) in block.transactions.iter().enumerate() {
        if is_coinbase(tx) {
            continue;
        }
        for input in tx.inputs.iter() {
            let h = input.prevout.hash;
            let funded_by_prior_non_cb = (1..spending_idx).any(|j| tx_ids[j] == h);
            if funded_by_prior_non_cb {
                filtered += 1;
            } else {
                keys_out.push(outpoint_to_key(&input.prevout));
            }
        }
    }
    filtered
}

/// One `compute_block_tx_ids` + filtered keys (reuses `tx_ids_buf`).
pub fn block_input_keys_and_tx_ids_filtered(
    block: &Block,
    tx_ids_buf: &mut Vec<Hash>,
    keys_out: &mut Vec<OutPointKey>,
) -> usize {
    use blvm_protocol::block::compute_block_tx_ids_into;
    compute_block_tx_ids_into(block, tx_ids_buf);
    block_input_keys_into_filtered_with_tx_ids(block, tx_ids_buf, keys_out)
}

pub fn block_input_keys_into_filtered(block: &Block, keys_out: &mut Vec<OutPointKey>) -> usize {
    use blvm_protocol::block::compute_block_tx_ids;
    let tx_ids = compute_block_tx_ids(block);
    block_input_keys_into_filtered_with_tx_ids(block, &tx_ids, keys_out)
}

/// Pre-computed sync batch for disk persistence. Applied by IbdUtxoStore::apply_sync_batch.
/// Inserts hold Arc<UTXO> to avoid clone in IBD v2 apply_sync_batch hot path.
pub struct SyncBatch {
    pub deletes: Vec<OutPointKey>,
    pub inserts: Vec<(OutPointKey, Arc<UTXO>)>,
    pub total_delta: isize,
}

/// Flush a batch of UTXO operations to disk. Splits into chunks of MAX_BATCH_OPS to stay
/// under TidesDB's TDB_MAX_TXN_OPS (100k). Used by IbdUtxoStore.
pub fn flush_batch_to_disk(
    batch: &[(OutPointKey, PendingValue)],
    disk: &dyn Tree,
) -> Result<usize> {
    if batch.is_empty() {
        return Ok(0);
    }
    let mut total_flushed = 0;
    let mut ser_buf = Vec::with_capacity(192);
    for chunk in batch.chunks(MAX_BATCH_OPS) {
        let mut b = disk.batch()?;
        for (key, value_opt) in chunk {
            match value_opt {
                Some(arc) => {
                    ser_buf.clear();
                    bincode::serialize_into(&mut ser_buf, arc.as_ref())
                        .map_err(|e| anyhow::anyhow!("UTXO serialize: {}", e))?;
                    b.put(key.as_slice(), ser_buf.as_slice());
                }
                None => b.delete(key.as_slice()),
            }
        }
        b.commit()?;
        total_flushed += chunk.len();
    }
    debug!(
        "flush_batch_to_disk: flushed {} operations to disk",
        total_flushed
    );
    Ok(total_flushed)
}

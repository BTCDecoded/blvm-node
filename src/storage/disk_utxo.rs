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
//! - **O(1) pending_writes lookup**: HashMap instead of Vec linear scan
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
use blvm_consensus::transaction::is_coinbase;
use blvm_consensus::types::{Block, Hash, OutPoint, UTXO};
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
pub(crate) fn load_keys_from_disk(
    disk: Arc<dyn Tree>,
    keys: Vec<OutPointKey>,
) -> Result<FxHashMap<OutPointKey, UTXO>> {
    if keys.is_empty() {
        return Ok(FxHashMap::default());
    }
    let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    let values = disk.get_many(&key_refs)?;
    let mut result = FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
    for (key, value) in keys.into_iter().zip(values.into_iter()) {
        if let Some(data) = value {
            if let Ok(utxo) = bincode::deserialize::<UTXO>(&data) {
                result.insert(key, utxo);
            }
        }
    }
    Ok(result)
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
    batch: &FxHashMap<OutPointKey, PendingValue>,
    disk: &dyn Tree,
) -> Result<usize> {
    if batch.is_empty() {
        return Ok(0);
    }
    let entries: Vec<_> = batch.iter().collect();
    let mut total_flushed = 0;
    for chunk in entries.chunks(MAX_BATCH_OPS) {
        let mut b = disk.batch();
        for (key, value_opt) in chunk {
            match value_opt {
                Some(arc) => {
                    let bytes = bincode::serialize(arc.as_ref())
                        .map_err(|e| anyhow::anyhow!("UTXO serialize: {}", e))?;
                    b.put(*key, bytes.as_slice());
                }
                None => b.delete(*key),
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

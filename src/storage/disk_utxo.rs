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
//! - **Optional byte limit**: `BLVM_UTXO_CACHE_MAX_MB` caps cache memory (MiB). When set, effective
//!   max_entries = min(entries, bytes/120). Use on memory-constrained systems to avoid OOM.

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
use blvm_consensus::bip_validation::Bip30Index;
use blvm_consensus::transaction::is_coinbase;
use blvm_consensus::types::{Block, Hash, OutPoint, UTXO, UtxoSet};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Fixed-size outpoint key: 32 bytes txid + 8 bytes index (big-endian)
pub(crate) type OutPointKey = [u8; 40];

/// Pending/flushing value: UTXO kept in memory; serialized only when flushing to disk.
/// Some(arc)=insert (Arc avoids clone on get_pending), None=delete. Serialize deferred to flush.
type PendingValue = Option<Arc<UTXO>>;

/// Serialize an OutPoint to a fixed-size storage key.
/// Zero-allocation: returns a stack-allocated array instead of Vec.
#[inline]
pub(crate) fn outpoint_to_key(outpoint: &OutPoint) -> OutPointKey {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(&outpoint.hash);
    key[32..40].copy_from_slice(&(outpoint.index as u64).to_be_bytes());
    key
}

/// Build OutPointKey from (txid, vout) without allocating OutPoint.
#[inline]
fn outpoint_key_from_parts(hash: &[u8; 32], index: u64) -> OutPointKey {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(hash);
    key[32..40].copy_from_slice(&index.to_be_bytes());
    key
}

/// Convert storage key back to OutPoint for cache removal.
#[inline]
pub(crate) fn key_to_outpoint(key: &OutPointKey) -> OutPoint {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[..32]);
    let index = u64::from_be_bytes(key[32..40].try_into().unwrap()) as u32;
    OutPoint { hash, index }
}

/// Threshold below which we use per-key get (inline) instead of batch get_many.
/// Avoids spawn+join overhead for small gap-fill; keeps validation from blocking on thread setup.
pub(crate) const INLINE_GAP_THRESHOLD: usize = 32;

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

/// Load UTXOs for few keys using per-key disk.get() inline.
/// Avoids spawn+join overhead when gap-fill has \<INLINE_GAP_THRESHOLD keys.
pub(crate) fn load_keys_from_disk_inline(
    disk: &dyn Tree,
    keys: &[OutPointKey],
) -> Result<FxHashMap<OutPointKey, UTXO>> {
    let mut result = FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
    for key in keys {
        if let Some(data) = disk.get(key)? {
            if let Ok(utxo) = bincode::deserialize::<UTXO>(&data) {
                result.insert(key.clone(), utxo);
            }
        }
    }
    Ok(result)
}

/// Collect all input outpoint keys from a block (for prefetch overlap).
pub(crate) fn block_input_keys(block: &Block) -> Vec<OutPointKey> {
    let mut keys = Vec::new();
    block_input_keys_into(block, &mut keys);
    keys
}

/// Same as `block_input_keys` but reuses buffer. Avoids per-block alloc in IBD v2 validation hot path.
#[inline]
pub(crate) fn block_input_keys_into(block: &Block, keys_out: &mut Vec<OutPointKey>) {
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

/// Pre-computed sync batch for disk persistence. Produced by `sync_block_to_batch` (pure),
/// applied by `DiskBackedUtxoSet::apply_sync_batch`. Enables pipelining: sync N runs in
/// parallel with validation N+1.
/// Inserts hold Arc<UTXO> to avoid clone in IBD v2 apply_sync_batch hot path.
pub struct SyncBatch {
    pub deletes: Vec<OutPointKey>,
    pub inserts: Vec<(OutPointKey, Arc<UTXO>)>,
    pub total_delta: isize,
}

/// Flush a SyncBatch directly to disk. Used by memory-first IBD (no DiskBackedUtxoSet).
pub fn flush_sync_batch_to_disk(batch: &SyncBatch, disk: &dyn Tree) -> Result<usize> {
    if batch.deletes.is_empty() && batch.inserts.is_empty() {
        return Ok(0);
    }
    let mut map = FxHashMap::with_capacity_and_hasher(
        batch.deletes.len() + batch.inserts.len(),
        Default::default(),
    );
    for key in &batch.deletes {
        map.insert(*key, None);
    }
    for (key, arc) in &batch.inserts {
        map.insert(*key, Some(Arc::clone(arc)));
    }
    DiskBackedUtxoSet::flush_batch_to_disk(&map, disk)
}

/// Build SyncBatch from overlay delta (when BLVM_USE_OVERLAY_DELTA=1).
/// Avoids sync_block_to_batch block walk; uses Arc::clone (no UTXO clone).
pub fn sync_batch_from_overlay_delta(delta: &blvm_consensus::block::UtxoDelta) -> SyncBatch {
    let deletes: Vec<OutPointKey> = delta.deletions.iter().map(outpoint_to_key).collect();
    let inserts: Vec<(OutPointKey, Arc<UTXO>)> = delta
        .additions
        .iter()
        .map(|(op, arc)| (outpoint_to_key(op), Arc::clone(arc)))
        .collect();
    let total_delta = inserts.len() as isize - deletes.len() as isize;
    SyncBatch {
        deletes,
        inserts,
        total_delta,
    }
}

/// Pure function: compute UTXO changes for a block without mutating any state.
/// Runs on spawn_blocking to overlap with validation of the next block.
/// Returns a batch that can be applied via `DiskBackedUtxoSet::apply_sync_batch`.
pub fn sync_block_to_batch(block: &Block, height: u64, tx_ids: &[Hash]) -> Result<SyncBatch> {
    #[cfg(feature = "profile")]
    let t_start = std::time::Instant::now();

    #[cfg(feature = "rayon")]
    let (deletes, inserts, total_delta) = {
        use rayon::prelude::*;
        let (deletes, inserts) = block
            .transactions
            .par_iter()
            .zip(tx_ids.par_iter())
            .map(|(tx, tx_id)| {
                let is_cb = is_coinbase(tx);
                let mut local_deletes = Vec::new();
                let mut local_inserts = Vec::new();
                if !is_cb {
                    for input in tx.inputs.iter() {
                        local_deletes.push(outpoint_to_key(&input.prevout));
                    }
                }
                for (vout, output) in tx.outputs.iter().enumerate() {
                    let mut key = [0u8; 40];
                    key[..32].copy_from_slice(tx_id);
                    key[32..].copy_from_slice(&(vout as u64).to_be_bytes());
                    let utxo = UTXO {
                        value: output.value,
                        script_pubkey: output.script_pubkey.as_slice().into(),
                        height,
                        is_coinbase: is_cb,
                    };
                    local_inserts.push((key, Arc::new(utxo)));
                }
                (local_deletes, local_inserts)
            })
            .reduce(
                || (Vec::new(), Vec::new()),
                |(mut a_d, mut a_i), (b_d, b_i)| {
                    a_d.extend(b_d);
                    a_i.extend(b_i);
                    (a_d, a_i)
                },
            );
        let delta = inserts.len() as isize - deletes.len() as isize;
        (deletes, inserts, delta)
    };

    #[cfg(not(feature = "rayon"))]
    let (deletes, inserts, total_delta) = {
        let est_inputs: usize = block
            .transactions
            .iter()
            .filter(|tx| !is_coinbase(tx))
            .map(|tx| tx.inputs.len())
            .sum();
        let est_outputs: usize = block.transactions.iter().map(|tx| tx.outputs.len()).sum();
        let mut deletes: Vec<OutPointKey> = Vec::with_capacity(est_inputs);
        let mut inserts: Vec<(OutPointKey, Arc<UTXO>)> = Vec::with_capacity(est_outputs);
        for (tx_idx, tx) in block.transactions.iter().enumerate() {
            let tx_id = tx_ids[tx_idx];
            let is_cb = is_coinbase(tx);
            if !is_cb {
                for input in tx.inputs.iter() {
                    deletes.push(outpoint_to_key(&input.prevout));
                }
            }
            for (vout, output) in tx.outputs.iter().enumerate() {
                let mut key = [0u8; 40];
                key[..32].copy_from_slice(&tx_id);
                key[32..].copy_from_slice(&(vout as u64).to_be_bytes());
                inserts.push((
                    key,
                    Arc::new(UTXO {
                        value: output.value,
                        script_pubkey: output.script_pubkey.as_slice().into(),
                        height,
                        is_coinbase: is_cb,
                    }),
                ));
            }
        }
        let delta = inserts.len() as isize - deletes.len() as isize;
        (deletes, inserts, delta)
    };

    #[cfg(feature = "profile")]
    let iter_ms = t_start.elapsed().as_millis() as u64;
    #[cfg(feature = "profile")]
    if iter_ms > 0 || inserts.len() > 20 {
        blvm_consensus::profile_log!(
            "[SYNC_BATCH_PERF] height={} iter_ms={} n_inserts={} n_deletes={}",
            height, iter_ms, inserts.len(), deletes.len()
        );
    }

    // No serialize here — UTXOs stay in memory; serialized only in flush_batch_to_disk
    Ok(SyncBatch {
        deletes,
        inserts,
        total_delta,
    })
}

/// Disk-backed UTXO set with bounded in-memory cache.
///
/// During IBD, the UTXO set grows to tens of millions of entries (>8GB at peak).
/// This struct keeps only a bounded subset in memory and spills the rest to disk.
///
/// ## Unified path:
///
/// One code path for all blocks. Prefetch → validate → sync → evict every block.
/// Early blocks: prefetch is fast (cache hit), sync has little pending, evict is no-op.
/// Late blocks: full disk operations. No mode switch, no bulk flush.
pub struct DiskBackedUtxoSet {
    /// In-memory cache — bounded subset of all UTXOs.
    /// This is passed (via take/restore) to connect_block for validation.
    cache: UtxoSet,

    /// Disk-backed store — ALL UTXOs are persisted here.
    disk: Arc<dyn Tree>,

    /// Maximum number of entries allowed in the in-memory cache.
    max_cache_entries: usize,

    /// Total UTXO count (tracked incrementally to avoid counting disk entries).
    total_utxo_count: usize,

    /// Pending disk writes (batched for performance).
    /// HashMap for O(1) lookup during prefetch instead of O(n) linear scan.
    /// Value: Some(UTXO) for inserts, None for deletes. Kept in memory; serialized only at flush.
    pending_writes: FxHashMap<OutPointKey, PendingValue>,

    /// Batches swapped out for async flush; still readable until flush completes.
    /// Caller spawns flush; when done, calls mark_flush_complete() to pop.
    flushing_batches: VecDeque<Arc<FxHashMap<OutPointKey, PendingValue>>>,

    /// Flush threshold — number of pending writes before swap for async flush.
    flush_threshold: usize,

    /// Stats
    pub stats_disk_loads: u64,
    pub stats_cache_hits: u64,
    pub stats_evictions: u64,
    pub stats_pending_hits: u64,
    
    /// Recently accessed outpoints (from last block) - avoid evicting these
    /// Uses OutPointKey to avoid per-input OutPoint clones (hot path: ~3k inputs/block)
    recently_accessed: FxHashSet<OutPointKey>,

    /// FIFO queue of cache insert keys for O(to_evict) eviction (avoids O(cache) scan).
    /// Capped at max_cache_entries. Tracks prefetch inserts; connect_block removes aren't reflected.
    eviction_queue: VecDeque<OutPointKey>,

    /// O(1) BIP30 duplicate-coinbase check. Maps coinbase txid → count of unspent outputs.
    /// Updated by connect_block during apply_transaction; eviction does not touch it.
    bip30_index: Bip30Index,
}

impl DiskBackedUtxoSet {
    /// Create a new disk-backed UTXO set.
    pub fn new(
        disk_tree: Arc<dyn Tree>,
        max_cache_entries: usize,
        flush_threshold: usize,
    ) -> Self {
        Self {
            cache: UtxoSet::with_capacity_and_hasher(max_cache_entries.min(2_000_000), Default::default()),
            disk: disk_tree,
            max_cache_entries,
            total_utxo_count: 0,
            pending_writes: FxHashMap::with_capacity_and_hasher(flush_threshold, Default::default()),
            flushing_batches: VecDeque::new(),
            flush_threshold,
            stats_disk_loads: 0,
            stats_cache_hits: 0,
            stats_evictions: 0,
            stats_pending_hits: 0,
            recently_accessed: FxHashSet::with_capacity_and_hasher(2000, Default::default()),
            eviction_queue: VecDeque::with_capacity(max_cache_entries.min(100_000)),
            bip30_index: Bip30Index::default(),
        }
    }

    /// Initialize by counting existing UTXOs on disk (for resuming IBD).
    pub fn initialize_count(&mut self) -> Result<()> {
        self.total_utxo_count = self.disk.len()?;
        if self.total_utxo_count > 0 {
            info!(
                "DiskBackedUtxoSet: Found {} existing UTXOs on disk (resuming IBD)",
                self.total_utxo_count
            );
        }
        Ok(())
    }

    /// Collect outpoint keys that need disk load (not in cache, not in pending/flushing with data).
    /// Skips same-block refs (tx spends output of earlier tx in block) — UtxoOverlay handles those;
    /// disk lookup would always miss.
    pub fn collect_gaps(&self, block: &Block) -> Vec<OutPointKey> {
        let tx_ids = blvm_consensus::block::compute_block_tx_ids(block);
        self.collect_gaps_with_tx_ids(block, &tx_ids)
    }

    /// Same as collect_gaps but uses precomputed tx_ids (#21). Caller computes once and shares with connect_block.
    pub fn collect_gaps_with_tx_ids(&self, block: &Block, tx_ids: &[blvm_consensus::types::Hash]) -> Vec<OutPointKey> {
        let est_inputs: usize = block
            .transactions
            .iter()
            .filter(|tx| !is_coinbase(tx))
            .map(|tx| tx.inputs.len())
            .sum();
        let mut keys_to_load = Vec::with_capacity(est_inputs);
        let mut block_outputs: FxHashSet<OutPointKey> = FxHashSet::default();
        for (tx, txid) in block.transactions.iter().zip(tx_ids.iter()) {
            if !is_coinbase(tx) {
                for input in tx.inputs.iter() {
                    let key = outpoint_to_key(&input.prevout);
                    if block_outputs.contains(&key) {
                        continue; // Same-block ref — overlay handles during validation
                    }
                    if self.cache.contains_key(&key_to_outpoint(&key)) {
                        continue;
                    }
                    if self.has_key_in_pending_or_flushing(&key).is_some() {
                        continue; // has data or deleted, skip disk load
                    }
                    keys_to_load.push(key);
                }
            }
            for (idx, _) in tx.outputs.iter().enumerate() {
                block_outputs.insert(outpoint_key_from_parts(txid, idx as u64));
            }
        }
        keys_to_load
    }

    /// Apply pending/flushing UTXOs to cache (in-memory only, no disk).
    /// Call before validate_block_only when inputs are in pending_writes or flushing_batches.
    pub fn apply_pending_hits(&mut self, block: &Block) {
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_key(&input.prevout);
                if self.cache.contains_key(&key_to_outpoint(&key)) {
                    continue;
                }
                if let Some(Some(arc)) = self.get_pending_or_flushing(&key) {
                    self.cache.insert(key_to_outpoint(&key), Arc::clone(&arc));
                    self.recently_accessed.insert(key);
                    self.push_eviction_key(key);
                    self.stats_pending_hits += 1;
                }
            }
        }
    }

    /// Check pending_writes and flushing_batches for a key. Returns Some(Some(utxo)) for insert,
    /// Some(None) for delete, None if not found.
    #[inline]
    fn get_pending_or_flushing(&self, key: &OutPointKey) -> Option<PendingValue> {
        if let Some(entry) = self.pending_writes.get(key) {
            return Some(entry.clone());
        }
        for batch in &self.flushing_batches {
            if let Some(entry) = batch.get(key) {
                return Some(entry.clone());
            }
        }
        None
    }

    /// Lightweight check: is key in pending/flushing? Some(true)=has data, Some(false)=deleted, None=not found.
    /// Avoids cloning Vec<u8> when caller only needs existence (e.g. filter_keys_to_fetch).
    #[inline]
    fn has_key_in_pending_or_flushing(&self, key: &OutPointKey) -> Option<bool> {
        if let Some(entry) = self.pending_writes.get(key) {
            return Some(entry.is_some());
        }
        for batch in &self.flushing_batches {
            if let Some(entry) = batch.get(key) {
                return Some(entry.is_some());
            }
        }
        None
    }

    /// Load a single UTXO from disk by outpoint.
    /// Checks pending_writes, flushing_batches, then disk.
    fn load_from_disk(&mut self, outpoint: &OutPoint) -> Result<Option<UTXO>> {
        let key = outpoint_to_key(outpoint);

        // Check pending writes and flushing batches first (UTXO stored directly, no deserialize)
        if let Some(entry) = self.get_pending_or_flushing(&key) {
            self.stats_pending_hits += 1;
            return match entry {
                Some(arc) => Ok(Some(arc.as_ref().clone())),
                None => Ok(None), // Was deleted
            };
        }

        // Check disk
        match self.disk.get(&key)? {
            Some(data) => Ok(Some(bincode::deserialize(&data)?)),
            None => Ok(None),
        }
    }

    /// Ensure all UTXOs needed by a block's inputs are in the in-memory cache.
    /// Applies pending/flushing hits only. Disk I/O is done via collect_gaps + spawn_blocking +
    /// load_keys_from_disk + merge_prefetch_buffer (caller's responsibility) to keep it off the
    /// critical path. Legacy path: use for non-IBD or when disk_utxo is used without the new loop.
    pub fn prefetch_block(&mut self, block: &Block, _profile_height: Option<u64>) -> Result<usize> {
        self.apply_pending_hits(block);
        Ok(0)
    }

    /// Clone of disk ref for async prefetch (spawn_blocking). Enables prefetch N+1 while validating N.
    #[inline]
    pub fn disk_clone_for_prefetch(&self) -> Arc<dyn Tree> {
        Arc::clone(&self.disk)
    }

    /// Load block input UTXOs from disk into a buffer (for prefetch overlap).
    /// Does NOT touch cache. Used to prefetch N+1 while validating N.
    /// Caller merges result via `merge_prefetch_buffer` before prefetch_block for N+1.
    pub fn prefetch_block_to_buffer(&self, block: &Block) -> Result<FxHashMap<OutPointKey, UTXO>> {
        let keys = block_input_keys(block);
        if keys.is_empty() {
            return Ok(FxHashMap::default());
        }
        load_keys_from_disk(Arc::clone(&self.disk), keys)
    }

    /// Filter keys to only those that need disk fetch. Skips cache hits and pending/flushing entries.
    /// Used by early prefetch to avoid redundant disk reads.
    /// Uses has_key_in_pending_or_flushing (no Vec clone) instead of get_pending_or_flushing.
    #[inline]
    pub fn filter_keys_to_fetch(&self, keys: &[OutPointKey]) -> Vec<OutPointKey> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            if self.cache.contains_key(&key_to_outpoint(key)) {
                continue;
            }
            if self.has_key_in_pending_or_flushing(key).is_some() {
                continue; // has data or deleted — skip disk fetch
            }
            out.push(*key);
        }
        out
    }

    /// Merge a prefetched buffer into cache. Skips entries that are pending deletes (spent in a prior block).
    /// Call before prefetch_block when starting validation of the prefetched block.
    /// Returns the number of keys actually inserted into cache.
    pub fn merge_prefetch_buffer(&mut self, buffer: FxHashMap<OutPointKey, UTXO>) -> usize {
        let mut merged = 0;
        for (key, utxo) in buffer {
            if self.has_key_in_pending_or_flushing(&key) == Some(false) {
                continue;
            }
            let op = key_to_outpoint(&key);
            if self.cache.contains_key(&op) {
                continue;
            }
            self.cache.insert(op, Arc::new(utxo));
            self.recently_accessed.insert(key);
            self.push_eviction_key(key);
            self.stats_disk_loads += 1;
            merged += 1;
        }
        merged
    }

    /// Same as merge_prefetch_buffer but takes Arc map (ReadyItem). No clone.
    pub fn merge_prefetch_buffer_arc(&mut self, buffer: &FxHashMap<OutPointKey, Arc<UTXO>>) -> usize {
        let mut merged = 0;
        for (key, arc) in buffer {
            if self.has_key_in_pending_or_flushing(key) == Some(false) {
                continue;
            }
            let op = key_to_outpoint(key);
            if self.cache.contains_key(&op) {
                continue;
            }
            self.cache.insert(op, Arc::clone(arc));
            self.recently_accessed.insert(*key);
            self.push_eviction_key(*key);
            self.stats_disk_loads += 1;
            merged += 1;
        }
        merged
    }

    /// Push a key to the eviction queue (called on cache insert). Capped at max_cache_entries.
    #[inline]
    fn push_eviction_key(&mut self, key: OutPointKey) {
        if self.eviction_queue.len() >= self.max_cache_entries {
            self.eviction_queue.pop_front();
        }
        self.eviction_queue.push_back(key);
    }

    /// Protect keys needed by upcoming blocks from eviction. Call before evict_if_needed.
    /// BLVM_UTXO_PROTECT_MAX_KEYS: cap keys processed (default: no cap). Use to limit cost on slow systems.
    pub fn protect_keys_for_next_blocks(&mut self, keys: &[OutPointKey]) {
        let max_keys = std::env::var("BLVM_UTXO_PROTECT_MAX_KEYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(usize::MAX);
        let keys = if keys.len() <= max_keys {
            keys
        } else {
            &keys[..max_keys]
        };
        for key in keys {
            if self.cache.contains_key(&key_to_outpoint(key)) {
                self.recently_accessed.insert(*key);
            }
        }
    }

    /// Get a mutable reference to the in-memory cache.
    #[inline]
    pub fn cache_mut(&mut self) -> &mut UtxoSet {
        &mut self.cache
    }

    /// Mutable reference to BIP30 index for O(1) duplicate-coinbase check during IBD.
    pub fn bip30_index_mut(&mut self) -> &mut Bip30Index {
        &mut self.bip30_index
    }

    /// Run a closure with mutable refs to cache and bip30_index to avoid multiple borrows.
    pub fn with_cache_and_bip30_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut UtxoSet, &mut Bip30Index) -> R,
    {
        f(&mut self.cache, &mut self.bip30_index)
    }

    /// Get the number of entries currently in the in-memory cache.
    #[inline]
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Get the total UTXO count (tracked incrementally by sync_block).
    #[inline]
    pub fn total_len(&self) -> usize {
        self.total_utxo_count
    }

    /// Apply a pre-computed sync batch (from sync_block_to_batch). Used by pipelined IBD
    /// after awaiting the async sync task. Also called by sync_block_with_txids for inline path.
    ///
    /// New outputs are protected from eviction: the next block may depend on them, and might
    /// not be in ready_buffer yet when protect_keys_for_next_blocks runs. Without this, we
    /// could evict block N's outputs before block N+1 (which spends them) is processed.
    pub fn apply_sync_batch(&mut self, batch: SyncBatch) {
        self.total_utxo_count = self
            .total_utxo_count
            .saturating_add_signed(batch.total_delta);
        for key in batch.deletes {
            self.pending_writes.insert(key, None);
            // Spent outputs are dead — remove from cache to free space immediately.
            // We will never look them up again; eviction can keep unspent entries.
            let outpoint = key_to_outpoint(&key);
            self.cache.remove(&outpoint);
        }
        for (key, arc) in batch.inserts {
            self.push_eviction_key(key);
            self.pending_writes.insert(key, Some(Arc::clone(&arc)));
            // Protect new outputs from eviction until next block can use them.
            // connect_block already added to cache; we add to recently_accessed so evict_if_needed
            // won't remove them before the next block (N+1) is validated.
            self.recently_accessed.insert(key);
        }
    }

    /// After block validation, record the block's UTXO changes for disk persistence.
    /// Only updates pending_writes — does NOT touch the cache (connect_block already did that).
    ///
    /// Uses pre-computed tx_ids to avoid re-hashing every transaction.
    /// Inline path (first block, or fallback): computes batch and applies immediately.
    /// Pipelined path: use sync_block_to_batch + spawn_blocking, then apply_sync_batch.
    pub fn sync_block_with_txids(&mut self, block: &Block, height: u64, tx_ids: &[Hash]) -> Result<()> {
        let batch = sync_block_to_batch(block, height, tx_ids)?;
        self.apply_sync_batch(batch);
        Ok(())
    }

    /// If pending_writes >= threshold, swap to flushing_batches and return batch for async flush.
    /// Caller spawns flush; when handle completes, caller calls mark_flush_complete().
    pub fn maybe_take_flush_batch(&mut self) -> Option<Arc<FxHashMap<OutPointKey, PendingValue>>> {
        if self.pending_writes.len() < self.flush_threshold {
            return None;
        }
        let batch = Arc::new(std::mem::take(&mut self.pending_writes));
        self.pending_writes = FxHashMap::with_capacity_and_hasher(self.flush_threshold, Default::default());
        self.flushing_batches.push_back(Arc::clone(&batch));
        Some(batch)
    }

    /// Call when an async flush completes. Pops the oldest batch from flushing_batches.
    pub fn mark_flush_complete(&mut self) {
        self.flushing_batches.pop_front();
    }

    /// Flush a batch to disk. Used by spawn_blocking (parallel_ibd) and by Drop.
    /// Splits into chunks of MAX_BATCH_OPS to stay under TidesDB's TDB_MAX_TXN_OPS (100k).
    pub(crate) fn flush_batch_to_disk(batch: &FxHashMap<OutPointKey, PendingValue>, disk: &dyn Tree) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let _count = batch.len();
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
        debug!("DiskBackedUtxoSet: flushed {} operations to disk (async)", total_flushed);
        Ok(total_flushed)
    }

    /// Flush all pending writes to disk. Chunks to stay under TidesDB txn ops limit.
    pub fn flush(&mut self) -> Result<usize> {
        if self.pending_writes.is_empty() {
            return Ok(0);
        }

        let entries: Vec<_> = self.pending_writes.drain().collect();
        let _count = entries.len();
        let mut total_flushed = 0;
        for chunk in entries.chunks(MAX_BATCH_OPS) {
            let mut batch = self.disk.batch();
            for (key, value_opt) in chunk {
                match value_opt {
                    Some(arc) => {
                        let bytes = bincode::serialize(arc.as_ref())
                            .map_err(|e| anyhow::anyhow!("UTXO serialize: {}", e))?;
                        batch.put(key, bytes.as_slice());
                    }
                    None => batch.delete(key),
                }
            }
            batch.commit()?;
            total_flushed += chunk.len();
        }
        debug!("DiskBackedUtxoSet: flushed {} operations to disk", total_flushed);
        Ok(total_flushed)
    }

    /// Evict entries from the in-memory cache to stay under the memory limit.
    /// Uses FIFO eviction queue for O(to_evict) when queue has candidates; falls back to
    /// O(cache) scan only when queue is exhausted (rare).
    /// Prefers evicting: dust first, then old outputs (height &lt; current_height - 100).
    /// Never evicts outputs from the last EVICT_MIN_AGE_BLOCKS (100) blocks.
    pub fn evict_if_needed(&mut self, current_height: u64) -> usize {
        let trigger_threshold = self.max_cache_entries + self.max_cache_entries / 10;
        if self.cache.len() <= trigger_threshold {
            return 0;
        }

        let target = self.max_cache_entries * 9 / 10;
        let to_evict = self.cache.len().saturating_sub(target);

        if to_evict == 0 {
            return 0;
        }

        let min_evictable_height = current_height.saturating_sub(EVICT_MIN_AGE_BLOCKS);
        let very_old_threshold = current_height.saturating_sub(EVICT_VERY_OLD_BLOCKS);
        let mut evicted = 0;
        let mut skipped_recent = 0;
        let queue_len = self.eviction_queue.len();

        // Fast path: collect evictable candidates from queue, sort by (very_old, dust, age), evict best
        let max_candidates = (to_evict * 4).min(queue_len);
        let mut candidates: Vec<(OutPointKey, i64, u64)> = Vec::with_capacity(max_candidates);
        while candidates.len() < max_candidates && skipped_recent <= queue_len {
            let key = match self.eviction_queue.pop_front() {
                Some(k) => k,
                None => break,
            };
            if self.recently_accessed.contains(&key) {
                continue;
            }
            let outpoint = key_to_outpoint(&key);
            if let Some(utxo) = self.cache.get(&outpoint) {
                if utxo.height > min_evictable_height {
                    self.eviction_queue.push_back(key);
                    skipped_recent += 1;
                    continue;
                }
                candidates.push((key, utxo.value, utxo.height));
            }
        }
        // Sort: very old first, then dust (low value), then by height
        candidates.sort_by(|a, b| {
            let very_old_a = a.2 < very_old_threshold;
            let very_old_b = b.2 < very_old_threshold;
            match (very_old_a, very_old_b) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => (a.1, a.2).cmp(&(b.1, b.2)),
            }
        });
        for (key, _, _) in candidates.drain(..to_evict.min(candidates.len())) {
            let op = key_to_outpoint(&key);
            if self.cache.remove(&op).is_some() {
                evicted += 1;
            }
        }
        // Push back unevicted candidates
        for (key, _, _) in candidates {
            self.eviction_queue.push_back(key);
        }

        // Fallback: scan cache, prefer very old > dust > old (O(cache))
        let remaining = to_evict.saturating_sub(evicted);
        if remaining > 0 {
            let very_old_threshold = current_height.saturating_sub(EVICT_VERY_OLD_BLOCKS);
            // Collect (OutPoint, value, height) for non-protected, evictable entries
            let mut candidates: Vec<(OutPoint, i64, u64)> = self
                .cache
                .iter()
                .filter(|(op, _)| !self.recently_accessed.contains(&outpoint_to_key(op)))
                .filter(|(_, utxo)| utxo.height <= min_evictable_height)
                .map(|(op, utxo)| (op.clone(), utxo.value, utxo.height))
                .collect();
            // Prefer: very old first (height < current - 10k), then dust (low value), then by height
            candidates.sort_by(|a, b| {
                let very_old_a = a.2 < very_old_threshold;
                let very_old_b = b.2 < very_old_threshold;
                match (very_old_a, very_old_b) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => (a.1, a.2).cmp(&(b.1, b.2)),
                }
            });
            for (op, _, _) in candidates.into_iter().take(remaining) {
                if self.cache.remove(&op).is_some() {
                    evicted += 1;
                }
            }
            let rem2 = to_evict.saturating_sub(evicted);
            if rem2 > 0 {
                // Last resort: evict from recently_accessed (protected) if still over limit
                let mut extra: Vec<OutPoint> = self
                    .cache
                    .keys()
                    .filter(|k| self.recently_accessed.contains(&outpoint_to_key(k)))
                    .take(rem2)
                    .cloned()
                    .collect();
                for k in &extra {
                    self.cache.remove(k);
                }
                evicted += extra.len();
            }
        }

        self.recently_accessed.clear();
        self.stats_evictions += evicted as u64;

        debug!(
            "DiskBackedUtxoSet: evicted {} entries (cache: {}/{}, total UTXOs: {})",
            evicted,
            self.cache.len(),
            self.max_cache_entries,
            self.total_utxo_count,
        );

        evicted
    }

    /// Log statistics for monitoring.
    pub fn log_stats(&self, height: u64) {
        info!(
            "UTXO-DISK at height {}: cache={}/{}, total={}, disk_loads={}, cache_hits={}, pending_hits={}, evictions={}, pending_writes={}",
            height,
            self.cache.len(),
            self.max_cache_entries,
            self.total_len(),
            self.stats_disk_loads,
            self.stats_cache_hits,
            self.stats_pending_hits,
            self.stats_evictions,
            self.pending_writes.len(),
        );
    }
}

impl Drop for DiskBackedUtxoSet {
    fn drop(&mut self) {
        if !self.pending_writes.is_empty() {
            if let Err(e) = self.flush() {
                warn!("DiskBackedUtxoSet: failed to flush pending on drop: {}", e);
            }
        }
        for batch in self.flushing_batches.drain(..) {
            if let Err(e) = Self::flush_batch_to_disk(&batch, self.disk.as_ref()) {
                warn!("DiskBackedUtxoSet: failed to flush batch on drop: {}", e);
            }
        }
    }
}

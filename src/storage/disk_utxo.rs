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

use crate::storage::database::Tree;
use anyhow::Result;

/// TidesDB max ops per transaction (TDB_MAX_TXN_OPS=100000). Batch splitting safety limit.
const MAX_BATCH_OPS: usize = 50_000;
use blvm_consensus::transaction::is_coinbase;
use blvm_consensus::types::{Block, Hash, OutPoint, UTXO, UtxoSet};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Fixed-size outpoint key: 32 bytes txid + 8 bytes index (big-endian)
pub(crate) type OutPointKey = [u8; 40];

/// Serialize an OutPoint to a fixed-size storage key.
/// Zero-allocation: returns a stack-allocated array instead of Vec.
#[inline]
fn outpoint_to_key(outpoint: &OutPoint) -> OutPointKey {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(&outpoint.hash);
    key[32..40].copy_from_slice(&outpoint.index.to_be_bytes());
    key
}

/// Convert storage key back to OutPoint for cache removal.
#[inline]
fn key_to_outpoint(key: &OutPointKey) -> OutPoint {
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&key[..32]);
    let index = u64::from_be_bytes(key[32..40].try_into().unwrap());
    OutPoint { hash, index }
}

/// Load UTXOs for given keys from disk. Used by prefetch overlap (spawn_blocking).
pub(crate) fn load_keys_from_disk(
    disk: Arc<dyn Tree>,
    keys: Vec<OutPointKey>,
) -> Result<HashMap<OutPointKey, UTXO>> {
    #[cfg(feature = "rayon")]
    {
        use rayon::prelude::*;
        let from_disk: Vec<(OutPointKey, UTXO)> = keys
            .par_iter()
            .filter_map(|key| {
                match disk.get(key) {
                    Ok(Some(data)) => {
                        bincode::deserialize::<UTXO>(&data)
                            .ok()
                            .map(|utxo| (*key, utxo))
                    }
                    _ => None,
                }
            })
            .collect();
        Ok(from_disk.into_iter().collect())
    }
    #[cfg(not(feature = "rayon"))]
    {
        let mut result = HashMap::with_capacity(keys.len());
        for key in keys {
            if let Ok(Some(data)) = disk.get(&key) {
                if let Ok(utxo) = bincode::deserialize::<UTXO>(&data) {
                    result.insert(key, utxo);
                }
            }
        }
        Ok(result)
    }
}

/// Collect all input outpoint keys from a block (for prefetch overlap).
pub(crate) fn block_input_keys(block: &Block) -> Vec<OutPointKey> {
    let est: usize = block
        .transactions
        .iter()
        .filter(|tx| !is_coinbase(tx))
        .map(|tx| tx.inputs.len())
        .sum();
    let mut keys = Vec::with_capacity(est);
    for tx in block.transactions.iter() {
        if is_coinbase(tx) {
            continue;
        }
        for input in tx.inputs.iter() {
            keys.push(outpoint_to_key(&input.prevout));
        }
    }
    keys
}

/// Collect and deduplicate outpoint keys from multiple blocks (for batched lookahead prefetch).
/// Reduces TidesDB round-trips by loading UTXOs for several blocks in one disk read batch.
pub(crate) fn block_input_keys_batch(blocks: &[&Block]) -> Vec<OutPointKey> {
    use std::collections::HashSet;
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
    let mut seen = HashSet::with_capacity(est);
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
    /// Value: Some(serialized_utxo) for inserts, None for deletes.
    pending_writes: HashMap<OutPointKey, Option<Vec<u8>>>,

    /// Batches swapped out for async flush; still readable until flush completes.
    /// Caller spawns flush; when done, calls mark_flush_complete() to pop.
    flushing_batches: VecDeque<Arc<HashMap<OutPointKey, Option<Vec<u8>>>>>,

    /// Flush threshold — number of pending writes before swap for async flush.
    flush_threshold: usize,

    /// Stats
    pub stats_disk_loads: u64,
    pub stats_cache_hits: u64,
    pub stats_evictions: u64,
    pub stats_pending_hits: u64,
    
    /// Recently accessed outpoints (from last block) - avoid evicting these
    /// Uses OutPointKey to avoid per-input OutPoint clones (hot path: ~3k inputs/block)
    recently_accessed: std::collections::HashSet<OutPointKey>,

    /// FIFO queue of cache insert keys for O(to_evict) eviction (avoids O(cache) scan).
    /// Capped at max_cache_entries. Tracks prefetch inserts; connect_block removes aren't reflected.
    eviction_queue: VecDeque<OutPointKey>,
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
            pending_writes: HashMap::with_capacity(flush_threshold),
            flushing_batches: VecDeque::new(),
            flush_threshold,
            stats_disk_loads: 0,
            stats_cache_hits: 0,
            stats_evictions: 0,
            stats_pending_hits: 0,
            recently_accessed: std::collections::HashSet::with_capacity(2000),
            eviction_queue: VecDeque::with_capacity(max_cache_entries.min(100_000)),
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
    /// Read-only; used before spawn_blocking to move disk I/O off the critical path.
    /// Must match prefetch_block logic exactly (cache, pending, flushing).
    pub fn collect_gaps(&self, block: &Block) -> Vec<OutPointKey> {
        let est_inputs: usize = block
            .transactions
            .iter()
            .filter(|tx| !is_coinbase(tx))
            .map(|tx| tx.inputs.len())
            .sum();
        let mut keys_to_load = Vec::with_capacity(est_inputs);
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let key = outpoint_to_key(&input.prevout);

                if self.cache.contains_key(&key_to_outpoint(&key)) {
                    continue;
                }
                if let Some(entry) = self.get_pending_or_flushing(&key) {
                    if entry.is_some() {
                        continue; // Pending insert — apply_pending_hits will handle
                    }
                    // entry.is_none() = pending delete — skip, don't add to keys_to_load
                    continue;
                }
                keys_to_load.push(key);
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
                if let Some(Some(data)) = self.get_pending_or_flushing(&key) {
                    if let Ok(utxo) = bincode::deserialize::<UTXO>(&data) {
                        self.cache.insert(key_to_outpoint(&key), utxo);
                        self.recently_accessed.insert(key);
                        self.push_eviction_key(key);
                        self.stats_pending_hits += 1;
                    }
                }
            }
        }
    }

    /// Check pending_writes and flushing_batches for a key. Returns Some(Some(data)) for insert,
    /// Some(None) for delete, None if not found. Clones to avoid holding ref across mutation.
    #[inline]
    fn get_pending_or_flushing(&self, key: &OutPointKey) -> Option<Option<Vec<u8>>> {
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

    /// Load a single UTXO from disk by outpoint.
    /// Checks pending_writes, flushing_batches, then disk.
    fn load_from_disk(&mut self, outpoint: &OutPoint) -> Result<Option<UTXO>> {
        let key = outpoint_to_key(outpoint);

        // Check pending writes and flushing batches first
        if let Some(entry) = self.get_pending_or_flushing(&key) {
            self.stats_pending_hits += 1;
            return match entry {
                Some(data) => Ok(Some(bincode::deserialize(&data)?)),
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
    pub fn prefetch_block_to_buffer(&self, block: &Block) -> Result<HashMap<OutPointKey, UTXO>> {
        let keys = block_input_keys(block);
        if keys.is_empty() {
            return Ok(HashMap::new());
        }
        load_keys_from_disk(Arc::clone(&self.disk), keys)
    }

    /// Merge a prefetched buffer into cache. Skips entries that are pending deletes (spent in a prior block).
    /// Call before prefetch_block when starting validation of the prefetched block.
    pub fn merge_prefetch_buffer(&mut self, buffer: HashMap<OutPointKey, UTXO>) {
        for (key, utxo) in buffer {
            // Skip if this outpoint was spent (pending delete in pending_writes or flushing_batches)
            if self.get_pending_or_flushing(&key) == Some(None) {
                continue;
            }
            // Skip if already in cache (e.g. from prior block's outputs)
            if self.cache.contains_key(&key_to_outpoint(&key)) {
                continue;
            }
            self.cache.insert(key_to_outpoint(&key), utxo);
            self.recently_accessed.insert(key);
            self.push_eviction_key(key);
            self.stats_disk_loads += 1;
        }
    }

    /// Push a key to the eviction queue (called on cache insert). Capped at max_cache_entries.
    #[inline]
    fn push_eviction_key(&mut self, key: OutPointKey) {
        if self.eviction_queue.len() >= self.max_cache_entries {
            self.eviction_queue.pop_front();
        }
        self.eviction_queue.push_back(key);
    }

    /// Get a mutable reference to the in-memory cache.
    #[inline]
    pub fn cache_mut(&mut self) -> &mut UtxoSet {
        &mut self.cache
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

    /// After block validation, record the block's UTXO changes for disk persistence.
    /// Only updates pending_writes — does NOT touch the cache (connect_block already did that).
    ///
    /// Uses pre-computed tx_ids to avoid re-hashing every transaction.
    /// Optimized: Parallelizes UTXO serialization for better CPU utilization.
    pub fn sync_block_with_txids(&mut self, block: &Block, height: u64, tx_ids: &[Hash]) -> Result<()> {
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
                        local_inserts.push((
                            key,
                            UTXO {
                                value: output.value,
                                script_pubkey: output.script_pubkey.clone(),
                                height,
                                is_coinbase: is_cb,
                            },
                        ));
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
            let est_inputs: usize = block.transactions.iter()
                .filter(|tx| !is_coinbase(tx))
                .map(|tx| tx.inputs.len())
                .sum();
            let est_outputs: usize = block.transactions.iter().map(|tx| tx.outputs.len()).sum();
            let mut deletes: Vec<OutPointKey> = Vec::with_capacity(est_inputs);
            let mut inserts: Vec<(OutPointKey, UTXO)> = Vec::with_capacity(est_outputs);
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
                        UTXO {
                            value: output.value,
                            script_pubkey: output.script_pubkey.clone(),
                            height,
                            is_coinbase: is_cb,
                        },
                    ));
                }
            }
            let delta = inserts.len() as isize - deletes.len() as isize;
            (deletes, inserts, delta)
        };

        self.total_utxo_count = self.total_utxo_count.saturating_add_signed(total_delta);

        // Parallel serialize UTXOs for inserts
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            
            let serialized_inserts: Vec<(OutPointKey, Vec<u8>)> = inserts
                .par_iter()
                .map(|(key, utxo)| {
                    let value = bincode::serialize(utxo)
                        .map_err(|e| anyhow::anyhow!("Failed to serialize UTXO: {}", e))?;
                    Ok((*key, value))
                })
                .collect::<Result<Vec<_>>>()?;
            
            // Insert deletes
            for key in deletes {
                self.pending_writes.insert(key, None);
            }
            
            // Insert serialized UTXOs
            for (key, value) in serialized_inserts {
                self.push_eviction_key(key);
                self.pending_writes.insert(key, Some(value));
            }
        }
        
        #[cfg(not(feature = "rayon"))]
        {
            // Insert deletes
            for key in deletes {
                self.pending_writes.insert(key, None);
            }
            
            // Serialize and insert UTXOs sequentially
            for (key, utxo) in inserts {
                self.push_eviction_key(key);
                let value = bincode::serialize(&utxo)
                    .map_err(|e| anyhow::anyhow!("Failed to serialize UTXO: {}", e))?;
                self.pending_writes.insert(key, Some(value));
            }
        }

        // Do NOT flush here — caller uses maybe_take_flush_batch() and spawns async
        Ok(())
    }

    /// If pending_writes >= threshold, swap to flushing_batches and return batch for async flush.
    /// Caller spawns flush; when handle completes, caller calls mark_flush_complete().
    pub fn maybe_take_flush_batch(&mut self) -> Option<Arc<HashMap<OutPointKey, Option<Vec<u8>>>>> {
        if self.pending_writes.len() < self.flush_threshold {
            return None;
        }
        let batch = Arc::new(std::mem::take(&mut self.pending_writes));
        self.pending_writes = HashMap::with_capacity(self.flush_threshold);
        self.flushing_batches.push_back(Arc::clone(&batch));
        Some(batch)
    }

    /// Call when an async flush completes. Pops the oldest batch from flushing_batches.
    pub fn mark_flush_complete(&mut self) {
        self.flushing_batches.pop_front();
    }

    /// Flush a batch to disk. Used by spawn_blocking (parallel_ibd) and by Drop.
    /// Splits into chunks of MAX_BATCH_OPS to stay under TidesDB's TDB_MAX_TXN_OPS (100k).
    pub(crate) fn flush_batch_to_disk(batch: &HashMap<OutPointKey, Option<Vec<u8>>>, disk: &dyn Tree) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let count = batch.len();
        let entries: Vec<_> = batch.iter().collect();
        let mut total_flushed = 0;
        for chunk in entries.chunks(MAX_BATCH_OPS) {
            let mut b = disk.batch();
            for (key, value_opt) in chunk {
                match value_opt {
                    Some(value) => b.put(*key, value),
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
        let count = entries.len();
        let mut total_flushed = 0;
        for chunk in entries.chunks(MAX_BATCH_OPS) {
            let mut batch = self.disk.batch();
            for (key, value_opt) in chunk {
                match value_opt {
                    Some(value) => batch.put(key, &value),
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
    pub fn evict_if_needed(&mut self) -> usize {
        let trigger_threshold = self.max_cache_entries + self.max_cache_entries / 10;
        if self.cache.len() <= trigger_threshold {
            return 0;
        }

        let target = self.max_cache_entries * 9 / 10;
        let to_evict = self.cache.len().saturating_sub(target);

        if to_evict == 0 {
            return 0;
        }

        let mut evicted = 0;

        // Fast path: pop from FIFO queue (O(to_evict)) — avoids O(cache) scan
        while evicted < to_evict {
            let key = match self.eviction_queue.pop_front() {
                Some(k) => k,
                None => break,
            };
            if self.recently_accessed.contains(&key) {
                continue; // Skip recently accessed
            }
            let outpoint = key_to_outpoint(&key);
            if self.cache.remove(&outpoint).is_some() {
                evicted += 1;
            }
            // If remove returned None, key was stale (already spent by connect_block)
        }

        // Fallback: if queue exhausted before evicting enough, scan cache (O(cache))
        let remaining = to_evict.saturating_sub(evicted);
        if remaining > 0 {
            let mut keys_to_evict: Vec<OutPoint> = self.cache.keys()
                .filter(|k| !self.recently_accessed.contains(&outpoint_to_key(k)))
                .take(remaining)
                .cloned()
                .collect();
            let rem2 = remaining.saturating_sub(keys_to_evict.len());
            if rem2 > 0 {
                keys_to_evict.extend(
                    self.cache.keys()
                        .filter(|k| self.recently_accessed.contains(&outpoint_to_key(k)))
                        .take(rem2)
                        .cloned(),
                );
            }
            for k in &keys_to_evict {
                self.cache.remove(k);
            }
            evicted += keys_to_evict.len();
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

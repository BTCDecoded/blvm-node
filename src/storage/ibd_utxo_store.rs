//! IBD v2 UTXO store: concurrent DashMap, zero lock contention.
//!
//! Replaces RwLock<DiskBackedUtxoSet> for IBD. Prefetch reads via .get();
//! validation writes via .insert/.remove. Flush task drains to disk.
//!
//! Enabled when BLVM_IBD_V2=1.

use crate::storage::database::Tree;
use crate::storage::disk_utxo::{key_to_outpoint, load_keys_from_disk, outpoint_to_key, SyncBatch, MAX_BATCH_OPS};
use anyhow::Result;
use blvm_consensus::block::compute_block_tx_ids;
use blvm_consensus::transaction::is_coinbase;
use blvm_consensus::types::{OutPoint, UTXO, UtxoSet};
use dashmap::DashMap;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::debug;

type OutPointKey = [u8; 40];

/// Pending value: Some = UTXO, None = spent (delete on flush).
type PendingValue = Option<Arc<UTXO>>;

/// IBD v2 concurrent UTXO store. No RwLock.
#[cfg(feature = "production")]
pub struct IbdUtxoStore {
    /// Concurrent cache. Prefetch reads; validation writes.
    cache: DashMap<OutPointKey, PendingValue>,
    disk: Arc<dyn Tree>,
    total_utxo_count: AtomicIsize,
    flush_threshold: usize,
    /// Pending writes for threshold-based disk flush.
    pending_writes: Mutex<FxHashMap<OutPointKey, PendingValue>>,
    /// BLVM_IBD_MEMORY_ONLY: skip disk reads during IBD (fresh from block 0 only).
    memory_only: bool,
    /// Max cache entries. Evict when over to avoid OOM. usize::MAX = no limit.
    max_entries: usize,
    /// FIFO queue for eviction order. Keys not in pending_writes are safe to evict.
    eviction_queue: Mutex<VecDeque<OutPointKey>>,
}

#[cfg(feature = "production")]
impl IbdUtxoStore {
    pub fn new(disk: Arc<dyn Tree>, flush_threshold: usize) -> Self {
        Self::new_with_options(disk, flush_threshold, false, usize::MAX)
    }

    #[inline]
    pub fn memory_only(&self) -> bool {
        self.memory_only
    }

    pub fn new_with_options(
        disk: Arc<dyn Tree>,
        flush_threshold: usize,
        memory_only: bool,
        max_entries: usize,
    ) -> Self {
        // Par-4: 128 shards reduce contention with 8–24 prefetch workers hitting cache concurrently.
        Self {
            cache: DashMap::with_shard_amount(128),
            disk,
            total_utxo_count: AtomicIsize::new(0),
            flush_threshold,
            pending_writes: Mutex::new(FxHashMap::default()),
            memory_only,
            max_entries,
            eviction_queue: Mutex::new(VecDeque::with_capacity(
                if max_entries == usize::MAX { 100_000 } else { max_entries.min(100_000) },
            )),
        }
    }

    /// Push key to eviction queue. Call after inserting into cache.
    #[inline]
    fn push_eviction_key(&self, key: OutPointKey) {
        if self.max_entries == usize::MAX {
            return;
        }
        if let Ok(mut q) = self.eviction_queue.lock() {
            if q.len() < self.max_entries * 2 {
                q.push_back(key);
            }
        }
    }

    /// Number of entries waiting to be flushed to disk.
    pub fn pending_len(&self) -> usize {
        self.pending_writes.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Evict entries over the cache limit. Entries in pending_writes are skipped
    /// (not yet on disk). When pending_writes blocks eviction, the MemoryGuard in
    /// the validation loop detects rising RSS and forces a flush.
    /// Call after take_flush_batch_force() to evict now-flushed entries.
    pub(crate) fn maybe_evict(&self) {
        if self.max_entries == usize::MAX {
            return;
        }
        let len = self.cache.len();
        if len <= self.max_entries {
            return;
        }
        let to_evict = len - self.max_entries;
        let mut evicted = 0;
        let pending = self.pending_writes.lock().expect("lock");
        let mut q = self.eviction_queue.lock().expect("lock");
        let max_tries = q.len().min(to_evict * 2 + 1000);
        for _ in 0..max_tries {
            if evicted >= to_evict {
                break;
            }
            let Some(key) = q.pop_front() else {
                break;
            };
            if pending.contains_key(&key) {
                q.push_back(key);
                continue;
            }
            if self.cache.remove(&key).is_some() {
                evicted += 1;
            }
        }
        if evicted > 0 {
            debug!("IbdUtxoStore: evicted {} entries (cache was over limit)", evicted);
        }
    }

    /// Evict aggressively when under RSS pressure. Evict 50% of current cache to free memory.
    /// The UTXO cache is often NOT the main memory consumer (block pipeline buffers dominate),
    /// so we evict hard to free whatever we can. Call after flush_pending_batch() completes.
    pub(crate) fn evict_aggressive_for_rss(&self) {
        let len = self.cache.len();
        if len == 0 {
            return;
        }
        let target = len / 2;
        let to_evict = len - target;
        if to_evict == 0 {
            return;
        }
        let mut evicted = 0;
        let pending = self.pending_writes.lock().expect("lock");
        let mut q = self.eviction_queue.lock().expect("lock");
        let max_tries = q.len().min(to_evict * 3 + 5000);
        for _ in 0..max_tries {
            if evicted >= to_evict {
                break;
            }
            let Some(key) = q.pop_front() else {
                break;
            };
            if pending.contains_key(&key) {
                q.push_back(key);
                continue;
            }
            if self.cache.remove(&key).is_some() {
                evicted += 1;
            }
        }
        if evicted > 0 {
            debug!("IbdUtxoStore: evicted {} of {} entries for RSS (target {})", evicted, len, target);
        }
    }

    /// Bootstrap with genesis coinbase output so block 1+ can find it.
    /// Call when start_height == 0 to ensure the store has the foundation.
    pub fn bootstrap_genesis(&self, genesis_block: &blvm_consensus::types::Block) {
        if genesis_block.transactions.is_empty() {
            return;
        }
        let tx_ids = compute_block_tx_ids(genesis_block);
        if tx_ids.is_empty() {
            return;
        }
        let coinbase = &genesis_block.transactions[0];
        if !is_coinbase(coinbase) || coinbase.outputs.is_empty() {
            return;
        }
        let outpoint = OutPoint {
            hash: tx_ids[0],
            index: 0,
        };
        let output = &coinbase.outputs[0];
        let utxo = UTXO {
            value: output.value,
            script_pubkey: output.script_pubkey.as_slice().into(),
            height: 0,
            is_coinbase: true,
        };
        let key = outpoint_to_key(&outpoint);
        if self.cache.get(&key).is_none() {
            self.cache.insert(key, Some(Arc::new(utxo)));
            self.push_eviction_key(key);
            self.total_utxo_count.fetch_add(1, Ordering::Relaxed);
            self.maybe_evict();
        }
    }

    /// Get UTXO by key. Used by prefetch for filtering.
    #[inline]
    pub fn get(&self, key: &OutPointKey) -> Option<UTXO> {
        self.cache.get(key).and_then(|r| r.as_ref().map(|a| (**a).clone()))
    }

    /// Insert UTXO. Validation calls on commit.
    #[inline]
    pub fn insert(&self, key: OutPointKey, utxo: UTXO) {
        self.cache.insert(key, Some(Arc::new(utxo)));
        self.push_eviction_key(key);
        self.maybe_evict();
    }

    /// Remove (spend) UTXO. Validation calls on commit.
    #[inline]
    pub fn remove(&self, key: &OutPointKey) {
        self.cache.remove(key);
    }

    /// Filter keys to those not in store (need disk load). Used by prefetch.
    #[inline]
    pub fn filter_keys_to_load(&self, keys: &[OutPointKey]) -> Vec<OutPointKey> {
        keys.iter()
            .filter(|k| !self.cache.contains_key(*k))
            .cloned()
            .collect()
    }

    /// Merge loaded UTXOs into store. Prefetch calls after load. Arc map = no clone.
    pub fn merge_loaded_arc(&self, loaded: &FxHashMap<OutPointKey, Arc<UTXO>>) -> usize {
        let mut count = 0;
        for (key, arc) in loaded {
            if !self.cache.contains_key(key) {
                self.cache.insert(*key, Some(Arc::clone(arc)));
                self.push_eviction_key(*key);
                count += 1;
            }
        }
        if count > 0 {
            self.maybe_evict();
        }
        count
    }

    /// Build UtxoSet from keys for validation. Prefetch builds this for ready_tx.
    /// Tries cache first, then disk for misses (handles resume / flushed-but-not-evicted).
    pub fn build_utxo_map(&self, keys: &[OutPointKey]) -> UtxoSet {
        let mut map = UtxoSet::default();
        let mut buf = Vec::new();
        self.supplement_utxo_map_with_buf(&mut map, keys, &mut buf);
        map
    }

    /// Fill existing map with UTXOs for keys. Clears map first. Reused for 5–10x BPS (avoids alloc/block).
    #[inline]
    pub fn build_utxo_map_into(&self, keys: &[OutPointKey], map: &mut UtxoSet) {
        map.clear();
        let mut buf = Vec::new();
        self.supplement_utxo_map_with_buf(map, keys, &mut buf);
    }

    /// Like build_utxo_map_into but reuses buf to avoid allocation.
    pub fn build_utxo_map_into_with_buf(
        &self,
        keys: &[OutPointKey],
        map: &mut UtxoSet,
        cache_misses_buf: &mut Vec<OutPointKey>,
    ) {
        map.clear();
        self.supplement_utxo_map_with_buf(map, keys, cache_misses_buf);
    }

    /// Build full Arc map for ready_tx. Prefetch calls after merge; all keys in cache → 0 disk reads.
    /// Validation receives complete map, never blocks on disk.
    #[inline]
    pub fn build_utxo_map_arc(&self, keys: &[OutPointKey]) -> FxHashMap<OutPointKey, Arc<UTXO>> {
        let mut map = FxHashMap::default();
        for key in keys {
            if let Some(ref r) = self.cache.get(key) {
                if let Some(arc) = r.as_ref() {
                    map.insert(*key, Arc::clone(arc));
                }
            }
        }
        map
    }

    /// Add missing keys to an existing UtxoSet. Uses Arc::clone for cache hits (no UTXO clone).
    /// Pass reusable buf to avoid per-call allocation when loading from disk.
    pub fn supplement_utxo_map_with_buf(
        &self,
        map: &mut UtxoSet,
        keys: &[OutPointKey],
        cache_misses_buf: &mut Vec<OutPointKey>,
    ) {
        cache_misses_buf.clear();
        for key in keys {
            let op = key_to_outpoint(key);
            if map.contains_key(&op) {
                continue;
            }
            if let Some(ref r) = self.cache.get(key) {
                if let Some(arc) = r.as_ref() {
                    map.insert(op, Arc::clone(arc));
                    continue;
                }
            }
            cache_misses_buf.push(*key);
        }
        if !cache_misses_buf.is_empty() && !self.memory_only {
            if let Ok(loaded) = load_keys_from_disk(Arc::clone(&self.disk), std::mem::take(cache_misses_buf)) {
                for (key, utxo) in loaded {
                    let arc = Arc::new(utxo);
                    map.insert(key_to_outpoint(&key), Arc::clone(&arc));
                    self.cache.insert(key, Some(arc));
                    self.push_eviction_key(key);
                }
                self.maybe_evict();
            }
        }
    }

    /// Apply sync batch (from UtxoDelta). Uses Arc::clone for inserts when batch has Arc (no UTXO clone).
    /// Also accumulates into pending_writes for threshold-based disk flush.
    pub fn apply_sync_batch(&self, batch: &SyncBatch) {
        self.total_utxo_count
            .fetch_add(batch.total_delta, Ordering::Relaxed);
        {
            let mut pending = self.pending_writes.lock().expect("lock");
            for key in &batch.deletes {
                self.remove(key);
                pending.insert(*key, None);
            }
            for (key, value) in &batch.inserts {
                self.cache.insert(*key, Some(Arc::clone(value)));
                pending.insert(*key, Some(Arc::clone(value)));
                self.push_eviction_key(*key);
            }
        }
        self.maybe_evict();
    }

    /// If pending_writes >= threshold, take batch for async flush. Caller spawns flush.
    pub fn maybe_take_flush_batch(&self) -> Option<Arc<FxHashMap<OutPointKey, PendingValue>>> {
        let mut pending = self.pending_writes.lock().expect("lock");
        if pending.len() < self.flush_threshold {
            return None;
        }
        Some(Arc::new(std::mem::take(&mut *pending)))
    }

    /// Take pending batch regardless of threshold (for BLVM_IBD_DEFER_FLUSH checkpoint flush).
    pub fn take_flush_batch_force(&self) -> Option<Arc<FxHashMap<OutPointKey, PendingValue>>> {
        let mut pending = self.pending_writes.lock().expect("lock");
        if pending.is_empty() {
            return None;
        }
        Some(Arc::new(std::mem::take(&mut *pending)))
    }

    /// Take any remaining pending writes for final flush (e.g. at IBD end).
    pub fn take_remaining_pending(&self) -> FxHashMap<OutPointKey, PendingValue> {
        let mut pending = self.pending_writes.lock().expect("lock");
        std::mem::take(&mut *pending)
    }

    /// Flush pending to disk. Returns count flushed.
    pub fn flush_pending_batch(&self, batch: &FxHashMap<OutPointKey, PendingValue>) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let mut total_flushed = 0;
        let mut iter = batch.iter();
        loop {
            let chunk: Vec<_> = iter.by_ref().take(MAX_BATCH_OPS).map(|(k, v)| (*k, v.clone())).collect();
            if chunk.is_empty() {
                break;
            }
            let mut b = self.disk.batch();
            for (key, value_opt) in &chunk {
                match value_opt {
                    Some(arc) => {
                        let bytes = bincode::serialize(arc.as_ref())
                            .map_err(|e| anyhow::anyhow!("UTXO serialize: {}", e))?;
                        b.put(key.as_slice(), bytes.as_slice());
                    }
                    None => b.delete(key.as_slice()),
                }
            }
            b.commit()?;
            total_flushed += chunk.len();
        }
        debug!("IbdUtxoStore: flushed {} operations to disk", total_flushed);
        // Evict flushed inserts from cache when over limit (they're on disk now).
        if self.max_entries != usize::MAX && self.cache.len() > self.max_entries {
            let mut evicted = 0;
            for (key, value_opt) in batch {
                if value_opt.is_some() {
                    if self.cache.remove(key).is_some() {
                        evicted += 1;
                    }
                    if self.cache.len() <= self.max_entries {
                        break;
                    }
                }
            }
            if evicted > 0 {
                debug!("IbdUtxoStore: evicted {} flushed entries (cache over limit)", evicted);
            }
        }
        Ok(total_flushed)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn total_count(&self) -> isize {
        self.total_utxo_count.load(Ordering::Relaxed)
    }

    pub fn disk_clone(&self) -> Arc<dyn Tree> {
        Arc::clone(&self.disk)
    }
}

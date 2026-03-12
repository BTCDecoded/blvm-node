//! IBD v2 UTXO store: concurrent DashMap, zero lock contention.
//!
//! Replaces RwLock<DiskBackedUtxoSet> for IBD. Prefetch reads via .get();
//! validation writes via .insert/.remove. Flush task drains to disk.

use crate::storage::database::Tree;
use crate::storage::disk_utxo::{
    key_to_outpoint, load_keys_from_disk, outpoint_to_key, SyncBatch, MAX_BATCH_OPS,
};
use anyhow::Result;
use blvm_consensus::block::compute_block_tx_ids;
use blvm_consensus::transaction::is_coinbase;
use blvm_consensus::types::{OutPoint, UtxoSet, UTXO};
use dashmap::DashMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::debug;

type OutPointKey = [u8; 40];

/// Pending value: Some = UTXO, None = spent (delete on flush).
type PendingValue = Option<Arc<UTXO>>;

/// Eviction strategy. BLVM_IBD_EVICTION: "dynamic" | "fifo" | "lifo" (default: fifo).
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "production")]
pub enum EvictionStrategy {
    /// Age/dust heuristics: prefer dust, very old (height < current - 10k), then old (height < current - 100).
    Dynamic,
    /// Oldest first (pop from front of queue).
    Fifo,
    /// Newest first (pop from back of queue).
    Lifo,
}

#[cfg(feature = "production")]
impl EvictionStrategy {
    fn from_env() -> Self {
        let s = std::env::var("BLVM_IBD_EVICTION").unwrap_or_default();
        let s = s.trim().to_lowercase();
        match s.as_str() {
            "dynamic" => Self::Dynamic,
            "lifo" => Self::Lifo,
            _ => Self::Fifo,
        }
    }
}

/// Don't evict outputs created in the last N blocks (likely to be spent soon).
const EVICT_MIN_AGE_BLOCKS: u64 = 100;
/// Prefer evicting outputs older than this (creation height < current - N).
const EVICT_VERY_OLD_BLOCKS: u64 = 10_000;
/// Dust threshold (satoshis) — eviction sort prefers lowest value first.
const EVICT_DUST_THRESHOLD: i64 = 546;

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
    /// FIFO/LIFO queue for eviction order. Keys not in pending_writes are safe to evict.
    eviction_queue: Mutex<VecDeque<OutPointKey>>,
    /// Eviction strategy: dynamic (age/dust), fifo, lifo.
    eviction_strategy: EvictionStrategy,
    /// For dynamic: keys accessed in last block — avoid evicting. Cleared after evict_if_needed.
    recently_accessed: Mutex<FxHashSet<OutPointKey>>,
    /// Stats: disk loads (cache miss → load from disk).
    stats_disk_loads: AtomicU64,
    /// Stats: cache hits (found in cache).
    stats_cache_hits: AtomicU64,
    /// Stats: evictions.
    stats_evictions: AtomicU64,
    /// Stats: pending hits (found in pending_writes before flush).
    stats_pending_hits: AtomicU64,
}

#[cfg(feature = "production")]
impl IbdUtxoStore {
    pub fn new(disk: Arc<dyn Tree>, flush_threshold: usize) -> Self {
        Self::new_with_options(disk, flush_threshold, false, usize::MAX)
    }

    /// Memory-only store for benchmarks and tests. No disk backing.
    pub fn new_memory_only() -> Self {
        struct NullTree;
        impl Tree for NullTree {
            fn insert(&self, _: &[u8], _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn get(&self, _: &[u8]) -> Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn remove(&self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn contains_key(&self, _: &[u8]) -> Result<bool> {
                Ok(false)
            }
            fn clear(&self) -> Result<()> {
                Ok(())
            }
            fn len(&self) -> Result<usize> {
                Ok(0)
            }
            fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
                Box::new(std::iter::empty())
            }
            fn batch(&self) -> Box<dyn crate::storage::database::BatchWriter + '_> {
                struct NullBatch;
                impl crate::storage::database::BatchWriter for NullBatch {
                    fn put(&mut self, _: &[u8], _: &[u8]) {}
                    fn delete(&mut self, _: &[u8]) {}
                    fn commit(self: Box<Self>) -> Result<()> {
                        Ok(())
                    }
                    fn len(&self) -> usize {
                        0
                    }
                }
                Box::new(NullBatch)
            }
        }
        Self::new_with_options(Arc::new(NullTree), usize::MAX, true, usize::MAX)
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
        let eviction_strategy = EvictionStrategy::from_env();
        // Par-4: 128 shards reduce contention with 8–24 prefetch workers hitting cache concurrently.
        Self {
            cache: DashMap::with_shard_amount(128),
            disk,
            total_utxo_count: AtomicIsize::new(0),
            flush_threshold,
            pending_writes: Mutex::new(FxHashMap::default()),
            memory_only,
            max_entries,
            eviction_queue: Mutex::new(VecDeque::with_capacity(if max_entries == usize::MAX {
                100_000
            } else {
                max_entries.min(100_000)
            })),
            eviction_strategy,
            recently_accessed: Mutex::new(FxHashSet::default()),
            stats_disk_loads: AtomicU64::new(0),
            stats_cache_hits: AtomicU64::new(0),
            stats_evictions: AtomicU64::new(0),
            stats_pending_hits: AtomicU64::new(0),
        }
    }

    /// True when eviction strategy is dynamic (needs protect_keys + evict_if_needed with height).
    #[inline]
    pub fn is_dynamic_eviction(&self) -> bool {
        self.eviction_strategy == EvictionStrategy::Dynamic
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

    /// Pop next key for eviction based on strategy. Fifo=front, Lifo=back.
    fn pop_eviction_candidate(&self, q: &mut VecDeque<OutPointKey>) -> Option<OutPointKey> {
        match self.eviction_strategy {
            EvictionStrategy::Fifo => q.pop_front(),
            EvictionStrategy::Lifo => q.pop_back(),
            EvictionStrategy::Dynamic => q.pop_front(), // fallback when no height
        }
    }

    /// Evict entries over the cache limit. Entries in pending_writes are skipped
    /// (not yet on disk). When pending_writes blocks eviction, the MemoryGuard in
    /// the validation loop detects rising RSS and forces a flush.
    /// Dynamic strategy: use evict_if_needed(height) from validation loop instead.
    pub(crate) fn maybe_evict(&self) {
        if self.max_entries == usize::MAX {
            return;
        }
        if self.eviction_strategy == EvictionStrategy::Dynamic {
            // Dynamic eviction needs height; validation loop calls evict_if_needed.
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
            let Some(key) = self.pop_eviction_candidate(&mut q) else {
                break;
            };
            if pending.contains_key(&key) {
                q.push_back(key);
                continue;
            }
            if self.cache.remove(&key).is_some() {
                evicted += 1;
                self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        if evicted > 0 {
            debug!(
                "IbdUtxoStore: evicted {} entries (cache was over limit)",
                evicted
            );
        }
    }

    /// Protect keys needed by upcoming blocks from eviction. Call before evict_if_needed when dynamic.
    pub fn protect_keys_for_next_blocks(&self, keys: &[OutPointKey]) {
        if self.eviction_strategy != EvictionStrategy::Dynamic {
            return;
        }
        if let Ok(mut recent) = self.recently_accessed.lock() {
            for key in keys {
                if self.cache.contains_key(key) {
                    recent.insert(*key);
                }
            }
        }
    }

    /// Evict when over limit, using age/dust heuristics. Only for Dynamic strategy.
    /// Call from validation loop after protect_keys_for_next_blocks. For fifo/lifo, maybe_evict handles it.
    pub fn evict_if_needed(&self, current_height: u64) -> usize {
        if self.eviction_strategy != EvictionStrategy::Dynamic {
            return 0;
        }
        if self.max_entries == usize::MAX {
            return 0;
        }
        let len = self.cache.len();
        let trigger = self.max_entries + self.max_entries / 10;
        if len <= trigger {
            return 0;
        }
        let target = self.max_entries * 9 / 10;
        let to_evict = len.saturating_sub(target);
        if to_evict == 0 {
            return 0;
        }
        let min_evictable_height = current_height.saturating_sub(EVICT_MIN_AGE_BLOCKS);
        let very_old_threshold = current_height.saturating_sub(EVICT_VERY_OLD_BLOCKS);
        let mut evicted = 0;
        let pending = self.pending_writes.lock().expect("lock");
        let mut q = self.eviction_queue.lock().expect("lock");
        let mut recent = self.recently_accessed.lock().expect("lock");
        let max_candidates = (to_evict * 4).min(q.len());
        let mut candidates: Vec<(OutPointKey, i64, u64)> = Vec::with_capacity(max_candidates);
        for _ in 0..max_candidates {
            let Some(key) = q.pop_front() else {
                break;
            };
            if recent.contains(&key) {
                q.push_back(key);
                continue;
            }
            if pending.contains_key(&key) {
                q.push_back(key);
                continue;
            }
            if let Some(ref r) = self.cache.get(&key) {
                if let Some(arc) = r.as_ref() {
                    let utxo = arc.as_ref();
                    if utxo.height > min_evictable_height {
                        q.push_back(key);
                        continue;
                    }
                    candidates.push((key, utxo.value, utxo.height));
                }
            }
        }
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
            if self.cache.remove(&key).is_some() {
                evicted += 1;
                self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        for (key, _, _) in candidates {
            q.push_back(key);
        }
        recent.clear();
        if evicted > 0 {
            debug!(
                "IbdUtxoStore: evicted {} entries (dynamic, cache was over limit)",
                evicted
            );
        }
        evicted
    }

    /// Evict aggressively when under RSS pressure. Evict 50% of current cache to free memory.
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
            let Some(key) = self.pop_eviction_candidate(&mut q) else {
                break;
            };
            if pending.contains_key(&key) {
                q.push_back(key);
                continue;
            }
            if self.cache.remove(&key).is_some() {
                evicted += 1;
                self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        if evicted > 0 {
            debug!(
                "IbdUtxoStore: evicted {} of {} entries for RSS (target {})",
                evicted, len, target
            );
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
        let r = self.cache.get(key);
        if let Some(ref v) = r {
            if v.is_some() {
                self.stats_cache_hits.fetch_add(1, Ordering::Relaxed);
            }
        }
        r.and_then(|r| r.as_ref().map(|a| (**a).clone()))
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

    /// Direct cache lookup for single-pass prefetch.
    #[inline]
    pub fn cache_get(
        &self,
        key: &OutPointKey,
    ) -> Option<dashmap::mapref::one::Ref<'_, OutPointKey, PendingValue>> {
        self.cache.get(key)
    }

    /// Insert loaded UTXO into cache and track for eviction. Used by single-pass prefetch.
    #[inline]
    pub fn cache_insert_and_track(&self, key: OutPointKey, arc: Arc<UTXO>) {
        self.cache.insert(key, Some(arc));
        self.push_eviction_key(key);
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
                    self.stats_cache_hits.fetch_add(1, Ordering::Relaxed);
                    map.insert(op, Arc::clone(arc));
                    continue;
                }
            }
            cache_misses_buf.push(*key);
        }
        if !cache_misses_buf.is_empty() && !self.memory_only {
            let to_load = std::mem::take(cache_misses_buf);
            let load_count = to_load.len();
            if let Ok(loaded) = load_keys_from_disk(Arc::clone(&self.disk), to_load) {
                self.stats_disk_loads
                    .fetch_add(load_count as u64, Ordering::Relaxed);
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
                if self.eviction_strategy == EvictionStrategy::Dynamic {
                    if let Ok(mut recent) = self.recently_accessed.lock() {
                        recent.insert(*key);
                    }
                }
            }
        }
        self.maybe_evict();
    }

    /// Apply UtxoDelta directly — skips intermediate SyncBatch Vec allocations.
    pub fn apply_utxo_delta(&self, delta: &blvm_consensus::block::UtxoDelta) {
        let total_delta = delta.additions.len() as isize - delta.deletions.len() as isize;
        self.total_utxo_count
            .fetch_add(total_delta, Ordering::Relaxed);
        {
            let mut pending = self.pending_writes.lock().expect("lock");
            for op in &delta.deletions {
                let key = outpoint_to_key(op);
                self.remove(&key);
                pending.insert(key, None);
            }
            for (op, arc) in &delta.additions {
                let key = outpoint_to_key(op);
                self.cache.insert(key, Some(Arc::clone(arc)));
                pending.insert(key, Some(Arc::clone(arc)));
                self.push_eviction_key(key);
                if self.eviction_strategy == EvictionStrategy::Dynamic {
                    if let Ok(mut recent) = self.recently_accessed.lock() {
                        recent.insert(key);
                    }
                }
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
    pub fn flush_pending_batch(
        &self,
        batch: &FxHashMap<OutPointKey, PendingValue>,
    ) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let mut total_flushed = 0;
        let mut iter = batch.iter();
        loop {
            let chunk: Vec<_> = iter
                .by_ref()
                .take(MAX_BATCH_OPS)
                .map(|(k, v)| (*k, v.clone()))
                .collect();
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
                debug!(
                    "IbdUtxoStore: evicted {} flushed entries (cache over limit)",
                    evicted
                );
            }
        }
        Ok(total_flushed)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Build full UtxoSet for IBD snapshot dump. Used when BLVM_IBD_SNAPSHOT_DIR is set.
    pub fn to_utxo_set_snapshot(&self) -> UtxoSet {
        self.cache
            .iter()
            .filter_map(|r| {
                let (key, val) = (r.key(), r.value());
                val.as_ref()
                    .map(|arc| (key_to_outpoint(key), Arc::clone(arc)))
            })
            .collect()
    }

    pub fn total_count(&self) -> isize {
        self.total_utxo_count.load(Ordering::Relaxed)
    }

    pub fn disk_clone(&self) -> Arc<dyn Tree> {
        Arc::clone(&self.disk)
    }

    /// Stats for logging. (disk_loads, cache_hits, evictions, pending_hits)
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        (
            self.stats_disk_loads.load(Ordering::Relaxed),
            self.stats_cache_hits.load(Ordering::Relaxed),
            self.stats_evictions.load(Ordering::Relaxed),
            self.stats_pending_hits.load(Ordering::Relaxed),
        )
    }
}

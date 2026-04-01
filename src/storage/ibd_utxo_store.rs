//! IBD v2 UTXO store: concurrent DashMap, zero lock contention.
//!
//! Replaces RwLock<DiskBackedUtxoSet> for IBD. Prefetch reads via .get();
//! validation writes via .insert/.remove. Flush task drains to disk.
//!
//! **Commit barrier:** `utxo_disk_commit_height` is the maximum block height for which
//! all UTXO mutations through that height are durable. A single serial flush worker
//! applies batches in submission order so parallel disk writes cannot reorder dependent ops.
//!
//! **Eviction:** resident cache entries carry a monotonic `generation` (insert stamp).
//! Eviction scans the map — no per-insert `VecDeque` locks.

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
use std::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tracing::debug;

type OutPointKey = [u8; 40];

#[inline]
fn consensus_deletion_key_to_store_key(
    k: &blvm_consensus::utxo_overlay::UtxoDeletionKey,
) -> OutPointKey {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(&k[..32]);
    let idx = u32::from_be_bytes(k[32..36].try_into().unwrap());
    key[32..40].copy_from_slice(&(idx as u64).to_be_bytes());
    key
}

/// Pending value: Some = UTXO, None = spent (delete on flush).
pub(crate) type PendingValue = Option<Arc<UTXO>>;

/// Deduplicated snapshot for disk flush (sorted by key, one row per outpoint).
pub type PendingFlushBatch = Vec<(OutPointKey, PendingValue)>;

/// Work item for the IBD UTXO flush worker: ops plus the highest block height they belong to.
#[derive(Clone)]
pub struct PendingFlushPackage {
    pub ops: Arc<PendingFlushBatch>,
    pub max_block_height: u64,
}

/// UTXO rows serialized for disk; committer thread only runs `BatchWriter` + `commit_no_wal`.
pub struct PreparedFlushPackage {
    pub chunks: Arc<Vec<Vec<(OutPointKey, Option<Vec<u8>>)>>>,
    pub max_block_height: u64,
}

/// In-memory cache line: generation orders victims for eviction scans.
#[derive(Clone)]
pub struct UtxoCacheSlot {
    pub generation: u64,
    pub utxo: Arc<UTXO>,
}

type PendingLogEntry = (OutPointKey, PendingValue, u64);

struct PendingState {
    log: Vec<PendingLogEntry>,
    key_set: FxHashSet<OutPointKey>,
}

impl PendingState {
    fn push_op(&mut self, key: OutPointKey, val: PendingValue, block_height: u64) {
        self.log.push((key, val, block_height));
        self.key_set.insert(key);
        const COMPACT_RATIO: usize = 32;
        let cap = self.key_set.len().saturating_mul(COMPACT_RATIO).max(16_384);
        if self.log.len() > cap {
            let mut raw = std::mem::take(&mut self.log);
            dedupe_pending_triples_in_place(&mut raw);
            self.key_set.clear();
            for (k, v, h) in raw {
                self.key_set.insert(k);
                self.log.push((k, v, h));
            }
        }
    }

    fn len_keys(&self) -> usize {
        self.key_set.len()
    }

    fn take_for_flush(&mut self) -> Vec<PendingLogEntry> {
        self.key_set.clear();
        std::mem::take(&mut self.log)
    }

    fn is_empty(&self) -> bool {
        self.log.is_empty()
    }
}

/// Sort by (key, height); last row per key wins (highest height = most recent op).
/// Uses in-place compaction to avoid a second `Vec` allocation for the compacted log.
fn dedupe_pending_triples_in_place(v: &mut Vec<PendingLogEntry>) {
    if v.len() <= 1 {
        return;
    }
    v.sort_unstable_by_key(|(k, _, h)| (*k, *h));
    let mut write = 0usize;
    let mut i = 0usize;
    while i < v.len() {
        let key = v[i].0;
        let mut j = i + 1;
        while j < v.len() && v[j].0 == key {
            j += 1;
        }
        let win = j - 1;
        if write != win {
            v.swap(write, win);
        }
        write += 1;
        i = j;
    }
    v.truncate(write);
}

fn pack_flush_package(raw: Vec<PendingLogEntry>) -> Option<PendingFlushPackage> {
    if raw.is_empty() {
        return None;
    }
    let (batch, max_h) = dedupe_to_batch_and_max(raw);
    if batch.is_empty() {
        None
    } else {
        Some(PendingFlushPackage {
            ops: Arc::new(batch),
            max_block_height: max_h,
        })
    }
}

fn dedupe_to_batch_and_max(mut v: Vec<PendingLogEntry>) -> (PendingFlushBatch, u64) {
    if v.is_empty() {
        return (Vec::new(), 0);
    }
    dedupe_pending_triples_in_place(&mut v);
    let mut max_h = 0u64;
    let mut batch = Vec::with_capacity(v.len());
    for (k, val, h) in v {
        max_h = max_h.max(h);
        batch.push((k, val));
    }
    batch.sort_unstable_by_key(|(k, _)| *k);
    (batch, max_h)
}

#[cfg(feature = "production")]
impl PendingFlushPackage {
    /// Encode UTXO inserts for the flush worker (disk I/O runs on the committer thread only).
    pub fn prepare_for_disk(&self) -> Result<PreparedFlushPackage> {
        let mut rows: Vec<(OutPointKey, Option<Vec<u8>>)> = Vec::with_capacity(self.ops.len());
        for (key, value_opt) in self.ops.iter() {
            let encoded = match value_opt {
                Some(arc) => {
                    let mut v = Vec::with_capacity(192);
                    bincode::serialize_into(&mut v, arc.as_ref())
                        .map_err(|e| anyhow::anyhow!("UTXO serialize: {}", e))?;
                    Some(v)
                }
                None => None,
            };
            rows.push((*key, encoded));
        }
        let chunks: Vec<Vec<_>> = rows
            .chunks(MAX_BATCH_OPS)
            .map(|c| c.to_vec())
            .collect();
        Ok(PreparedFlushPackage {
            chunks: Arc::new(chunks),
            max_block_height: self.max_block_height,
        })
    }
}

/// Eviction strategy. BLVM_IBD_EVICTION: "dynamic" | "fifo" | "lifo" (default: fifo).
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "production")]
pub enum EvictionStrategy {
    /// Age/dust heuristics: prefer dust, very old (height < current - 10k), then old.
    Dynamic,
    /// Evict lowest insert-generation first (monotonic stamp per cache resident).
    Fifo,
    /// Evict highest insert-generation first.
    Lifo,
}

#[cfg(feature = "production")]
impl EvictionStrategy {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "dynamic" => Self::Dynamic,
            "lifo" => Self::Lifo,
            _ => Self::Fifo,
        }
    }

    fn from_env() -> Self {
        let s = std::env::var("BLVM_IBD_EVICTION").unwrap_or_default();
        Self::from_str(&s)
    }
}

const EVICT_MIN_AGE_BLOCKS: u64 = 100;
const EVICT_VERY_OLD_BLOCKS: u64 = 10_000;
const EVICT_SCAN_CAP: usize = 65_536;

/// IBD v2 concurrent UTXO store. No RwLock on the hot map.
#[cfg(feature = "production")]
pub struct IbdUtxoStore {
    cache: DashMap<OutPointKey, UtxoCacheSlot>,
    disk: Arc<dyn Tree>,
    total_utxo_count: AtomicIsize,
    flush_threshold: usize,
    pending_state: Mutex<PendingState>,
    memory_only: bool,
    /// Effective UTXO cache entry cap (may be tuned down under memory pressure during IBD).
    max_entries_cap: AtomicUsize,
    eviction_strategy: EvictionStrategy,
    recently_accessed: Mutex<FxHashSet<OutPointKey>>,
    /// Monotonically assigned per resident insert / recache (eviction sort key).
    cache_generation: AtomicU64,
    /// Highest block height whose UTXO mutations are fully on disk (flush worker updates).
    utxo_disk_commit_height: AtomicU64,
    /// Wakes validation threads blocked in `wait_utxo_disk_through` when `note_utxo_flush_completed` runs.
    utxo_barrier_mu: Mutex<()>,
    utxo_barrier_cv: Condvar,
    /// UTXOs that have been taken from the pending log and sent to the flush worker but not yet
    /// confirmed on disk. These must be protected from cache eviction and are a valid fallback
    /// for supplement lookups. Keyed by OutPointKey; value is the UTXO (insertion only).
    /// Race window: after take_for_flush clears pending.key_set and before flush_pending_batch
    /// confirms the write. Without this, maybe_evict can evict in-flight entries from cache,
    /// making them invisible to supplement (not in cache, not on disk yet).
    in_flight_insertions: Mutex<FxHashMap<OutPointKey, Arc<UTXO>>>,
    stats_disk_loads: AtomicU64,
    stats_cache_hits: AtomicU64,
    stats_evictions: AtomicU64,
    stats_pending_hits: AtomicU64,
}

#[cfg(feature = "production")]
impl IbdUtxoStore {
    pub fn new(disk: Arc<dyn Tree>, flush_threshold: usize) -> Self {
        Self::new_with_options(
            disk,
            flush_threshold,
            false,
            usize::MAX,
            EvictionStrategy::from_env(),
            0,
        )
    }

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
        Self::new_with_options(
            Arc::new(NullTree),
            usize::MAX,
            true,
            usize::MAX,
            EvictionStrategy::from_env(),
            0,
        )
    }

    #[inline]
    pub fn memory_only(&self) -> bool {
        self.memory_only
    }

    /// `utxo_disk_commit_through`: resume baseline — UTXOs on disk through this height (chain watermark).
    pub fn new_with_options(
        disk: Arc<dyn Tree>,
        flush_threshold: usize,
        memory_only: bool,
        max_entries: usize,
        eviction_strategy: EvictionStrategy,
        utxo_disk_commit_through: u64,
    ) -> Self {
        Self {
            cache: DashMap::with_shard_amount(128),
            disk,
            total_utxo_count: AtomicIsize::new(0),
            flush_threshold,
            pending_state: Mutex::new(PendingState {
                log: Vec::new(),
                key_set: FxHashSet::default(),
            }),
            memory_only,
            max_entries_cap: AtomicUsize::new(max_entries),
            eviction_strategy,
            recently_accessed: Mutex::new(FxHashSet::default()),
            cache_generation: AtomicU64::new(1),
            utxo_disk_commit_height: AtomicU64::new(utxo_disk_commit_through),
            utxo_barrier_mu: Mutex::new(()),
            utxo_barrier_cv: Condvar::new(),
            in_flight_insertions: Mutex::new(FxHashMap::default()),
            stats_disk_loads: AtomicU64::new(0),
            stats_cache_hits: AtomicU64::new(0),
            stats_evictions: AtomicU64::new(0),
            stats_pending_hits: AtomicU64::new(0),
        }
    }

    #[inline]
    fn max_entries_effective(&self) -> usize {
        self.max_entries_cap.load(Ordering::Relaxed)
    }

    /// Shrink or grow the in-memory UTXO cache cap while IBD runs (pressure-driven).
    /// No-op when eviction is disabled (`usize::MAX`) or store is memory-only test stub.
    pub fn tune_max_entries_for_pressure(&self, new_cap: usize, current_height: u64) {
        if self.memory_only {
            return;
        }
        let old = self.max_entries_cap.load(Ordering::Relaxed);
        if old == usize::MAX {
            return;
        }
        let new_cap = new_cap.max(4_096);
        if new_cap == old {
            return;
        }
        self.max_entries_cap.store(new_cap, Ordering::Relaxed);
        if new_cap < old {
            if self.eviction_strategy == EvictionStrategy::Dynamic {
                self.evict_if_needed(current_height);
            }
            self.maybe_evict();
        }
    }

    #[inline]
    fn next_cache_generation(&self) -> u64 {
        self.cache_generation.fetch_add(1, Ordering::Relaxed)
    }

    #[inline]
    fn cache_put(&self, key: OutPointKey, utxo: Arc<UTXO>) {
        let gen = self.next_cache_generation();
        self.cache.insert(key, UtxoCacheSlot {
            generation: gen,
            utxo,
        });
    }

    /// Called by the dedicated flush worker after a successful `flush_pending_batch`.
    pub fn note_utxo_flush_completed(&self, max_block_height: u64) {
        self.utxo_disk_commit_height
            .fetch_max(max_block_height, Ordering::Release);
        let _held = self.utxo_barrier_mu.lock().expect("utxo barrier mu");
        self.utxo_barrier_cv.notify_all();
    }

    #[inline]
    pub fn utxo_disk_commit_height_snapshot(&self) -> u64 {
        self.utxo_disk_commit_height.load(Ordering::Acquire)
    }

    /// Block until UTXO rows through `min_height` are durable (monotonic barrier).
    pub fn wait_utxo_disk_through(&self, min_height: u64) {
        let mut guard = self.utxo_barrier_mu.lock().expect("utxo barrier mu");
        while self.utxo_disk_commit_height.load(Ordering::Acquire) < min_height {
            guard = self
                .utxo_barrier_cv
                .wait(guard)
                .expect("utxo barrier cv");
        }
    }

    #[inline]
    pub fn is_dynamic_eviction(&self) -> bool {
        self.eviction_strategy == EvictionStrategy::Dynamic
    }

    #[inline]
    /// When true, UTXOs loaded from disk for this supplement are not re-inserted into the cache.
    /// Trigger only when nearly full (≥98%) so 95–97% still recaches disk hits and avoids repeat reads.
    pub(crate) fn skip_recache_disk_hits(&self) -> bool {
        self.max_entries_effective() != usize::MAX
            && self.cache.len().saturating_mul(100)
                >= self.max_entries_effective().saturating_mul(98)
    }

    pub fn pending_len(&self) -> usize {
        self.pending_state
            .lock()
            .map(|p| p.len_keys())
            .unwrap_or(0)
    }

    fn eviction_scan_cap(&self, to_evict: usize) -> usize {
        let hint = to_evict.saturating_mul(8).max(1024);
        hint.min(EVICT_SCAN_CAP).min(self.cache.len().saturating_add(1))
    }

    pub(crate) fn maybe_evict(&self) {
        if self.max_entries_effective() == usize::MAX {
            return;
        }
        if self.eviction_strategy == EvictionStrategy::Dynamic {
            return;
        }
        let len = self.cache.len();
        if len <= self.max_entries_effective() {
            return;
        }
        let to_evict = len - self.max_entries_effective();
        let pending = self.pending_state.lock().expect("lock");
        let in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
        let scan_cap = self.eviction_scan_cap(to_evict);
        let mut cand: Vec<(OutPointKey, u64)> = Vec::new();
        for r in self.cache.iter() {
            if cand.len() >= scan_cap {
                break;
            }
            let k = *r.key();
            if pending.key_set.contains(&k) {
                continue;
            }
            // Protect in-flight: taken from pending but not yet confirmed on disk.
            // Evicting these causes supplement to miss them (cache miss + disk miss).
            if in_flight.contains_key(&k) {
                continue;
            }
            cand.push((k, r.value().generation));
        }
        drop(in_flight);
        drop(pending);
        if self.eviction_strategy == EvictionStrategy::Lifo {
            cand.sort_by_key(|(_, g)| std::cmp::Reverse(*g));
        } else {
            cand.sort_by_key(|(_, g)| *g);
        }
        let mut evicted = 0;
        for (key, _) in cand {
            if evicted >= to_evict {
                break;
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

    pub fn evict_if_needed(&self, current_height: u64) -> usize {
        if self.eviction_strategy != EvictionStrategy::Dynamic {
            return 0;
        }
        if self.max_entries_effective() == usize::MAX {
            return 0;
        }
        let len = self.cache.len();
        let trigger = self.max_entries_effective() + self.max_entries_effective() / 10;
        if len <= trigger {
            return 0;
        }
        let target = self.max_entries_effective() * 9 / 10;
        let to_evict = len.saturating_sub(target);
        if to_evict == 0 {
            return 0;
        }
        let min_evictable_height = current_height.saturating_sub(EVICT_MIN_AGE_BLOCKS);
        let very_old_threshold = current_height.saturating_sub(EVICT_VERY_OLD_BLOCKS);
        let pending = self.pending_state.lock().expect("lock");
        let in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
        let mut recent = self.recently_accessed.lock().expect("lock");
        let scan_cap = self.eviction_scan_cap(to_evict.saturating_mul(4));
        let mut candidates: Vec<(OutPointKey, i64, u64)> = Vec::new();
        for r in self.cache.iter() {
            if candidates.len() >= scan_cap {
                break;
            }
            let k = *r.key();
            if recent.contains(&k) {
                continue;
            }
            if pending.key_set.contains(&k) {
                continue;
            }
            if in_flight.contains_key(&k) {
                continue;
            }
            let utxo = r.value().utxo.as_ref();
            if utxo.height > min_evictable_height {
                continue;
            }
            candidates.push((k, utxo.value, utxo.height));
        }
        drop(in_flight);
        drop(pending);
        candidates.sort_by(|a, b| {
            let very_old_a = a.2 < very_old_threshold;
            let very_old_b = b.2 < very_old_threshold;
            match (very_old_a, very_old_b) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => (a.1, a.2).cmp(&(b.1, b.2)),
            }
        });
        let mut evicted = 0;
        for (key, _, _) in candidates.into_iter().take(to_evict) {
            if self.cache.remove(&key).is_some() {
                evicted += 1;
                self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            }
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

    pub(crate) fn evict_aggressive_for_rss(&self) {
        let len = self.cache.len();
        if len == 0 {
            return;
        }
        // Under Emergency, keep only 1/8 of max_entries. The old 50% target with a 64K scan
        // cap barely removed ~65K entries from ~840K — nowhere near enough to recover.
        let keep = self.max_entries_effective() / 8;
        let to_evict = len.saturating_sub(keep);
        if to_evict == 0 {
            return;
        }
        let pending = self.pending_state.lock().expect("lock");
        let in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
        // No scan cap — iterate the full cache. This is O(n) but Emergency is rare
        // and avoiding OOM is more important than avoiding a brief scan pause.
        let mut cand: Vec<(OutPointKey, u64)> = Vec::with_capacity(len);
        for r in self.cache.iter() {
            let k = *r.key();
            if pending.key_set.contains(&k) || in_flight.contains_key(&k) {
                continue;
            }
            cand.push((k, r.value().generation));
        }
        drop(in_flight);
        drop(pending);
        cand.sort_unstable_by_key(|(_, g)| *g);
        let mut evicted = 0;
        for (key, _) in cand {
            if evicted >= to_evict {
                break;
            }
            if self.cache.remove(&key).is_some() {
                evicted += 1;
                self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        if evicted > 0 {
            tracing::warn!(
                "IbdUtxoStore: EMERGENCY evict {} of {} entries (keep {})",
                evicted, len, keep
            );
        }
    }

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
            self.cache_put(key, Arc::new(utxo));
            self.total_utxo_count.fetch_add(1, Ordering::Relaxed);
            self.maybe_evict();
        }
    }

    #[inline]
    pub fn get(&self, key: &OutPointKey) -> Option<UTXO> {
        let r = self.cache.get(key);
        if r.is_some() {
            self.stats_cache_hits.fetch_add(1, Ordering::Relaxed);
        }
        r.map(|r| (*r.utxo).clone())
    }

    #[inline]
    pub fn insert(&self, key: OutPointKey, utxo: UTXO) {
        self.cache_put(key, Arc::new(utxo));
        self.maybe_evict();
    }

    #[inline]
    pub fn remove(&self, key: &OutPointKey) {
        self.cache.remove(key);
    }

    #[inline]
    pub fn cache_get(
        &self,
        key: &OutPointKey,
    ) -> Option<dashmap::mapref::one::Ref<'_, OutPointKey, UtxoCacheSlot>> {
        self.cache.get(key)
    }

    #[inline]
    pub fn cache_insert_and_track(&self, key: OutPointKey, arc: Arc<UTXO>) {
        self.cache_put(key, arc);
        self.maybe_evict();
    }

    pub fn cache_insert_and_track_batch(&self, pairs: &[(OutPointKey, Arc<UTXO>)]) {
        if pairs.is_empty() {
            return;
        }
        for &(key, ref arc) in pairs {
            self.cache_put(key, Arc::clone(arc));
        }
        self.maybe_evict();
    }

    pub fn build_utxo_map(&self, keys: &[OutPointKey]) -> UtxoSet {
        let mut map = UtxoSet::default();
        let mut buf = Vec::new();
        self.supplement_utxo_map_with_buf(&mut map, keys, &mut buf);
        map
    }

    #[inline]
    pub fn build_utxo_map_into(&self, keys: &[OutPointKey], map: &mut UtxoSet) {
        map.clear();
        let mut buf = Vec::new();
        self.supplement_utxo_map_with_buf(map, keys, &mut buf);
    }

    pub fn build_utxo_map_into_with_buf(
        &self,
        keys: &[OutPointKey],
        map: &mut UtxoSet,
        cache_misses_buf: &mut Vec<OutPointKey>,
    ) {
        map.clear();
        self.supplement_utxo_map_with_buf(map, keys, cache_misses_buf);
    }

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
                self.stats_cache_hits.fetch_add(1, Ordering::Relaxed);
                map.insert(op, Arc::clone(&r.utxo));
                continue;
            }
            cache_misses_buf.push(*key);
        }
        if !cache_misses_buf.is_empty() && !self.memory_only {
            let need_inflight_scan = self.max_entries_effective() != usize::MAX
                && {
                    let inflight = self.in_flight_insertions.lock().expect("in_flight lock");
                    !inflight.is_empty()
                };
            let to_load = std::mem::take(cache_misses_buf);
            let load_count = to_load.len();
            if let Ok((loaded, keys_scanned)) =
                load_keys_from_disk(Arc::clone(&self.disk), to_load)
            {
                self.stats_disk_loads
                    .fetch_add(load_count as u64, Ordering::Relaxed);
                let skip_recache = self.skip_recache_disk_hits();
                if skip_recache {
                    for (key, utxo) in loaded {
                        let arc = Arc::new(utxo);
                        map.insert(key_to_outpoint(&key), Arc::clone(&arc));
                    }
                } else {
                    let mut pairs: Vec<(OutPointKey, Arc<UTXO>)> =
                        Vec::with_capacity(loaded.len());
                    for (key, utxo) in loaded {
                        let arc = Arc::new(utxo);
                        map.insert(key_to_outpoint(&key), Arc::clone(&arc));
                        pairs.push((key, arc));
                    }
                    if !pairs.is_empty() {
                        self.cache_insert_and_track_batch(&pairs);
                    }
                }
                if need_inflight_scan {
                    let in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
                    for key in &keys_scanned {
                        let op = key_to_outpoint(key);
                        if map.contains_key(&op) {
                            continue;
                        }
                        if let Some(arc) = in_flight.get(key) {
                            map.insert(op, Arc::clone(arc));
                            self.stats_pending_hits.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }

    pub fn apply_sync_batch(&self, batch: &SyncBatch, block_height: u64) {
        self.total_utxo_count
            .fetch_add(batch.total_delta, Ordering::Relaxed);
        {
            let mut pending = self.pending_state.lock().expect("lock");
            for key in &batch.deletes {
                self.remove(key);
                pending.push_op(*key, None, block_height);
            }
            for (key, value) in &batch.inserts {
                self.cache_put(*key, Arc::clone(value));
                pending.push_op(*key, Some(Arc::clone(value)), block_height);
                if self.eviction_strategy == EvictionStrategy::Dynamic {
                    if let Ok(mut recent) = self.recently_accessed.lock() {
                        recent.insert(*key);
                    }
                }
            }
        }
        self.maybe_evict();
    }

    pub fn apply_utxo_delta(
        &self,
        mut delta: blvm_consensus::block::UtxoDelta,
        block_height: u64,
    ) {
        let total_delta = delta.additions.len() as isize - delta.deletions.len() as isize;
        self.total_utxo_count
            .fetch_add(total_delta, Ordering::Relaxed);
        {
            let mut pending = self.pending_state.lock().expect("lock");
            for dk in delta.deletions {
                let key = consensus_deletion_key_to_store_key(&dk);
                self.remove(&key);
                pending.push_op(key, None, block_height);
            }
            for (op, arc) in delta.additions.drain() {
                let key = outpoint_to_key(&op);
                self.cache_put(key, Arc::clone(&arc));
                pending.push_op(key, Some(arc), block_height);
                if self.eviction_strategy == EvictionStrategy::Dynamic {
                    if let Ok(mut recent) = self.recently_accessed.lock() {
                        recent.insert(key);
                    }
                }
            }
        }
        self.maybe_evict();
    }

    /// After producing a flush package, register its insertion entries in `in_flight_insertions`
    /// so that eviction and supplement can find them during the disk-write window.
    fn register_in_flight(&self, pkg: &PendingFlushPackage) {
        if self.max_entries_effective() == usize::MAX {
            return; // Eviction disabled; no need to track in-flight.
        }
        let mut in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
        for (key, value_opt) in pkg.ops.iter() {
            if let Some(arc) = value_opt {
                in_flight.insert(*key, Arc::clone(arc));
            }
        }
    }

    pub fn maybe_take_flush_batch(&self) -> Option<PendingFlushPackage> {
        let secondary = if self.max_entries_effective() == usize::MAX {
            usize::MAX
        } else {
            (self.max_entries_effective() * 20 / 100).max(1)
        };
        let mut st = self.pending_state.lock().expect("lock");
        let n = st.len_keys();
        if n < self.flush_threshold && n < secondary {
            return None;
        }
        let raw = st.take_for_flush();
        drop(st);
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    pub fn take_flush_batch_force(&self) -> Option<PendingFlushPackage> {
        let mut st = self.pending_state.lock().expect("lock");
        if st.is_empty() {
            return None;
        }
        let raw = st.take_for_flush();
        drop(st);
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Remaining pending ops after validation stops (for final drain to the flush worker).
    pub fn take_remaining_flush_package(&self) -> Option<PendingFlushPackage> {
        let mut st = self.pending_state.lock().expect("lock");
        if st.is_empty() {
            return None;
        }
        let raw = st.take_for_flush();
        drop(st);
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    pub fn flush_pending_batch(&self, batch: &[(OutPointKey, PendingValue)]) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let mut total_flushed = 0;
        let mut ser_buf = Vec::with_capacity(192);
        for chunk in batch.chunks(MAX_BATCH_OPS) {
            let mut b = self.disk.batch();
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
            b.commit_no_wal()?;
            total_flushed += chunk.len();
        }
        debug!("IbdUtxoStore: flushed {} operations to disk", total_flushed);

        // Remove confirmed insertions from in_flight BEFORE cache eviction.
        // Once on disk, entries are safe to evict from cache (disk is the source of truth).
        if self.max_entries_effective() != usize::MAX {
            let mut in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
            for (key, value_opt) in batch {
                if value_opt.is_some() {
                    in_flight.remove(key);
                }
            }
        }

        if self.max_entries_effective() != usize::MAX && self.cache.len() > self.max_entries_effective() {
            let mut evicted = 0;
            for (key, value_opt) in batch {
                if value_opt.is_some() {
                    if self.cache.remove(key).is_some() {
                        evicted += 1;
                    }
                    if self.cache.len() <= self.max_entries_effective() {
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

    pub fn flush_prepared_package(&self, pkg: &PreparedFlushPackage) -> Result<usize> {
        let mut total_flushed = 0;
        for chunk in pkg.chunks.iter() {
            if chunk.is_empty() {
                continue;
            }
            let mut b = self.disk.batch();
            for (key, value_opt) in chunk {
                match value_opt {
                    Some(bytes) => b.put(key.as_slice(), bytes.as_slice()),
                    None => b.delete(key.as_slice()),
                }
            }
            b.commit_no_wal()?;
            total_flushed += chunk.len();
        }
        if total_flushed == 0 {
            return Ok(0);
        }
        debug!(
            "IbdUtxoStore: flushed {} prepared operations to disk",
            total_flushed
        );

        if self.max_entries_effective() != usize::MAX {
            let mut in_flight = self.in_flight_insertions.lock().expect("in_flight lock");
            for chunk in pkg.chunks.iter() {
                for (key, value_opt) in chunk {
                    if value_opt.is_some() {
                        in_flight.remove(key);
                    }
                }
            }
        }

        if self.max_entries_effective() != usize::MAX && self.cache.len() > self.max_entries_effective()
        {
            let mut evicted = 0;
            'outer: for chunk in pkg.chunks.iter() {
                for (key, value_opt) in chunk {
                    if value_opt.is_some() {
                        if self.cache.remove(key).is_some() {
                            evicted += 1;
                        }
                        if self.cache.len() <= self.max_entries_effective() {
                            break 'outer;
                        }
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

    pub fn to_utxo_set_snapshot(&self) -> UtxoSet {
        self.cache
            .iter()
            .map(|r| {
                let key = r.key();
                let slot = r.value();
                (key_to_outpoint(key), Arc::clone(&slot.utxo))
            })
            .collect()
    }

    pub fn total_count(&self) -> isize {
        self.total_utxo_count.load(Ordering::Relaxed)
    }

    pub fn disk_clone(&self) -> Arc<dyn Tree> {
        Arc::clone(&self.disk)
    }

    /// Force the ibd_utxos column family memtable to flush to SST before the watermark is written.
    ///
    /// UTXO batches are committed with `commit_no_wal` for IBD throughput, which means they live
    /// in the RocksDB memtable until a background flush. If the process is killed between the
    /// `commit_no_wal` and the next background flush, those writes are lost even though the
    /// watermark (written with WAL via chain_info) survives — leaving the DB inconsistent.
    ///
    /// Calling this before `set_utxo_watermark` makes the no-WAL data SST-durable first.
    pub fn flush_disk(&self) -> Result<()> {
        self.disk.flush_to_disk()
    }

    pub fn stats(&self) -> (u64, u64, u64, u64) {
        (
            self.stats_disk_loads.load(Ordering::Relaxed),
            self.stats_cache_hits.load(Ordering::Relaxed),
            self.stats_evictions.load(Ordering::Relaxed),
            self.stats_pending_hits.load(Ordering::Relaxed),
        )
    }
}

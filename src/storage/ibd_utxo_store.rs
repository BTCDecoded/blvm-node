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
use crate::storage::utxo_value_codec::ValueCodec;
use anyhow::Result;
use blvm_muhash::{serialize_coin_for_muhash, MuHash3072};
use blvm_protocol::block::compute_block_tx_ids;
use blvm_protocol::transaction::is_coinbase;
use blvm_protocol::types::{OutPoint, UtxoSet, UTXO};
use dashmap::{DashMap, DashSet};
use hex;
use rustc_hash::{FxHashMap, FxHashSet};
#[cfg(feature = "production")]
use std::str::FromStr;
use std::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use tracing::debug;

/// Per-op MuHash during IBD flush is **off by default**. Each delete previously required a
/// synchronous `disk.get(key)` for correctness vs create+spend folding — a serial disk read
/// per deletion that caps retire throughput on slow disks.
///
/// Default off: MuHash is recomputed in bulk from the committed UTXO set at the IBD watermark
/// (Phase 3 export), where a single sequential scan is far cheaper than per-op disk reads.
///
/// Set `BLVM_IBD_ENABLE_PER_OP_MUHASH=1` to restore running per-op MuHash updates (e.g. for
/// checkpoint consistency debugging). `BLVM_IBD_SKIP_PER_OP_MUHASH=1` is now a no-op (already
/// the default) but kept for backwards compatibility.
fn ibd_per_op_muhash_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // Explicit opt-in overrides the default-off.
        if std::env::var("BLVM_IBD_ENABLE_PER_OP_MUHASH")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return true;
        }
        // Legacy env var: BLVM_IBD_SKIP_PER_OP_MUHASH=0 re-enables (skip=false → enabled=true).
        // BLVM_IBD_SKIP_PER_OP_MUHASH=1 is now the default behaviour, so it's a no-op.
        if let Ok(val) = std::env::var("BLVM_IBD_SKIP_PER_OP_MUHASH") {
            let skip = val == "1" || val.eq_ignore_ascii_case("true");
            return !skip;
        }
        false // default: per-op MuHash disabled during IBD
    })
}

type OutPointKey = [u8; 40];

#[inline]
fn utxo_muhash_preimage_ibd(op: &OutPoint, utxo: &UTXO) -> Vec<u8> {
    serialize_coin_for_muhash(
        &op.hash,
        op.index,
        utxo.height as u32,
        utxo.is_coinbase,
        utxo.value,
        utxo.script_pubkey.as_ref(),
    )
}

#[inline]
fn consensus_deletion_key_to_store_key(
    k: &blvm_protocol::utxo_overlay::UtxoDeletionKey,
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
    /// The set of block heights whose ops are fully included in this batch. The flush worker
    /// calls `IbdUtxoStore::release_protected_heights` with this set after the disk write
    /// completes, removing those heights from `protected_heights` and making their cache
    /// entries eligible for eviction.
    pub heights: Arc<FxHashSet<u32>>,
}

/// UTXO rows serialized for disk; committer thread iterates `rows.chunks(MAX_BATCH_OPS)`.
/// Flat layout (no nested Vec-of-Vecs) eliminates the `c.to_vec()` copy per chunk that
/// was previously allocating ~59 MB extra per 500k-op flush.
///
/// `slab` holds all serialized UTXO bytes packed contiguously. `rows` stores `(key,
/// Some((slab_start, slab_len)))` for adds and `(key, None)` for deletes. This eliminates
/// the per-add `Vec<u8>` clone that previously allocated one ~80-byte heap object per UTXO
/// (250k allocs × 80 B = ~20 MB per 500k-op flush reduced to a single slab).
#[derive(Clone)]
pub struct PreparedFlushPackage {
    pub rows: Arc<Vec<(OutPointKey, Option<(u32, u32)>)>>,
    pub slab: Arc<Vec<u8>>,
    pub max_block_height: u64,
}

/// Controls which operation types are written to RocksDB in a flush call.
/// Used to implement two-phase crash-safe commit: ADDs flushed before watermark,
/// DELs flushed after watermark. See `flush_prepared_package_adds_only` /
/// `flush_prepared_package_dels_only`.
#[derive(Clone, Copy, PartialEq)]
enum FlushFilter {
    All,
    AddsOnly,
    DelsOnly,
}

/// Sentinel `block_height` value for cache entries that require no eviction protection
/// (disk-loaded or genesis entries). Any entry with this height may be freely evicted.
pub(crate) const UNPROTECTED_HEIGHT: u32 = u32::MAX;

/// In-memory cache line: generation orders victims for eviction scans.
/// `block_height` enables height-granular eviction protection: entries whose height is in
/// `IbdUtxoStore::protected_heights` are never evicted. Use `UNPROTECTED_HEIGHT` for
/// entries that do not require protection (disk-loaded, genesis).
#[derive(Clone)]
pub struct UtxoCacheSlot {
    pub generation: u64,
    pub utxo: Arc<UTXO>,
    /// Block height at which this UTXO was created. `UNPROTECTED_HEIGHT` if not protected.
    pub block_height: u32,
}

type PendingLogEntry = (OutPointKey, PendingValue, u64);

/// Number of independent shards over the pending log. Workers route ops by `key[0] & MASK` so
/// N validation workers contend on N different mutexes instead of one. Empirically at h=300k+
/// the single-mutex `PendingState` was the dominant serializer (pending grew to >1.2M entries
/// while workers blocked waiting for the lock); sharding to 16 essentially eliminates that
/// contention because Bitcoin txids are uniformly distributed, so traffic spreads evenly.
///
/// Eviction protection that previously lived in `PendingState.key_set` is now provided by
/// `worker_preinserted` (lock-free DashSet) extended to cover the full worker→pending→flush
/// lifetime, so there is no per-shard key_set anymore.
pub(crate) const PENDING_SHARDS: usize = 16;
const PENDING_SHARD_MASK: usize = PENDING_SHARDS - 1;

#[inline]
pub(crate) fn pending_shard_idx(key: &OutPointKey) -> usize {
    // key[0] is the first byte of a cryptographically uniform txid hash → uniform shard
    // distribution. No need for a separate hash function.
    (key[0] as usize) & PENDING_SHARD_MASK
}

/// Sort by (key, height); last row per key wins (highest height = most recent op).
/// Uses in-place compaction to avoid a second `Vec` allocation for the compacted log.
fn dedupe_pending_triples_in_place(v: &mut Vec<PendingLogEntry>) {
    if v.len() <= 1 {
        return;
    }
    #[cfg(feature = "rayon")]
    {
        use blvm_protocol::rayon::prelude::*;
        v.par_sort_unstable_by_key(|(k, _, h)| (*k, *h));
    }
    #[cfg(not(feature = "rayon"))]
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
    let (batch, max_h, heights) = dedupe_to_batch_and_max(raw);
    // Always return Some even when batch is empty (all ops cancelled by dedup). We still
    // need the heights set so the flush worker can call release_protected_heights — otherwise
    // those heights stay stuck forever, falsely protecting unrelated cache entries.
    Some(PendingFlushPackage {
        ops: Arc::new(batch),
        max_block_height: max_h,
        heights: Arc::new(heights),
    })
}

fn dedupe_to_batch_and_max(
    mut v: Vec<PendingLogEntry>,
) -> (PendingFlushBatch, u64, FxHashSet<u32>) {
    if v.is_empty() {
        return (Vec::new(), 0, FxHashSet::default());
    }
    // Collect ALL heights BEFORE deduplication. If a create at height H is cancelled by a
    // delete in the same batch (net zero), H would disappear from the deduped batch — it
    // would never appear in the heights set and would remain stuck in `protected_heights`
    // forever, falsely protecting unrelated cache entries and blocking eviction.
    let mut all_heights: FxHashSet<u32> = FxHashSet::default();
    for (_, _, h) in v.iter() {
        if *h != 0 {
            all_heights.insert(*h as u32);
        }
    }
    dedupe_pending_triples_in_place(&mut v);
    let mut max_h = 0u64;
    let mut batch = Vec::with_capacity(v.len());
    for (k, val, h) in v {
        max_h = max_h.max(h);
        batch.push((k, val));
    }
    #[cfg(feature = "rayon")]
    {
        use blvm_protocol::rayon::prelude::*;
        batch.par_sort_unstable_by_key(|(k, _)| *k);
    }
    #[cfg(not(feature = "rayon"))]
    batch.sort_unstable_by_key(|(k, _)| *k);
    (batch, max_h, all_heights)
}

#[cfg(feature = "production")]
impl PendingFlushPackage {
    /// Encode UTXO inserts for the flush worker (disk I/O runs on the committer thread only).
    pub fn prepare_for_disk(&self, codec: ValueCodec) -> Result<PreparedFlushPackage> {
        // Single slab: all serialized UTXO bytes packed contiguously. Rows store (start, len)
        // offsets into the slab. Eliminates one Vec<u8> heap allocation per add operation
        // (previously `ser_buf.clone()` = 250k allocs × ~80 B = ~20 MB per 500k-op flush).
        let n_adds = self.ops.iter().filter(|(_, v)| v.is_some()).count();
        let mut slab: Vec<u8> = Vec::with_capacity(n_adds * 100);
        let mut rows: Vec<(OutPointKey, Option<(u32, u32)>)> = Vec::with_capacity(self.ops.len());
        for (key, value_opt) in self.ops.iter() {
            let encoded = match value_opt {
                Some(arc) => {
                    let start = slab.len() as u32;
                    let bytes = crate::storage::utxo_value_codec::encode_utxo_with_codec(
                        codec,
                        arc.as_ref(),
                    )?;
                    slab.extend_from_slice(&bytes);
                    let end = slab.len() as u32;
                    Some((start, end - start))
                }
                None => None,
            };
            rows.push((*key, encoded));
        }
        Ok(PreparedFlushPackage {
            rows: Arc::new(rows),
            slab: Arc::new(slab),
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
impl FromStr for EvictionStrategy {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_lowercase().as_str() {
            "dynamic" => Self::Dynamic,
            "lifo" => Self::Lifo,
            _ => Self::Fifo,
        })
    }
}

#[cfg(feature = "production")]
impl EvictionStrategy {
    fn from_env() -> Self {
        let s = std::env::var("BLVM_IBD_EVICTION").unwrap_or_default();
        s.parse().unwrap_or(Self::Fifo)
    }
}

const EVICT_MIN_AGE_BLOCKS: u64 = 100;
const EVICT_VERY_OLD_BLOCKS: u64 = 10_000;
// Eviction scan cap: limits how many DashMap entries we examine per eviction sweep.
// Lower cap = faster eviction but may miss some old entries (we accept that trade-off;
// eviction correctness only requires removing SOME non-protected entries, not the OLDEST).
// With ~8000 adds/block at h=360k and ~9% protected rate, scanning to_evict*2 = 16k
// entries finds ~14.5k unprotected candidates — enough to evict the 8k we need.
const EVICT_SCAN_CAP: usize = 16_384;

/// IBD v2 concurrent UTXO store. No RwLock on the hot map.
#[cfg(feature = "production")]
pub struct IbdUtxoStore {
    cache: DashMap<OutPointKey, UtxoCacheSlot>,
    disk: Arc<dyn Tree>,
    total_utxo_count: AtomicIsize,
    flush_threshold: usize,
    /// Sharded pending log: workers push ops to one of `PENDING_SHARDS` independent mutexes
    /// chosen by `pending_shard_idx(key)`. Eliminates the single-mutex contention that
    /// dominated retire-thread CPU at h=300k+ (workers were serialized through one lock
    /// while the queue grew to >1M entries). Each shard is a plain `Vec<PendingLogEntry>`;
    /// dedupe runs once at flush time when shard logs are merged.
    pending_shards: Vec<Mutex<Vec<PendingLogEntry>>>,
    /// Approximate total size of `pending_shards`. Read lock-free from `maybe_take_flush_batch_through`
    /// and `pending_len`; writes happen on the `apply_*`/`take_*` paths. Slightly racy with
    /// in-flight pushes, but correctness only requires the flush trigger to fire eventually.
    pending_log_size: AtomicUsize,
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
    /// UTXOs taken from the pending log and sent to the flush worker but not yet confirmed
    /// on disk. Used as a supplement-fallback (cache miss → in_flight → disk) so a
    /// concurrent disk read in the in-flight window can still see the value.
    /// DashMap (sharded, no global lock) so worker threads can insert concurrently without
    /// serialising against each other or the flush path.
    in_flight_insertions: DashMap<OutPointKey, Arc<UTXO>>,
    /// Lock-free DashSet of block heights that are currently protected from cache eviction.
    /// Contains at most `pipeline_depth + max_utxo_flushes_in_flight` entries (~36 u32s)
    /// instead of one entry per UTXO key (which reached 6M entries at h=300k+). A cache entry
    /// is protected iff `slot.block_height != UNPROTECTED_HEIGHT` and
    /// `protected_heights.contains(&slot.block_height)`.
    ///
    /// Lifetime: a height H is inserted by `worker_cache_put_protected` (or the non-worker
    /// `apply_utxo_delta` path) when the first UTXO from H enters the cache. It is removed
    /// by `release_protected_heights` after the flush batch covering H is committed to disk.
    protected_heights: DashSet<u32>,
    stats_disk_loads: AtomicU64,
    stats_cache_hits: AtomicU64,
    stats_evictions: AtomicU64,
    stats_pending_hits: AtomicU64,
    value_codec: ValueCodec,
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
            ValueCodec::Bincode,
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
            fn batch(&self) -> Result<Box<dyn crate::storage::database::BatchWriter + '_>> {
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
                Ok(Box::new(NullBatch))
            }
        }
        Self::new_with_options(
            Arc::new(NullTree),
            usize::MAX,
            true,
            usize::MAX,
            EvictionStrategy::from_env(),
            0,
            ValueCodec::Bincode,
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
        value_codec: ValueCodec,
    ) -> Self {
        Self {
            cache: DashMap::with_shard_amount(128),
            disk,
            total_utxo_count: AtomicIsize::new(0),
            flush_threshold,
            pending_shards: (0..PENDING_SHARDS)
                .map(|_| Mutex::new(Vec::new()))
                .collect(),
            pending_log_size: AtomicUsize::new(0),
            memory_only,
            max_entries_cap: AtomicUsize::new(max_entries),
            eviction_strategy,
            recently_accessed: Mutex::new(FxHashSet::default()),
            cache_generation: AtomicU64::new(1),
            utxo_disk_commit_height: AtomicU64::new(utxo_disk_commit_through),
            utxo_barrier_mu: Mutex::new(()),
            utxo_barrier_cv: Condvar::new(),
            in_flight_insertions: DashMap::default(),
            protected_heights: DashSet::new(),
            stats_disk_loads: AtomicU64::new(0),
            stats_cache_hits: AtomicU64::new(0),
            stats_evictions: AtomicU64::new(0),
            stats_pending_hits: AtomicU64::new(0),
            value_codec,
        }
    }

    #[inline]
    pub fn value_codec(&self) -> ValueCodec {
        self.value_codec
    }

    #[inline]
    fn decode_utxo_bytes(&self, bytes: &[u8]) -> Result<UTXO> {
        // heed3 / rkyv path: use access_utxo + utxo_from_archived directly — skips
        // the rkyv::deserialize trait dispatch overhead and the bincode fallback probe.
        #[cfg(feature = "heed3")]
        if self.value_codec == ValueCodec::Rkyv {
            let archived = crate::storage::rkyv_codec::access_utxo(bytes)?;
            return Ok(crate::storage::rkyv_codec::utxo_from_archived(archived));
        }
        crate::storage::utxo_value_codec::decode_utxo_with_codec(self.value_codec, bytes)
    }

    #[inline]
    fn encode_utxo_bytes(&self, utxo: &UTXO) -> Result<Vec<u8>> {
        crate::storage::utxo_value_codec::encode_utxo_with_codec(self.value_codec, utxo)
    }

    #[inline]
    fn decode_slab_utxo(&self, slab: &[u8], start: u32, len: u32) -> Result<UTXO> {
        self.decode_utxo_bytes(&slab[start as usize..][..len as usize])
    }

    #[inline]
    fn max_entries_effective(&self) -> usize {
        self.max_entries_cap.load(Ordering::Relaxed)
    }

    /// Public read-only view of the current effective entry cap. Used by the retire path to
    /// decide how aggressively to scan for evictions under Emergency pressure.
    #[inline]
    pub fn cache_cap(&self) -> usize {
        self.max_entries_cap.load(Ordering::Relaxed)
    }

    /// Number of block heights currently protected from eviction. Under height-granular
    /// protection this is O(pipeline_depth + flushes_in_flight) ≈ 36 entries, not O(N_utxos).
    #[inline]
    pub fn protected_len(&self) -> usize {
        self.protected_heights.len()
    }

    /// Release eviction protection for a set of heights after their flush batch has been
    /// committed to disk. Called by the flush worker thread after `flush_prepared_package`.
    pub fn release_protected_heights(&self, heights: &FxHashSet<u32>) {
        for &h in heights {
            self.protected_heights.remove(&h);
        }
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
            self.maybe_evict_tl();
            // DashMap's backing HashMap does NOT automatically shrink when entries are removed —
            // it holds capacity for the peak entry count forever. After dropping a large batch
            // of entries via eviction, call shrink_to_fit so the per-shard HashMaps release
            // their excess slot allocations back to the allocator. This is the primary mechanism
            // that lets mimalloc actually return pages to the OS: without it, the HashMap
            // keeps all its bucket slots alive (preventing page decommit) even after all the
            // Arc<UTXO>s in those buckets are dropped.
            // Shrink the backing HashMap allocation when:
            //   (a) the cap dropped by ≥20% in one call (large adaptive cut), OR
            //   (b) the cap dropped by any amount AND the live entry count is already below
            //       70% of new_cap — meaning eviction has genuinely freed entries and the
            //       DashMap backing arrays are now significantly over-allocated.
            //
            // Without (b), successive 10%-per-call adaptive shrinks (the common path under
            // mild-to-moderate pressure) never satisfied the old `new_cap < old * 8/10`
            // condition, so the DashMap kept capacity for the peak entry count indefinitely —
            // wasting 100–200 MB of RSS even after the logical cache shrank substantially.
            let cache_len = self.cache.len();
            let large_cut = new_cap < old * 8 / 10;
            let live_below_cap = new_cap < old && cache_len < new_cap * 7 / 10;
            if large_cut || live_below_cap {
                self.cache.shrink_to_fit();
            }
        }
    }

    #[inline]
    fn next_cache_generation(&self) -> u64 {
        self.cache_generation.fetch_add(1, Ordering::Relaxed)
    }

    #[inline]
    fn cache_put(&self, key: OutPointKey, utxo: Arc<UTXO>, block_height: u32) {
        let gen = self.next_cache_generation();
        self.cache.insert(
            key,
            UtxoCacheSlot {
                generation: gen,
                utxo,
                block_height,
            },
        );
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
            guard = self.utxo_barrier_cv.wait(guard).expect("utxo barrier cv");
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

    /// True when the store has a finite cache limit (i.e. eviction is enabled).
    #[inline]
    pub(crate) fn max_entries_is_bounded(&self) -> bool {
        self.max_entries_effective() != usize::MAX
    }

    /// Check `in_flight_insertions` for any keys in `keys` not already in `map`.
    /// Used by the prefetch path as defence-in-depth against the flush-commit race.
    pub(crate) fn supplement_in_flight_for_keys(
        &self,
        keys: &[OutPointKey],
        map: &mut rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    ) {
        if self.in_flight_insertions.is_empty() {
            return;
        }
        for key in keys {
            if map.contains_key(key) {
                continue;
            }
            if let Some(arc) = self.in_flight_insertions.get(key) {
                map.insert(*key, Arc::clone(arc.value()));
                self.stats_pending_hits.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending_log_size.load(Ordering::Relaxed)
    }

    pub fn recently_accessed_len(&self) -> usize {
        self.recently_accessed.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight_insertions.len()
    }

    fn eviction_scan_cap(&self, to_evict: usize) -> usize {
        // Multiplier 2: at ~9% protected rate scanning 2× gives 91% yield → ~1.82× unprotected
        // candidates, which comfortably covers to_evict. Old multiplier of 8 was 4× wasteful.
        let hint = to_evict.saturating_mul(2).max(512);
        hint.min(EVICT_SCAN_CAP)
            .min(self.cache.len().saturating_add(1))
    }

    /// Bounded-cache eviction sweep (retire hot path).
    ///
    /// Visible to integration tests / benches in `tests/` that mirror IBD timing; not a stable API.
    #[doc(hidden)]
    pub fn maybe_evict(&self, evict_scratch: &mut Vec<(OutPointKey, u64)>) {
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
        let scan_cap = self.eviction_scan_cap(to_evict);
        evict_scratch.clear();
        // Single-set protection: `worker_preinserted` (DashSet, lock-free) covers the entire
        // lifetime worker_cache_put_protected → apply_utxo_delta → flush. This replaces the
        // previous {pending.key_set + in_flight + worker_preinserted} triple-check, which
        // required acquiring two mutexes (pending_state, in_flight_insertions) for every
        // eviction sweep. Lock-free is critical because eviction can fire while many workers
        // are pushing to the (now sharded) pending log concurrently.
        for r in self.cache.iter() {
            if evict_scratch.len() >= scan_cap {
                break;
            }
            let v = r.value();
            if v.block_height != UNPROTECTED_HEIGHT
                && self.protected_heights.contains(&v.block_height)
            {
                continue;
            }
            evict_scratch.push((*r.key(), v.generation));
        }
        if self.eviction_strategy == EvictionStrategy::Lifo {
            evict_scratch.sort_by_key(|(_, g)| std::cmp::Reverse(*g));
        } else {
            evict_scratch.sort_by_key(|(_, g)| *g);
        }
        let pending_now = self.pending_log_size.load(Ordering::Relaxed);
        let mut evicted = 0;
        for (key, _) in evict_scratch.iter() {
            if evicted >= to_evict {
                break;
            }
            if self.cache.remove(key).is_some() {
                evicted += 1;
                self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        if evicted > 0 {
            debug!(
                "IbdUtxoStore: evicted {} entries (cache over limit, pending={})",
                evicted, pending_now
            );
        }
    }

    pub fn protect_keys_for_next_blocks(&self, keys: &[OutPointKey]) {
        if self.eviction_strategy != EvictionStrategy::Dynamic {
            return;
        }
        if let Ok(mut recent) = self.recently_accessed.lock() {
            // Reset every cycle: only protect keys from the CURRENT lookahead window.
            // Old entries (spent UTXOs from thousands of blocks ago) were accumulating
            // forever, consuming gigabytes of heap by h=250k+ (50M entries × 40B ≈ 2 GB).
            // The set only needs to live for one evict_if_needed() call — the very next
            // call after this one — so clearing here is correct and memory-safe.
            recent.clear();
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
            if !self.protected_heights.is_empty() {
                let v = r.value();
                if v.block_height != UNPROTECTED_HEIGHT
                    && self.protected_heights.contains(&v.block_height)
                {
                    continue;
                }
            }
            let utxo = r.value().utxo.as_ref();
            if utxo.height > min_evictable_height {
                continue;
            }
            candidates.push((k, utxo.value, utxo.height));
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

    pub fn evict_aggressive_for_rss(&self) {
        let len = self.cache.len();
        if len == 0 {
            return;
        }
        // Under Emergency, keep only 1/8 of max_entries, but never below the working-set
        // floor. Dropping the cache to near-zero causes every subsequent block's inputs
        // (~5–8k UTXOs at h=700k) to miss and go to RocksDB — which can be slower than
        // the OOM pressure itself. 64k entries (~12 MB) covers several blocks' working set
        // and keeps the pipeline alive while RSS recovers.
        let keep = (self.max_entries_effective() / 8).max(64_000);
        let to_evict = len.saturating_sub(keep);
        if to_evict == 0 {
            return;
        }
        // Streaming eviction via DashMap::retain. This holds shard locks briefly per shard
        // and removes in place, avoiding the previous `Vec<(OutPointKey, u64)>` allocation
        // (~48 B × 6 M ≈ 290 MB transient) and the O(N log N) sort. Eviction order is
        // shard-iteration order rather than generation-order, which is acceptable under
        // Emergency because we're dropping caches that will be lazy-loaded from RocksDB
        // on the next worker miss. An age-bucketed in-memory index can evict in age order with
        // a height-bounded window; here we accept shard order under Emergency.
        let evicted_before = self.stats_evictions.load(Ordering::Relaxed);
        let mut remaining = to_evict;
        self.cache.retain(|_k, v| {
            if remaining == 0 {
                return true;
            }
            if v.block_height != UNPROTECTED_HEIGHT
                && self.protected_heights.contains(&v.block_height)
            {
                return true;
            }
            remaining -= 1;
            self.stats_evictions.fetch_add(1, Ordering::Relaxed);
            false
        });
        let evicted = self.stats_evictions.load(Ordering::Relaxed) - evicted_before;
        if evicted > 0 {
            tracing::warn!(
                "IbdUtxoStore: EMERGENCY evict {} of {} entries (keep {}, protected_heights={})",
                evicted,
                len,
                keep,
                self.protected_heights.len(),
            );
        }
    }

    pub fn bootstrap_genesis(&self, genesis_block: &blvm_protocol::types::Block) {
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
            self.cache_put(key, Arc::new(utxo), UNPROTECTED_HEIGHT);
            self.total_utxo_count.fetch_add(1, Ordering::Relaxed);
            self.maybe_evict_tl();
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
        self.cache_put(key, Arc::new(utxo), UNPROTECTED_HEIGHT);
        self.maybe_evict_tl();
    }

    #[inline]
    pub fn remove(&self, key: &OutPointKey) {
        // With height-granular protection, eviction protection is tracked per height in
        // `protected_heights`, not per key. A height is only released after its flush batch
        // commits. Removing a key from the cache here (create-then-spend within one flush
        // window) is fine: the entry is gone from the cache and won't be evicted. The height
        // protection remains until flush, which is correct — there may be other UTXOs from
        // the same height still in the cache that need protection.
        self.cache.remove(key);
        // Also evict from in_flight_insertions. Eager registration (apply_utxo_delta /
        // apply_sync_batch) inserts every new UTXO into in_flight_insertions as a cache-miss
        // fallback during the pending→flush window. Without this, spent UTXOs accumulate
        // permanently in in_flight_insertions between flushes: each block adds N entries,
        // flush only removes the flushed batch, so by h≈200k on a 16 GiB host the map holds
        // 7.6M stale entries consuming ~760 MB of DashMap overhead alone.
        // Removing here is safe: a spent UTXO will never be looked up again during IBD
        // (Bitcoin consensus prohibits double-spends), so the cache-miss fallback path for
        // this key is no longer needed. The pending DELETE op for this key proceeds normally
        // and the subsequent flush cleanup is a no-op for the already-removed entry.
        if self.max_entries_effective() != usize::MAX {
            self.in_flight_insertions.remove(key);
        }
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
        self.cache_put(key, arc, UNPROTECTED_HEIGHT);
        self.maybe_evict_tl();
    }

    pub fn cache_insert_and_track_batch(&self, pairs: &[(OutPointKey, Arc<UTXO>)]) {
        if pairs.is_empty() {
            return;
        }
        for &(key, ref arc) in pairs {
            self.cache_put(key, Arc::clone(arc), UNPROTECTED_HEIGHT);
        }
        self.maybe_evict_tl();
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

    /// Parallel variant: DashMap cache lookups are issued concurrently across all rayon threads,
    /// then disk misses are loaded in a single batched RocksDB read. At IBD steady-state
    /// (large cache, few misses) the parallel fan-out covers most blocks.
    /// Falls back to serial if the block has ≤ 32 inputs (overhead > gain).
    #[cfg(feature = "production")]
    pub fn build_utxo_map_parallel(
        &self,
        keys: &[OutPointKey],
        map: &mut UtxoSet,
        cache_misses_buf: &mut Vec<OutPointKey>,
    ) {
        use blvm_protocol::rayon::prelude::*;
        const PAR_THRESHOLD: usize = 32;
        if keys.len() <= PAR_THRESHOLD {
            map.clear();
            return self.supplement_utxo_map_with_buf(map, keys, cache_misses_buf);
        }
        // Parallel cache lookup: collect hits and miss keys.
        let (hits, misses): (Vec<_>, Vec<_>) = keys
            .par_iter()
            .map(|key| {
                if let Some(ref r) = self.cache.get(key) {
                    self.stats_cache_hits.fetch_add(1, Ordering::Relaxed);
                    (Some((key_to_outpoint(key), Arc::clone(&r.utxo))), None)
                } else {
                    (None, Some(*key))
                }
            })
            .unzip();
        // Insert cache hits into UtxoSet on one thread (HashMap is not Sync).
        map.clear();
        map.reserve(keys.len());
        for opt in hits.into_iter().flatten() {
            map.insert(opt.0, opt.1);
        }
        // Load misses from disk — same as serial supplement path.
        let disk_keys: Vec<OutPointKey> = misses.into_iter().flatten().collect();
        if !disk_keys.is_empty() {
            // Re-use the buf so the call signature matches the serial variant.
            *cache_misses_buf = disk_keys;
            // Borrow of cache_misses_buf is consumed by the serial supplement path.
            let keys_to_supplement: Vec<OutPointKey> = std::mem::take(cache_misses_buf);
            let dummy_buf = cache_misses_buf; // now empty
            self.supplement_utxo_map_with_buf(map, &keys_to_supplement, dummy_buf);
        }
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
            // PRE-DISK in_flight check: eliminates a race where a flush thread commits ADD(X)
            // to disk and then removes X from in_flight BETWEEN our disk lookup and the
            // post-disk in_flight scan. By checking in_flight FIRST we capture X while it's
            // still pending (before commit), or confirm it was already committed (disk lookup
            // below will then find it). Keys found here are removed from the disk-load list.
            if self.max_entries_effective() != usize::MAX && !self.in_flight_insertions.is_empty() {
                cache_misses_buf.retain(|key| {
                    let op = key_to_outpoint(key);
                    if map.contains_key(&op) {
                        return false;
                    }
                    if let Some(arc) = self.in_flight_insertions.get(key) {
                        map.insert(op, Arc::clone(arc.value()));
                        self.stats_pending_hits.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                    true
                });
            }
            let to_load = std::mem::take(cache_misses_buf);
            let load_count = to_load.len();
            if load_count == 0 {
                return;
            }
            if let Ok((loaded, keys_scanned)) =
                load_keys_from_disk(Arc::clone(&self.disk), to_load, self.value_codec)
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
                    let mut pairs: Vec<(OutPointKey, Arc<UTXO>)> = Vec::with_capacity(loaded.len());
                    for (key, utxo) in loaded {
                        let arc = Arc::new(utxo);
                        map.insert(key_to_outpoint(&key), Arc::clone(&arc));
                        pairs.push((key, arc));
                    }
                    if !pairs.is_empty() {
                        self.cache_insert_and_track_batch(&pairs);
                    }
                }
                // POST-DISK in_flight scan: catches the residual race where a flush committed
                // X to disk DURING the disk load above (so disk missed it), then removed X
                // from in_flight. Rare but necessary for full coverage.
                if self.max_entries_effective() != usize::MAX
                    && !self.in_flight_insertions.is_empty()
                {
                    for key in &keys_scanned {
                        let op = key_to_outpoint(key);
                        if map.contains_key(&op) {
                            continue;
                        }
                        if let Some(arc) = self.in_flight_insertions.get(key) {
                            map.insert(op, Arc::clone(arc.value()));
                            self.stats_pending_hits.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                // Log any keys that are still missing after all lookups: cache miss + disk miss.
                // These are the UTXOs that will cause IBD_MISSING_UTXO. Logging here gives us
                // the state of the store AT THE MOMENT of the miss, not after the fact.
                for key in &keys_scanned {
                    let op = key_to_outpoint(key);
                    if !map.contains_key(&op) {
                        let in_cache = self.cache.get(key).is_some();
                        let in_inflight = self.max_entries_effective() != usize::MAX
                            && self.in_flight_insertions.contains_key(key);
                        tracing::error!(
                            "[UTXO_TOTAL_MISS] key={} in_cache={} in_inflight={} protected_len={} pending_len={} cache_len={}",
                            hex::encode(key),
                            in_cache,
                            in_inflight,
                            self.protected_heights.len(),
                            self.pending_log_size.load(Ordering::Relaxed),
                            self.cache.len(),
                        );
                    }
                }
            }
        }
    }

    /// Convenience wrapper that uses a thread-local scratch buffer.
    /// Only for callers that are NOT on the hot retire path (e.g. `insert`, `cache_insert_and_track`,
    /// `apply_sync_batch`). The retire path uses `maybe_evict_with_scratch` directly.
    pub(crate) fn maybe_evict_tl(&self) {
        thread_local! {
            static TL_EVICT_SCRATCH: std::cell::RefCell<Vec<(OutPointKey, u64)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        TL_EVICT_SCRATCH.with(|cell| {
            self.maybe_evict(&mut cell.borrow_mut());
        });
    }

    /// Workers call this after a successful validation to pre-populate the cache with the block's
    /// output UTXOs. This moves the DashMap insert cost off the serial retire thread and into the
    /// N-way parallel worker pool.
    ///
    /// The block height H is registered in `protected_heights` for the full lifetime
    /// worker → pending → flush. After the flush batch for H commits to disk, the flush worker
    /// calls `release_protected_heights` to remove H from the set, making all entries at H
    /// eligible for eviction. Protection cost: O(pipeline_depth) u32s, not O(N_utxos) keys.
    pub fn worker_cache_put_protected(
        &self,
        additions: &rustc_hash::FxHashMap<blvm_protocol::OutPoint, Arc<UTXO>>,
        height: u64,
    ) {
        if additions.is_empty() {
            return;
        }
        let h = height as u32;
        // Register the height as protected BEFORE inserting into the cache so that eviction
        // scans never observe a cache entry at height H without H being in protected_heights.
        self.protected_heights.insert(h);
        for (op, arc) in additions.iter() {
            let key = outpoint_to_key(op);
            self.cache_put(key, Arc::clone(arc), h);
        }
    }

    pub fn apply_sync_batch(&self, batch: &SyncBatch, block_height: u64) {
        self.total_utxo_count
            .fetch_add(batch.total_delta, Ordering::Relaxed);
        // Apply additions to cache + protect them via height-granular protection (kept until
        // flush confirms disk durability). Then route ops to the sharded pending log.
        for key in &batch.deletes {
            self.remove(key);
        }
        let h = block_height as u32;
        if !batch.inserts.is_empty() {
            self.protected_heights.insert(h);
        }
        for (key, value) in &batch.inserts {
            self.cache_put(*key, Arc::clone(value), h);
            if self.eviction_strategy == EvictionStrategy::Dynamic {
                if let Ok(mut recent) = self.recently_accessed.lock() {
                    recent.insert(*key);
                }
            }
        }
        let total = batch.deletes.len() + batch.inserts.len();
        // Eagerly register inserts into in_flight_insertions (DashMap: no global lock).
        if self.max_entries_effective() != usize::MAX && !batch.inserts.is_empty() {
            for (key, arc) in &batch.inserts {
                self.in_flight_insertions
                    .entry(*key)
                    .or_insert_with(|| Arc::clone(arc));
            }
        }
        self.push_to_pending_shards(
            batch
                .deletes
                .iter()
                .map(|k| (*k, None))
                .chain(batch.inserts.iter().map(|(k, v)| (*k, Some(Arc::clone(v))))),
            block_height,
        );
        // push_to_pending_shards updates the global counter once per call.
        let _ = total;
        self.maybe_evict_tl();
    }

    /// Apply a UTXO delta to the in-memory cache and pending log.
    ///
    /// `del_scratch` and `add_scratch` are caller-owned reusable buffers: the retire thread
    /// owns them across blocks so we avoid two heap allocs per block (~3k dels + ~5k adds at
    /// h=300k+). Both are cleared on entry; callers must not rely on their contents afterwards.
    ///
    /// `additions_already_in_cache` indicates whether the validation worker already pre-inserted
    /// the additions via `worker_cache_put_protected`. When `true` (the IBD production path),
    /// we skip the per-addition `cache.insert`, which is the single largest source of retire-thread
    /// CPU at h=300k+ (~3-8k DashMap writes per block, plus the redundant Arc::clone). Bench/test
    /// callers that don't go through a worker pass `false`.
    pub fn apply_utxo_delta(
        &self,
        delta: &blvm_protocol::block::UtxoDelta,
        block_height: u64,
        del_scratch: &mut Vec<OutPointKey>,
        add_scratch: &mut Vec<(OutPointKey, Arc<UTXO>)>,
        additions_already_in_cache: bool,
    ) {
        let total_delta = delta.additions.len() as isize - delta.deletions.len() as isize;
        self.total_utxo_count
            .fetch_add(total_delta, Ordering::Relaxed);
        let dynamic = self.eviction_strategy == EvictionStrategy::Dynamic;
        // Apply delta to DashMap cache: on the IBD hot path the worker has already
        // populated the cache via `worker_cache_put_protected`, so we only need to remove
        // deletions here. For non-worker callers (benches) we also insert additions and
        // register them via height-granular protection so eviction is consistent.
        del_scratch.clear();
        del_scratch.reserve(delta.deletions.len());
        for dk in &delta.deletions {
            let key = consensus_deletion_key_to_store_key(dk);
            self.remove(&key);
            del_scratch.push(key);
        }
        add_scratch.clear();
        add_scratch.reserve(delta.additions.len());
        if additions_already_in_cache {
            for (op, arc) in delta.additions.iter() {
                let key = outpoint_to_key(op);
                add_scratch.push((key, Arc::clone(arc)));
            }
        } else {
            let h = block_height as u32;
            if !delta.additions.is_empty() {
                self.protected_heights.insert(h);
            }
            for (op, arc) in delta.additions.iter() {
                let key = outpoint_to_key(op);
                self.cache_put(key, Arc::clone(arc), h);
                add_scratch.push((key, Arc::clone(arc)));
            }
        }
        // Route ops to the sharded pending log. Each op goes to its own shard
        // chosen by `pending_shard_idx`, so N parallel workers pushing concurrently rarely
        // contend on the same mutex (they did before, when there was a single global mutex).
        // Eagerly register additions into in_flight_insertions: covers the race where a
        // pending-shard drain takes *part* of height H's ops into the first flush batch,
        // that batch commits and calls release_protected_heights(H), but H's remaining ops
        // are still in pending_shards awaiting the second batch. Between first-batch-commit
        // and second-batch-commit those cache entries are unprotected *and* not yet on disk.
        // in_flight_insertions is now a DashMap (sharded, no global lock) so concurrent
        // workers each take a shard lock — no single bottleneck, no BPS regression.
        if self.max_entries_effective() != usize::MAX && !add_scratch.is_empty() {
            for (key, arc) in add_scratch.iter() {
                self.in_flight_insertions
                    .entry(*key)
                    .or_insert_with(|| Arc::clone(arc));
            }
        }
        self.push_to_pending_shards(
            del_scratch.iter().map(|&k| (k, None)).chain(
                add_scratch
                    .iter()
                    .map(|(k, arc)| (*k, Some(Arc::clone(arc)))),
            ),
            block_height,
        );
        // Batch the recently_accessed updates: previously locked once per addition.
        // At h=300k+ blocks have ~8000 outputs → 8000 mutex acquires per block; the
        // ibd-retire thread spent 96% CPU on these lock churns alone, capping BPS.
        // One lock per delta is O(N) work but ~O(1) lock contention.
        if dynamic {
            if let Ok(mut recent) = self.recently_accessed.lock() {
                recent.reserve(delta.additions.len());
                for op in delta.additions.keys() {
                    let key = outpoint_to_key(op);
                    recent.insert(key);
                }
            }
        }
    }

    /// Push (key, value) pairs to the sharded pending log. Items are bucketed by
    /// `pending_shard_idx(key)` so each worker contends only on its target shards.
    /// `pending_log_size` is updated once at the end with the total count.
    fn push_to_pending_shards<I>(&self, items: I, block_height: u64)
    where
        I: IntoIterator<Item = (OutPointKey, PendingValue)>,
    {
        // Stack-allocated fixed-size array of small Vecs avoids a heap alloc when the bucket
        // count is small (most blocks have <16k ops total → ~1k per shard).
        let mut buckets: [Vec<PendingLogEntry>; PENDING_SHARDS] = Default::default();
        let mut total = 0usize;
        for (key, val) in items {
            let s = pending_shard_idx(&key);
            buckets[s].push((key, val, block_height));
            total += 1;
        }
        if total == 0 {
            return;
        }
        for (i, bucket) in buckets.iter_mut().enumerate() {
            if bucket.is_empty() {
                continue;
            }
            let mut shard = self.pending_shards[i].lock().expect("pending shard lock");
            // append() moves elements; bucket becomes empty afterwards.
            shard.append(bucket);
        }
        self.pending_log_size.fetch_add(total, Ordering::Relaxed);
    }

    /// Drain all pending shards into a single Vec, briefly locking each shard. Used by
    /// shutdown final drain only (`take_remaining_flush_package`).
    fn drain_all_pending_shards(&self) -> Vec<PendingLogEntry> {
        let approx = self.pending_log_size.load(Ordering::Relaxed);
        let mut all = Vec::with_capacity(approx);
        for shard in self.pending_shards.iter() {
            let mut s = shard.lock().expect("pending shard lock");
            if !s.is_empty() {
                all.append(&mut *s);
            }
        }
        let taken = all.len();
        // saturating_sub handles the (rare) case where workers incremented size but hadn't
        // yet appended to the shard when we drained — counter snaps back into sync at the
        // next push.
        let prev = self.pending_log_size.load(Ordering::Relaxed);
        self.pending_log_size
            .store(prev.saturating_sub(taken), Ordering::Relaxed);
        all
    }

    /// Drain only pending ops whose stamped block height is `<= max_block_height_inclusive`.
    ///
    /// **Height cap:** for callers that must not pull ops above a given block (tests, or any
    /// pipeline that is *not* strictly height-ordered before `apply_utxo_delta`).
    ///
    /// **Parallel IBD:** the feeder applies [`Self::apply_utxo_delta`] in **strict ascending
    /// block height** (`OrderedReadyBridge`), so ops are appended to `pending_shards` in consensus
    /// order. Production retire therefore uses [`Self::maybe_take_flush_batch`] /
    /// [`Self::take_flush_batch_force`] (`max_block_height_inclusive = u64::MAX`): draining the
    /// full log is safe and avoids scanning retained “future-height” rows on every tick when
    /// retire lags validation.
    fn drain_pending_through_height(
        &self,
        max_block_height_inclusive: u64,
    ) -> Vec<PendingLogEntry> {
        let approx = self.pending_log_size.load(Ordering::Relaxed);
        // Allocate for the full expected drain size. The old min(65536) cap caused
        // 2–3 Vec reallocations per flush at h=300k+ (320k pending entries × 56B each
        // = ~18 MB re-copy). approx is a slightly-stale atomic counter but always the
        // right order-of-magnitude for the expected drain count.
        let mut all = Vec::with_capacity(approx);
        let mut drained = 0usize;
        // Fast path when validation is far ahead of retire (workers fill shards with
        // entries for heights far above `max_block_height_inclusive`). The previous
        // implementation called `s.drain(..)` to empty the shard and then rebuilt the
        // shard from a fresh `keep: Vec`, which allocated `O(retained × 80 B)` bytes per
        // call. With 8 M pending entries and 99 % retained per drain (because `next_height`
        // ≪ `worker_height`), this reallocated ~700 MB per drain — pinning the retire thread
        // at ~85 % CPU on alloc/copy and starving the actual flush work.
        //
        // `swap_remove` is O(1) per drained entry; iteration is O(shard_len) which is
        // unavoidable (we must inspect every entry's height). No realloc, no second `Vec`.
        // Eviction order is disrupted but `pack_flush_package` re-sorts by key + height
        // anyway, so order doesn't matter to correctness.
        for shard in self.pending_shards.iter() {
            let mut s = shard.lock().expect("pending shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() {
                if s[i].2 <= max_block_height_inclusive {
                    all.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        let prev = self.pending_log_size.load(Ordering::Relaxed);
        self.pending_log_size
            .store(prev.saturating_sub(drained), Ordering::Relaxed);
        all
    }

    /// True iff every shard log is empty AND the global counter is zero. Used by final-drain
    /// paths to skip building an empty package.
    fn all_pending_shards_empty(&self) -> bool {
        if self.pending_log_size.load(Ordering::Relaxed) > 0 {
            return false;
        }
        // Atomic counter can be slightly stale vs. shard contents in the racy window
        // described above; if it claims zero we still trust it for the fast path. A genuinely
        // non-empty shard with zero counter would be a counter underflow bug elsewhere.
        true
    }

    /// After producing a flush package, register its insertion entries in `in_flight_insertions`
    /// so that eviction and supplement can find them during the disk-write window.
    fn register_in_flight(&self, pkg: &PendingFlushPackage) {
        if self.max_entries_effective() == usize::MAX {
            return; // Eviction disabled; no need to track in-flight.
        }
        for (key, value_opt) in pkg.ops.iter() {
            if let Some(arc) = value_opt {
                self.in_flight_insertions
                    .entry(*key)
                    .or_insert_with(|| Arc::clone(arc));
            }
        }
    }

    /// Flush when `pending_log_size` crosses thresholds, draining **all** heights (`u64::MAX`).
    /// Parallel IBD retire uses this: apply order is strict by height, so a full drain is safe.
    pub fn maybe_take_flush_batch(&self) -> Option<PendingFlushPackage> {
        self.maybe_take_flush_batch_through(u64::MAX)
    }

    pub fn maybe_take_flush_batch_through(
        &self,
        max_block_height_inclusive: u64,
    ) -> Option<PendingFlushPackage> {
        let secondary = if self.max_entries_effective() == usize::MAX {
            usize::MAX
        } else {
            (self.max_entries_effective() * 20 / 100).max(1)
        };
        let n = self.pending_log_size.load(Ordering::Relaxed);
        if n < self.flush_threshold && n < secondary {
            return None;
        }
        let raw = self.drain_pending_through_height(max_block_height_inclusive);
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Like [`Self::maybe_take_flush_batch`] but drains only ADD ops, leaving DEL ops in place.
    /// Non-durable async flushes use this instead of `maybe_take_flush_batch` so that spend
    /// tombstones never reach SST ahead of the watermark (see `drain_pending_adds_only`).
    pub fn maybe_take_flush_batch_adds_only(&self) -> Option<PendingFlushPackage> {
        let secondary = if self.max_entries_effective() == usize::MAX {
            usize::MAX
        } else {
            (self.max_entries_effective() * 20 / 100).max(1)
        };
        let n = self.pending_log_size.load(Ordering::Relaxed);
        if n < self.flush_threshold && n < secondary {
            return None;
        }
        let raw = self.drain_pending_adds_only();
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Force-flush all pending ops (`u64::MAX` height bound). See [`Self::maybe_take_flush_batch`].
    pub fn take_flush_batch_force(&self) -> Option<PendingFlushPackage> {
        self.take_flush_batch_force_through(u64::MAX)
    }

    pub fn take_flush_batch_force_through(
        &self,
        max_block_height_inclusive: u64,
    ) -> Option<PendingFlushPackage> {
        if self.all_pending_shards_empty() {
            return None;
        }
        let raw = self.drain_pending_through_height(max_block_height_inclusive);
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Drain only ADD (insert) ops from the pending log, leaving DEL (delete/spend) ops in place.
    ///
    /// Used by non-durable async flushes so that tombstones are never auto-flushed to SST
    /// before the watermark has advanced past their spend height. If we write DEL tombstones
    /// to the memtable without WAL, RocksDB can auto-flush them to SST (when the write buffer
    /// fills) even though the watermark hasn't yet advanced past that block. On crash+resume
    /// RocksDB would return "not found" for a UTXO whose ADD is in an older SST but whose DEL
    /// tombstone is in a newer one — causing UTXO_TOTAL_MISS. Keeping DELs in the pending_log
    /// until the next durable checkpoint (Phase 3) guarantees they are only persisted after
    /// `persist_ibd_utxo_flush_checkpoint` has advanced the watermark past their spend height.
    fn drain_pending_adds_only(&self) -> Vec<PendingLogEntry> {
        let approx = self.pending_log_size.load(Ordering::Relaxed);
        let mut all = Vec::with_capacity(approx);
        let mut drained = 0usize;
        for shard in self.pending_shards.iter() {
            let mut s = shard.lock().expect("pending shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() {
                // PendingValue = Option<Arc<UTXO>>; Some = ADD, None = DEL
                if s[i].1.is_some() {
                    all.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        let prev = self.pending_log_size.load(Ordering::Relaxed);
        self.pending_log_size
            .store(prev.saturating_sub(drained), Ordering::Relaxed);
        all
    }

    /// Force-flush only ADD ops, leaving DEL (spend) ops in the pending log for the next
    /// durable checkpoint's Phase 3. Non-durable async flushes use this to prevent phantom
    /// DEL tombstones from reaching SST files ahead of the watermark.
    pub fn take_flush_batch_adds_only(&self) -> Option<PendingFlushPackage> {
        if self.all_pending_shards_empty() {
            return None;
        }
        let raw = self.drain_pending_adds_only();
        let pkg = pack_flush_package(raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Remaining pending ops after validation stops (for final drain to the flush worker).
    pub fn take_remaining_flush_package(&self) -> Option<PendingFlushPackage> {
        if self.all_pending_shards_empty() {
            return None;
        }
        let raw = self.drain_all_pending_shards();
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
            let mut b = self.disk.batch()?;
            for (key, value_opt) in chunk {
                match value_opt {
                    Some(arc) => {
                        ser_buf.clear();
                        let encoded = self.encode_utxo_bytes(arc.as_ref())?;
                        ser_buf.extend_from_slice(&encoded);
                        b.put(key.as_slice(), ser_buf.as_slice());
                    }
                    None => b.delete(key.as_slice()),
                }
            }
            b.commit_no_wal()?;
            total_flushed += chunk.len();
        }
        debug!("IbdUtxoStore: flushed {} operations to disk", total_flushed);

        // Remove entries from in_flight_insertions. For INSERT ops the UTXO is now on disk so
        // we no longer need the fallback reference. For DELETE ops we eagerly remove any stale
        // INSERT entry that was registered when the UTXO was created: the UTXO is gone from disk
        // and any in-flight reference is now invalid. Without this, every UTXO that is both
        // created and deleted within the same flush window leaks permanently in in_flight_insertions
        // (the ADD is registered eagerly, the DELETE never cleans it up), consuming ~134B/leaked
        // entry and growing to several GB by h=300k+.
        if self.max_entries_effective() != usize::MAX {
            for (key, _value_opt) in batch {
                self.in_flight_insertions.remove(key);
            }
        }

        if self.max_entries_effective() != usize::MAX
            && self.cache.len() > self.max_entries_effective()
        {
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

    /// Compute the MuHash contribution for `pkg` without writing anything to RocksDB.
    ///
    /// Call this in the retire loop (single-threaded, full rayon pool available) *before* spawning
    /// the async commit thread. The async thread then calls `flush_prepared_package` with
    /// `muhash = None` — keeping rayon muhash computations sequential and contention-free.
    pub fn compute_package_muhash(
        &self,
        pkg: &PreparedFlushPackage,
        local_mh: &mut MuHash3072,
    ) -> Result<()> {
        if !ibd_per_op_muhash_enabled() {
            return Ok(());
        }
        let slab = pkg.slab.as_slice();
        let codec = self.value_codec;
        for chunk in pkg.rows.chunks(MAX_BATCH_OPS) {
            if chunk.is_empty() {
                continue;
            }
            #[cfg(feature = "rayon")]
            {
                use blvm_protocol::rayon::prelude::*;
                let disk = &self.disk;
                let chunk_sub_mh = chunk
                    .par_iter()
                    .try_fold(
                        MuHash3072::new,
                        |mut acc, (key, value_opt)| -> anyhow::Result<MuHash3072> {
                            match value_opt {
                                Some((start, len)) => {
                                    let utxo: UTXO =
                                        crate::storage::utxo_value_codec::decode_utxo_with_codec(
                                            codec,
                                            &slab[*start as usize..][..*len as usize],
                                        )?;
                                    let op = key_to_outpoint(key);
                                    let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                                    acc.insert_mut(&pre);
                                }
                                None => {
                                    let Some(disk_bytes) = disk.get(key.as_slice())? else {
                                        return Ok(acc);
                                    };
                                    let utxo: UTXO =
                                        crate::storage::utxo_value_codec::decode_utxo_with_codec(
                                            codec,
                                            &disk_bytes,
                                        )?;
                                    let op = key_to_outpoint(key);
                                    let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                                    acc.remove_mut(&pre);
                                }
                            }
                            Ok(acc)
                        },
                    )
                    .try_reduce(MuHash3072::new, |a, b| Ok(a.multiply(&b)))?;
                let old = std::mem::take(local_mh);
                *local_mh = old.multiply(&chunk_sub_mh);
            }
            #[cfg(not(feature = "rayon"))]
            {
                for (key, value_opt) in chunk {
                    match value_opt {
                        Some((start, len)) => {
                            let utxo: UTXO = self.decode_slab_utxo(slab, *start, *len)?;
                            let op = key_to_outpoint(key);
                            let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                            local_mh.insert_mut(&pre);
                        }
                        None => {
                            let Some(disk_bytes) = self.disk.get(key.as_slice())? else {
                                continue;
                            };
                            let utxo: UTXO = self.decode_utxo_bytes(&disk_bytes)?;
                            let op = key_to_outpoint(key);
                            let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                            local_mh.remove_mut(&pre);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Filter for two-phase crash-safe commit: write only ADDs in phase 1 (before watermark),
    /// write only DELs in phase 2 (after watermark). Both phases use `flush_disk()` afterwards.
    /// The invariant: if we crash after phase 2's `flush_disk()` but before the next checkpoint,
    /// the stale (undeleted) UTXOs are harmless — Bitcoin double-spend prevention guarantees
    /// they will never be referenced again after the watermark has advanced past their spend block.
    pub fn flush_prepared_package(
        &self,
        pkg: &PreparedFlushPackage,
        mut muhash: Option<&mut MuHash3072>,
    ) -> Result<usize> {
        self.flush_prepared_package_filtered(pkg, muhash, FlushFilter::All)
    }

    pub fn flush_prepared_package_adds_only(&self, pkg: &PreparedFlushPackage) -> Result<usize> {
        self.flush_prepared_package_filtered(pkg, None, FlushFilter::AddsOnly)
    }

    pub fn flush_prepared_package_dels_only(&self, pkg: &PreparedFlushPackage) -> Result<usize> {
        self.flush_prepared_package_filtered(pkg, None, FlushFilter::DelsOnly)
    }

    fn flush_prepared_package_filtered(
        &self,
        pkg: &PreparedFlushPackage,
        mut muhash: Option<&mut MuHash3072>,
        filter: FlushFilter,
    ) -> Result<usize> {
        let mut total_flushed = 0;
        let slab = pkg.slab.as_slice();
        let codec = self.value_codec;
        for chunk in pkg.rows.chunks(MAX_BATCH_OPS) {
            if chunk.is_empty() {
                continue;
            }
            if ibd_per_op_muhash_enabled() {
                if let Some(mhref) = muhash.as_mut() {
                    // Parallel MuHash: each rayon task computes a local sub-accumulator for its
                    // row, then `try_reduce` combines them with `multiply` (commutative ⇒ order-
                    // independent). Insert rows deserialise from the in-memory slab (CPU-bound);
                    // delete rows do a single point-read from RocksDB (cheap from block cache).
                    // Using rayon here collapses ~8 seconds of sequential SHA-256 + ChaCha20 +
                    // 3072-bit multiply per 200 k rows into ~0.8 seconds on a 10-core host.
                    #[cfg(feature = "rayon")]
                    {
                        use blvm_protocol::rayon::prelude::*;
                        let disk = &self.disk;
                        let chunk_sub_mh = chunk
                            .par_iter()
                            .try_fold(
                                MuHash3072::new,
                                |mut acc, (key, value_opt)| -> anyhow::Result<MuHash3072> {
                                    match value_opt {
                                        Some((start, len)) => {
                                            let utxo: UTXO =
                                                self.decode_slab_utxo(slab, *start, *len)?;
                                            let op = key_to_outpoint(key);
                                            let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                                            acc.insert_mut(&pre);
                                        }
                                        None => {
                                            // Persisted `ibd_utxos` has no row: net delete. The disk
                                            // batch still removes the key; MuHash must not subtract a
                                            // coin that was never durably inserted.
                                            let Some(disk_bytes) = disk.get(key.as_slice())? else {
                                                return Ok(acc);
                                            };
                                            let utxo: UTXO = self.decode_utxo_bytes(&disk_bytes)?;
                                            let op = key_to_outpoint(key);
                                            let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                                            acc.remove_mut(&pre);
                                        }
                                    }
                                    Ok(acc)
                                },
                            )
                            .try_reduce(MuHash3072::new, |a, b| Ok(a.multiply(&b)))?;
                        let old_mh = std::mem::take(&mut **mhref);
                        **mhref = old_mh.multiply(&chunk_sub_mh);
                    }
                    #[cfg(not(feature = "rayon"))]
                    {
                        // Sequential fallback (non-production builds without rayon).
                        for (key, value_opt) in chunk {
                            match value_opt {
                                Some((start, len)) => {
                                    let utxo: UTXO =
                                        crate::storage::utxo_value_codec::decode_utxo_with_codec(
                                            codec,
                                            &slab[*start as usize..][..*len as usize],
                                        )?;
                                    let op = key_to_outpoint(key);
                                    let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                                    mhref.insert_mut(&pre);
                                }
                                None => {
                                    let Some(disk_bytes) = self.disk.get(key.as_slice())? else {
                                        debug!(
                                            "IbdUtxoStore: MuHash skip delete (no SST row; net-no-op vs durable set), key_prefix={}",
                                            hex::encode(&key[..8])
                                        );
                                        continue;
                                    };
                                    let utxo: UTXO =
                                        crate::storage::utxo_value_codec::decode_utxo_with_codec(
                                            codec,
                                            &disk_bytes,
                                        )?;
                                    let op = key_to_outpoint(key);
                                    let pre = utxo_muhash_preimage_ibd(&op, &utxo);
                                    mhref.remove_mut(&pre);
                                }
                            }
                        }
                    }
                }
            }
            let mut b = self.disk.batch()?;
            let mut ops_in_batch = 0usize;
            for (key, value_opt) in chunk {
                match value_opt {
                    Some((start, len)) => {
                        if filter != FlushFilter::DelsOnly {
                            b.put(key.as_slice(), &slab[*start as usize..][..*len as usize]);
                            ops_in_batch += 1;
                        }
                    }
                    None => {
                        if filter != FlushFilter::AddsOnly {
                            b.delete(key.as_slice());
                            ops_in_batch += 1;
                        }
                    }
                }
            }
            if ops_in_batch > 0 {
                b.commit_no_wal()?;
            }
            total_flushed += ops_in_batch;
        }
        if total_flushed == 0 {
            return Ok(0);
        }
        debug!(
            "IbdUtxoStore: flushed {} prepared operations to disk",
            total_flushed
        );

        // Release in_flight_insertions now that disk has the data. Height-based protection
        // is released by the caller (`push_utxo_flush_from_retire`) via `release_protected_heights`
        // after this function returns — not here — so all cache entries at the flushed heights
        // remain protected until the caller explicitly clears them.
        // For DELETE ops we also clear any stale INSERT that was eagerly registered: the UTXO
        // no longer exists on disk so the in_flight reference (if any) is invalid.
        if self.max_entries_effective() != usize::MAX {
            for (key, _value_opt) in pkg.rows.iter() {
                self.in_flight_insertions.remove(key);
            }
        }

        if self.max_entries_effective() != usize::MAX
            && self.cache.len() > self.max_entries_effective()
        {
            let mut evicted = 0;
            for (key, value_opt) in pkg.rows.iter() {
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

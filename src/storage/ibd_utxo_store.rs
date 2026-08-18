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
    MAX_BATCH_OPS, SyncBatch, key_to_outpoint, load_keys_from_disk, outpoint_to_key,
};
use crate::storage::utxo_value_codec::ValueCodec;
use anyhow::Result;
use blvm_muhash::{MuHash3072, serialize_coin_for_muhash};
use blvm_protocol::block::compute_block_tx_ids;
use blvm_protocol::transaction::is_coinbase;
use blvm_protocol::types::{OutPoint, UTXO, UtxoSet};
use dashmap::{DashMap, DashSet};
use hex;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
#[cfg(feature = "production")]
use std::str::FromStr;
use std::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use tracing::{debug, info, warn};

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
pub(crate) fn ibd_per_op_muhash_enabled() -> bool {
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
    /// The set of block heights whose ADD ops are fully drained from `add_shards` (no
    /// remaining pending ADDs for that height). The flush worker calls
    /// `IbdUtxoStore::release_protected_heights` with this set after the disk write
    /// completes. Partial capped drains must omit heights that still have ADDs pending.
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
// 32 shards: with 24 validation workers each appending to add_shards[key[0] & mask],
// P(collision on same shard) ≈ 24/32 = 0.75 workers per shard vs 24/16 = 1.5 before.
// Halving contention here is free — the shard Vecs are tiny until flush time.
pub(crate) const PENDING_SHARDS: usize = 32;
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
    if v.len() >= 8192 {
        use blvm_protocol::rayon::prelude::*;
        v.par_sort_unstable_by_key(|(k, _, h)| (*k, *h));
    } else {
        v.sort_unstable_by_key(|(k, _, h)| (*k, *h));
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

fn pack_flush_package(
    store: &IbdUtxoStore,
    raw: Vec<PendingLogEntry>,
) -> Option<PendingFlushPackage> {
    if raw.is_empty() {
        return None;
    }
    let (batch, max_h, heights) = dedupe_to_batch_and_max(raw);
    // Only release heights with zero remaining ADDs in `add_shards` (see
    // `pending_add_count_by_height`). Partial capped drains must not unprotect a height
    // while sibling ADDs from the same block are still pending.
    let releasable = store.filter_heights_fully_pending_drained(heights);
    Some(PendingFlushPackage {
        ops: Arc::new(batch),
        max_block_height: max_h,
        heights: Arc::new(releasable),
    })
}

/// Optimized pack for the adds-only drain path (split add_shards only).
///
/// Since `drain_pending_adds_only` returns only ADD entries from `add_shards` and Bitcoin
/// prevents creating the same UTXO twice without an intervening spend (BIP-30 / height>91880),
/// there are no duplicates to resolve. Skips `dedupe_pending_triples_in_place` entirely and
/// sorts by key directly — one sort instead of one sort + dedup pass + second sort (previously
/// present before the redundant-sort fix). This is the dominant pack code path during IBD
/// `ibd_defer_flush` mode where adds-only flushes fire every ~2M ops.
fn pack_flush_package_adds_only(
    store: &IbdUtxoStore,
    raw: Vec<PendingLogEntry>,
) -> Option<PendingFlushPackage> {
    if raw.is_empty() {
        return None;
    }
    let mut candidate_heights: FxHashSet<u32> = FxHashSet::default();
    let mut max_h = 0u64;
    let mut batch: PendingFlushBatch = Vec::with_capacity(raw.len());
    for (k, val, h) in raw {
        if h != 0 {
            candidate_heights.insert(h as u32);
        }
        max_h = max_h.max(h);
        batch.push((k, val));
    }
    let all_heights = store.filter_heights_fully_pending_drained(candidate_heights);
    #[cfg(feature = "rayon")]
    if batch.len() >= 8192 {
        use blvm_protocol::rayon::prelude::*;
        batch.par_sort_unstable_by_key(|(k, _)| *k);
    } else {
        batch.sort_unstable_by_key(|(k, _)| *k);
    }
    #[cfg(not(feature = "rayon"))]
    batch.sort_unstable_by_key(|(k, _)| *k);
    Some(PendingFlushPackage {
        ops: Arc::new(batch),
        max_block_height: max_h,
        heights: Arc::new(all_heights),
    })
}

/// Optimized pack for the del-only backlog drain (tombstones only).
fn pack_flush_package_dels_only(
    store: &IbdUtxoStore,
    raw: Vec<PendingLogEntry>,
) -> Option<PendingFlushPackage> {
    if raw.is_empty() {
        return None;
    }
    let mut candidate_heights: FxHashSet<u32> = FxHashSet::default();
    let mut max_h = 0u64;
    let mut batch: PendingFlushBatch = Vec::with_capacity(raw.len());
    for (k, val, h) in raw {
        debug_assert!(val.is_none(), "del-only pack received an ADD entry");
        if h != 0 {
            candidate_heights.insert(h as u32);
        }
        max_h = max_h.max(h);
        batch.push((k, None));
    }
    let releasable = store.filter_heights_fully_pending_drained(candidate_heights);
    #[cfg(feature = "rayon")]
    if batch.len() >= 8192 {
        use blvm_protocol::rayon::prelude::*;
        batch.par_sort_unstable_by_key(|(k, _)| *k);
    } else {
        batch.sort_unstable_by_key(|(k, _)| *k);
    }
    #[cfg(not(feature = "rayon"))]
    batch.sort_unstable_by_key(|(k, _)| *k);
    Some(PendingFlushPackage {
        ops: Arc::new(batch),
        max_block_height: max_h,
        heights: Arc::new(releasable),
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
    // dedupe_pending_triples_in_place sorts by (k, h) and deduplicates; the surviving entries
    // are already in ascending-key order. The second sort below (previously present) was
    // redundant and wasted ~20% retire CPU at h=300k+ on 320k-op batches.
    dedupe_pending_triples_in_place(&mut v);
    let mut max_h = 0u64;
    let mut batch = Vec::with_capacity(v.len());
    for (k, val, h) in v {
        max_h = max_h.max(h);
        batch.push((k, val));
    }
    // batch is already sorted by k (dedupe_pending_triples_in_place guarantees it).
    (batch, max_h, all_heights)
}

#[cfg(feature = "production")]
impl PendingFlushPackage {
    /// Encode UTXO inserts for the flush worker (disk I/O runs on the committer thread only).
    pub fn prepare_for_disk(&self, codec: ValueCodec) -> Result<PreparedFlushPackage> {
        // For large batches, encode UTXO values in parallel (rayon) then assemble the slab
        // sequentially (just memcpys). At 500k ops this cuts serialization from ~120ms to
        // ~15ms on a 16-core host (encoding is CPU-bound and embarrassingly parallel).
        //
        // For small batches the per-task rayon overhead exceeds the encoding cost, so we
        // fall through to the sequential path.
        #[cfg(feature = "rayon")]
        if self.ops.len() >= 4096 {
            use blvm_protocol::rayon::prelude::*;
            // Phase 1: parallel encode — each op produces Option<Vec<u8>>.
            let encoded: Vec<(OutPointKey, Option<Vec<u8>>)> = self
                .ops
                .par_iter()
                .map(|(key, value_opt)| -> Result<(OutPointKey, Option<Vec<u8>>)> {
                    let bytes = match value_opt {
                        Some(arc) => Some(
                            crate::storage::utxo_value_codec::encode_utxo_with_codec(
                                codec,
                                arc.as_ref(),
                            )?,
                        ),
                        None => None,
                    };
                    Ok((*key, bytes))
                })
                .collect::<Result<Vec<_>>>()?;

            // Phase 2: sequential slab assembly (pure memcpy, very fast).
            let n_adds = encoded.iter().filter(|(_, v)| v.is_some()).count();
            let mut slab: Vec<u8> = Vec::with_capacity(n_adds * 100);
            let rows: Vec<(OutPointKey, Option<(u32, u32)>)> = encoded
                .into_iter()
                .map(|(key, opt_bytes)| {
                    let offsets = opt_bytes.map(|bytes| {
                        let start = slab.len() as u32;
                        let len = bytes.len() as u32;
                        slab.extend_from_slice(&bytes);
                        (start, len)
                    });
                    (key, offsets)
                })
                .collect();

            return Ok(PreparedFlushPackage {
                rows: Arc::new(rows),
                slab: Arc::new(slab),
                max_block_height: self.max_block_height,
            });
        }

        // Sequential path for small batches or when rayon is unavailable.
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
    cache: DashMap<OutPointKey, UtxoCacheSlot, FxBuildHasher>,
    disk: Arc<dyn Tree>,
    total_utxo_count: AtomicIsize,
    flush_threshold: usize,
    /// Sharded pending log for ADD (insert) ops only.
    ///
    /// Split from `del_shards` so that `drain_pending_adds_only()` (the hot path during
    /// `ibd_defer_flush`) is O(N_adds) rather than O(N_adds + N_dels). Without the split,
    /// DELs accumulate to ~2M entries over the ~667-block inter-checkpoint window and every
    /// adds_only drain must scan all of them — pinning the retire thread near 100% CPU at
    /// h=500k+. Each shard is a plain `Vec<PendingLogEntry>` (always ADD ops); dedupe runs
    /// once at flush time when shard logs are merged.
    add_shards: Vec<Mutex<Vec<PendingLogEntry>>>,
    /// Sharded pending log for DEL (spend/delete) ops only. Same PENDING_SHARDS layout as
    /// `add_shards`. DELs accumulate here until the next durable checkpoint (two-phase
    /// crash-safe commit), never touching `drain_pending_adds_only`.
    del_shards: Vec<Mutex<Vec<PendingLogEntry>>>,
    /// Approximate total size of `add_shards` + `del_shards` combined. Read lock-free from
    /// `maybe_take_flush_batch_through`
    /// and `pending_len`; writes happen on the `apply_*`/`take_*` paths. Slightly racy with
    /// in-flight pushes, but correctness only requires the flush trigger to fire eventually.
    pending_log_size: AtomicUsize,
    /// Count of ADD ops still in `add_shards` per block height. Used to gate
    /// `release_protected_heights`: a height is releasable only when its counter is zero
    /// (all ADDs for that height have been drained from the pending log). Without this,
    /// capped partial drains (`drain_pending_adds_only`) release protection for heights that
    /// still have ADDs in other shards → FIFO eviction drops cache entries before LMDB flush
    /// → UTXO_TOTAL_MISS during IBD.
    pending_add_count_by_height: DashMap<u32, AtomicUsize, FxBuildHasher>,
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
    in_flight_insertions: DashMap<OutPointKey, Arc<UTXO>, FxBuildHasher>,
    /// Lock-free DashSet of block heights that are currently protected from cache eviction.
    /// Contains at most `pipeline_depth + max_utxo_flushes_in_flight` entries (~36 u32s)
    /// instead of one entry per UTXO key (which reached 6M entries at h=300k+). A cache entry
    /// is protected iff `slot.block_height != UNPROTECTED_HEIGHT` and
    /// `protected_heights.contains(&slot.block_height)`.
    ///
    /// Lifetime: a height H is inserted by `worker_cache_put_protected` (or the non-worker
    /// `apply_utxo_delta` path) when the first UTXO from H enters the cache. It is removed
    /// by `release_protected_heights` after the flush batch covering H is committed to disk.
    protected_heights: DashSet<u32, FxBuildHasher>,
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
            cache: DashMap::with_hasher_and_shard_amount(FxBuildHasher::default(), 128),
            disk,
            total_utxo_count: AtomicIsize::new(0),
            flush_threshold,
            add_shards: (0..PENDING_SHARDS)
                .map(|_| Mutex::new(Vec::new()))
                .collect(),
            del_shards: (0..PENDING_SHARDS)
                .map(|_| Mutex::new(Vec::new()))
                .collect(),
            pending_log_size: AtomicUsize::new(0),
            pending_add_count_by_height: DashMap::with_hasher(FxBuildHasher::default()),
            memory_only,
            max_entries_cap: AtomicUsize::new(max_entries),
            eviction_strategy,
            recently_accessed: Mutex::new(FxHashSet::default()),
            cache_generation: AtomicU64::new(1),
            utxo_disk_commit_height: AtomicU64::new(utxo_disk_commit_through),
            utxo_barrier_mu: Mutex::new(()),
            utxo_barrier_cv: Condvar::new(),
            in_flight_insertions: DashMap::with_hasher(FxBuildHasher::default()),
            protected_heights: DashSet::with_hasher(FxBuildHasher::default()),
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

    #[inline]
    fn note_pending_add_pushed(&self, block_height: u64) {
        if block_height == 0 {
            return;
        }
        self.pending_add_count_by_height
            .entry(block_height as u32)
            .or_insert_with(|| AtomicUsize::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_add_shards_drained(&self, entries: &[PendingLogEntry]) {
        for (_, _, h) in entries {
            if *h == 0 {
                continue;
            }
            let height = *h as u32;
            if let Some(entry) = self.pending_add_count_by_height.get(&height) {
                let prev = entry.fetch_sub(1, Ordering::Relaxed);
                if prev == 1 {
                    drop(entry);
                    self.pending_add_count_by_height.remove(&height);
                }
            }
        }
    }

    /// Subset of `candidate_heights` with no remaining ADD ops in `add_shards`.
    fn filter_heights_fully_pending_drained(
        &self,
        candidate_heights: FxHashSet<u32>,
    ) -> FxHashSet<u32> {
        candidate_heights
            .into_iter()
            .filter(|h| {
                self.pending_add_count_by_height
                    .get(h)
                    .map(|c| c.load(Ordering::Relaxed) == 0)
                    .unwrap_or(true)
            })
            .collect()
    }

    /// Pending ADD count for height `h` still in `add_shards` (test/diagnostic hook).
    #[doc(hidden)]
    pub fn pending_add_count_for_height(&self, height: u32) -> usize {
        self.pending_add_count_by_height
            .get(&height)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Disable cache eviction for the duration of the no-LMDB local-replay phase.
    ///
    /// In this mode LMDB is empty so any evicted UTXO would be permanently lost (future lookups
    /// would miss in cache, miss in in_flight, and miss in LMDB). Setting the cap to `usize::MAX`
    /// (the "eviction disabled" sentinel) also causes `apply_utxo_delta` to skip populating
    /// `in_flight_insertions`, eliminating the eager-registration memory leak.
    pub fn set_no_evict_for_local_replay(&self) {
        self.max_entries_cap.store(usize::MAX, Ordering::Relaxed);
    }

    /// Re-enable eviction after the local-replay→download transition. Sets the cap to the
    /// provided value so MemoryGuard's pressure-based tuning can take over immediately.
    pub fn restore_evict_after_local_replay(&self, cap: usize) {
        // All UTXOs are now on disk (bulk hydration just ran), so in_flight_insertions
        // entries from local replay are stale — disk is the source of truth. Clear them
        // before re-enabling eviction so the inflight DashMap doesn't accumulate millions
        // of entries that can never be drained (drain_in_flight_for_batch skips when
        // max_entries == usize::MAX, which was the case during no-LMDB replay).
        let inflight_count = self.in_flight_insertions.len();
        if inflight_count > 0 {
            self.in_flight_insertions.clear();
            tracing::info!(
                "[IBD_UTXO_STORE] restore_evict_after_local_replay: cleared {} stale \
                 in_flight_insertions (all now on disk after bulk hydration)",
                inflight_count
            );
        }
        self.max_entries_cap.store(cap.max(4_096), Ordering::Relaxed);

        // Compact DashMap backing arrays after the local-replay peak.
        //
        // During local replay the UTXO cache grows to 15–20M entries (no eviction cap).
        // DashMap's hashbrown shards allocate capacity for the peak but never auto-shrink.
        // After capping at 8M, the 128 shard HashMaps each hold 3–8× their needed capacity,
        // wasting ~700 MB of table memory and keeping ~10+ GB of Arc<UTXO> mimalloc pages
        // resident (fragmented pages with freed slots can't be reclaimed until all live
        // objects in the page are gone — shrink_to_fit compacts live entries into fewer
        // pages, allowing the rest to be decommitted by mi_collect).
        //
        // This is a one-time cost of ~1–2s at the transition (128 shard rehashes of 100k
        // entries each). The RSS reduction is typically 1–3 GB from table alone, plus
        // additional mimalloc page decommit from the mi_collect below.
        let cache_cap_before = self.cache.capacity();
        let cache_len = self.cache.len();
        self.cache.shrink_to_fit();
        let inflight_cap_before = self.in_flight_insertions.capacity();
        self.in_flight_insertions.shrink_to_fit();
        tracing::info!(
            "[IBD_UTXO_STORE] restore_evict_after_local_replay: shrink_to_fit complete \
             cache cap {} → {} (len={}), inflight cap {} → {}",
            cache_cap_before,
            self.cache.capacity(),
            cache_len,
            inflight_cap_before,
            self.in_flight_insertions.capacity(),
        );

        // Return freed mimalloc pages to the OS immediately. Without this, the freed
        // DashMap slot memory stays in mimalloc thread-local freelists and the kernel
        // sees no RSS drop. mi_collect(true) forces a global heap scan and decommits
        // empty segments via madvise(MADV_DONTNEED) (MIMALLOC_PURGE_DECOMMITS=1).
        #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
        unsafe {
            libmimalloc_sys::mi_collect(true);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
    }

    /// Remove all keys in `batch` from `in_flight_insertions`.
    ///
    /// In the no-LMDB local-replay mode the retire loop discards `PendingFlushPackage`s without
    /// writing to disk, so the normal LMDB-write path that clears `in_flight_insertions` never
    /// runs. Without this call, workers eagerly inserting into `in_flight_insertions` (in
    /// `apply_utxo_delta`) would cause unbounded growth — ~20 M entries / 5 GB by h=390k.
    ///
    /// NOTE: We must NOT skip this when `max_entries_effective() == usize::MAX`. That case is
    /// exactly no-LMDB local replay, where the flush path never runs and draining here is the
    /// only mechanism preventing unbounded `in_flight_insertions` growth.
    pub fn drain_in_flight_for_batch(&self, batch: &PendingFlushBatch) {
        if self.in_flight_insertions.is_empty() {
            return;
        }
        for (key, _) in batch {
            self.in_flight_insertions.remove(key);
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

        // ── Unconditional physical-overalloc sweep ───────────────────────────────
        // Rate-limited to once per 1 000 heights (O(N) rehash is ~50 ms at 8 M entries
        // — negligible at 300+ BPS, but firing every block would be ~15 ms/s of wasted
        // CPU in the retire hot path).
        //
        // Catches the scenario where MemoryGuard nudges the logical cap by small
        // increments on every call (so new_cap != old is always true and the stable-cap
        // overalloc check below is never reached) while the physical DashMap capacity
        // sits 3-4× above the live entry count from a past burst insertion.  At h=447k
        // in prior sessions cache_cap jumped to 30 M with only 8 M live entries and
        // never shrank, wasting ~14 GB of backing DashMap structure.
        if current_height % 1_000 == 0 {
            let physical_cap = self.cache.capacity();
            let live_len = self.cache.len();
            if physical_cap > live_len.saturating_mul(4)
                && physical_cap.saturating_sub(live_len) > 10_000_000
            {
                tracing::info!(
                    "[IBD_UTXO_STORE] shrink_to_fit (unconditional overalloc): \
                     cap={} len={} logical_max={}",
                    physical_cap, live_len, new_cap
                );
                self.cache.shrink_to_fit();
            }
        }

        if new_cap == old {
            // Even if the logical cap hasn't changed, the physical DashMap backing
            // may be severely over-allocated (cap >> len) from a past peak. Check
            // and shrink if so. This fires on every call where cap is stable, so
            // guard with a 2M-slot gap to avoid shrink_to_fit on every iteration.
            let cache_cap = self.cache.capacity();
            let cache_len = self.cache.len();
            if cache_cap > cache_len.saturating_mul(2)
                && cache_cap.saturating_sub(cache_len) > 2_000_000
            {
                tracing::info!(
                    "[IBD_UTXO_STORE] shrink_to_fit (overalloc/stable): cap={} len={} max={}",
                    cache_cap, cache_len, old
                );
                self.cache.shrink_to_fit();
            }
            // `in_flight_insertions` peaks under near-eviction pressure then stays at that
            // capacity forever (it is only shrunk at local-replay end). At h=420k+ the map
            // can balloon to 7M+ capacity with only ~800k live entries — ~6 GB wasted DashMap
            // structure that mimalloc cannot return to the OS while the backing arrays are held.
            // Use a 4× overalloc threshold (looser than the 2× for cache, because inflight
            // turnover is high and frequent shrink_to_fit during active insertion spikes would
            // cause excessive rehash churn). Guard with 1M-slot minimum gap.
            let inflight_cap = self.in_flight_insertions.capacity();
            let inflight_len = self.in_flight_insertions.len();
            if inflight_cap > inflight_len.saturating_mul(4)
                && inflight_cap.saturating_sub(inflight_len) > 1_000_000
            {
                tracing::info!(
                    "[IBD_UTXO_STORE] shrink_to_fit (inflight overalloc/stable): \
                     cap={} len={} max={}",
                    inflight_cap, inflight_len, old
                );
                self.in_flight_insertions.shrink_to_fit();
            }
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
            let cache_cap = self.cache.capacity();
            let large_cut = new_cap < old * 8 / 10;
            let live_below_cap = cache_len < new_cap * 7 / 10;
            if large_cut || live_below_cap {
                tracing::info!(
                    "[IBD_UTXO_STORE] shrink_to_fit (cap-driven): cap={} len={} new_max={} \
                     (large_cut={} live_below_cap={})",
                    cache_cap, cache_len, new_cap, large_cut, live_below_cap
                );
                self.cache.shrink_to_fit();
            }
            // Shrink inflight whenever the logical cap drops — lower cap means fewer entries
            // will be protected, so prior inflight capacity is no longer needed.
            let inflight_cap = self.in_flight_insertions.capacity();
            let inflight_len = self.in_flight_insertions.len();
            if inflight_cap > inflight_len.saturating_mul(4)
                && inflight_cap.saturating_sub(inflight_len) > 500_000
            {
                tracing::info!(
                    "[IBD_UTXO_STORE] shrink_to_fit (inflight cap-driven): \
                     cap={} len={} new_max={}",
                    inflight_cap, inflight_len, new_cap
                );
                self.in_flight_insertions.shrink_to_fit();
            }
        }
    }

    #[inline]
    fn next_cache_generation(&self) -> u64 {
        self.cache_generation.fetch_add(1, Ordering::Relaxed)
    }

    /// Claim `count` consecutive generation values in a single atomic operation.
    /// Returns the first value; caller assigns [base, base+count) to individual entries.
    /// Reduces contention from N atomic fetch_adds (one per UTXO) to 1 per block.
    #[inline]
    fn claim_cache_generations(&self, count: usize) -> u64 {
        self.cache_generation.fetch_add(count as u64, Ordering::Relaxed)
    }

    #[inline]
    fn cache_put(&self, key: OutPointKey, utxo: Arc<UTXO>, block_height: u32) {
        let cache_gen = self.next_cache_generation();
        self.cache.insert(
            key,
            UtxoCacheSlot {
                generation: cache_gen,
                utxo,
                block_height,
            },
        );
    }

    /// Insert all entries in `additions` into the cache with a single atomic generation bump.
    /// Entries within the same block share a generation range, which is fine for eviction
    /// (granularity is block-level, not per-UTXO). Reduces worker-path atomic traffic
    /// from O(outputs) to O(1) per block per worker.
    #[inline]
    fn cache_put_batch(
        &self,
        additions: &rustc_hash::FxHashMap<blvm_protocol::OutPoint, Arc<UTXO>>,
        block_height: u32,
    ) {
        if additions.is_empty() {
            return;
        }
        let base_gen = self.claim_cache_generations(additions.len());
        for (i, (op, arc)) in additions.iter().enumerate() {
            let key = outpoint_to_key(op);
            self.cache.insert(
                key,
                UtxoCacheSlot {
                    generation: base_gen + i as u64,
                    utxo: Arc::clone(arc),
                    block_height,
                },
            );
        }
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

    /// Number of entries currently resident in the in-memory UTXO cache.
    /// Each entry costs ~216B on average (40B key + Arc<UTXO> ~176B).
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Pending-log flush threshold (ops) before retire considers a batch due.
    pub fn flush_threshold(&self) -> usize {
        self.flush_threshold
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
        // Claim all generations in one atomic fetch_add instead of one per entry.
        let base_gen = self.claim_cache_generations(pairs.len());
        for (i, &(key, ref arc)) in pairs.iter().enumerate() {
            self.cache.insert(
                key,
                UtxoCacheSlot {
                    generation: base_gen + i as u64,
                    utxo: Arc::clone(arc),
                    block_height: UNPROTECTED_HEIGHT,
                },
            );
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
        // Use batch generation claim: one atomic fetch_add for all outputs in this block
        // instead of one per UTXO. Eviction granularity stays at block level.
        self.cache_put_batch(additions, h);
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
        if additions_already_in_cache {
            let max_eff = self.max_entries_effective();
            let bounded = max_eff != usize::MAX;
            // Only populate in_flight_insertions when the cache is near its eviction threshold.
            // in_flight_insertions is only needed as a fallback for cache misses during the
            // pending→flush window — if the cache is well below capacity, eviction cannot fire
            // and every pending UTXO is already reachable via the cache. Populating inflight
            // unconditionally duplicates 8-16 M entries in memory (one Arc per UTXO in both
            // cache and inflight), adding GBs of DashMap overhead and causing OOM at higher
            // block heights where checkpoint stalls let retire threads race ahead.
            let near_eviction = bounded && {
                let cache_len = self.cache.len();
                // Trigger inflight population only when within 20% of the cap; below that,
                // no eviction can occur so inflight lookups will never be needed.
                cache_len.saturating_add(cache_len / 4) >= max_eff
            };
            self.push_to_pending_shards(
                del_scratch
                    .iter()
                    .map(|&k| (k, None))
                    .chain(delta.additions.iter().map(|(op, arc)| {
                        let key = outpoint_to_key(op);
                        if near_eviction {
                            self.in_flight_insertions
                                .entry(key)
                                .or_insert_with(|| Arc::clone(arc));
                        }
                        (key, Some(Arc::clone(arc)))
                    })),
                block_height,
            );
        } else {
            add_scratch.reserve(delta.additions.len());
            let h = block_height as u32;
            if !delta.additions.is_empty() {
                self.protected_heights.insert(h);
            }
            for (op, arc) in delta.additions.iter() {
                let key = outpoint_to_key(op);
                self.cache_put(key, Arc::clone(arc), h);
                add_scratch.push((key, Arc::clone(arc)));
            }
            let max_eff2 = self.max_entries_effective();
            if max_eff2 != usize::MAX && !add_scratch.is_empty() {
                let cache_len2 = self.cache.len();
                let near_eviction2 = cache_len2.saturating_add(cache_len2 / 4) >= max_eff2;
                if near_eviction2 {
                    for (key, arc) in add_scratch.iter() {
                        self.in_flight_insertions
                            .entry(*key)
                            .or_insert_with(|| Arc::clone(arc));
                    }
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
        }
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

    /// Push (key, value) pairs to the sharded pending log. ADDs go to `add_shards`, DELs to
    /// `del_shards`, so `drain_pending_adds_only` can drain in O(N_adds) without scanning
    /// accumulated DEL entries. Items are bucketed by `pending_shard_idx(key)`.
    fn push_to_pending_shards<I>(&self, items: I, block_height: u64)
    where
        I: IntoIterator<Item = (OutPointKey, PendingValue)>,
    {
        // Stack-allocated buckets: one set for adds, one for dels. Vec::new() is zero-cost
        // (no heap alloc until first push), and most blocks need <16k ops total.
        let mut add_buckets: [Vec<PendingLogEntry>; PENDING_SHARDS] = Default::default();
        let mut del_buckets: [Vec<PendingLogEntry>; PENDING_SHARDS] = Default::default();
        let mut total = 0usize;
        for (key, val) in items {
            let s = pending_shard_idx(&key);
            if val.is_some() {
                add_buckets[s].push((key, val, block_height));
                self.note_pending_add_pushed(block_height);
            } else {
                del_buckets[s].push((key, val, block_height));
            }
            total += 1;
        }
        if total == 0 {
            return;
        }
        for i in 0..PENDING_SHARDS {
            if !add_buckets[i].is_empty() {
                self.add_shards[i].lock().expect("add shard lock").append(&mut add_buckets[i]);
            }
            if !del_buckets[i].is_empty() {
                self.del_shards[i].lock().expect("del shard lock").append(&mut del_buckets[i]);
            }
        }
        self.pending_log_size.fetch_add(total, Ordering::Relaxed);
    }

    /// Drain all pending shards (both add_shards and del_shards) into a single Vec. Used by
    /// shutdown final drain only (`take_remaining_flush_package`).
    fn drain_all_pending_shards(&self) -> Vec<PendingLogEntry> {
        let approx = self.pending_log_size.load(Ordering::Relaxed);
        let mut all = Vec::with_capacity(approx);
        let mut add_drained: Vec<PendingLogEntry> = Vec::new();
        for shard in self.add_shards.iter() {
            let mut s = shard.lock().expect("add shard lock");
            if !s.is_empty() {
                add_drained.append(&mut *s);
            }
        }
        self.note_add_shards_drained(&add_drained);
        all.append(&mut add_drained);
        for shard in self.del_shards.iter() {
            let mut s = shard.lock().expect("del shard lock");
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
    /// block height** (`OrderedReadyBridge`), so ops are appended to `add_shards`/`del_shards`
    /// in consensus order. Production retire therefore uses [`Self::maybe_take_flush_batch`] /
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
        let mut add_drained: Vec<PendingLogEntry> = Vec::new();
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
        // Drain add_shards first (track for pending_add_count_by_height), then del_shards.
        for shard in self.add_shards.iter() {
            let mut s = shard.lock().expect("add shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() {
                if s[i].2 <= max_block_height_inclusive {
                    add_drained.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        if !add_drained.is_empty() {
            self.note_add_shards_drained(&add_drained);
            all.extend(add_drained);
        }
        for shard in self.del_shards.iter() {
            let mut s = shard.lock().expect("del shard lock");
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
        if drained > 0 {
            tracing::warn!(
                "[DRAIN_FORCE] drain_pending_through_height(max_h={max_block_height_inclusive}): \
                 drained={drained} (add+del combined, NO CAP)",
            );
        }
        all
    }

    /// Capped variant of `take_flush_batch_force_through`: drains at most `max_entries` ops
    /// with block height <= `max_block_height_inclusive` from both add_shards and del_shards.
    /// Call in a loop until it returns None to fully drain the backlog without one huge batch.
    pub fn take_flush_batch_force_through_capped(
        &self,
        max_block_height_inclusive: u64,
        max_entries: usize,
    ) -> Option<PendingFlushPackage> {
        if self.all_pending_shards_empty() {
            return None;
        }
        let raw = self.drain_pending_through_height_capped(max_block_height_inclusive, max_entries);
        let pkg = pack_flush_package(self, raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    fn drain_pending_through_height_capped(
        &self,
        max_block_height_inclusive: u64,
        max_entries: usize,
    ) -> Vec<PendingLogEntry> {
        let mut all = Vec::with_capacity(max_entries.min(65_536));
        let mut drained = 0usize;
        let mut add_drained: Vec<PendingLogEntry> = Vec::new();
        for shard in self.add_shards.iter() {
            if drained >= max_entries {
                break;
            }
            let mut s = shard.lock().expect("add shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() && drained < max_entries {
                if s[i].2 <= max_block_height_inclusive {
                    add_drained.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        if !add_drained.is_empty() {
            self.note_add_shards_drained(&add_drained);
            all.extend(add_drained);
        }
        for shard in self.del_shards.iter() {
            if drained >= max_entries {
                break;
            }
            let mut s = shard.lock().expect("del shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() && drained < max_entries {
                if s[i].2 <= max_block_height_inclusive {
                    all.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        if drained > 0 {
            self.pending_log_size
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                    Some(prev.saturating_sub(drained))
                })
                .ok();
            tracing::debug!(
                "[DRAIN_FORCE_CAPPED] max_h={max_block_height_inclusive} max_entries={max_entries} drained={drained} cap_hit={}",
                drained >= max_entries
            );
        }
        all
    }

    /// Discard all DEL tombstones with `block_height <= max_block_height_inclusive` from
    /// `del_shards` WITHOUT writing them to LMDB. Used when `BLVM_IBD_SKIP_DEL_LMDB=1`:
    /// DEL tombstones serve only as in-memory cache invalidation signals, but they
    /// accumulate in `del_shards` indefinitely since `ibd_flush_del_backlog_through_watermark`
    /// is skipped. Without this purge, `pending_len()` grows to millions of entries,
    /// causing `idle_flush_A/B` spam and wasting ~400MB+ of RAM.
    pub fn discard_del_backlog_through_watermark(&self, max_block_height_inclusive: u64) -> usize {
        let mut discarded = 0usize;
        for shard in self.del_shards.iter() {
            let mut s = shard.lock().expect("del shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() {
                if s[i].2 <= max_block_height_inclusive {
                    s.swap_remove(i);
                    discarded += 1;
                } else {
                    i += 1;
                }
            }
        }
        if discarded > 0 {
            self.pending_log_size
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                    Some(prev.saturating_sub(discarded))
                })
                .ok();
        }
        discarded
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
        let pkg = pack_flush_package(self, raw)?;
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
        // ADDs are duplicate-free by Bitcoin consensus; use the optimized adds-only packer
        // (skips dedupe pass, sorts by key only — saves ~20% retire CPU vs pack_flush_package).
        let pkg = pack_flush_package_adds_only(self, raw)?;
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
        let pkg = pack_flush_package(self, raw)?;
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
        // With split add_shards/del_shards, all entries in add_shards are ADDs by construction.
        // No per-entry filtering needed — drain the add_shards content directly.
        // This is O(N_adds) rather than O(N_adds + N_dels_accumulated), eliminating the
        // growing scan cost that pinned the retire thread at ~88% CPU at h=500k+.
        //
        // BATCH CAP: at SegWit heights (~h=600k+) the LMDB B-tree is 25+ GB on disk.
        // Unbounded drain causes packages of 1.2-1.4M ops which take 15-20 seconds each
        // to write to LMDB. While the durability thread is busy, the channel fills (4 slots),
        // the retire loop blocks, and workers keep inserting — causing the pending log to
        // accumulate 9M+ entries and pushing anon-RSS to 75 GB → OOM.
        // Capping at 100k entries keeps each package to ~20 MB, checkpoints to ~1-2 seconds,
        // and channel-full blocks short enough that the pending log stays bounded.
        //
        // Adaptive cap: scale with pending queue depth so the durability thread catches up
        // faster during BPS bursts that temporarily outpace the fixed 100k drain.
        // Hard ceiling is 400k (~2-3s per transaction on a 15 GB LMDB) to avoid starving
        // the retirement channel.  Env override is treated as a hard ceiling, not a floor.
        let pending_depth = self.pending_log_size.load(Ordering::Relaxed);
        let max_drain = Self::adaptive_drain_cap_for_pending(pending_depth);
        let mut all = Vec::new();
        let mut drained = 0usize;
        'outer: for shard in self.add_shards.iter() {
            if drained >= max_drain {
                break;
            }
            let mut s = shard.lock().expect("add shard lock");
            if s.is_empty() {
                continue;
            }
            let remaining_budget = max_drain - drained;
            if s.len() <= remaining_budget {
                drained += s.len();
                all.append(&mut *s);
            } else {
                // Partial drain: take first `remaining_budget` entries, leave the rest.
                let taken: Vec<PendingLogEntry> = s.drain(..remaining_budget).collect();
                drained += taken.len();
                all.extend(taken);
                break 'outer;
            }
        }
        if drained > 0 {
            self.note_add_shards_drained(&all);
            self.pending_log_size
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                    Some(prev.saturating_sub(drained))
                })
                .ok();
        }
        let remaining = self.pending_log_size.load(std::sync::atomic::Ordering::Relaxed);
        if drained > 0 {
            tracing::debug!(
                "[DRAIN_ADDS] drained={drained} cap_hit={} remaining_pending={remaining}",
                drained >= max_drain
            );
        }
        all
    }

    /// Adaptive 100k–400k by pending depth; `BLVM_IBD_DRAIN_CAP` is a hard ceiling (min), not replacement.
    pub fn adaptive_drain_cap(&self) -> usize {
        Self::adaptive_drain_cap_for_pending(self.pending_log_size.load(Ordering::Relaxed))
    }

    fn adaptive_drain_cap_for_pending(pending_depth: usize) -> usize {
        let adaptive = if pending_depth > 2_000_000 {
            400_000
        } else if pending_depth > 1_000_000 {
            300_000
        } else if pending_depth > 500_000 {
            200_000
        } else {
            100_000
        };
        std::env::var("BLVM_IBD_DRAIN_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|env_cap| adaptive.min(env_cap))
            .unwrap_or(adaptive)
    }

    /// True when any ADD in `add_shards` has block height `<= max_h`.
    pub fn has_pending_adds_at_or_below(&self, max_h: u64) -> bool {
        for shard in self.add_shards.iter() {
            let s = shard.lock().expect("add shard lock");
            if s.iter().any(|(_, _, h)| *h <= max_h) {
                return true;
            }
        }
        false
    }

    fn drain_add_shards_through_height_capped(
        &self,
        max_block_height_inclusive: u64,
        max_entries: usize,
    ) -> Vec<PendingLogEntry> {
        let mut all = Vec::with_capacity(max_entries.min(65_536));
        let mut drained = 0usize;
        for shard in self.add_shards.iter() {
            if drained >= max_entries {
                break;
            }
            let mut s = shard.lock().expect("add shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() && drained < max_entries {
                if s[i].2 <= max_block_height_inclusive {
                    all.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        if drained > 0 {
            self.note_add_shards_drained(&all);
            self.pending_log_size
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                    Some(prev.saturating_sub(drained))
                })
                .ok();
        }
        all
    }

    fn drain_del_shards_through_height_capped(
        &self,
        max_block_height_inclusive: u64,
        max_entries: usize,
    ) -> Vec<PendingLogEntry> {
        let mut all = Vec::with_capacity(max_entries.min(65_536));
        let mut drained = 0usize;
        for shard in self.del_shards.iter() {
            if drained >= max_entries {
                break;
            }
            let mut s = shard.lock().expect("del shard lock");
            if s.is_empty() {
                continue;
            }
            let mut i = 0;
            while i < s.len() && drained < max_entries {
                if s[i].2 <= max_block_height_inclusive {
                    all.push(s.swap_remove(i));
                    drained += 1;
                } else {
                    i += 1;
                }
            }
        }
        if drained > 0 {
            self.pending_log_size
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                    Some(prev.saturating_sub(drained))
                })
                .ok();
        }
        all
    }

    /// Capped adds-only drain for heights `<= max_h` (A1 leftover-add sync before del_backlog).
    pub fn take_flush_batch_adds_only_through_capped(
        &self,
        max_block_height_inclusive: u64,
        max_entries: usize,
    ) -> Option<PendingFlushPackage> {
        let raw =
            self.drain_add_shards_through_height_capped(max_block_height_inclusive, max_entries);
        if raw.is_empty() {
            return None;
        }
        let pkg = pack_flush_package_adds_only(self, raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Capped del-only drain for heights `<= max_h` (del_backlog loop).
    pub fn take_flush_batch_dels_only_through_capped(
        &self,
        max_block_height_inclusive: u64,
        max_entries: usize,
    ) -> Option<PendingFlushPackage> {
        if self.pending_log_size.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let raw =
            self.drain_del_shards_through_height_capped(max_block_height_inclusive, max_entries);
        if raw.is_empty() {
            return None;
        }
        let pkg = pack_flush_package_dels_only(self, raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Force-flush only ADD ops, leaving DEL (spend) ops in the pending log for the next
    /// durable checkpoint's Phase 3. Non-durable async flushes use this to prevent phantom
    /// DEL tombstones from reaching SST files ahead of the watermark.
    pub fn take_flush_batch_adds_only(&self) -> Option<PendingFlushPackage> {
        if self.all_pending_shards_empty() {
            return None;
        }
        let raw = self.drain_pending_adds_only();
        let pkg = pack_flush_package_adds_only(self, raw)?;
        self.register_in_flight(&pkg);
        Some(pkg)
    }

    /// Remaining pending ops after validation stops (for final drain to the flush worker).
    pub fn take_remaining_flush_package(&self) -> Option<PendingFlushPackage> {
        if self.all_pending_shards_empty() {
            return None;
        }
        let raw = self.drain_all_pending_shards();
        let pkg = pack_flush_package(self, raw)?;
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
            // Fast path: when the underlying tree is Heed3, write directly into one write
            // transaction per chunk without the intermediate Vec<(Vec<u8>, Option<Vec<u8>>)>
            // buffer that BatchWriter::put allocates. At 200k ops/chunk this removes ~50-100 MB
            // of redundant heap copies per batch, reducing GC pressure and flush latency.
            #[cfg(feature = "heed3")]
            let ops_in_batch = if let Some(heed3_tree) = self.disk.as_heed3_tree() {
                let iter = chunk.iter().filter_map(|(key, value_opt)| {
                    match value_opt {
                        Some((start, len)) => {
                            if filter == FlushFilter::DelsOnly { return None; }
                            Some((key.as_slice(), Some(&slab[*start as usize..][..*len as usize])))
                        }
                        None => {
                            if filter == FlushFilter::AddsOnly { return None; }
                            Some((key.as_slice(), None))
                        }
                    }
                });
                heed3_tree.write_slice_batch(iter)?
            } else {
                let mut b = self.disk.batch()?;
                let mut n = 0usize;
                for (key, value_opt) in chunk {
                    match value_opt {
                        Some((start, len)) => {
                            if filter != FlushFilter::DelsOnly {
                                b.put(key.as_slice(), &slab[*start as usize..][..*len as usize]);
                                n += 1;
                            }
                        }
                        None => {
                            if filter != FlushFilter::AddsOnly {
                                b.delete(key.as_slice());
                                n += 1;
                            }
                        }
                    }
                }
                if n > 0 { b.commit_no_wal()?; }
                n
            };
            #[cfg(not(feature = "heed3"))]
            let ops_in_batch = {
                let mut b = self.disk.batch()?;
                let mut n = 0usize;
                for (key, value_opt) in chunk {
                    match value_opt {
                        Some((start, len)) => {
                            if filter != FlushFilter::DelsOnly {
                                b.put(key.as_slice(), &slab[*start as usize..][..*len as usize]);
                                n += 1;
                            }
                        }
                        None => {
                            if filter != FlushFilter::AddsOnly {
                                b.delete(key.as_slice());
                                n += 1;
                            }
                        }
                    }
                }
                if n > 0 { b.commit_no_wal()?; }
                n
            };
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

        let max_eff = self.max_entries_effective();
        if max_eff != usize::MAX {
            let cache_len = self.cache.len();
            if cache_len > max_eff {
                // Use a local countdown to avoid calling DashMap::len() (O(shards)) in the
                // inner loop. cache_len is a snapshot; actual count may be slightly off due
                // to concurrent inserts, but the eviction target is approximate anyway.
                let mut to_evict = cache_len.saturating_sub(max_eff);
                let mut evicted = 0usize;
                for (key, value_opt) in pkg.rows.iter() {
                    if to_evict == 0 {
                        break;
                    }
                    if value_opt.is_some() {
                        if self.cache.remove(key).is_some() {
                            evicted += 1;
                            to_evict = to_evict.saturating_sub(1);
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
        }
        Ok(total_flushed)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Total capacity (across all shards) of the backing hashbrown tables.
    /// After peak-then-eviction phases, this stays at the high-water-mark until process exit
    /// (DashMap tables do not shrink). Each slot costs sizeof(K)+sizeof(V) = 40+24 = 64 bytes
    /// plus 1 byte of control metadata, so: capacity × 65 ≈ bytes allocated in DashMap tables.
    pub fn cache_capacity(&self) -> usize {
        self.cache.capacity()
    }

    /// Total capacity of in_flight_insertions DashMap (same caveat: does not shrink).
    pub fn inflight_capacity(&self) -> usize {
        self.in_flight_insertions.capacity()
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

    /// Flush (fdatasync) without the post-sync madvise(MADV_DONTNEED).
    /// Use for the DEL-phase sync inside `ibd_flush_del_backlog_through_watermark`
    /// where the preceding ADD-phase `flush_disk()` already evicted the pages.
    /// Skipping the duplicate madvise avoids re-evicting freshly written DEL
    /// tombstones that LMDB will immediately re-fault on the next B-tree access.
    pub fn flush_disk_sync_only(&self) -> Result<()> {
        #[cfg(feature = "heed3")]
        if let Some(tree) = self.disk.as_heed3_tree() {
            tree.force_sync_only()?;
            return Ok(());
        }
        // Fallback for non-Heed3 backends: full flush (includes madvise equivalent).
        self.disk.flush_to_disk()
    }

    /// Write every live UTXO currently in the in-memory cache to LMDB as a one-shot bulk
    /// insert.  Called once at the local-replay → download transition after an IBD genesis
    /// restart (the LMDB store was empty during local replay because all pending-log drains
    /// were discarded in-memory).
    ///
    /// The resulting LMDB state is equivalent to what would have been written by N sequential
    /// checkpoint flushes, but without the LMDB write overhead during hot local replay.
    ///
    /// Safety: must only be called when the LMDB store is **empty** (fresh after wipe) so
    /// there are no stale entries to worry about.  After this returns the store reflects the
    /// live UTXO set and normal incremental durability can resume.
    pub fn flush_full_cache_to_lmdb(&self) -> Result<usize> {
        use crate::storage::utxo_value_codec::encode_utxo_with_codec;
        let codec = self.value_codec;
        let len = self.cache.len();
        info!(
            "[IBD_UTXO_STORE] flush_full_cache_to_lmdb: streaming {len} cache entries to LMDB \
             in chunks of {MAX_BATCH_OPS} (no full-Vec collection to avoid OOM)"
        );
        // Stream directly from the DashMap in fixed-size chunks — do NOT collect all entries
        // into a Vec first.  At 76 M UTXOs that Vec would be ~6–15 GB depending on encoded
        // size, which reliably triggers OOM on 90 GB machines when added to the existing RSS.
        // Without a global sort we lose MDB_APPEND locality, but random B-tree inserts are
        // acceptable here (this is a one-shot write, not the hot incremental path).
        let mut batch: Vec<(OutPointKey, Vec<u8>)> = Vec::with_capacity(MAX_BATCH_OPS);
        let mut total = 0usize;
        let mut encode_errors = 0usize;

        let flush_batch = |batch: &[(OutPointKey, Vec<u8>)]| -> Result<usize> {
            #[cfg(feature = "heed3")]
            if let Some(heed3_tree) = self.disk.as_heed3_tree() {
                let iter = batch.iter().map(|(k, v)| (k.as_slice(), Some(v.as_slice())));
                return heed3_tree.write_slice_batch(iter);
            }
            let mut b = self.disk.batch()?;
            for (k, v) in batch {
                b.put(k.as_slice(), v.as_slice());
            }
            b.commit_no_wal()?;
            Ok(batch.len())
        };

        for entry in self.cache.iter() {
            let key = *entry.key();
            match encode_utxo_with_codec(codec, entry.value().utxo.as_ref()) {
                Ok(bytes) => batch.push((key, bytes)),
                Err(e) => {
                    encode_errors += 1;
                    warn!("[IBD_UTXO_STORE] flush_full_cache_to_lmdb: encode error key {:?}: {e}", &key[..4]);
                }
            }
            if batch.len() >= MAX_BATCH_OPS {
                total += flush_batch(&batch)?;
                batch.clear();
                if total % (MAX_BATCH_OPS * 50) == 0 {
                    info!("[IBD_UTXO_STORE] flush_full_cache_to_lmdb: {total}/{len} written…");
                }
            }
        }
        if !batch.is_empty() {
            total += flush_batch(&batch)?;
        }
        if let Err(e) = self.disk.flush_to_disk() {
            warn!("[IBD_UTXO_STORE] flush_full_cache_to_lmdb: flush_to_disk failed: {e}");
        }
        info!(
            "[IBD_UTXO_STORE] flush_full_cache_to_lmdb: complete \
             ({total} written, {encode_errors} encode errors)"
        );
        Ok(total)
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

#[cfg(test)]
mod del_backlog_unit_tests {
    use super::IbdUtxoStore;

    #[test]
    fn adaptive_drain_cap_respects_env_ceiling() {
        unsafe {
            std::env::set_var("BLVM_IBD_DRAIN_CAP", "150000");
        }
        assert_eq!(IbdUtxoStore::adaptive_drain_cap_for_pending(3_000_000), 150_000);
        assert_eq!(IbdUtxoStore::adaptive_drain_cap_for_pending(100_000), 100_000);
        unsafe {
            std::env::remove_var("BLVM_IBD_DRAIN_CAP");
        }
    }
}

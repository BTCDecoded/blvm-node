//! IBD validation loop — runs on a dedicated `std::thread`.
//!
//! Each block: **connect** (build a `UtxoSet` view from the store, run `validate_block_only` and
//! BIP30 on that view), then **retire** the returned delta on [`IbdUtxoStore`] (apply, protect /
//! evict, flush decision). The connect path does not write canonical in-memory UTXO state; retire
//! does. Reads from the feeder buffer, validates, and flushes to storage in batches.

use super::feeder::FeederState;
use super::memory::{self, MemoryGuard, PressureLevel};
use crate::storage::blockstore::BlockStore;
use crate::storage::disk_utxo::{
    block_input_keys_batch_into_arc, block_input_keys_into_filtered,
    block_input_keys_into_filtered_with_tx_ids, key_to_outpoint, outpoint_to_key, OutPointKey,
};
use crate::storage::ibd_utxo_store::{IbdUtxoStore, PendingFlushPackage};
use crate::storage::Storage;
use crate::utils::time::current_timestamp;
use anyhow::Result;
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{
    segwit::Witness, BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, UTXO,
};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

/// Reuse `Arc<Vec<Vec<Witness>>>` of empty stacks for pre-segwit blocks (same `n` as tx count).
/// Validation runs on one thread — `thread_local` avoids a global mutex on this hot path.
thread_local! {
    static EMPTY_WITNESS_STACKS: RefCell<FxHashMap<usize, Arc<Vec<Vec<Witness>>>>> =
        RefCell::new(FxHashMap::default());
}

fn shared_empty_witness_stacks(n_tx: usize) -> Arc<Vec<Vec<Witness>>> {
    EMPTY_WITNESS_STACKS.with(|cell| {
        let mut g = cell.borrow_mut();
        if let Some(a) = g.get(&n_tx) {
            return Arc::clone(a);
        }
        let arc = Arc::new(vec![Vec::new(); n_tx]);
        if g.len() > 512 {
            g.clear();
        }
        g.insert(n_tx, Arc::clone(&arc));
        arc
    })
}

/// Wall-clock ms at last `mi_collect` / `malloc_trim` (lock-free throttle for RSS-pressure path).
static LAST_IBD_HEAP_TRIM_WALL_MS: AtomicU64 = AtomicU64::new(0);
// 2 s interval: after large eviction batches the DashMap contains many freed Arc<UTXO> slots
// whose backing mimalloc pages haven't been decommitted yet. Running mi_collect more often
// (every 2 s vs old 10 s) returns those pages to the OS faster, reducing sustained RSS and
// preventing the kernel from paging them to swap under memory pressure.
const IBD_HEAP_TRIM_MIN_INTERVAL_MS: u64 = 2_000;

/// Block-counter for throttling `evict_aggressive_for_rss`. The function walks every DashMap
/// shard (`retain` holds each shard's write lock briefly), which on a 6 M-entry cache with
/// 99 % protected ratio is ~6 M iterations × ~100 ns ≈ 500 ms — too slow to run on every block
/// when retire is the rate-limiting step. Run every Nth Emergency block instead.
static IBD_EMERGENCY_EVICT_BLOCKS_SEEN: AtomicU64 = AtomicU64::new(0);
const IBD_EMERGENCY_EVICT_EVERY_N_BLOCKS: u64 = 8;
/// Skip `evict_aggressive_for_rss` when fewer than this many cache entries are unprotected
/// (i.e. eligible for eviction). Below this threshold the O(N) scan finds essentially nothing
/// and burns retire-thread CPU. The protection set is drained by `flush_prepared_package`, so
/// when protections are saturating the cache the right action is to flush, not to scan.
const IBD_EMERGENCY_EVICT_MIN_UNPROTECTED: usize = 32_768;

fn ibd_maybe_heap_trim() {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    loop {
        let prev = LAST_IBD_HEAP_TRIM_WALL_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < IBD_HEAP_TRIM_MIN_INTERVAL_MS {
            return;
        }
        if LAST_IBD_HEAP_TRIM_WALL_MS
            .compare_exchange_weak(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

use super::ibd_staging::empty_utxo_delta;
use super::ParallelIBD;

use blvm_protocol::block::UtxoDelta;

/// Post-validation retire step. Workers have **already** mutated the UTXO cache + pending log
/// on their own thread, so this function no longer touches the delta itself —
/// it only runs the *coordinated* per-block work: eviction, dynamic-protect, memory-pressure
/// signaling, and flush decisions. Returning the optional `PendingFlushPackage` lets the
/// caller spawn the disk flush off the retire thread.
///
/// `_delta` is kept on the signature to retain the dynamic-eviction `protect_keys_for_next_blocks`
/// data flow (callers pass the live block buffer); the apply-side work is gone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ibd_v2_retire_apply_utxo_delta(
    next_height: u64,
    store: &IbdUtxoStore,
    blocks_buf: &[Arc<Block>],
    keys_buf: &mut Vec<OutPointKey>,
    keys_seen: &mut rustc_hash::FxHashSet<OutPointKey>,
    evict_scratch: &mut Vec<(OutPointKey, u64)>,
    mem_guard: &mut MemoryGuard,
    max_ahead_live: &Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
) -> (u64, u64, Option<PendingFlushPackage>, bool) {
    // Eviction throttle: only run every EVICT_INTERVAL_BLOCKS blocks. Each call iterates the
    // DashMap (≥512 entries scanned) holding shard locks, sorts by generation, and removes
    // up to `to_evict`. Running it every block burned ~30% of retire-thread CPU even when
    // only a handful of evictions were needed. With a 30M-entry cache and ~10 net adds per
    // block, throttling to every 16 blocks lets the cache overshoot by ~160 entries — well
    // below the 1% slack threshold.
    const EVICT_INTERVAL_BLOCKS: u64 = 16;
    if next_height % EVICT_INTERVAL_BLOCKS == 0 {
        store.maybe_evict(evict_scratch);
    }
    #[cfg(feature = "profile")]
    let mut protect_evict_ms: u64 = 0;
    #[cfg(not(feature = "profile"))]
    let protect_evict_ms: u64 = 0;
    if store.is_dynamic_eviction() {
        #[cfg(feature = "profile")]
        let t_protect_evict = std::time::Instant::now();
        block_input_keys_batch_into_arc(blocks_buf, keys_buf, keys_seen);
        store.protect_keys_for_next_blocks(keys_buf);
        store.evict_if_needed(next_height);
        #[cfg(feature = "profile")]
        {
            protect_evict_ms = t_protect_evict.elapsed().as_millis() as u64;
        }
    }
    let pressure_level = mem_guard.should_flush(Some((max_ahead_live, nominal_max_ahead)));
    // Publish for lock-free reads from the dispatcher (avoids contending on `mem_mtx` per block —
    // the retire thread holds that lock for the whole apply+evict+flush-decision sequence).
    memory::publish_ibd_pressure(pressure_level);
    // Self-adapting UTXO cache cap. Reads ACTUAL process RSS (captures mimalloc fragmentation,
    // RocksDB growth, and every other allocator) and shrinks the cache when we approach the
    // RSS budget; grows it back when memory frees up. Throttled internally to one evaluation
    // per ~2 s. This replaces the old static-cap-per-host-tier model with a runtime that
    // self-corrects regardless of host RAM, other workloads, or fragmentation patterns.
    if let Some(new_cap) = mem_guard.compute_adaptive_cache_cap() {
        let old_len = store.len();
        store.tune_max_entries_for_pressure(new_cap, next_height);
        let evicted = old_len.saturating_sub(store.len());
        // Force heap pages back to the OS immediately after a large eviction. Without this,
        // mimalloc holds freed Arc<UTXO> pages resident until ALL objects in each 64 KB
        // page are freed — which with random eviction ordering can take thousands of blocks.
        // The forced mi_collect + malloc_trim bypass the normal 2s throttle when we just
        // dropped a significant number of cache entries, making the adaptive RSS response
        // visible to the kernel within the same ~2s poll cycle.
        if evicted > 32_768 {
            #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
            unsafe {
                libmimalloc_sys::mi_collect(true);
            }
            #[cfg(target_os = "linux")]
            unsafe {
                libc::malloc_trim(0);
            }
            // Reset the normal heap-trim throttle so the next periodic trim doesn't skip.
            LAST_IBD_HEAP_TRIM_WALL_MS.store(0, Ordering::Relaxed);
        }
    }
    // Force-flush ONLY at Critical/Emergency. At Elevated we let `maybe_take_flush_batch` decide
    // (it triggers at the normal threshold). Forcing a flush on every Elevated transition produced
    // a storm of tiny flushes (pending<10k) at h=366k onward, each followed by heap_trim — both
    // ate retire CPU and walloped BPS from 145 → 60. Critical/Emergency still force-flush because
    // those levels mean we're seconds from OOM and need to reclaim aggressively.
    let rss_pressure = pressure_level >= PressureLevel::Critical;
    let rss_pressure_elevated_only = pressure_level == PressureLevel::Elevated;
    if rss_pressure {
        let pending_now = store.pending_len();
        info!(
            "[IBD_V2] height={} RSS pressure ({:?}, cache={}, pending={}), forcing flush",
            next_height,
            pressure_level,
            store.len(),
            pending_now
        );
        // Under Emergency: full-cache eviction sweep, gated on protection ratio. Walking 6 M
        // DashMap entries with 99 % protected ratio is wasted work — the scan can only evict
        // `cache.len() - protected_len()` entries no matter how often we run it. The right
        // action when protections saturate is to *flush* (which calls `flush_prepared_package`
        // → drains `worker_preinserted`), not to scan. So we run eviction at most every 8th
        // Emergency block, and skip even that when the unprotected population is tiny.
        // In a bounded height-window UTXO design, in-memory work cannot pile up against a
        // huge protection set; this store can — so flush, not more scans, is the release valve.
        if pressure_level == PressureLevel::Emergency {
            let n = IBD_EMERGENCY_EVICT_BLOCKS_SEEN.fetch_add(1, Ordering::Relaxed);
            if n % IBD_EMERGENCY_EVICT_EVERY_N_BLOCKS == 0 {
                let cache_now = store.len();
                let protected_now = store.protected_len();
                let evictable = cache_now.saturating_sub(protected_now);
                if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                    store.evict_aggressive_for_rss();
                }
            }
        }
        // Force-flush under Critical/Emergency. With height-granular protection (Gap 1),
        // `protected_heights` holds only ~pipeline_depth entries (≤64), so deferring a
        // forced flush for a few blocks no longer risks a protection-deadlock.
        // Under Emergency we always flush immediately. Under Critical we gate on a minimum
        // batch size to avoid creating a storm of tiny L0 SSTs that fragment RocksDB and
        // drive compaction pressure. 1000 ops (was 10000) is enough to batch usefully while
        // still allowing rapid relief when large flood-attack blocks (~1k outputs each) push
        // the pending log up quickly — the old 10k threshold stalled flushes entirely when
        // pending hovered at 2k-9k, leaving protected UTXOs stuck and causing eviction/miss.
        let pending_now = store.pending_len();
        const CRITICAL_MIN_FLUSH_OPS: usize = 1_000;
        let should_force =
            pressure_level == PressureLevel::Emergency || pending_now >= CRITICAL_MIN_FLUSH_OPS;
        // Full pending drain (`take_flush_batch_force` / `maybe_take_flush_batch`): the feeder
        // applies UTXO deltas in strict ascending height (`OrderedReadyBridge`), so `pending_shards`
        // only receives ops after each lower height has been applied — safe to flush without a
        // per-retire-height cap. That avoids scanning millions of retained rows every tick when
        // retire lags validation (see `IbdUtxoStore::drain_pending_through_height`).
        // Workers also push pending before dispatching retire work (`retire_dispatcher.rs`
        // invariant 1). The flush package's `max_block_height` may exceed `local_last_retired`;
        // the chain watermark may advance past `global_last_retired` (invariant 2: watermark
        // tracks worker production, not the contiguous retire floor).
        let batch = if should_force {
            store.take_flush_batch_force()
        } else {
            store.maybe_take_flush_batch()
        };
        ibd_maybe_heap_trim();
        (0u64, protect_evict_ms, batch, true)
    } else if rss_pressure_elevated_only {
        // Elevated: don't disturb the pipeline. Take a normal-threshold flush if it's already due,
        // skip heap_trim. The download-ahead reduction (already applied by adjust_max_ahead_live)
        // is enough to ease the pressure.
        let batch = store.maybe_take_flush_batch();
        (0u64, protect_evict_ms, batch, false)
    } else if ibd_defer_flush {
        let at_checkpoint = next_height > 0 && next_height % ibd_defer_checkpoint == 0;
        let batch = if at_checkpoint {
            store.take_flush_batch_force()
        } else {
            None
        };
        (0u64, protect_evict_ms, batch, false)
    } else {
        let batch = store.maybe_take_flush_batch();
        (0u64, protect_evict_ms, batch, false)
    }
}

/// True when this height should emit a profile sample line for interval `sample`.
/// `sample == 0` means interval sampling is off (e.g. only `disk` / `blocked` in BLVM_IBD_DEBUG)
/// — never use `% sample` in that case.
#[cfg(feature = "profile")]
#[inline]
fn ibd_profile_height_matches_sample(sample: u64, height: u64) -> bool {
    sample == 1 || (sample > 0 && height % sample == 0)
}

#[inline]
fn dynamic_utxo_cap(level: PressureLevel, nominal: usize) -> usize {
    if nominal == usize::MAX {
        return usize::MAX;
    }
    match level {
        PressureLevel::Emergency => (nominal / 4).max(8_192),
        PressureLevel::Critical => (nominal * 2 / 3).max(nominal / 2),
        PressureLevel::Elevated => (nominal * 9 / 10).max(nominal * 4 / 5),
        PressureLevel::None => nominal,
    }
}

#[inline]
fn dynamic_prefetch_lookahead(level: PressureLevel, nominal: usize) -> usize {
    let n = nominal.clamp(1, 128);
    match level {
        PressureLevel::Emergency => 8,
        PressureLevel::Critical => (n / 2).clamp(12, 48),
        PressureLevel::Elevated => ((n * 2 / 3).max(24)).min(n),
        PressureLevel::None => n,
    }
}

/// One block handed to the background retire thread (after validation enqueues the delta in `staged`).
///
/// Fields are `pub(crate)` so the sibling `retire_dispatcher` module can route on `height`
/// without importing the (mostly internal) validation pipeline. Construction stays
/// inside `validation_loop` (the only producer).
pub(crate) struct IbdRetireWork {
    pub(crate) height: u64,
    pub(crate) blocks_buf: Vec<Arc<Block>>,
    pub(crate) block: Arc<Block>,
}

/// How many retire packages to commit to the memtable before issuing a synchronous
/// `flush_disk()` + `persist_ibd_utxo_flush_checkpoint`. Larger batches → fewer L0 SSTs
/// → much less compaction churn at h≥180 k where retire produces ~10 packages/second.
///
/// Default (`8`) was chosen so each durability boundary writes ≥1× write_buffer_size
/// (192 MB on 16 GiB hosts) of data, producing one large SST instead of 8 micro-SSTs.
/// At `1` (legacy behaviour) the per-package `flush_cf` cycle wedges retire at h~190 k
/// because compaction can't drain L0 fast enough.
///
/// Override via `BLVM_IBD_RETIRE_FLUSH_BATCH=N`. `N=1` restores per-package durability
/// (useful for stress testing crash-recovery; soft autorepair still handles a watermark
/// gap after a partial-batch crash, but each partial batch loses up to `N-1` packages
/// of progress on restart).
fn retire_flush_batch_size() -> usize {
    std::env::var("BLVM_IBD_RETIRE_FLUSH_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n >= 1)
        // 16 async commits before each durability flush (was 8). Each commit writes
        // ~19 MB to the ibd_utxos memtable; 16 × 19 MB = 304 MB before flush_disk.
        // With max_write_buffer_number=10 × 64 MB = 640 MB capacity, no memtable stall.
        // Halves the number of flush_disk calls vs the old default of 8, reducing
        // durability-drain stalls (pipeline pauses) at h=160-300k by ~2×.
        .unwrap_or(16)
}

/// Push a UTXO disk flush from the retire thread; joins older flushes when the in-flight cap is hit.
///
/// Concurrency uses [`memory::utxo_flush_concurrency_cap`]: bounded burst under healthy pressure,
/// strict tier cap under Critical+. Never uses an unbounded ceiling (historically 1024 → OOM).
///
/// **Batched durability.** Most calls only run `flush_prepared_package` (writes rows to the
/// `ibd_utxos` memtable via `commit_no_wal`) on a spawned thread; they skip `flush_disk` and
/// `persist_ibd_utxo_flush_checkpoint`. Every `retire_flush_batch_size()`-th call (and shutdown
/// drains; see `take_remaining_flush_package`) runs the durability path *synchronously*: it
/// drains all in-flight commits, then `flush_disk` (forces memtable → SST) and
/// `persist_ibd_utxo_flush_checkpoint` (atomic watermark+running-MuHash bump). This collapses
/// `BATCH` micro-SSTs into one large SST, reducing L0 churn ~`BATCH`× and eliminating the
/// h~190 k retire wedge where RocksDB's compaction couldn't drain L0 fast enough.
///
/// Crash safety: between durability boundaries the watermark stays at the last persisted
/// height; on restart the soft autorepair pass detects `chain_tip > watermark` and replays
/// the gap. The strict `flush_disk` → `persist_checkpoint` ordering inside the durability
/// path preserves the invariant that watermark never advances past durable ibd_utxos rows.
fn push_utxo_flush_from_retire(
    store: &Arc<IbdUtxoStore>,
    storage_wm: &Arc<Storage>,
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: &Arc<AtomicUsize>,
    next_height: u64,
    max_utxo_flushes_in_flight: usize,
    pkg: PendingFlushPackage,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
) -> Result<()> {
    let flush_limit = memory::utxo_flush_concurrency_cap(max_utxo_flushes_in_flight).max(1);
    let batch_count = retire_flush_batch_size();
    let n = retire_flush_counter.fetch_add(1, Ordering::Relaxed);
    // Do durability on the first call (cold-start: nothing in memtable yet, but we
    // want a clean checkpoint before workload heats up) and every Nth thereafter.
    let do_durability = batch_count <= 1 || n % batch_count == 0;
    let mut q = utxo_flush_handles.lock();
    // flush_limit cap: join the oldest in-flight commit to keep concurrency bounded.
    // Each joined thread returns its local MuHash3072 sub-accumulator; we fold it into
    // the global accumulator here. The lock is held for a single Num3072 multiply
    // (~10 µs), not for the full batch muhash loop (was seconds under the old design).
    while q.len() >= flush_limit {
        let Some(handle) = q.pop_front() else {
            return Err(anyhow::anyhow!(
                "IBD invariant violated: UTXO flush wait queue empty under backpressure"
            ));
        };
        match handle.join() {
            Ok(Ok(sub_mh)) => {
                let mut mh_guard = ibd_muhash.lock();
                *mh_guard = std::mem::take(&mut *mh_guard).multiply(&sub_mh);
            }
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!("UTXO flush panicked: {:?}", e));
            }
        }
    }
    let batch_size = pkg.ops.len();
    let heights = Arc::clone(&pkg.heights);
    if do_durability {
        // Synchronous durability path. Drain ALL in-flight async commits first so the
        // memtable contains every prior package's rows before we flush_cf. Without this,
        // the watermark could advance past not-yet-committed data on a slow-async/fast-sync
        // race. After the drain, we run this package's commit, then flush_disk, then
        // persist_ibd_utxo_flush_checkpoint atomically as the durability boundary.
        //
        // Parallel MuHash: async threads each computed a *local* MuHash3072 sub-accumulator
        // (no shared mutex during their execution). We collect all sub-accumulators here and
        // fold them into the global accumulator with a single brief lock — replacing the
        // previous per-thread mutex hold (seconds each, serialized) with a batch of
        // microsecond-cost Num3072 multiplies.
        let mut combined_sub_mh = blvm_muhash::MuHash3072::new();
        while let Some(handle) = q.pop_front() {
            match handle.join() {
                Ok(Ok(sub_mh)) => {
                    combined_sub_mh = combined_sub_mh.multiply(&sub_mh);
                }
                Ok(Err(e)) => return Err(e),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "UTXO flush panicked while draining for durability: {:?}",
                        e
                    ));
                }
            }
        }
        drop(q);
        let prepared = pkg.prepare_for_disk()?;
        // Drop the raw ops Vec immediately after serialization. After prepare_for_disk()
        // the slab holds all UTXO bytes; the ops Arc<Vec<(key, Arc<UTXO>)>> (~18 MB at
        // 320k ops) and its Arc<UTXO> references are no longer needed here — in_flight_insertions
        // already holds separate refs for the supplement path until note_utxo_flush_completed.
        drop(pkg);
        // Pre-compute muhash for the durability batch in the retire loop (full rayon pool,
        // no lock held). This mirrors the async-path change: muhash always computed here,
        // never inside the commit thread.
        let mut shutdown_local_mh = blvm_muhash::MuHash3072::new();
        store.compute_package_muhash(&prepared, &mut shutdown_local_mh)?;
        let muhash_running = {
            let mut mh_guard = ibd_muhash.lock();
            // Incorporate all async batches' sub-accumulators + this durability batch's hash.
            *mh_guard = std::mem::take(&mut *mh_guard)
                .multiply(&combined_sub_mh)
                .multiply(&shutdown_local_mh);
            // Commit the durability batch to RocksDB (muhash already computed above, skip it).
            store.flush_prepared_package(&prepared, None)?;
            mh_guard.serialize_running_state()
        };
        // `commit_no_wal` leaves rows in the memtable until flush_cf — persist SST first so a
        // crash cannot advance `ibd_utxo_watermark` past durable ibd_utxos rows.
        store.flush_disk()?;
        storage_wm
            .chain()
            .persist_ibd_utxo_flush_checkpoint(prepared.max_block_height, &muhash_running)?;
        store.release_protected_heights(&heights);
        store.note_utxo_flush_completed(prepared.max_block_height);
        debug!(
            "[IBD_DEBUG] Block {}: durability flush boundary (batch_size={}, n={})",
            next_height, batch_size, n,
        );
    } else {
        // Async commit path: rows go to memtable via commit_no_wal. No flush_disk, no
        // watermark bump — those happen at the next durability boundary. release_protected
        // and note_utxo_flush_completed CAN safely run pre-durability: they only affect
        // in-memory eviction policy, not on-disk state. If we crash before the next
        // durability boundary, soft autorepair detects the watermark gap and replays.
        //
        // MuHash computed IN the async thread (not in the retire loop):
        // For delete ops, compute_package_muhash does one RocksDB point-read per deletion
        // to fetch the UTXO for its preimage. At h=180k-380k (Satoshi Dice era) a 320k-op
        // batch contains ~170k deletes × 0.13 ms each ≈ 22 seconds of I/O. Doing this in
        // the retire loop (before spawning the thread) blocked the loop for 22s per flush,
        // freezing `local_last_retired` and causing apparent 30-120s IBD stalls.
        // Doing it inside the async thread keeps the retire loop free; up to `flush_limit`
        // threads run concurrently so their disk reads overlap (SSD handles parallel IOPS).
        // Each thread returns a local sub-accumulator; joined into ibd_muhash at flush_limit
        // cap joins and at durability drains (same sub-accumulator contract as before).
        let prepared = pkg.prepare_for_disk()?;
        drop(pkg);
        let store_clone = Arc::clone(store);
        q.push_back(std::thread::spawn(move || {
            // Compute muhash in the async thread: concurrent with other threads and with
            // retire loop processing. Retire loop is never blocked by this I/O.
            let mut local_mh = blvm_muhash::MuHash3072::new();
            store_clone.compute_package_muhash(&prepared, &mut local_mh)?;
            store_clone.flush_prepared_package(&prepared, None)?;
            store_clone.release_protected_heights(&heights);
            store_clone.note_utxo_flush_completed(prepared.max_block_height);
            Ok(local_mh)
        }));
        debug!(
            "[IBD_DEBUG] Block {}: async commit (batch_size={}, in_flight={}, n={})",
            next_height,
            batch_size,
            q.len(),
            n,
        );
    }
    Ok(())
}

/// Drop all work channels (so each retire shard exits `recv`), join every shard, then
/// propagate the first stored error (if any). Equivalent to the pre-sharding behavior
/// for `BLVM_IBD_RETIRE_SHARDS=1`; for `>=2` it serially shuts down each shard. Errors
/// from `JoinHandle::join()` (panic in a retire thread) take precedence over `retire_err`,
/// because a panic indicates a programming bug we want to surface, while `retire_err`
/// is the path errors take when retire returned cleanly with a stored error.
fn retire_thread_shutdown(
    retire_dispatcher: &mut super::retire_dispatcher::RetireDispatcher,
    retire_err: &Arc<Mutex<Option<anyhow::Error>>>,
) -> Result<()> {
    retire_dispatcher.shutdown_and_join()?;
    if let Some(e) = retire_err.lock().take() {
        return Err(e);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Parallel validation: dedicated `ibd-validate` worker thread.
//
// The orchestrator (main validation loop) builds the UTXO view for block h
// and hands it off to the worker. While the worker runs script verification,
// the orchestrator begins the store lookup for block h+1 (`overlap_prep`).
// This overlaps I/O (cold UTXO cache read) with CPU (script evaluation),
// giving a real throughput boost at heights 200k+ where both are non-trivial.
//
// max_in_flight = 1: at most one `ValidateJob` is live at any time, so BIP30
// state is simply moved into the job and returned in the result — no cloning.
// ──────────────────────────────────────────────────────────────────────────────

/// Everything the validation worker needs to build View(h) AND run `validate_block_only`.
///
/// Pipeline pattern: the orchestrator only takes cheap snapshots (Arc clones) and ships them to the
/// worker. The expensive work — UTXO cache lookups, disk supplement, staged-delta fold,
/// speculative-additions overlay, script verification — all happens on the worker thread, in
/// parallel across the N-thread worker pool.
struct ValidateJob {
    height: u64,
    /// Arc clone kept in the main thread too — only ref-count cost.
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    /// Canonical BIP30 index: moved in, updated in-place, returned as `bip30_post`.
    bip30_index: Bip30Index,
    /// Snapshot of recent_headers_buf (≤11 elements, cheap Arc clones).
    recent_headers: Vec<Arc<BlockHeader>>,
    /// Precomputed tx-ids from the feeder coordinator.
    tx_ids: Vec<Hash>,
    cached_network_time: u64,
    /// Pre-extracted input keys (filtered: skips coinbase + intra-block spends).
    keys: Vec<OutPointKey>,
    /// Snapshot of speculative additions for in-flight blocks `h_other < h`. Each is an
    /// `Arc<UtxoSet>` for cheap sharing across workers. Workers iterate this list
    /// (max = `pipeline_depth_live - 1`) on cache misses for keys whose source block has not
    /// yet completed validation (i.e. `worker_cache_put_protected` has not yet run for it).
    /// Already-validated-but-not-flushed (staged) blocks live in the cache and are found via
    /// the cheaper `cache_get` fast path — no separate staged_snapshot walk needed.
    spec_adds_snapshot: Vec<(u64, Arc<UtxoSet>)>,
    /// Optional UTXO map preloaded by the upstream IBD prefetcher; when non-empty, worker
    /// uses it as the initial fill (skipping the full cache scan).
    prefetched: rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
}

/// Results returned from the validation worker for one block.
struct ValidateResult {
    height: u64,
    /// Tx ids are already available on the job and unused after validation; drop them here to
    /// avoid an `into_owned()` alloc (Vec<Hash> memcpy) per block on the IBD hot path.
    result: Result<Option<UtxoDelta>>,
    /// BIP30 index after applying this block's coinbase rules.
    bip30_post: Bip30Index,
    /// Wall time spent inside the worker (view-build + `validate_block_only`).
    elapsed: std::time::Duration,
    /// Wall time spent building the view only (cache + supplement + fold + overlay).
    /// Useful for orchestrator EMA-driven prefetch lookahead tuning.
    view_build_ms: u64,
}

// ──────────────────────────────────────────────────────────────────────────────
// N-parallel validation pipeline:
//   - Up to N blocks are dispatched to N worker threads simultaneously.
//   - Each block h+k gets View(h+k) built using the store + staged deltas up to
//     h-1 PLUS speculative outputs from in-flight blocks h..h+k-1.
//   - Speculative outputs = all UTXO additions a block creates, computable
//     directly from its transaction outputs without running validation.
//     They equal D(h).additions for any valid block, so correctness holds.
//   - Results arrive in any order; the orchestrator retires in strict ascending
//     order so IbdUtxoStore invariants are preserved.
//   - BIP30-sensitive range [91710..91855]: force pipeline_depth_live=1 (also serializes workers).
//   - pipeline_depth (max in-flight) is decoupled from n_validate_workers (concurrent execution).
//     A deeper pipeline lets the dispatcher front-run so a single slow block at the head of the
//     in-order queue doesn't starve other workers.
// ──────────────────────────────────────────────────────────────────────────────

/// Per-block data carried from dispatch through result processing.
struct InFlightEntry {
    height: u64,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    feeder_est_bytes: usize,
    utxo_base_ms: u64,
    utxo_base_tune_ms: u64,
    prefetch_ms: u64,
    apply_pending_ms: u64,
    /// Input keys for this block — populated only on the error dump path (re-derived from
    /// the block). Kept as `Option` so we never clone ~5k keys per dispatched block just for
    /// a path that is hit at most once per run.
    input_keys: Option<Vec<OutPointKey>>,
}

// `speculative_additions_from_block` lives in `prefetch::build_spec_adds` now: building this
// `UtxoSet` ran on the validation dispatcher (single-threaded hot path) and was ~O(outputs)
// HashMap inserts + `Arc::new(UTXO)` allocations per block. We moved it onto the prefetch
// worker pool so the `cpus * 2` workers (otherwise stalled on RocksDB MultiGet RTTs) absorb it
// in parallel — i.e. spend-side prep on worker threads before validation consumes the view.

/// Worker thread loop: build the per-block UTXO view, then validate.
///
/// Each worker owns its own scratch buffers (`utxo_base`, key buffers) so allocations amortise
/// across the worker's lifetime. With N workers and N concurrent jobs, view-build runs N-way
/// parallel; the orchestrator stays a thin dispatcher.
///
/// `max_pending_ops` bounds `pending_shards` (UTXO ops awaiting disk flush). When the pending
/// log exceeds this, the worker yields/sleeps so the retire thread can drain. Without this,
/// validation can race tens of thousands of blocks ahead of retire, accumulating millions of
/// pending ops in RAM (→ OOM on 16 GiB hosts).
///
/// The cap is an `Arc<AtomicUsize>` so the retire-loop controller (Tier 3) can **adapt**
/// it online based on observed RSS pressure and drain throughput — see
/// [`adapt_max_pending_ops_tick`] for the policy. Workers reload the cap on every
/// backpressure check, so adaptive shrink/grow takes effect within one block. A loaded
/// value of `0` disables the limit entirely (high-RAM hosts where the full pipeline
/// trivially fits in RAM).
#[allow(clippy::too_many_arguments)]
fn run_validation_worker_shared(
    rx: crossbeam_channel::Receiver<ValidateJob>,
    tx: mpsc::Sender<ValidateResult>,
    parallel_ibd: Arc<super::ParallelIBD>,
    blockstore: Arc<crate::storage::blockstore::BlockStore>,
    protocol: Arc<blvm_protocol::BitcoinProtocolEngine>,
    store: Arc<IbdUtxoStore>,
    last_retired: Arc<AtomicU64>,
    max_pending_ops: Arc<AtomicUsize>,
) {
    // Per-worker scratch buffers. UtxoSet capacity carries over (~peak inputs of recent block).
    let mut utxo_base: UtxoSet = UtxoSet::default();
    let mut supplement_cache_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_missing_buf: Vec<OutPointKey> = Vec::new();
    // Per-worker apply scratch (one-per-thread; capacity grows to peak block size and amortises).
    // Each worker mutates the UTXO cache + pending log for its own block, so the
    // 8k DashMap ops + 16k pending pushes that used to bottleneck a single retire thread now run
    // N-way parallel. Pending-state mutex still serialises the bulk pushes, but the cache work
    // (deletions, worker_preinserted retire) is fully concurrent across workers.
    let mut del_scratch: Vec<OutPointKey> = Vec::new();
    let mut add_scratch: Vec<(OutPointKey, Arc<UTXO>)> = Vec::new();

    loop {
        let mut job = match rx.recv() {
            Ok(j) => j,
            Err(_) => break,
        };
        let height = job.height;

        // ─── Build View(h) ──────────────────────────────────────────────────
        // Key-driven layered lookup. For each input key we check (in order):
        //   1. job.prefetched      — UTXOs the prefetch pool pre-loaded into RAM.
        //   2. store.cache_get     — DashMap (lock-free shards). Catches every UTXO
        //                            from already-validated blocks (staged + flushed).
        //                            `worker_cache_put_protected` populated it; the
        //                            `worker_preinserted` DashSet protects from eviction
        //                            until disk-flush, so this is the canonical fast hit.
        //   3. spec_adds_snapshot  — speculative additions from in-flight blocks whose
        //                            workers have NOT yet returned (so cache_put hasn't
        //                            run for them). Bounded by `pipeline_depth` (~32).
        //   4. store.supplement    — store cache (re-checked) + RocksDB. The disk
        //                            fallback for the rare miss after the prefetcher
        //                            ran. Serial deserialize per worker (no rayon).
        let t_view = std::time::Instant::now();
        utxo_base.clear();
        utxo_base.reserve(job.keys.len());
        let still_missing = &mut keys_missing_buf;
        still_missing.clear();

        for k in job.keys.iter() {
            let op = key_to_outpoint(k);
            if let Some(arc) = job.prefetched.get(k) {
                utxo_base.insert(op, Arc::clone(arc));
            } else {
                still_missing.push(*k);
            }
        }

        // FAST PATH: by the time this worker runs, every retired-but-not-flushed block (i.e.
        // every entry in `staged_snapshot`) has called `worker_cache_put_protected`, so its
        // outputs are in the cache (DashMap, lock-free) and protected from eviction by
        // `worker_preinserted` until disk flush completes. A direct `cache_get` here replaces
        // the O(staged_snapshot.len() × still_missing.len()) walk that dominated `utxo_base_ms`
        // (~100 ms/block at h>270k where staged backed up behind retire). The staged_snapshot
        // walk is fully redundant once cache is consulted, so we drop it entirely.
        if !still_missing.is_empty() {
            still_missing.retain(|k| {
                if let Some(ref r) = store.cache_get(k) {
                    let op = key_to_outpoint(k);
                    utxo_base.insert(op, Arc::clone(&r.utxo));
                    return false;
                }
                true
            });
        }

        // SLOW PATH (rare): keys for in-flight blocks whose worker has not yet returned and so
        // hasn't run `worker_cache_put_protected`. Bounded by `pipeline_depth` (~32), and most
        // entries are quickly drained as workers finish — so this loop is small in practice.
        if !still_missing.is_empty() && !job.spec_adds_snapshot.is_empty() {
            still_missing.retain(|k| {
                let op = key_to_outpoint(k);
                for (_sh, set) in job.spec_adds_snapshot.iter().rev() {
                    if let Some(u) = set.get(&op) {
                        utxo_base.insert(op, Arc::clone(u));
                        return false;
                    }
                }
                true
            });
        }

        if !still_missing.is_empty() {
            store.supplement_utxo_map_with_buf(
                &mut utxo_base,
                still_missing,
                &mut supplement_cache_buf,
            );
        }
        let view_build_ms = t_view.elapsed().as_millis() as u64;

        // ─── Validate ──────────────────────────────────────────────────────
        let recent_opt: Option<&[Arc<BlockHeader>]> = if job.recent_headers.is_empty() {
            None
        } else {
            Some(job.recent_headers.as_slice())
        };
        let t_val = std::time::Instant::now();
        let raw = parallel_ibd.validate_block_only(
            &blockstore,
            protocol.as_ref(),
            &mut utxo_base,
            Some(&mut job.bip30_index),
            job.block_arc.as_ref(),
            Some(Arc::clone(&job.block_arc)),
            job.witnesses_storage.as_slice(),
            Some(&job.witnesses_storage),
            job.height,
            recent_opt,
            job.cached_network_time,
            Some(&job.tx_ids),
        );
        let elapsed = t_val.elapsed();
        // Drop tx-ids immediately — they live in `job.tx_ids` and `ValidateResult` discards them
        // on the success path. `into_owned()` would copy every hash into a new Vec; skip it.
        let result = raw.map(|(_ids, delta)| delta);
        // Worker-side commit: pre-populate the UTXO cache, *and* stamp the pending
        // log + apply deletions, all on this worker. The retire thread used to do all of this
        // serially while N validation workers stalled on its single mutex; moving it here lets
        // the cache work run N-way parallel (DashMap is sharded). The pending-state mutex still
        // serialises the bulk pushes, but it holds for ≪1 ms per block and is contention-bounded
        // by N rather than blocked behind the retire thread's apply+evict+flush sequence.
        //
        // Protection invariant preserved at every instant: a key is in `worker_preinserted`
        // (until apply removes it), then in `pending.key_set` (until take_for_flush), then in
        // `in_flight_insertions` (until disk flush completes). `maybe_evict` checks all three.
        if let Ok(Some(ref delta)) = &result {
            store.worker_cache_put_protected(&delta.additions, height);
            store.apply_utxo_delta(delta, height, &mut del_scratch, &mut add_scratch, true);
        }
        let _ = tx.send(ValidateResult {
            height,
            result,
            bip30_post: job.bip30_index,
            elapsed,
            view_build_ms,
        });
        // Pending-ops backpressure: throttle BEFORE picking up the next job, never before
        // sending the current ValidateResult.
        //
        // Placing the spin here (post-send) instead of pre-apply_utxo_delta is the critical
        // correctness fix for the deadlock observed at h≈179k (2026-05-09):
        //
        //   Old order: cache_put → [SPIN] → apply_delta → tx.send
        //   New order: cache_put → apply_delta → tx.send → [SPIN]
        //
        // With the old order, all workers could park before tx.send, so the validation loop
        // got no ValidateResult, sent no retire work, and pending never drained — a true
        // circular deadlock broken only by the 30s safety valve. With the new order workers
        // always complete the block (apply + send), so retire always has work to drain from,
        // and pending falls below cap within milliseconds. The safety valve below is kept as
        // a last-resort guard against pathological RocksDB stalls, but should never trigger
        // in normal operation.
        //
        // Overshoot: up to N_workers × max_block_ops past the cap before parking. On a 16 GiB
        // host that is ≈12 × 5000 = 60k ops × ~300 B ≈ 18 MB — negligible.
        let cap = max_pending_ops.load(Ordering::Relaxed);
        if cap > 0 {
            let mut spins = 0u32;
            let spin_start = std::time::Instant::now();
            const WORKER_SPIN_MAX: std::time::Duration = std::time::Duration::from_secs(60);
            const WORKER_SPIN_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
            let mut last_log = spin_start;
            loop {
                let pending_now = store.pending_len();
                let cap_now = max_pending_ops.load(Ordering::Relaxed);
                if pending_now <= cap_now {
                    break;
                }
                if spin_start.elapsed() >= WORKER_SPIN_MAX {
                    warn!(
                        "[IBD_BACKPRESSURE_RELEASE] worker waited {}s for pending to drain \
                         (height={}, pending={}, cap={}) — retire wedged; proceeding",
                        spin_start.elapsed().as_secs(),
                        height,
                        pending_now,
                        cap_now
                    );
                    break;
                }
                if last_log.elapsed() >= WORKER_SPIN_LOG_EVERY {
                    warn!(
                        "[IBD_BACKPRESSURE] worker waiting for pending to drain before next job \
                         (height={}, pending={}, cap={}, waited={}s)",
                        height,
                        pending_now,
                        cap_now,
                        spin_start.elapsed().as_secs()
                    );
                    last_log = std::time::Instant::now();
                }
                if spins < 8 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                spins = spins.saturating_add(1);
            }
        }
    }
}

/// Adapt `max_pending_ops` online based on RSS pressure and pending-log fill ratio.
///
/// The nominal cap at IBD start comes from RAM tier defaults or `BLVM_IBD_MAX_PENDING_OPS`; it is
/// only the **anchor**. On real workloads the right live cap depends on (a) what RSS the host is
/// using now (pressure climbs as the UTXO cache grows past h≈200 k) and (b) whether retire keeps
/// up with validation.
/// Fixing the live cap at nominal alone breaks both ways: under pressure cap too high → OOM
/// risk; under calm cap too low → unnecessary validation throttle although retire could absorb more.
///
/// **Policy.**
/// - `Emergency` → halve the cap (floor `nominal/16`, hard floor `100 k`). RSS is
///   seconds from OOM, so the **only** safe action is shrinking the headroom validators
///   are allowed to occupy.
/// - `Critical` → multiply by 0.75 (floor `nominal/8`, hard floor `500 k`). Memory
///   guard is recommending eviction + flush; lowering the cap helps both happen sooner.
/// - `Elevated` → hold. The pressure response (lower `max_ahead_live`, more frequent
///   flushes) is enough; further cap-shrink would just stall workers without helping RSS.
/// - `None` → if `pending_len < cap/4` retire is keeping up trivially, grow the cap by
///   10% (capped at `1.1 × nominal`). Otherwise hold.
///
/// **Throttle.** Adaptation runs at most once per ~500 ms (controlled by
/// `last_adapt_ms`). Adjusting more often produces oscillation when pressure flicks
/// between bands every few blocks.
///
/// **Disabled when `nominal == 0`.** That's the ≥32 GiB tier where backpressure is off
/// outright; never re-engage it adaptively, because the host class is sized to absorb
/// the full pipeline by configuration.
fn adapt_max_pending_ops_tick(
    cap: &AtomicUsize,
    nominal: usize,
    pressure: PressureLevel,
    pending_len: usize,
    last_adapt_ms: &AtomicU64,
) {
    if nominal == 0 {
        return;
    }
    const TICK_INTERVAL_MS: u64 = 500;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = last_adapt_ms.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < TICK_INTERVAL_MS {
        return;
    }
    // CAS so two retire shards racing this don't both adapt in the same window.
    if last_adapt_ms
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let current = cap.load(Ordering::Relaxed);
    let new = match pressure {
        PressureLevel::Emergency => {
            // Aggressive shrink. Floor protects against runaway-tiny caps that would
            // stall validation entirely (pending_len would never drop fast enough to
            // unblock a worker if the cap dropped below the in-flight-block working set).
            (current / 2).max(nominal / 16).max(100_000)
        }
        PressureLevel::Critical => (current * 3 / 4).max(nominal / 8).max(500_000),
        PressureLevel::Elevated => current,
        PressureLevel::None => {
            // Grow only when retire is *clearly* ahead — pending_len well below current.
            // Ceiling is 1.1× nominal (not 2×) to prevent a fast-drain burst from
            // granting workers a massive head-start that fills RAM once blocks get heavier
            // (the regression seen at h≈130k where 16M ops accumulated and retire crawled
            // at 6 BPS for hours). A 10% buffer above the budgeted nominal is enough to
            // absorb transient bursts without blowing the memory budget.
            if pending_len < current / 4 {
                let grown = (current as u128).saturating_mul(11) / 10;
                let max = (nominal as u128).saturating_mul(11) / 10;
                grown.min(max).max(nominal as u128 / 4) as usize
            } else {
                current
            }
        }
    };

    if new != current {
        cap.store(new, Ordering::Relaxed);
        if matches!(pressure, PressureLevel::Critical | PressureLevel::Emergency)
            || (pressure == PressureLevel::None && new > current)
        {
            // Log meaningful transitions only — Elevated holds wouldn't appear here, and
            // None-with-no-change returns early. Helps correlate observed BPS dips with
            // cap shrinks during post-mortems.
            tracing::debug!(
                "[IBD_ADAPT] max_pending_ops {} → {} (pressure={:?}, pending={}, nominal={})",
                current,
                new,
                pressure,
                pending_len,
                nominal
            );
        }
    }
}

/// Parameters for the validation loop. Holds all captured state from the spawn closure.
pub struct ValidationParams {
    pub feeder_state: FeederState,
    pub ibd_store: Arc<IbdUtxoStore>,
    pub blockstore: Arc<BlockStore>,
    pub storage: Arc<Storage>,
    pub parallel_ibd: Arc<ParallelIBD>,
    pub protocol: Arc<BitcoinProtocolEngine>,
    pub utxo_mutex: Arc<std::sync::Mutex<UtxoSet>>,
    pub effective_end_height: u64,
    pub start_height: u64,
    pub validation_height: Arc<std::sync::atomic::AtomicU64>,
    pub mem_guard: MemoryGuard,
    pub max_ahead_live: Arc<std::sync::atomic::AtomicU64>,
    pub nominal_max_ahead: u64,
    /// Resolved **nominal** UTXO cache cap (from [`MemoryGuard::utxo_max_entries`] at IBD start).
    pub utxo_nominal_max_entries: usize,
    /// UTXO prefetch lookahead: **env > ibd.toml > default** (see [`super::ParallelIBDConfig::from_config`]).
    pub utxo_prefetch_lookahead: usize,
    /// Broadcast sender: validation loop broadcasts the height it is waiting for when stalled.
    /// Download workers subscribe and abort/retry stuck chunks that contain the stall height.
    pub stall_tx: tokio::sync::broadcast::Sender<u64>,
}

/// Run the IBD validation loop. Called from std::thread::spawn.
pub fn run_validation_loop(params: ValidationParams) -> Result<()> {
    let feeder_state = params.feeder_state;
    let ibd_store_v2_for_validation = params.ibd_store;
    let blockstore = params.blockstore;
    let storage_clone = params.storage;
    let parallel_ibd = params.parallel_ibd;
    let protocol = params.protocol;
    let _utxo_mutex = params.utxo_mutex;
    let effective_end_height = params.effective_end_height;
    let start_height = params.start_height;
    let validation_height = params.validation_height;
    let mem_guard = params.mem_guard;
    let system_total_ram_mb = mem_guard.system_total_ram_mb();
    // Extract the spec_adds_bytes Arc before the guard goes behind a Mutex so the coordinator
    // can update it lock-free. MemoryGuard::memory_snapshot() reads it via Relaxed load.
    let spec_adds_bytes = Arc::clone(&mem_guard.spec_adds_bytes);
    let mem_mtx = Arc::new(Mutex::new(mem_guard));
    let max_ahead_live = params.max_ahead_live;
    let nominal_max_ahead = params.nominal_max_ahead;
    let utxo_nominal_max_entries = params.utxo_nominal_max_entries;
    let stall_tx = params.stall_tx;
    let nominal_prefetch_lookahead = params.utxo_prefetch_lookahead.clamp(1, 128);
    let utxo_prefetch_lookahead_live = AtomicUsize::new(nominal_prefetch_lookahead);

    //
    // Blocks may arrive out of order. We maintain a small reorder buffer
    // and flush in-order blocks immediately to minimize memory usage.
    //
    // PERFORMANCE OPTIMIZATION: We use deferred (batched) storage to avoid
    // per-block database writes. Validated blocks are stored in a pending
    // buffer and flushed in batches of 1000 blocks. This improves IBD
    // performance from ~2 blocks/sec to ~50+ blocks/sec.
    let mut blocks_synced = 0;
    let validation_start = std::time::Instant::now();

    // IBD Profiling (profile feature): BLVM_IBD_DEBUG=profile,blocked,disk or =profile:100,blocked or =full
    // Format: comma-separated. profile[:sample][:slow_ms] (e.g. profile:100 = every 100th block; profile:1:50 = slow threshold 50ms)
    #[cfg(feature = "profile")]
    let (ibd_profile_sample, ibd_profile_slow_ms, ibd_profile, ibd_disk_profile, ibd_blocked_log) = {
        let mut sample: u64 = 0;
        let mut slow: u64 = 0;
        let mut disk = false;
        let mut blocked_log = false;
        if let Ok(val) = std::env::var("BLVM_IBD_DEBUG") {
            let parts: Vec<&str> = val.split(',').map(|s| s.trim()).collect();
            let full = parts.iter().any(|p| *p == "full");
            for p in &parts {
                let p = *p;
                if p == "full" {
                    sample = sample.max(1);
                    disk = true;
                    blocked_log = true;
                } else if p == "profile" {
                    sample = sample.max(1);
                } else if let Some(rest_s) = p.strip_prefix("profile:") {
                    // Skip full "profile:" (8 chars); p[7..] wrongly kept a leading ':' and broke "profile:100"
                    let rest: Vec<&str> = rest_s.split(':').collect();
                    if !rest.is_empty() && !rest[0].is_empty() {
                        if let Ok(n) = rest[0].parse::<u64>() {
                            if rest.len() >= 2 && !rest[1].is_empty() {
                                // profile:sample:slow (e.g. profile:100:50)
                                sample = sample.max(n.max(1));
                                if let Ok(s) = rest[1].parse::<u64>() {
                                    slow = s;
                                }
                            } else if n < 100 {
                                // profile:50 = slow threshold 50ms (plan compat)
                                sample = sample.max(1);
                                slow = n;
                            } else {
                                // profile:100 = sample every 100 blocks
                                sample = sample.max(n);
                            }
                        }
                    }
                } else if p == "blocked" {
                    blocked_log = true;
                } else if p == "disk" {
                    disk = true;
                }
            }
            if full && sample == 0 {
                sample = 1;
                disk = true;
                blocked_log = true;
            }
            if sample > 0 && !blocked_log {
                blocked_log = true; // default blocked_log=ON when profile sampling is on
            }
        }
        let on = sample > 0 || disk;
        if on {
            info!("IBD profiling ENABLED (BLVM_IBD_DEBUG): sample_interval={}, slow_threshold_ms={}, disk_io={}, blocked_log={}", sample, slow, disk, blocked_log);
        }
        if blocked_log {
            info!("IBD_BLOCKED_LOG ENABLED: every validation-blocking stall will be logged");
        }
        (sample, slow, on, disk, blocked_log)
    };
    #[cfg(not(feature = "profile"))]
    let (ibd_profile_sample, ibd_profile_slow_ms, ibd_profile, ibd_disk_profile, ibd_blocked_log) =
        (0u64, 0u64, false, false, false);

    // Track last 11 block headers for BIP113 median-time-past calculation
    // Vec + drain keeps contiguity; avoids VecDeque::make_contiguous() per-block alloc
    let mut recent_headers_buf: VecDeque<Arc<BlockHeader>> = VecDeque::with_capacity(12);
    // Reusable scratch Vec for the per-job recent_headers snapshot: avoid one collect() alloc per
    // dispatch (the deque holds ≤11 Arc<BlockHeader> ptrs; small but occurs every block).
    let mut recent_snap_buf: Vec<Arc<BlockHeader>> = Vec::with_capacity(12);

    // DEFERRED STORAGE: Buffer validated blocks for batch commit
    // Keep flush interval small to avoid OOM on systems with limited RAM (16GB)
    // Capture both base values once: they are constant after `MemoryGuard` init, so the
    // dispatcher can compute pressure-scaled live values per block without contending on
    // `mem_mtx` (which the retire thread holds across `apply_utxo_delta` + flush decisions).
    let (storage_flush_interval, ibd_budget_mb) = {
        let g = mem_mtx.lock();
        (g.storage_flush_interval, g.budget_mb())
    };
    let mut pending_blocks: Vec<(Arc<Block>, Arc<Vec<Vec<Witness>>>, u64)> =
        Vec::with_capacity(storage_flush_interval);
    /// Sum of feeder `est_bytes` for entries in `pending_blocks` (same heuristic as [`super::types::estimate_block_bytes`]; pressure-path flush only).
    let mut pending_storage_bytes: u64 = 0;
    let skip_storage = false;
    let initial_buffer_limit = mem_mtx.lock().buffer_limit(start_height);

    info!(
        "Validation loop starting (deferred storage: flush every ~{} blocks [pressure-scaled], extra flush under Critical/Emergency when pending bytes exceed budget cap, initial buffer limit: {}, utxo_prefetch_lookahead_nominal: {})...",
        storage_flush_interval,
        initial_buffer_limit,
        nominal_prefetch_lookahead,
    );

    let mut next_validation_height = start_height;

    // FEEDER BUFFER: Block feeder drains ready_rx into shared state. We read next block and
    // lookahead blocks for protect_keys. Buffer fills while validation runs.

    // Async flush: block batches on std::thread (validation runs off tokio).
    let mut flush_handles: VecDeque<std::thread::JoinHandle<Result<()>>> = VecDeque::new();
    let utxo_flush_handles = Arc::new(Mutex::new(VecDeque::<
        std::thread::JoinHandle<Result<blvm_muhash::MuHash3072>>,
    >::new()));
    // Per-IBD-run counter shared across retire shards (kept at 1 by default; increments only
    // on each `push_utxo_flush_from_retire` call). Drives the durability batching schedule
    // documented on `push_utxo_flush_from_retire`.
    let retire_flush_counter: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let (max_block_flushes_in_flight, max_utxo_flushes_under_pressure) = {
        let g = mem_mtx.lock();
        (g.max_block_flushes, g.max_utxo_flushes)
    };

    let ibd_defer_flush = mem_mtx.lock().defer_flush;
    let ibd_defer_checkpoint = mem_mtx.lock().defer_checkpoint_interval;

    // Reusable buffers for protect_keys (avoids 2–4 Vec+HashSet allocs per block).
    let mut blocks_buf: Vec<Arc<Block>> = Vec::with_capacity(nominal_prefetch_lookahead.max(8));
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen: rustc_hash::FxHashSet<OutPointKey> = rustc_hash::FxHashSet::default();
    // IBD v2: reuse buffer for block_input_keys (avoids ~80KB alloc per block).
    let mut keys_v2_buf: Vec<OutPointKey> = Vec::new();
    // Orchestrator no longer builds views — workers do, in parallel. Buffers
    // (utxo_base, keys_missing_buf, supplement_cache_buf) live inside each worker.

    // N-parallel pipeline state.
    // `in_flight` tracks dispatched jobs in order; `pending_results` buffers
    // out-of-order ValidateResult arrivals until we can process them in order.
    // `spec_adds` holds the speculative UTXO outputs for each in-flight block (`Arc<UtxoSet>` so
    // workers receive cheap pointer clones in their job snapshot). Lookahead blocks consult this
    // list to plug UTXOs that aren't yet in the store or staged.
    //
    // Pipeline depth (max in-flight) is decoupled from worker count below — capacity of 64 covers
    // a 4× pipeline_depth multiplier on 16-core hosts (clamp = 64). The dispatcher front-
    // runs the worker pool so a single slow block (cache-miss → 80ms view-build) does not starve
    // all N workers at the head of the in-order queue.
    let mut in_flight: VecDeque<InFlightEntry> = VecDeque::with_capacity(64);
    let mut pending_results: BTreeMap<u64, ValidateResult> = BTreeMap::new();
    // BTreeMap keyed by height so we can drop entries early (as soon as worker_cache_put_protected
    // has run for that height) without a linear scan. VecDeque forced us to wait until retire.
    let mut spec_adds: std::collections::BTreeMap<u64, Arc<UtxoSet>> =
        std::collections::BTreeMap::new();

    // Cache BLVM_IBD_SNAPSHOT_DIR once at loop init (was std::env::var per block)
    let snapshot_dir_base: Option<String> = std::env::var("BLVM_IBD_SNAPSHOT_DIR").ok();
    // Same for optional BPS CSV (read on periodic IBD log intervals only, but avoid env lookup each time)
    let ibd_bps_csv_path: Option<String> = std::env::var("BLVM_IBD_BPS_CSV").ok();
    // #48: Tunable yield interval (default 500 for 5–10K BPS; fewer yields = less validation interruption)
    let yield_interval: u64 = std::env::var("BLVM_IBD_YIELD_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);

    // BIP30 O(1) index: for non-disk path, maintain locally. For disk path, DiskBackedUtxoSet owns it.
    let mut bip30_index = Bip30Index::default();
    // Arc<UtxoDelta> so under-lock snapshots in the dispatcher fold are pointer-bumps only,
    // not deep clones of the delta vectors. Retire takes the Arc out (refcount drops to 1 after
    // the dispatcher's transient fold clones go out of scope) and operates on the inner value.
    let staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    // Lock-free counter that mirrors staged.len(). The dispatcher increments it on each
    // staged.insert; retire threads decrement it after staged.remove. Reading this without
    // holding the staged mutex lets the dispatch loop add a cheap backpressure cap that
    // prevents staged from growing unboundedly when retire stalls during a UTXO durability
    // flush (flush_disk can take 1–3 s at h>300k; validation at 500 BPS would otherwise
    // accumulate 500–1500 Arc<UtxoDelta> entries × ~800 KB each ≈ 400–1200 MB).
    let staged_count: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let retire_err: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    // Sharded retire (default `BLVM_IBD_RETIRE_SHARDS=1` → original single-threaded behavior).
    // N=1 path is bit-identical: the dispatcher just wraps a single mpsc channel and a single
    // retire thread, and `publisher.publish` is a no-op fold over a one-element list.
    let num_retire_shards = super::retire_dispatcher::configured_retire_shards();
    if num_retire_shards > 1 {
        info!(
            "[IBD_RETIRE] sharded retire enabled: BLVM_IBD_RETIRE_SHARDS={} (workers contend on \
             mem_mtx/mh_acc; sweet spot is 2..=4)",
            num_retire_shards
        );
    }
    // Recent rate: blocks since last status / elapsed since last status. Shows burst vs wait (avg can overstate when mostly waiting).
    let mut last_log_blocks: u64 = 0;
    let mut last_log_instant = std::time::Instant::now();
    // 5a: Adaptive mi_collect — run more often when RSS grows fast
    let mut last_rss_mb: u64 = 0;
    let mut last_collect_block: u64 = 0;
    // EMA of utxo-base build time for prefetch lookahead (single validation thread — no mutex).
    let mut prefetch_base_ema: Option<f64> = None;
    // Reusable Vec capacity for per-block dispatch snapshot. Capacity sized to handle the
    // configured pipeline depth (up to 64 in_flight at typical 8-16 worker / 16-core hosts);
    // it is a small Arc-clone vector so oversizing is cheap.
    let mut spec_adds_snapshot_buf: Vec<(u64, Arc<UtxoSet>)> = Vec::with_capacity(64);

    // Incremental UTXO commitment during IBD (Core-style; no full scan). Retire thread mutates
    // the tree under a mutex; store_commitment is called there after UTXO apply.
    #[cfg(all(feature = "utxo-commitments", feature = "production"))]
    let (commitment_tree_shared, commitment_store_opt) = {
        let pm = storage_clone.pruning();
        let tree = pm
            .as_ref()
            .and_then(|p| p.commitment_store())
            .and_then(|_| blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree::new().ok());
        let store = pm.and_then(|p| p.commitment_store());
        if tree.is_some() && store.is_some() {
            info!("IBD: incremental UTXO commitment enabled (applying delta per block)");
        }
        (tree.map(|t| Arc::new(Mutex::new(t))), store)
    };
    #[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
    // Placeholder — retire thread skips commitment; types must not pull in optional deps.
    #[allow(unused_variables)]
    let (commitment_tree_shared, commitment_store_opt) = (None::<()>, None::<()>);

    let storage_for_retire = Arc::clone(&storage_clone);

    let ibd_muhash_accumulator: Arc<Mutex<blvm_muhash::MuHash3072>> = Arc::new(Mutex::new(
        crate::storage::ibd_utxo_muhash::load_ibd_muhash_from_chain(storage_clone.chain())?,
    ));

    // Pending-ops backpressure: cap entries in `pending_shards` at a RAM-tier-derived limit.
    // The retire thread drains pending one height at a time; without a cap, validation races
    // ahead and accumulates millions of pending UTXO ops in RAM (~200 B/entry). At h=200 k on
    // a 16 GiB host we observed 22.5 M ops (~4.6 GB) → OOM.
    //
    // Why entries instead of block-lag: ops/block varies wildly by chain era (150/block early,
    // 8 000/block late), so a fixed block-lag cap is either useless or devastating depending on
    // height. Entries map directly to memory.
    //
    // Defaults without `BLVM_IBD_MAX_PENDING_OPS` (GiB from [`MemoryGuard::total_gb_rounded`]):
    //   under 16 GiB: 1 M
    //   16–23 GiB: 1.5 M
    //   24–31 GiB: 3 M
    //   ≥32 GiB: 0 (backpressure disabled for this knob)
    // Nominal cap = tier default or env override. The adaptive controller treats
    // this as the **anchor**: pressure pushes the live cap below nominal, calm pushes it
    // back up, never higher than `1.1 × nominal` (a 10% buffer above the budgeted RAM
    // class to absorb transient bursts without letting workers run arbitrarily far ahead).
    let max_pending_ops_nominal: usize = std::env::var("BLVM_IBD_MAX_PENDING_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            let total_gb = MemoryGuard::total_gb_rounded(system_total_ram_mb);
            if total_gb >= 32 {
                0
            } else if total_gb >= 24 {
                3_000_000
            } else if total_gb >= 16 {
                // 1.5M cap × ~300 B/entry ≈ 450 MB peak. Sized so the full pipeline (cache
                // ≤1.1 GB + pending ≤450 MB + inflight ≤150 MB + RocksDB ≤700 MB + mimalloc
                // retention) stays inside the 6.5 GB RSS budget on a 16 GiB host shared with
                // a typical desktop workload (~5 GB).
                //
                // History — DO NOT lower further on 16 GiB hosts without retesting both ends:
                //   8M → 1.5M (h≈195k OOM, in_flight_insertions leak compounded with cache):
                //     fixed by the in_flight_insertions removal in `IbdUtxoStore::remove`.
                //   1.5M → 600k (h≈187k swap-thrash hypothesis): WRONG. Smaller cap forced
                //     retire into 1-block-at-a-time mode while a single backlog drain still
                //     packaged ~660k ops (workers race to fill cap on every release). Each
                //     drain still overflowed the 64 MB memtable, but now happened *every*
                //     block instead of every 1.5M ops. Net effect: 1 block per 30 s vs the
                //     ~3000 BPS we get with the cap doing its actual job.
                //   1.5M → 0 (cap disabled): IBD ran at ~3000 BPS to h=201k, then validation
                //     stalled on a download issue. With no cap, valres_rx + staged BTreeMap +
                //     pending_blocks grew unbounded, anon-rss hit 10.6 GB, kernel OOM-killed
                //     blvm. Cap is also a pipeline throttle, not just memory throttle.
                //
                // The safety valve in `run_validation_worker_shared` (30 s spin ceiling)
                // protects us from an absolute deadlock if retire ever goes truly wedged
                // (e.g. RocksDB compaction stall). A backpressure-release does temporarily
                // overshoot the cap, but the next retire iteration drains it within seconds
                // under normal pressure.
                1_500_000
            } else {
                1_000_000
            }
        });
    // Live cap that the validation workers read on every backpressure check. The retire
    // loop's `adapt_max_pending_ops_tick` mutates this atomic at most every ~500 ms based
    // on observed RSS pressure (`memory::ibd_pressure_*`) and drain headroom
    // (`pending_len` vs cap).  When `nominal == 0` (≥32 GiB hosts), backpressure is off
    // and adaptation is a no-op.
    let max_pending_ops: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(max_pending_ops_nominal));
    let max_pending_ops_last_adapt_ms: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    if max_pending_ops_nominal > 0 {
        info!(
            "IBD: pending-ops backpressure active (max_pending_ops={}, adaptive: shrinks under \
             RSS pressure; calm growth up to ~1.1× nominal, Emergency floor max(nominal/16, 100k) — \
             see adapt_max_pending_ops_tick)",
            max_pending_ops_nominal
        );
    } else {
        info!("IBD: pending-ops backpressure disabled (32 GiB+ tier — full pipeline fits in RAM)");
    }

    // Background retire dispatcher: 1..N retire threads, sharded by `height % N`.
    // Each shard runs the same retire loop body as before; only the cursor wiring
    // differs (`local_last_retired` per shard, `publisher` recomputes the global min).
    // `_retire_dispatcher` is held by the outer scope until shutdown — dropping it
    // closes all senders and joins all retire threads.
    #[cfg(all(feature = "utxo-commitments", feature = "production"))]
    let mut _retire_dispatcher = {
        let staged_outer = Arc::clone(&staged);
        let staged_count_outer = Arc::clone(&staged_count);
        let store_outer = Arc::clone(&ibd_store_v2_for_validation);
        let mem_mtx_outer = Arc::clone(&mem_mtx);
        let utxo_flush_handles_outer = Arc::clone(&utxo_flush_handles);
        let retire_flush_counter_outer = Arc::clone(&retire_flush_counter);
        let max_ahead_live_outer = Arc::clone(&max_ahead_live);
        let blockstore_outer = Arc::clone(&blockstore);
        let ctree_outer = commitment_tree_shared.clone();
        let cst_outer = commitment_store_opt.clone();
        let storage_wm_outer = Arc::clone(&storage_for_retire);
        let ibd_mh_outer = Arc::clone(&ibd_muhash_accumulator);
        let retire_err_outer = Arc::clone(&retire_err);
        let mpo_outer = Arc::clone(&max_pending_ops);
        let mpo_last_outer = Arc::clone(&max_pending_ops_last_adapt_ms);
        super::retire_dispatcher::RetireDispatcher::spawn(
            num_retire_shards,
            start_height.saturating_sub(1),
            |i, work_rx, local_last_retired, publisher| {
                let staged = Arc::clone(&staged_outer);
                let staged_count = Arc::clone(&staged_count_outer);
                let store = Arc::clone(&store_outer);
                let mem_mtx = Arc::clone(&mem_mtx_outer);
                let utxo_flush_handles = Arc::clone(&utxo_flush_handles_outer);
                let retire_flush_counter = Arc::clone(&retire_flush_counter_outer);
                let max_ahead_live = Arc::clone(&max_ahead_live_outer);
                let blockstore = Arc::clone(&blockstore_outer);
                let ctree = ctree_outer.clone();
                let cst = cst_outer.clone();
                let storage_wm = Arc::clone(&storage_wm_outer);
                let ibd_mh = Arc::clone(&ibd_mh_outer);
                let retire_err = Arc::clone(&retire_err_outer);
                let mpo = Arc::clone(&mpo_outer);
                let mpo_last = Arc::clone(&mpo_last_outer);
                std::thread::Builder::new()
                    .name(format!("ibd-retire-{i}"))
                    .spawn(move || {
                        run_ibd_retire_loop_with_commitment(
                            work_rx,
                            staged,
                            staged_count,
                            local_last_retired,
                            publisher,
                            store,
                            storage_wm,
                            mem_mtx,
                            max_ahead_live,
                            nominal_max_ahead,
                            ibd_defer_flush,
                            ibd_defer_checkpoint,
                            max_utxo_flushes_under_pressure,
                            utxo_flush_handles,
                            retire_flush_counter,
                            retire_err,
                            blockstore,
                            ctree,
                            cst,
                            ibd_mh,
                            mpo,
                            max_pending_ops_nominal,
                            mpo_last,
                        );
                    })
                    .expect("spawn IBD retire shard")
            },
        )
    };
    #[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
    let mut _retire_dispatcher = {
        let staged_outer = Arc::clone(&staged);
        let staged_count_outer = Arc::clone(&staged_count);
        let store_outer = Arc::clone(&ibd_store_v2_for_validation);
        let mem_mtx_outer = Arc::clone(&mem_mtx);
        let utxo_flush_handles_outer = Arc::clone(&utxo_flush_handles);
        let retire_flush_counter_outer = Arc::clone(&retire_flush_counter);
        let max_ahead_live_outer = Arc::clone(&max_ahead_live);
        let storage_wm_outer = Arc::clone(&storage_for_retire);
        let ibd_mh_outer = Arc::clone(&ibd_muhash_accumulator);
        let retire_err_outer = Arc::clone(&retire_err);
        let mpo_outer = Arc::clone(&max_pending_ops);
        let mpo_last_outer = Arc::clone(&max_pending_ops_last_adapt_ms);
        super::retire_dispatcher::RetireDispatcher::spawn(
            num_retire_shards,
            start_height.saturating_sub(1),
            |i, work_rx, local_last_retired, publisher| {
                let staged = Arc::clone(&staged_outer);
                let staged_count = Arc::clone(&staged_count_outer);
                let store = Arc::clone(&store_outer);
                let mem_mtx = Arc::clone(&mem_mtx_outer);
                let utxo_flush_handles = Arc::clone(&utxo_flush_handles_outer);
                let retire_flush_counter = Arc::clone(&retire_flush_counter_outer);
                let max_ahead_live = Arc::clone(&max_ahead_live_outer);
                let storage_wm = Arc::clone(&storage_wm_outer);
                let ibd_mh = Arc::clone(&ibd_mh_outer);
                let retire_err = Arc::clone(&retire_err_outer);
                let mpo = Arc::clone(&mpo_outer);
                let mpo_last = Arc::clone(&mpo_last_outer);
                std::thread::Builder::new()
                    .name(format!("ibd-retire-{i}"))
                    .spawn(move || {
                        run_ibd_retire_loop_no_commitment(
                            work_rx,
                            staged,
                            staged_count,
                            local_last_retired,
                            publisher,
                            store,
                            storage_wm,
                            mem_mtx,
                            max_ahead_live,
                            nominal_max_ahead,
                            ibd_defer_flush,
                            ibd_defer_checkpoint,
                            max_utxo_flushes_under_pressure,
                            utxo_flush_handles,
                            retire_flush_counter,
                            retire_err,
                            ibd_mh,
                            mpo,
                            max_pending_ops_nominal,
                            mpo_last,
                        );
                    })
                    .expect("spawn IBD retire shard")
            },
        )
    };

    // Public `last_retired` exposed to validation workers (for backpressure) and to all
    // existing call-sites is the dispatcher's `global_last_retired = min(local across shards)`.
    // For N=1 this is bit-identical to the old single atomic.
    let last_retired: Arc<AtomicU64> = Arc::clone(_retire_dispatcher.global_last_retired());

    // ── N-parallel validation worker pool ───────────────────────────────────
    // `BLVM_IBD_MAX_PARALLEL` overrides. Otherwise default scales with **RAM**:
    // low-memory hosts stay at half-cores (capped) to limit RSS; 32+ GiB hosts
    // use most logical CPUs so heavy post-300k blocks keep CPU saturated.
    let n_validate_workers: usize = std::env::var("BLVM_IBD_MAX_PARALLEL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let total_gb = MemoryGuard::total_gb_rounded(system_total_ram_mb);
            if total_gb >= 32 {
                cpus.saturating_sub(1).clamp(4, 24)
            } else if total_gb >= 24 {
                (cpus * 3 / 4).clamp(2, 16)
            } else if total_gb >= 16 {
                // At h=450k+ the bottleneck shifts to CPU-bound ECDSA verification, not memory.
                // The adaptive cache cap handles RSS automatically. Use cpus-2 to leave headroom
                // for coordinator, retire, and prefetch threads while maximising validation
                // parallelism. On 12-core/16GB this raises workers 8 → 10 (+25% BPS ceiling).
                cpus.saturating_sub(2).clamp(4, 16)
            } else {
                (cpus / 2).clamp(1, 6)
            }
        });

    // Pipeline depth (max in-flight blocks) is **decoupled** from worker count. With
    // pipeline_depth == n_workers, a single slow block (cache-miss → 80ms view-build) at the
    // head of the in-order queue idles all N-1 workers waiting for the orchestrator to advance.
    // We run N workers but allow a **deeper** job queue than N: workers stay
    // saturated and out-of-order completions buffer in `pending_results` until the head retires.
    //
    // Default = 32, clamped to [n_validate_workers, 64], then floored at n_validate_workers.
    // Each in-flight slot holds:
    //   - one `Arc<Block>` (refcount bump)
    //   - the pre-fetched UTXO map for that block (~few hundred KB at h=300k)
    //   - one small Arc-clone snapshot (`spec_adds_snapshot`)
    // 32 in-flight slots ≈ ~10–20 MB additional RAM (negligible vs the multi-GB UTXO cache).
    //
    // Override via `BLVM_IBD_PIPELINE_DEPTH`. Floor at `n_validate_workers`: a deeper pipeline
    // never hurts, but a pipeline shallower than the worker pool wastes worker threads.
    //
    // **Out-of-order `apply_utxo_delta`:** workers may commit height H+k before H. Flush batches
    // are therefore **height-capped** to the block the retire thread is processing (see
    // `IbdUtxoStore::drain_pending_through_height`) so `ibd_utxo_watermark` never skips ahead of
    // a sequentially valid durable UTXO set.
    //
    // Secondary concern: deep pipelines increase same-batch ADD/DELETE dedup on the same key;
    // keeping depth modest still reduces `in_flight_insertions` edge cases (see pack_flush_package).
    let n_pipeline_depth: usize = std::env::var("BLVM_IBD_PIPELINE_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| 32_usize.clamp(n_validate_workers, 64))
        .max(n_validate_workers);

    info!(
        "IBD: n_validate_workers={} pipeline_depth={}",
        n_validate_workers, n_pipeline_depth
    );

    // crossbeam_channel: native multi-consumer recv() with no Mutex — workers can dequeue concurrently.
    // unbounded() so dispatcher never blocks on a slow worker; the in-flight cap enforces backpressure.
    let (valjob_tx, valjob_rx) = crossbeam_channel::unbounded::<ValidateJob>();
    let (valres_tx, valres_rx) = mpsc::channel::<ValidateResult>();
    let mut _validate_workers: Vec<JoinHandle<()>> = Vec::with_capacity(n_validate_workers);
    for i in 0..n_validate_workers {
        let rx = valjob_rx.clone();
        let tx = valres_tx.clone();
        let pi = Arc::clone(&parallel_ibd);
        let bs = Arc::clone(&blockstore);
        let pr = Arc::clone(&protocol);
        let st = Arc::clone(&ibd_store_v2_for_validation);
        let lr = Arc::clone(&last_retired);
        let mpo = Arc::clone(&max_pending_ops);
        _validate_workers.push(
            std::thread::Builder::new()
                .name(format!("ibd-validate-{i}"))
                .spawn(move || run_validation_worker_shared(rx, tx, pi, bs, pr, st, lr, mpo))
                .expect("spawn IBD validate worker"),
        );
    }
    drop(valjob_rx); // workers hold all live Receiver clones; dropping the prototype lets shutdown propagate
    drop(valres_tx); // workers hold all live Sender clones
                     // ────────────────────────────────────────────────────────────────────────

    // ────────────────────────────────────────────────────────────────────────
    // Watchdog: log full pipeline state if validation_height stops advancing.
    //
    // Without this, a wedged validation loop produces zero log output until the
    // coordinator stall watchdog fires (and that only sees the download side).
    // The pipeline can deadlock at any of: worker pending-ops spin, valres_rx.recv()
    // (no result coming), block-flush handle.join(), retire pushing to RocksDB
    // under stop-write throttling, etc. This watchdog observes the height atomic
    // every 30 s and dumps the queue/cap/handle state when it sees a freeze, so
    // the next post-mortem can pinpoint which stage is stuck instead of guessing.
    let watchdog_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_handle = {
        let validation_height = Arc::clone(&validation_height);
        let store = Arc::clone(&ibd_store_v2_for_validation);
        let max_pending_ops_w = Arc::clone(&max_pending_ops);
        let staged_count_w = Arc::clone(&staged_count);
        let utxo_flush_handles_w = Arc::clone(&utxo_flush_handles);
        let staged_w = Arc::clone(&staged);
        let shutdown = Arc::clone(&watchdog_shutdown);
        let feeder_state_w = Arc::clone(&feeder_state);
        std::thread::Builder::new()
            .name("ibd-validation-watchdog".into())
            .spawn(move || {
                const POLL: std::time::Duration = std::time::Duration::from_secs(10);
                const FREEZE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(30);
                let mut last_height = validation_height.load(Ordering::Relaxed);
                let mut last_change = std::time::Instant::now();
                let mut last_log = std::time::Instant::now() - FREEZE_THRESHOLD;
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(POLL);
                    let h = validation_height.load(Ordering::Relaxed);
                    if h != last_height {
                        last_height = h;
                        last_change = std::time::Instant::now();
                        continue;
                    }
                    if last_change.elapsed() < FREEZE_THRESHOLD {
                        continue;
                    }
                    // Throttle to one snapshot per FREEZE_THRESHOLD even if poll is faster.
                    if last_log.elapsed() < FREEZE_THRESHOLD {
                        continue;
                    }
                    last_log = std::time::Instant::now();
                    let pending = store.pending_len();
                    let cap = max_pending_ops_w.load(Ordering::Relaxed);
                    let staged_n = staged_count_w.load(Ordering::Relaxed);
                    // Use try_lock to avoid watchdog deadlocking if these mutexes are held
                    // by a thread stuck in the shutdown/flush path (which would cause the
                    // watchdog to silently hang and never log the freeze diagnostic).
                    let utxo_flushes = utxo_flush_handles_w
                        .try_lock()
                        .map(|g| g.len())
                        .unwrap_or(usize::MAX);
                    let cache = store.len();
                    let inflight = store.in_flight_len();
                    let staged_lo_hi = staged_w
                        .try_lock()
                        .map(|g| {
                            let lo = g.keys().next().copied();
                            let hi = g.keys().next_back().copied();
                            (lo, hi, g.len())
                        })
                        .unwrap_or((None, None, usize::MAX));
                    let feeder_buf_len = feeder_state_w
                        .0
                        .try_lock()
                        .map(|g| g.0.len())
                        .unwrap_or(usize::MAX);
                    warn!(
                        "[IBD_WATCHDOG] validation frozen at h={} for {}s — pending={}/{} staged_count={} \
                         staged_btreemap=(lo={:?},hi={:?},len={}) feeder_buffer={} \
                         utxo_flush_in_flight={} cache_entries={} inflight_insertions={} \
                         (likely cause: workers parked on pending cap, retire wedged on RocksDB \
                         flush_disk, block-flush handle saturated, or premature IBD break — \
                         MAX values indicate mutex was contended/unavailable)",
                        h,
                        last_change.elapsed().as_secs(),
                        pending,
                        cap,
                        staged_n,
                        staged_lo_hi.0,
                        staged_lo_hi.1,
                        staged_lo_hi.2,
                        feeder_buf_len,
                        utxo_flushes,
                        cache,
                        inflight,
                    );
                }
            })
            .expect("spawn ibd-validation-watchdog")
    };

    loop {
        // === DISPATCH PHASE: fill pipeline up to pipeline_depth_live ===
        // BIP30 adjacency guard: the two exceptional heights on mainnet (91722, 91842)
        // require sequential BIP30 state propagation — force depth=1 to serialize through them.
        // Otherwise pipeline_depth controls how far ahead the dispatcher can run, while
        // n_validate_workers controls how many of those in-flight blocks execute concurrently.
        let pipeline_depth_live: usize = if (91710..=91855).contains(&next_validation_height) {
            1
        } else {
            n_pipeline_depth
        };

        // Backpressure: when retire is stalling (e.g. during a durability flush_disk call),
        // staged accumulates one Arc<UtxoDelta> per completed block. Each entry holds ~700 KB
        // of UTXO data at late heights; with validation at 500 BPS a 3-second stall fills
        // ~1500 entries ≈ 1 GB. Cap the backlog strictly so the orchestrator pauses dispatch
        // rather than flooding staged.
        //
        // NO in_flight.is_empty() bypass here. The old bypass dispatched ONE block per
        // iteration when staged >= cap and in_flight was empty. Because each dispatched block
        // completes and re-inserts into staged, staged grew without bound when retire was
        // stuck (observed: 32510 entries × 480 KB = 15+ GB → OOM at h≈214k). The bypass
        // was originally added to prevent a premature `break` below; that is now handled
        // by the break guard checking staged_count > 0 (see below).
        //
        // Cap sized so staged never becomes a major RSS consumer:
        //   h=82k  : ~10 KB/entry  → 256 × 10 KB  = 2.6 MB  (negligible)
        //   h=218k : ~480 KB/entry → 256 × 480 KB  = 123 MB  (safe)
        //   h=600k : ~700 KB/entry → 256 × 700 KB  = 179 MB  (safe)
        // 256 minimum (was 128): at h>270k staged_count was pegged at 158 > old cap=128,
        // keeping the dispatcher paused while workers idled. With 256, dispatcher runs
        // longer before pausing, reducing worker idle time during retire's drain cycles.
        let staged_dispatch_cap = (n_pipeline_depth * 4).max(256);
        // Snapshot once per orchestrator iteration (Relaxed load, ~1 atomic read per loop).
        let staged_now = staged_count.load(Ordering::Relaxed);

        while in_flight.len() < pipeline_depth_live && staged_now < staged_dispatch_cap {
            let is_first = in_flight.is_empty();

            // Get next block: blocking if no in-flight work, non-blocking otherwise.
            let block_tuple_opt = if is_first {
                const FEEDER_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                let next_block = loop {
                    let mut guard = feeder_state.0.lock();
                    if let Some((arc_b, w, input_keys, u, tx_ids, spec_adds, est_bytes)) =
                        guard.0.remove(next_validation_height)
                    {
                        guard.2 = guard.2.saturating_sub(est_bytes);
                        feeder_state.1.notify_one();
                        break Some((
                            next_validation_height,
                            arc_b,
                            w,
                            input_keys,
                            u,
                            tx_ids,
                            spec_adds,
                            est_bytes,
                        ));
                    }
                    if guard.1 && guard.0.is_empty() {
                        break None;
                    }
                    #[cfg(feature = "profile")]
                    let wait_start = std::time::Instant::now();
                    let wait = feeder_state.1.wait_for(&mut guard, FEEDER_WAIT_TIMEOUT);
                    #[cfg(feature = "profile")]
                    if ibd_profile {
                        let wait_ms = wait_start.elapsed().as_millis() as u64;
                        if wait_ms >= 1 {
                            let buffer_len_after = guard.0.len();
                            let ts_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            blvm_protocol::profile_log!(
                                "[IBD_STALL_WAIT] next_height={} duration_ms={} buffer_after={} ts_ms={}",
                                next_validation_height, wait_ms, buffer_len_after, ts_ms
                            );
                        }
                    }
                    if wait.timed_out() {
                        let cur_min = guard.0.min_buffered_height();
                        warn!(
                            "[IBD_STALL] Validation waiting for block {} (buffer has {} blocks, min_height={:?}) — coordinator/feeder may be blocked",
                            next_validation_height, guard.0.len(), cur_min
                        );
                        let _ = stall_tx.send(next_validation_height);
                    }
                };
                next_block
            } else {
                // Non-blocking: grab lookahead block only if already in feeder.
                let next_h = next_validation_height;
                let mut guard = feeder_state.0.lock();
                guard
                    .0
                    .remove(next_h)
                    .map(|(arc_b, w, ik, u, tx_ids, spec_adds, est_bytes)| {
                        guard.2 = guard.2.saturating_sub(est_bytes);
                        feeder_state.1.notify_one();
                        (next_h, arc_b, w, ik, u, tx_ids, spec_adds, est_bytes)
                    })
            };

            let (
                h,
                block_arc_d,
                witnesses_d,
                mut input_keys_from_feeder,
                prefetched_utxos_d,
                tx_ids_precomputed_d,
                spec_adds_d,
                feeder_est_bytes_d,
            ) = match block_tuple_opt {
                None => break,
                Some(t) => t,
            };
            if blocks_synced == 0 && in_flight.is_empty() {
                info!("Validation: first block received, height {}", h);
            }

            // 4d: Lookahead blocks buffer for dynamic eviction protect_keys.
            let need_blocks_buf = ibd_store_v2_for_validation.is_dynamic_eviction();
            if need_blocks_buf {
                blocks_buf.clear();
                let guard = feeder_state.0.lock();
                let prefetch_look = utxo_prefetch_lookahead_live
                    .load(Ordering::Relaxed)
                    .clamp(1, 128);
                for off in 1..=prefetch_look {
                    let bh = h + off as u64;
                    if let Some((b, _, _, _, _, _, _)) = guard.0.get(bh) {
                        blocks_buf.push(Arc::clone(b));
                    }
                }
            }

            // `witnesses_d` is `SharedWitnesses = Arc<Vec<Vec<Witness>>>` from the feeder;
            // no Arc::new() needed — reuse the same Arc that was allocated at download time.
            let witnesses_storage_d: Arc<Vec<Vec<Witness>>> = if witnesses_d.is_empty() {
                shared_empty_witness_stacks(block_arc_d.transactions.len())
            } else if witnesses_d.len() != block_arc_d.transactions.len() {
                return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                    Ok(()) => Err(anyhow::anyhow!(
                        "Witness count mismatch at height {}: {} witnesses for {} transactions",
                        h,
                        witnesses_d.len(),
                        block_arc_d.transactions.len()
                    )),
                    Err(e) => Err(e),
                };
            } else {
                witnesses_d
            };

            // Dispatch: snapshot only, view-build runs on the worker.
            // Extract input keys (cheap; uses precomputed feeder keys when available).
            if input_keys_from_feeder.is_empty() {
                // Feeder already computed tx_ids for this block; avoid `compute_block_tx_ids` +
                // a fresh `Vec<Hash>` alloc inside `block_input_keys_into_filtered`.
                block_input_keys_into_filtered_with_tx_ids(
                    block_arc_d.as_ref(),
                    tx_ids_precomputed_d.as_slice(),
                    &mut keys_v2_buf,
                );
            } else {
                std::mem::swap(&mut keys_v2_buf, &mut input_keys_from_feeder);
            }
            if h <= 200 {
                debug!(
                    "[IBD_V2] height={} keys_needed={} store_len={}",
                    h,
                    keys_v2_buf.len(),
                    ibd_store_v2_for_validation.len()
                );
            }

            // Spec snapshot: shallow Arc clones of in-flight blocks' speculative additions.
            // List grows with pipeline_depth (max pipeline_depth_live - 1); the pre-alloc buf
            // is sized for the configured depth so reuse is cheap.
            //
            // staged_snapshot was eliminated: every staged block has already called
            // `worker_cache_put_protected` so its outputs live in the cache (DashMap) and
            // are protected from eviction by `worker_preinserted` until disk flush. The
            // worker hits them via a single `cache_get` (lock-free shard) — fully replaces
            // the O(staged.len() × still_missing) walk that dominated `utxo_base_ms` (~100
            // ms/block when retire was behind).
            spec_adds_snapshot_buf.clear();
            spec_adds_snapshot_buf.extend(spec_adds.iter().map(|(sh, set)| (*sh, Arc::clone(set))));
            // Move the filled buffer into the job; replace with a fresh pre-sized buf for the
            // next dispatch. Avoids the second Vec alloc + Arc-tuple memcpy from .clone().
            let spec_adds_snapshot =
                std::mem::replace(&mut spec_adds_snapshot_buf, Vec::with_capacity(64));

            // Optional debug/profile snapshot (rare, off by default).
            if let Some(ref base) = snapshot_dir_base {
                const SNAPSHOT_HEIGHTS: &[u64] = &[
                    50_000, 90_000, 125_000, 133_000, 145_000, 175_000, 181_000, 190_000, 200_000,
                ];
                if SNAPSHOT_HEIGHTS.contains(&h) {
                    let utxo_set = ibd_store_v2_for_validation.to_utxo_set_snapshot();
                    ParallelIBD::dump_ibd_snapshot(
                        h,
                        block_arc_d.as_ref(),
                        witnesses_storage_d.as_slice(),
                        &utxo_set,
                        base,
                    );
                }
            }

            recent_snap_buf.clear();
            recent_snap_buf.extend(recent_headers_buf.iter().cloned());
            let recent_snap = std::mem::replace(&mut recent_snap_buf, Vec::with_capacity(12));
            // Per-job wall clock for header validation (reject future blocks). Cheap vs ECDSA work.
            let cached_network_time = current_timestamp();

            // Speculative additions (all outputs block h creates) were precomputed on the
            // prefetch worker pool — see `prefetch::build_spec_adds`. Here we only do a cheap
            // pointer-bump into the in-flight queue. (Was a per-block ~O(outputs) HashMap +
            // Arc::new(UTXO) loop on this single-threaded dispatcher; that work is now N-way
            // parallel on the prefetch pool instead of this dispatcher.)
            let spec_arc: Arc<UtxoSet> = spec_adds_d;
            // Track spec_adds bytes for MemoryGuard budget: each UTXO is ~64 bytes on heap.
            let spec_entry_bytes = (spec_arc.len() as u64).saturating_mul(64);
            spec_adds_bytes.fetch_add(spec_entry_bytes, Ordering::Relaxed);
            spec_adds.insert(h, Arc::clone(&spec_arc));

            // Take the keys vec out of the dispatcher's reusable buffer.
            // keys_for_job is moved into the worker job; InFlightEntry no longer stores a
            // clone — if validation fails we re-derive keys from the block on the error path.
            let keys_for_job: Vec<OutPointKey> = std::mem::take(&mut keys_v2_buf);

            // Past the BIP30 exceptional range (91710..=91855) no duplicate coinbase
            // txids can occur in a valid Bitcoin chain. Sending an empty index to each
            // worker is correct: the check always passes, and we avoid cloning a
            // potentially large FxHashMap<Hash, usize> × pipeline_depth times per block.
            // Within the range we must clone the live state because workers update it
            // in-place (sequential at depth=1, but each job needs a snapshot).
            let bip30_for_job: Bip30Index = if h > 91855 {
                Bip30Index::default()
            } else {
                bip30_index.clone()
            };

            let job_send = valjob_tx.send(ValidateJob {
                height: h,
                block_arc: Arc::clone(&block_arc_d),
                witnesses_storage: Arc::clone(&witnesses_storage_d),
                bip30_index: bip30_for_job,
                recent_headers: recent_snap,
                tx_ids: tx_ids_precomputed_d,
                cached_network_time,
                keys: keys_for_job,
                spec_adds_snapshot,
                prefetched: prefetched_utxos_d,
            });
            if job_send.is_err() {
                return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                    Ok(()) => Err(anyhow::anyhow!(
                        "IBD validate workers stopped (failed to send job at height {})",
                        h
                    )),
                    Err(e) => Err(e),
                };
            }
            in_flight.push_back(InFlightEntry {
                height: h,
                block_arc: block_arc_d,
                witnesses_storage: witnesses_storage_d,
                feeder_est_bytes: feeder_est_bytes_d,
                utxo_base_ms: 0,
                utxo_base_tune_ms: 0,
                prefetch_ms: 0,
                apply_pending_ms: 0,
                input_keys: None,
            });
            next_validation_height = h + 1;
        } // end dispatch while

        // Terminate when feeder is exhausted, the validation pipeline is empty, AND the
        // retire thread has processed all staged blocks. Checking staged_count prevents:
        //
        // (a) Premature break: old code exited whenever in_flight was empty, even if staged
        //     had thousands of unretired blocks (causing corrupted UTXO state or missed work).
        //     This was the original motivation for the in_flight.is_empty() bypass in the
        //     dispatch while loop above — that bypass is now removed since we wait here.
        //
        // (b) OOM via bypass: the bypass dispatched 1 block per iteration when staged >= cap
        //     and in_flight was empty, growing staged unboundedly while retire was stuck
        //     (observed: 32510 entries × 480 KB = 15+ GB → OOM at h≈214k on 16 GiB hosts).
        //
        // When in_flight = 0 but staged > 0: retire is still processing. Sleep 10 ms to
        // avoid a busy spin, then re-enter the dispatch loop. When both are zero, IBD done.
        if in_flight.is_empty() {
            let staged_remaining = staged_count.load(Ordering::Relaxed);
            if staged_remaining == 0 {
                break;
            }
            // Retire still has work. Yield CPU briefly, then re-check.
            // Also bail out if retire errored (it won't drain staged, so we'd spin forever).
            if retire_err.lock().is_some() {
                return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                    Ok(()) => Err(anyhow::anyhow!(
                        "IBD retire thread failed while waiting for staged drain (staged_remaining={})",
                        staged_remaining
                    )),
                    Err(e) => Err(e),
                };
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        // === COLLECT PHASE: wait for the next in-order result ===
        // SAFETY: in_flight is non-empty — the is_empty() guard above would have continued.
        let next_process_h = in_flight
            .front()
            .ok_or_else(|| anyhow::anyhow!("IBD coordinator: in_flight empty at collect phase"))?
            .height;
        // Drain any results that arrived out of order.
        while let Ok(vres) = valres_rx.try_recv() {
            // Early spec_adds drop: once worker_cache_put_protected has run (on the worker,
            // before sending the result), this block's outputs are in the DashMap cache.
            // Future workers dispatched at higher heights will find them via cache_get —
            // the spec_adds entry is no longer needed and can be freed now.
            if let Some(set) = spec_adds.remove(&vres.height) {
                let freed = (set.len() as u64).saturating_mul(64);
                spec_adds_bytes.fetch_sub(
                    freed.min(spec_adds_bytes.load(Ordering::Relaxed)),
                    Ordering::Relaxed,
                );
            }
            pending_results.insert(vres.height, vres);
        }
        // Blocking wait until we have the result for the front-of-queue entry.
        while !pending_results.contains_key(&next_process_h) {
            match valres_rx.recv() {
                Ok(vres) => {
                    if let Some(set) = spec_adds.remove(&vres.height) {
                        let freed = (set.len() as u64).saturating_mul(64);
                        spec_adds_bytes.fetch_sub(
                            freed.min(spec_adds_bytes.load(Ordering::Relaxed)),
                            Ordering::Relaxed,
                        );
                    }
                    pending_results.insert(vres.height, vres);
                }
                Err(_) => {
                    return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                        Ok(()) => Err(anyhow::anyhow!(
                            "IBD validate workers disconnected at height {}",
                            next_process_h
                        )),
                        Err(e) => Err(e),
                    };
                }
            }
        }

        // === EXTRACT PER-BLOCK VARIABLES FROM IN-FLIGHT ENTRY ===
        let mut entry = in_flight.pop_front().ok_or_else(|| {
            anyhow::anyhow!(
                "IBD coordinator: in_flight drained unexpectedly at height {next_process_h}"
            )
        })?;
        let vres = pending_results.remove(&next_process_h).ok_or_else(|| {
            anyhow::anyhow!("IBD coordinator: result missing for height {next_process_h}")
        })?;
        // Safety-net: if the early-drop (on result reception) missed an entry, clean it up now.
        // With the early drop path, this should rarely fire (entry is normally already gone).
        while spec_adds
            .first_key_value()
            .map(|(sh, _)| *sh <= next_process_h)
            .unwrap_or(false)
        {
            if let Some((_, set)) = spec_adds.pop_first() {
                let freed = (set.len() as u64).saturating_mul(64);
                spec_adds_bytes.fetch_sub(
                    freed.min(spec_adds_bytes.load(Ordering::Relaxed)),
                    Ordering::Relaxed,
                );
            }
        }

        let next_height = entry.height;
        let block_arc = entry.block_arc.clone();
        let witnesses_storage = entry.witnesses_storage.clone();
        let feeder_est_bytes = entry.feeder_est_bytes;
        // View-build now happens inside the worker; tune EMA from the worker-reported time.
        let utxo_base_ms = vres.view_build_ms;
        let utxo_base_tune_ms_holder = vres.view_build_ms;
        let prefetch_ms = entry.prefetch_ms;
        let apply_pending_ms = entry.apply_pending_ms;
        // keys_v2_buf re-derivation is deferred to the error path. The previous code
        // unconditionally walked every transaction in every block on the dispatcher thread
        // to fill keys_v2_buf "for the dump_failed_block error path" — but the dispatcher
        // is single-threaded and this duplicate walk was hot per-block CPU that delayed
        // the next dispatch. On the (rare) validation-error path, we recompute the keys
        // there before dump_failed_block.
        keys_v2_buf.clear();
        let witnesses_to_use: &[Vec<Witness>] = witnesses_storage.as_slice();

        // Only propagate the returned BIP30 state while within the exceptional range.
        // Workers past h=91855 received Bip30Index::default() and updated it with just
        // one block's worth of entries — accepting that back would evict our accumulated
        // state. Once the range is cleared we also free the index memory since it is
        // no longer referenced for anything.
        if vres.height <= 91855 {
            bip30_index = vres.bip30_post;
            if vres.height == 91855 {
                bip30_index.clear();
                bip30_index.shrink_to_fit();
                info!(
                    "[IBD] BIP30 exceptional range complete at h=91855 — cleared BIP30 index \
                       (eliminates per-dispatch clone cost for remaining ~{}k blocks)",
                    (700_000u64.saturating_sub(91855)) / 1000
                );
            }
        }
        let validation_time = vres.elapsed;
        // vres.result carries only Option<UtxoDelta> — tx ids are not propagated.
        let validation_result = vres.result;

        #[cfg(feature = "profile")]
        let ibd_log_this_height =
            ibd_blocked_log && ibd_profile_height_matches_sample(ibd_profile_sample, next_height);
        #[cfg(feature = "profile")]
        if ibd_log_this_height {
            blvm_protocol::profile_log!(
                "[IBD_VALIDATION] height={} phase=start (validate+suggested sync)",
                next_height
            );
        }

        // === STAGE + RETIRE ===
        let (sync_ms, evict_ms, utxo_flush_batch, rss_pressure, apply_utxo_ms, validation_result) =
            match validation_result {
                Ok(utxo_delta_opt) => {
                    let delta = Arc::new(utxo_delta_opt.unwrap_or_else(empty_utxo_delta));
                    {
                        let mut m = staged.lock();
                        m.insert(next_height, delta);
                    }
                    staged_count.fetch_add(1, Ordering::Relaxed);
                    // Only the dynamic-eviction code path inside the retire helper uses
                    // `blocks_buf` (for `protect_keys_for_next_blocks`). In FIFO/LIFO modes
                    // the cloned Vec<Arc<Block>> is pure waste — one Vec alloc + N Arc bumps
                    // per block on the dispatcher critical path. Send an empty Vec instead.
                    let retire_blocks_buf = if ibd_store_v2_for_validation.is_dynamic_eviction() {
                        blocks_buf.clone()
                    } else {
                        Vec::new()
                    };
                    if _retire_dispatcher
                        .send(IbdRetireWork {
                            height: next_height,
                            blocks_buf: retire_blocks_buf,
                            block: Arc::clone(&block_arc),
                        })
                        .is_err()
                    {
                        return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                            Ok(()) => Err(anyhow::anyhow!(
                                "IBD retire thread stopped (failed to send retire work at height {})",
                                next_height
                            )),
                            Err(e) => Err(e),
                        };
                    }
                    (
                        0u64,
                        0u64,
                        None::<PendingFlushPackage>,
                        false,
                        0u64,
                        Ok(None::<UtxoDelta>),
                    )
                }
                Err(e) => (0u64, 0u64, None, false, 0u64, Err(e)),
            };

        let utxo_base_tune_ms = utxo_base_tune_ms_holder;
        #[allow(unused_variables)]
        let gap_fill_ms = 0u64;

        // Lock-free pressure read: retire thread publishes the latest level via
        // `publish_ibd_pressure` after each `should_flush`. Avoids serializing on `mem_mtx`,
        // which retire holds across the heavy apply+evict+flush sequence (the contention
        // there capped dispatcher throughput far below worker capacity at h>300k).
        let ibd_pressure = memory::ibd_pressure_level_snapshot();

        // Prefetch lookahead: EMA on utxo-base build time (no /proc); widen when supplement is slow.
        let ms = utxo_base_tune_ms as f64;
        let ema = match prefetch_base_ema {
            None => {
                prefetch_base_ema = Some(ms);
                ms
            }
            Some(prev) => {
                let n = prev * (63.0 / 64.0) + ms * (1.0 / 64.0);
                prefetch_base_ema = Some(n);
                n
            }
        };
        let mut target = nominal_prefetch_lookahead;
        if ema > 12.0 {
            target = (nominal_prefetch_lookahead + 32).min(128);
        } else if ema > 8.0 {
            target = (nominal_prefetch_lookahead * 4 / 3).min(128);
        } else if ema > 5.0 {
            target = (nominal_prefetch_lookahead + 16).min(128);
        } else if ema < 0.75 && blocks_synced > 1_000 {
            target = nominal_prefetch_lookahead.saturating_sub(8).max(48);
        }
        let with_pressure = dynamic_prefetch_lookahead(ibd_pressure, target);
        utxo_prefetch_lookahead_live.store(with_pressure, Ordering::Relaxed);

        // V2: no pipelined sync (overlay delta applied directly).

        #[cfg(feature = "profile")]
        if ibd_log_this_height {
            blvm_protocol::profile_log!(
                "[IBD_VALIDATION] height={} phase=end utxo_base_ms={} validation_ms={} apply_utxo_ms={} apply_pending_ms={} sync_ms={} evict_ms={}",
                next_height,
                utxo_base_ms,
                validation_time.as_millis(),
                apply_utxo_ms,
                apply_pending_ms,
                sync_ms,
                evict_ms
            );
            if apply_pending_ms > 2 {
                blvm_protocol::profile_log!(
                    "[IBD_BLOCKED] phase=apply_pending height={} duration_ms={} (pending_writes/flushing scan for cache hits)",
                    next_height, apply_pending_ms
                );
            }
            if sync_ms > 5 {
                blvm_protocol::profile_log!(
                    "[IBD_BLOCKED] phase=sync_await height={} duration_ms={} (validation waited for previous block sync+evict)",
                    next_height, sync_ms
                );
            }
        }
        if let Err(ref e) = validation_result {
            error!(
                "Failed to prefetch/validate block at height {}: {}",
                next_height, e
            );
        }

        match validation_result {
            Ok(_utxo_delta) => {
                // Sync/evict already done in block_in_place; UTXO retire runs on `ibd-retire`.
                blocks_synced += 1;
                let n_txs = block_arc.transactions.len();
                let n_inputs: usize = block_arc
                    .transactions
                    .iter()
                    .map(|tx| tx.inputs.len())
                    .sum();

                // Track recent headers for BIP113 MTP (keep last 11). Clone header before moving
                // `block_arc` into `pending_blocks` so flush `Arc::try_unwrap` usually succeeds.
                let header_rc = Arc::new(block_arc.header.clone());
                if !skip_storage {
                    pending_storage_bytes =
                        pending_storage_bytes.saturating_add(feeder_est_bytes as u64);
                    pending_blocks.push((block_arc, Arc::clone(&witnesses_storage), next_height));
                }
                recent_headers_buf.push_back(header_rc);
                if recent_headers_buf.len() > 11 {
                    recent_headers_buf.pop_front();
                }

                // Update shared validation height (allows download workers to track progress)
                validation_height.store(next_height, Ordering::Relaxed);

                // Pure-function variants: pressure-scaled values from the captured base + budget.
                // No `mem_mtx` acquisition on the per-block hot path.
                let flush_interval_live = MemoryGuard::storage_flush_interval_live_for(
                    storage_flush_interval,
                    ibd_pressure,
                );
                let byte_cap = MemoryGuard::storage_flush_pending_bytes_pressure_cap_for(
                    ibd_budget_mb,
                    ibd_pressure,
                );
                let pressure_min_blocks =
                    MemoryGuard::storage_flush_pressure_min_blocks(flush_interval_live);
                let flush_by_interval = pending_blocks.len() >= flush_interval_live;
                let flush_by_pressure_bytes = byte_cap.is_some_and(|cap| {
                    pending_storage_bytes >= cap && pending_blocks.len() >= pressure_min_blocks
                });
                let (flush_ms, flushed_block_count) = if !skip_storage
                    && (flush_by_interval || flush_by_pressure_bytes)
                {
                    let flush_start = std::time::Instant::now();
                    // Fully overlapped async flush: backpressure when at MemoryGuard cap
                    while flush_handles.len() >= max_block_flushes_in_flight {
                        let in_flight = flush_handles.len();
                        let wait_start = std::time::Instant::now();
                        debug!(
                            "[IBD_DEBUG] Block {}: awaiting block storage flush slot (in_flight={}, pending_blocks={})",
                            next_height,
                            in_flight,
                            pending_blocks.len()
                        );
                        let Some(handle) = flush_handles.pop_front() else {
                            return match retire_thread_shutdown(
                                &mut _retire_dispatcher,
                                &retire_err,
                            ) {
                                Ok(()) => Err(anyhow::anyhow!(
                                    "IBD invariant violated: block storage flush wait queue empty under backpressure"
                                )),
                                Err(e) => Err(e),
                            };
                        };
                        match handle.join() {
                            Ok(Ok(())) => {
                                let waited_ms = wait_start.elapsed().as_millis() as u64;
                                debug!(
                                    "[IBD_DEBUG] Block {}: block storage flush slot free (waited {}ms)",
                                    next_height, waited_ms
                                );
                                #[cfg(feature = "profile")]
                                if ibd_blocked_log && waited_ms > 0 {
                                    blvm_protocol::profile_log!(
                                        "[IBD_BLOCKED]                                                 phase=block_flush_await height={} duration_ms={} in_flight={} utxo_flush={} (validation waited for block storage write)",
                                        next_height,
                                        waited_ms,
                                        in_flight,
                                        utxo_flush_handles.lock().len()
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                return match retire_thread_shutdown(
                                    &mut _retire_dispatcher,
                                    &retire_err,
                                ) {
                                    Ok(()) => Err(e),
                                    Err(e2) => Err(e2),
                                };
                            }
                            Err(e) => {
                                return match retire_thread_shutdown(
                                    &mut _retire_dispatcher,
                                    &retire_err,
                                ) {
                                    Ok(()) => Err(anyhow::anyhow!(
                                        "Block storage flush thread panicked: {:?}",
                                        e
                                    )),
                                    Err(e2) => Err(e2),
                                };
                            }
                        }
                    }
                    // UTXO flushes run in parallel (fire-and-forget); no barrier here.
                    // On crash, min(chain_tip, watermark) rewinds to the last safe point.
                    let to_flush = std::mem::take(&mut pending_blocks);
                    pending_storage_bytes = 0;
                    let blockstore_clone = Arc::clone(&blockstore);
                    let storage_for_flush = storage_clone.clone();
                    let to_flush_count = to_flush.len();
                    #[cfg(feature = "profile")]
                    if ibd_profile
                        && ibd_profile_height_matches_sample(ibd_profile_sample, next_height)
                    {
                        blvm_protocol::profile_log!(
                            "[IBD_BLOCK_FLUSH_SPAWN] height={} blocks={} in_flight={}",
                            next_height,
                            to_flush_count,
                            flush_handles.len(),
                        );
                    }
                    flush_handles.push_back(std::thread::spawn(move || {
                        ParallelIBD::do_flush_to_storage(
                            blockstore_clone.as_ref(),
                            Some(&storage_for_flush),
                            to_flush,
                        )
                    }));
                    let flush_elapsed = flush_start.elapsed().as_millis() as u64;
                    debug!(
                        "[IBD_DEBUG] Block {}: spawned block storage flush (blocks={}, in_flight={}, await_took={}ms)",
                        next_height,
                        to_flush_count,
                        flush_handles.len(),
                        flush_elapsed
                    );
                    (flush_elapsed, to_flush_count)
                } else {
                    (0, 0)
                };
                if !skip_storage && pending_blocks.is_empty() && flush_ms > 0 {
                    debug!(
                        "Started async flush ({} blocks, interval_live={}, pressure={:?}, by_bytes={}, {} in flight)",
                        flushed_block_count,
                        flush_interval_live,
                        ibd_pressure,
                        flush_by_pressure_bytes,
                        flush_handles.len()
                    );
                }

                // IBD Profiling: log per-block breakdown when enabled (profile feature)
                // Ready-queue: prefetch_await=0 by design (validation never awaits prefetch).
                #[cfg(feature = "profile")]
                if ibd_profile {
                    let prefetch_await_ms = 0u64; // Ready-queue: no prefetch_await
                    let val_ms = validation_time.as_millis() as u64;
                    let total_ms = prefetch_await_ms
                        + gap_fill_ms
                        + prefetch_ms
                        + utxo_base_ms
                        + val_ms
                        + apply_utxo_ms
                        + sync_ms
                        + evict_ms
                        + flush_ms;
                    let disk_total = prefetch_await_ms
                        + gap_fill_ms
                        + prefetch_ms
                        + sync_ms
                        + evict_ms
                        + flush_ms;
                    let should_log =
                        ibd_profile_height_matches_sample(ibd_profile_sample, next_height)
                            || (ibd_disk_profile
                                && (prefetch_await_ms > 0
                                    || gap_fill_ms > 0
                                    || prefetch_ms > 0
                                    || sync_ms > 0
                                    || evict_ms > 0))
                            || (ibd_profile_slow_ms > 0
                                && (prefetch_await_ms >= ibd_profile_slow_ms
                                    || gap_fill_ms >= ibd_profile_slow_ms
                                    || prefetch_ms >= ibd_profile_slow_ms
                                    || utxo_base_ms >= ibd_profile_slow_ms
                                    || val_ms >= ibd_profile_slow_ms
                                    || apply_utxo_ms >= ibd_profile_slow_ms
                                    || sync_ms >= ibd_profile_slow_ms
                                    || evict_ms >= ibd_profile_slow_ms
                                    || flush_ms >= ibd_profile_slow_ms));
                    if should_log && total_ms > 0 {
                        blvm_protocol::profile_log!(
                            "[IBD_PROFILE] height={} total_ms={} prefetch_await={} gap_fill={} prefetch={} utxo_base={} validation={} apply_utxo={} sync={} evict={} flush_coord={} disk_total={} txs={} inputs={}",
                            next_height,
                            total_ms,
                            prefetch_await_ms,
                            gap_fill_ms,
                            prefetch_ms,
                            utxo_base_ms,
                            val_ms,
                            apply_utxo_ms,
                            sync_ms,
                            evict_ms,
                            flush_ms,
                            disk_total,
                            n_txs,
                            n_inputs
                        );
                        let (dl, ch, ev, _ph) = ibd_store_v2_for_validation.stats();
                        let utxo_stats = (ibd_store_v2_for_validation.len(), dl, ch, ev);
                        blvm_protocol::profile_log!(
                            "[IBD_PIPELINE] height={} utxo_flush={} block_flush={} pending={} utxo_cache={} disk_loads={} cache_hits={} evictions={}",
                            next_height,
                            utxo_flush_handles.lock().len(),
                            flush_handles.len(),
                            pending_blocks.len(),
                            utxo_stats.0,
                            utxo_stats.1,
                            utxo_stats.2,
                            utxo_stats.3
                        );
                    }
                }
            }
            Err(e) => {
                for handle in utxo_flush_handles.lock().drain(..) {
                    let _ = handle.join();
                }
                for handle in flush_handles.drain(..) {
                    let _ = handle.join();
                }
                if !skip_storage && !pending_blocks.is_empty() {
                    let _ = parallel_ibd.flush_pending_blocks(
                        &blockstore,
                        Some(&storage_clone),
                        &mut pending_blocks,
                    );
                }
                error!("Failed to validate block at height {}: {}", next_height, e);
                // Re-derive input keys ONLY on error: the dispatcher hot-path no longer
                // recomputes them per-block. dump_failed_block diagnostics still need the
                // full key list, so build it here.
                block_input_keys_into_filtered(block_arc.as_ref(), &mut keys_v2_buf);
                // Diagnostic: workers now build views, so we can't peek the worker's snapshot.
                // Re-resolve from the cache to flag keys absent at this moment in time.
                {
                    let store = &ibd_store_v2_for_validation;
                    for k in keys_v2_buf.iter() {
                        let in_cache = store.cache_get(k).is_some();
                        if !in_cache {
                            error!(
                                "[IBD_MISSING_UTXO] height={} key={} in_cache=false (not in IbdUtxoStore cache at error time)",
                                next_height,
                                hex::encode(k),
                            );
                        }
                    }
                }
                let utxo_for_dump = ibd_store_v2_for_validation.build_utxo_map(&keys_v2_buf);
                ParallelIBD::dump_failed_block(
                    next_height,
                    block_arc.as_ref(),
                    witnesses_to_use,
                    &utxo_for_dump,
                    &e,
                );
                return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                    Ok(()) => Err(e),
                    Err(e2) => Err(e2),
                };
            }
        }

        // CRITICAL: Yield to the runtime (BLVM_IBD_YIELD_INTERVAL, default 100)
        // Allows download workers to progress; fewer yields = less validation interruption
        if yield_interval > 0 && blocks_synced % yield_interval == 0 {
            #[cfg(feature = "profile")]
            if ibd_profile && ibd_profile_height_matches_sample(ibd_profile_sample, next_height) {
                blvm_protocol::profile_log!(
                    "[IBD_YIELD] blocks_synced={} utxo_flush={} block_flush={} (yielding to runtime)",
                    blocks_synced,
                    utxo_flush_handles.lock().len(),
                    flush_handles.len()
                );
            }
            std::thread::yield_now();
        }

        // Periodic mimalloc page return. 5a: adaptive — every 1000 blocks, or sooner
        // when RSS grew >50MB since last collect (heavy allocation bursts).
        if blocks_synced > 0 && blocks_synced % 500 == 0 {
            #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
            {
                let current_rss_mb = mem_mtx.lock().current_rss_mb();
                let rss_growth_mb = current_rss_mb.saturating_sub(last_rss_mb);
                let blocks_since_collect = blocks_synced.saturating_sub(last_collect_block);
                if rss_growth_mb > 50 || blocks_since_collect >= 1000 {
                    ibd_maybe_heap_trim();
                    last_rss_mb = mem_mtx.lock().current_rss_mb();
                    last_collect_block = blocks_synced;
                }
            }
        }

        // Progress logging: early (1, 10, 100), then every 100 until 10k (so monitors/logs
        // aren't stuck showing ~99 for hundreds of blocks), then every 1000.
        let should_log = blocks_synced == 1
            || blocks_synced == 10
            || blocks_synced == 100
            || (blocks_synced > 100
                && blocks_synced < 10_000
                && blocks_synced % 100 == 0
                && blocks_synced % 1000 != 0)
            || (blocks_synced > 0 && blocks_synced % 1000 == 0);
        if should_log {
            // Don't show BPS at blocks 1, 10: elapsed includes header sync + handshake (~15-20s),
            // which makes rate look absurdly low (1/17 = 0.06 blocks/s). From block 100 we have
            // meaningful validation throughput to measure.
            let total_elapsed = validation_start.elapsed().as_secs_f64();
            let average_rate = if blocks_synced >= 100 && total_elapsed > 0.0 {
                blocks_synced as f64 / total_elapsed
            } else {
                0.0
            };
            // Recent rate: blocks since last status / time since last status. Shows actual burst vs wait.
            // When avg >> recent, we're mostly waiting (download bottleneck). When avg ≈ recent, pipeline is full.
            let blocks_since_last = blocks_synced.saturating_sub(last_log_blocks);
            let recent_elapsed = last_log_instant.elapsed().as_secs_f64();
            let recent_rate = if blocks_since_last > 0 && recent_elapsed > 0.01 {
                blocks_since_last as f64 / recent_elapsed
            } else {
                0.0
            };
            last_log_blocks = blocks_synced;
            last_log_instant = std::time::Instant::now();

            let remaining = effective_end_height.saturating_sub(next_height);
            // Use recent window rate for ETA when available: global average is inflated by
            // the trivially fast pre-SegWit empty blocks and gives a wildly optimistic ETA
            // once the node hits blocks with real UTXO/script work (h>100k).
            let eta_rate = if blocks_synced >= 1000 && recent_rate > 0.0 {
                recent_rate
            } else if average_rate > 0.0 {
                average_rate
            } else {
                f64::INFINITY
            };
            let eta = if eta_rate.is_finite() && eta_rate > 0.0 {
                remaining as f64 / eta_rate
            } else {
                f64::INFINITY
            };
            let buffer_size = feeder_state.0.lock().0.len();

            // Show recent window as primary (current throughput); global avg as secondary context.
            let rate_str = if blocks_synced < 100 {
                "warming up (rate after block 100)".to_string()
            } else if blocks_synced >= 1000 && blocks_since_last > 0 {
                format!("{recent_rate:.1} blocks/s (avg since start: {average_rate:.1} blocks/s)")
            } else {
                format!("{average_rate:.1} blocks/s")
            };
            info!(
                "IBD: {} / {} ({:.1}%) - {} - buffer: {} - ETA: {:.0}s",
                next_height,
                effective_end_height,
                (next_height as f64 / effective_end_height as f64) * 100.0,
                rate_str,
                buffer_size,
                eta
            );
            // Memory diagnostics: log RSS breakdown and data structure sizes
            if blocks_synced % 5000 == 0 {
                let (rss_kb, swap_kb) = {
                    #[cfg(target_os = "linux")]
                    {
                        let rss = std::fs::read_to_string("/proc/self/status")
                            .ok()
                            .and_then(|s| {
                                s.lines()
                                    .find(|l| l.starts_with("VmRSS:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok())
                            })
                            .unwrap_or(0);
                        let swap = std::fs::read_to_string("/proc/self/status")
                            .ok()
                            .and_then(|s| {
                                s.lines()
                                    .find(|l| l.starts_with("VmSwap:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok())
                            })
                            .unwrap_or(0);
                        (rss, swap)
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        (0u64, 0u64)
                    }
                };
                let store_info = format!(
                    "utxo_cache={} pending={} inflight={} recent_prot={} spec_adds={}",
                    ibd_store_v2_for_validation.len(),
                    ibd_store_v2_for_validation.pending_len(),
                    ibd_store_v2_for_validation.in_flight_len(),
                    ibd_store_v2_for_validation.recently_accessed_len(),
                    spec_adds.len(),
                );
                info!(
                    "[MEM] h={} rss={}MB swap={}MB {} feeder={} threads={}",
                    next_height,
                    rss_kb / 1024,
                    swap_kb / 1024,
                    store_info,
                    buffer_size,
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(0)
                );
            }
            // BPS CSV for Core-comparable metrics (height,elapsed_sec) — same format as bitcoin-core-ibd-bench.sh
            if let Some(ref path) = ibd_bps_csv_path {
                let elapsed_sec = validation_start.elapsed().as_secs();
                let create_header = !std::path::Path::new(path).exists()
                    || std::fs::metadata(path)
                        .map(|m| m.len() == 0)
                        .unwrap_or(true);
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    use std::io::Write;
                    if create_header {
                        let _ = writeln!(f, "height,elapsed_sec");
                    }
                    let _ = writeln!(f, "{next_height},{elapsed_sec}");
                }
            }
            #[cfg(feature = "profile")]
            if ibd_profile {
                blvm_protocol::profile_log!(
                    "[IBD_PREFETCH_STATS] height={} utxo_flush={} block_flush={}",
                    next_height,
                    utxo_flush_handles.lock().len(),
                    flush_handles.len()
                );
                if blocks_synced > 0 && blocks_synced % 5000 == 0 {
                    // IBD_UTXO_PATH: cumulative UTXO path stats for overlap/eviction analysis
                    let (dl, ch, ev, ph) = ibd_store_v2_for_validation.stats();
                    blvm_protocol::profile_log!(
                        "[IBD_UTXO_PATH] height={} disk_loads={} cache_hits={} evictions={} pending_hits={} cache_len={} (cumulative since start)",
                        next_height,
                        dl,
                        ch,
                        ev,
                        ph,
                        ibd_store_v2_for_validation.len()
                    );
                }
                if let Some((rss_mb, avail_mb)) = mem_mtx.lock().memory_diag() {
                    blvm_protocol::profile_log!(
                        "[IBD_DIAG] height={} rss_mb={} avail_mb={} utxo_flush={} block_flush={}",
                        next_height,
                        rss_mb,
                        avail_mb,
                        utxo_flush_handles.lock().len(),
                        flush_handles.len()
                    );
                }
            }
        }
    }

    // Signal validation workers to finish, then the retire thread.
    drop(valjob_tx);
    for worker in _validate_workers {
        if let Err(e) = worker.join() {
            warn!("IBD validate worker join error: {:?}", e);
        }
    }
    // Signal retire thread to finish, then take any last flush and join UTXO workers.
    retire_thread_shutdown(&mut _retire_dispatcher, &retire_err)?;

    // Final UTXO flush: drain remaining pending ops, then join all in-flight handles.
    // Collect sub-MuHash accumulators from all async threads (they ran without the global
    // mh_acc mutex) and fold them into the global accumulator before persisting the final
    // checkpoint.
    let mut shutdown_pkg_height: Option<u64> = None;
    if let Some(pkg) = ibd_store_v2_for_validation.take_remaining_flush_package() {
        shutdown_pkg_height = Some(pkg.max_block_height);
        let heights = Arc::clone(&pkg.heights);
        // Pre-compute muhash in the main thread (full rayon pool) before spawning commit thread.
        let prepared = pkg.prepare_for_disk()?;
        drop(pkg);
        let mut local_mh = blvm_muhash::MuHash3072::new();
        ibd_store_v2_for_validation.compute_package_muhash(&prepared, &mut local_mh)?;
        let store_clone = Arc::clone(&ibd_store_v2_for_validation);
        utxo_flush_handles
            .lock()
            .push_back(std::thread::spawn(move || {
                store_clone.flush_prepared_package(&prepared, None)?;
                store_clone.release_protected_heights(&heights);
                store_clone.note_utxo_flush_completed(prepared.max_block_height);
                Ok(local_mh)
            }));
    }
    // Drain all handles, collecting each thread's local MuHash sub-accumulator.
    let mut combined_shutdown_sub_mh = blvm_muhash::MuHash3072::new();
    for handle in utxo_flush_handles.lock().drain(..) {
        match handle.join() {
            Ok(Ok(sub_mh)) => {
                combined_shutdown_sub_mh = combined_shutdown_sub_mh.multiply(&sub_mh);
            }
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!("UTXO flush panicked at shutdown: {:?}", e));
            }
        }
    }
    // Fold all async threads' sub-accumulators into the global (multiply by identity is a no-op
    // so this is safe whether or not any handles had real muhash work to contribute).
    {
        let mut mh_guard = ibd_muhash_accumulator.lock();
        *mh_guard = std::mem::take(&mut *mh_guard).multiply(&combined_shutdown_sub_mh);
    }
    // Persist the final checkpoint only when there was a remaining flush package (the flush_disk
    // + persist_checkpoint pair ensures the watermark advances past the last UTXO batch).
    if let Some(max_height) = shutdown_pkg_height {
        let muhash_running = ibd_muhash_accumulator.lock().serialize_running_state();
        ibd_store_v2_for_validation.flush_disk()?;
        storage_clone
            .chain()
            .persist_ibd_utxo_flush_checkpoint(max_height, &muhash_running)?;
    }
    let last_validated = next_validation_height.saturating_sub(1);
    if let Err(e) = ibd_store_v2_for_validation.flush_disk() {
        warn!(
            "Failed to flush ibd_utxos memtable at final shutdown (height {}): {}",
            last_validated, e
        );
    }

    for handle in flush_handles.drain(..) {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Block storage flush thread panicked: {:?}",
                    e
                ));
            }
        }
    }
    if !skip_storage && !pending_blocks.is_empty() {
        info!("Flushing final {} pending blocks", pending_blocks.len());
        parallel_ibd.flush_pending_blocks(
            &blockstore,
            Some(&storage_clone),
            &mut pending_blocks,
        )?;
    }

    // Do not advance `ibd_utxo_watermark` from chain tip here. During parallel IBD the block index
    // can reach `chain_tip` before UTXO flush workers persist `ibd_utxos` through that height.
    // Watermark must advance only from flush worker paths after `flush_disk` (see
    // `push_utxo_flush_from_retire`). Bumping from tip caused resume at height H with an empty or
    // partial `ibd_utxos` tree → immediate `UTXO not found for input`.

    // Signal the watchdog to exit so the validation thread joins cleanly on success. Early-return
    // (error) paths skip this and let the watchdog die when the process exits — fine because the
    // watchdog only logs and holds no resources besides Arc clones.
    watchdog_shutdown.store(true, Ordering::Relaxed);
    let _ = watchdog_handle.join();

    Ok(())
}

/// `local_last_retired` + `publisher`: see [`run_ibd_retire_loop_no_commitment`] — same
/// sharding semantics. Commitment-tree updates happen on this shard's heights only; the
/// commitment tree itself is `Mutex`-guarded, so multi-shard concurrent commitment
/// updates serialize on that lock. With `BLVM_IBD_RETIRE_SHARDS=1` behavior is unchanged.
///
/// `max_pending_ops` + `max_pending_ops_nominal` + `max_pending_ops_last_adapt_ms`: the
/// adaptive backpressure cap (see [`adapt_max_pending_ops_tick`]). Updated at most once
/// per 500 ms from this loop, read by every validation worker.
#[cfg(all(feature = "utxo-commitments", feature = "production"))]
#[allow(clippy::too_many_arguments)]
fn run_ibd_retire_loop_with_commitment(
    work_rx: mpsc::Receiver<IbdRetireWork>,
    staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>,
    staged_count: Arc<AtomicUsize>,
    local_last_retired: Arc<AtomicU64>,
    publisher: Arc<super::retire_dispatcher::GlobalProgressPublisher>,
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    mem_mtx: Arc<Mutex<MemoryGuard>>,
    max_ahead_live: Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
    max_utxo_flushes_under_pressure: usize,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: Arc<AtomicUsize>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    blockstore: Arc<BlockStore>,
    commitment_tree: Option<
        Arc<Mutex<blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree>>,
    >,
    commitment_cstore: Option<Arc<crate::storage::commitment_store::CommitmentStore>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
    max_pending_ops: Arc<AtomicUsize>,
    max_pending_ops_nominal: usize,
    max_pending_ops_last_adapt_ms: Arc<AtomicU64>,
) {
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen = rustc_hash::FxHashSet::default();
    let mut evict_scratch: Vec<(OutPointKey, u64)> = Vec::new();
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle — coordinator is paused in EMERGENCY admission. The
                // pressure atomic is only updated from `ibd_v2_retire_apply_utxo_delta`, so
                // if we block here the coordinator never sees recovery. Re-read /proc, publish,
                // and run flush+evict to actively break the deadlock.
                if memory::ibd_pressure_is_emergency() {
                    let level = {
                        let mut mem = mem_mtx.lock();
                        let level = mem.should_flush(Some((&max_ahead_live, nominal_max_ahead)));
                        memory::publish_ibd_pressure(level);
                        level
                    };
                    if level >= PressureLevel::Critical {
                        let evictable = store.len().saturating_sub(store.protected_len());
                        if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                            store.evict_aggressive_for_rss();
                        }
                        // Idle-flush: drain everything. Workers push pending strictly
                        // before dispatching retire work, so all in-pending entries are
                        // safe to flush; the watermark may advance past local_last_retired
                        // and that's correct (`retire_dispatcher.rs` invariant 2).
                        let _ = local_last_retired;
                        if let Some(pkg) = store.take_flush_batch_force() {
                            if let Err(e) = push_utxo_flush_from_retire(
                                &store,
                                &storage_wm,
                                &utxo_flush_handles,
                                &retire_flush_counter,
                                0,
                                max_utxo_flushes_under_pressure,
                                pkg,
                                &ibd_muhash,
                            ) {
                                *retire_err.lock() = Some(e);
                                return;
                            }
                        }
                        ibd_maybe_heap_trim();
                    }
                }
                continue;
            }
        };
        let h = work.height;
        // Workers have already mutated cache + pending log for this height;
        // the retire thread no longer needs to read or apply the delta. The commitment tree is
        // the only consumer that still wants the delta, so we look it up under a short lock.
        let delta_arc = {
            let g = staged.lock();
            g.get(&h).cloned()
        };
        if let (Some(cref), Some(_), Some(delta_arc)) = (
            commitment_tree.as_ref(),
            commitment_cstore.as_ref(),
            delta_arc.as_ref(),
        ) {
            let mut t = cref.lock();
            let store_r = store.as_ref();
            for dk in &delta_arc.deletions {
                let op = blvm_protocol::utxo_overlay::utxo_deletion_key_to_outpoint(dk);
                let key = outpoint_to_key(&op);
                if let Some(utxo) = store_r.get(&key) {
                    if let Err(e) = t.remove(&op, &utxo) {
                        warn!("IBD commitment: remove failed at height {}: {}", h, e);
                    }
                }
            }
            for (op, arc) in &delta_arc.additions {
                if let Err(e) = t.insert(*op, arc.as_ref().clone()) {
                    warn!("IBD commitment: insert failed at height {}: {}", h, e);
                }
            }
        }
        let mut mem = mem_mtx.lock();
        let (opt_pkg, _) = {
            let (_s, _e, p, r) = ibd_v2_retire_apply_utxo_delta(
                h,
                store.as_ref(),
                &work.blocks_buf,
                &mut keys_buf,
                &mut keys_seen,
                &mut evict_scratch,
                &mut mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r)
        };
        drop(mem);
        if let (Some(cref), Some(cstore)) = (commitment_tree.as_ref(), commitment_cstore.as_ref()) {
            let block_hash = blockstore.get_block_hash(work.block.as_ref());
            let commitment = {
                let t = cref.lock();
                t.generate_commitment(block_hash, h)
            };
            if let Err(e) = cstore.store_commitment(&block_hash, h, &commitment) {
                warn!("IBD commitment: store failed at height {}: {}", h, e);
                *retire_err.lock() = Some(e);
                return;
            }
        }
        // Update this shard's local cursor and recompute the dispatcher-wide
        // `global_last_retired = min(local across shards)`. With N=1 the publisher is a
        // no-op trivially and the orchestrator's fold check `sh <= lr_now` sees the same
        // value the original single-thread `last_retired.store(h)` would have produced.
        publisher.publish(&local_last_retired, h);
        // Adaptive cap tick: cheap (atomic loads + early-out via 500 ms throttle).
        // `ibd_pressure_level_snapshot()` reads what `ibd_v2_retire_apply_utxo_delta`
        // just published — same value the memory guard observed for this height.
        adapt_max_pending_ops_tick(
            &max_pending_ops,
            max_pending_ops_nominal,
            memory::ibd_pressure_level_snapshot(),
            store.pending_len(),
            &max_pending_ops_last_adapt_ms,
        );
        // Safe to release staged[h] now: store has the data and `local_last_retired`
        // covers it. Each shard owns disjoint heights (height % N), so no two shards
        // ever touch the same staged entry.
        staged.lock().remove(&h);
        staged_count.fetch_sub(1, Ordering::Relaxed);
        if let Some(pkg) = opt_pkg {
            if let Err(e) = push_utxo_flush_from_retire(
                &store,
                &storage_wm,
                &utxo_flush_handles,
                &retire_flush_counter,
                h,
                max_utxo_flushes_under_pressure,
                pkg,
                &ibd_muhash,
            ) {
                *retire_err.lock() = Some(e);
                return;
            }
        }
    }
}

#[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
/// `local_last_retired` is the per-shard cursor (each shard owns one). Publishing through
/// `publisher` recomputes `min(local_last_retired across shards)` and stores it as the
/// dispatcher's `global_last_retired`. Validation workers and any caller that needs a
/// contiguously-retired floor read the global value; this loop reads only its own local
/// for `take_flush_batch_force_through(flush_cap)` — drain-by-height is monotone and the
/// shared pending log is safe to drain past the floor (workers populate ops before
/// dispatch sends `IbdRetireWork`, so all heights `<= local_last_retired` already have
/// their ops in pending). With `BLVM_IBD_RETIRE_SHARDS=1` (the default), `local` and the
/// dispatcher's global atomic are kept in lock-step by `publisher.publish` — behavior is
/// identical to the pre-sharding single-thread retire.
///
/// `max_pending_ops` + `max_pending_ops_nominal` + `max_pending_ops_last_adapt_ms`: the
/// adaptive backpressure cap (see [`adapt_max_pending_ops_tick`]). Updated at most once
/// per 500 ms from this loop, read by every validation worker.
#[allow(clippy::too_many_arguments)]
fn run_ibd_retire_loop_no_commitment(
    work_rx: mpsc::Receiver<IbdRetireWork>,
    staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>,
    staged_count: Arc<AtomicUsize>,
    local_last_retired: Arc<AtomicU64>,
    publisher: Arc<super::retire_dispatcher::GlobalProgressPublisher>,
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    mem_mtx: Arc<Mutex<MemoryGuard>>,
    max_ahead_live: Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
    max_utxo_flushes_under_pressure: usize,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: Arc<AtomicUsize>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
    max_pending_ops: Arc<AtomicUsize>,
    max_pending_ops_nominal: usize,
    max_pending_ops_last_adapt_ms: Arc<AtomicU64>,
) {
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen = rustc_hash::FxHashSet::default();
    let mut evict_scratch: Vec<(OutPointKey, u64)> = Vec::new();
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle — no in-flight blocks. This happens when the coordinator
                // holds in the EMERGENCY admission pause. If we just park here, the deadlock
                // is permanent: coordinator checks `ibd_pressure_is_emergency()` (an atomic),
                // but that atomic is only updated by `publish_ibd_pressure()` which is only
                // called from `ibd_v2_retire_apply_utxo_delta` — which only runs when retire
                // receives work. So we must re-read /proc and publish fresh pressure here.
                //
                // Additionally, flush pending ops and evict to actually free memory, otherwise
                // even if the atomic cleared, RSS would still be too high to re-enter.
                if memory::ibd_pressure_is_emergency() {
                    let level = {
                        let mut mem = mem_mtx.lock();
                        let level = mem.should_flush(Some((&max_ahead_live, nominal_max_ahead)));
                        memory::publish_ibd_pressure(level);
                        level
                    };
                    if level >= PressureLevel::Critical {
                        let evictable = store.len().saturating_sub(store.protected_len());
                        if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                            store.evict_aggressive_for_rss();
                        }
                        // Idle-flush: drain everything. Workers push pending strictly before
                        // dispatching retire work, so any in-pending entry is for a block
                        // that already finished validation; advancing the watermark past
                        // local_last_retired is allowed (`retire_dispatcher.rs` invariant 2).
                        let _ = local_last_retired;
                        if let Some(pkg) = store.take_flush_batch_force() {
                            if let Err(e) = push_utxo_flush_from_retire(
                                &store,
                                &storage_wm,
                                &utxo_flush_handles,
                                &retire_flush_counter,
                                0,
                                max_utxo_flushes_under_pressure,
                                pkg,
                                &ibd_muhash,
                            ) {
                                *retire_err.lock() = Some(e);
                                return;
                            }
                        }
                        ibd_maybe_heap_trim();
                    }
                }
                continue;
            }
        };
        let h = work.height;
        // Workers have already mutated cache + pending log for this height;
        // retire only runs the *coordinated* per-block work (eviction + flush decisions).
        let mut mem = mem_mtx.lock();
        let (opt_pkg, _) = {
            let (_s, _e, p, r) = ibd_v2_retire_apply_utxo_delta(
                h,
                store.as_ref(),
                &work.blocks_buf,
                &mut keys_buf,
                &mut keys_seen,
                &mut evict_scratch,
                &mut mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r)
        };
        drop(mem);
        // Update this shard's local cursor and recompute the dispatcher-wide
        // `global_last_retired = min(local across shards)`. With N=1 the publisher
        // is a no-op trivially and `global == local` always.
        publisher.publish(&local_last_retired, h);
        // Adaptive cap tick: cheap (atomic loads + early-out via 500 ms throttle).
        // `ibd_pressure_level_snapshot()` reads what `ibd_v2_retire_apply_utxo_delta`
        // just published — same value the memory guard observed for this height.
        adapt_max_pending_ops_tick(
            &max_pending_ops,
            max_pending_ops_nominal,
            memory::ibd_pressure_level_snapshot(),
            store.pending_len(),
            &max_pending_ops_last_adapt_ms,
        );
        // Safe to release staged[h] now: store has the data and `local_last_retired`
        // covers it. Each shard owns disjoint heights (height % N), so no two shards
        // ever touch the same staged entry.
        staged.lock().remove(&h);
        // Decrement the lock-free mirror after removal so the orchestrator's
        // dispatch-backpressure cap sees the freed slot immediately (Relaxed is
        // sufficient — the orchestrator only reads this for a soft throttle).
        staged_count.fetch_sub(1, Ordering::Relaxed);
        if let Some(pkg) = opt_pkg {
            if let Err(e) = push_utxo_flush_from_retire(
                &store,
                &storage_wm,
                &utxo_flush_handles,
                &retire_flush_counter,
                h,
                max_utxo_flushes_under_pressure,
                pkg,
                &ibd_muhash,
            ) {
                *retire_err.lock() = Some(e);
                return;
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests for pure-logic helpers in this file. The retire-loop bodies themselves
// drive too much shared state (`IbdUtxoStore`, `MemoryGuard`, RocksDB, …) to be
// meaningfully unit-tested here; integration coverage belongs in the IBD smoke
// tests. What we *can* lock down here is the controller policy.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_cap(initial: usize) -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(initial))
    }
    fn fresh_last_adapt_zero() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    /// Nominal=0 is the "backpressure off" tier (≥32 GiB). The controller must NEVER
    /// re-engage backpressure adaptively, regardless of pressure level.
    #[test]
    fn adapt_nominal_zero_is_inert() {
        let cap = fresh_cap(0);
        let last = fresh_last_adapt_zero();
        for level in [
            PressureLevel::None,
            PressureLevel::Elevated,
            PressureLevel::Critical,
            PressureLevel::Emergency,
        ] {
            adapt_max_pending_ops_tick(&cap, 0, level, 1_000_000, &last);
            assert_eq!(cap.load(Ordering::Relaxed), 0);
        }
    }

    /// Emergency must aggressively shrink the cap (but respect floors).
    #[test]
    fn adapt_emergency_halves_cap() {
        let cap = fresh_cap(8_000_000);
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Emergency, 8_000_000, &last);
        let new = cap.load(Ordering::Relaxed);
        assert!(new < 8_000_000, "Emergency must shrink");
        assert!(new >= 100_000, "Emergency must respect 100k floor");
    }

    /// Critical multiplies by 0.75; floor `nominal/8` keeps it from collapsing.
    #[test]
    fn adapt_critical_multiplies_by_three_quarters() {
        let cap = fresh_cap(8_000_000);
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Critical, 8_000_000, &last);
        let new = cap.load(Ordering::Relaxed);
        assert!(new < 8_000_000);
        assert!(
            new >= 8_000_000 / 8,
            "Critical must respect nominal/8 floor"
        );
    }

    /// Elevated is a hold — cap unchanged.
    #[test]
    fn adapt_elevated_is_hold() {
        let cap = fresh_cap(8_000_000);
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Elevated, 8_000_000, &last);
        assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
    }

    /// `None` + low pending → grow by ~10%, capped at `1.1 × nominal` (integer ×11/10).
    #[test]
    fn adapt_none_grows_when_drain_keeps_up() {
        let nominal = 8_000_000;
        let cap = fresh_cap(nominal);
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, nominal, PressureLevel::None, 100_000, &last);
        let new = cap.load(Ordering::Relaxed);
        assert!(new > nominal, "None + drain-ahead must grow cap");
        let ceiling = nominal.saturating_mul(11).saturating_div(10);
        assert!(
            new <= ceiling,
            "Must respect 1.1× nominal ceiling (got {new}, ceiling {ceiling})"
        );
    }

    /// `None` + high pending → hold (no point growing if validator is racing ahead).
    #[test]
    fn adapt_none_holds_when_pending_full() {
        let cap = fresh_cap(8_000_000);
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::None, 7_000_000, &last);
        assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
    }

    /// Throttle: if `last_adapt_ms` is recent, the call is a no-op.
    #[test]
    fn adapt_throttle_skips_recent_calls() {
        let cap = fresh_cap(8_000_000);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = Arc::new(AtomicU64::new(now_ms));
        adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Emergency, 8_000_000, &last);
        assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
    }

    /// Repeated Emergency ticks (with throttle reset) must shrink toward — but never
    /// below — `max(nominal/16, 100_000)`.
    #[test]
    fn adapt_emergency_respects_floor_under_repeat() {
        let nominal = 8_000_000;
        let cap = fresh_cap(nominal);
        for _ in 0..50 {
            let last = fresh_last_adapt_zero();
            adapt_max_pending_ops_tick(&cap, nominal, PressureLevel::Emergency, nominal, &last);
        }
        let final_cap = cap.load(Ordering::Relaxed);
        assert!(
            final_cap >= 100_000,
            "must respect hard floor (got {final_cap})",
        );
        assert!(
            final_cap <= nominal / 8,
            "must shrink well below nominal (got {final_cap} for nominal {nominal})",
        );
    }
}

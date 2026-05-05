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
use anyhow::Result;
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{segwit::Witness, BitcoinProtocolEngine, Block, BlockHeader, Hash, UtxoSet, UTXO};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use parking_lot::Mutex;
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
            unsafe { libmimalloc_sys::mi_collect(true); }
            #[cfg(target_os = "linux")]
            unsafe { libc::malloc_trim(0); }
            // Reset the normal heap-trim throttle so the next periodic trim doesn't skip.
            LAST_IBD_HEAP_TRIM_WALL_MS.store(0, Ordering::Relaxed);
        }
    }
    // Force-flush ONLY at Critical/Emergency. At Elevated we let `maybe_take_flush_batch_through` decide
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
        let should_force = pressure_level == PressureLevel::Emergency
            || pending_now >= CRITICAL_MIN_FLUSH_OPS;
        let batch = if should_force {
            store.take_flush_batch_force_through(next_height)
        } else {
            store.maybe_take_flush_batch_through(next_height)
        };
        ibd_maybe_heap_trim();
        (0u64, protect_evict_ms, batch, true)
    } else if rss_pressure_elevated_only {
        // Elevated: don't disturb the pipeline. Take a normal-threshold flush if it's already due,
        // skip heap_trim. The download-ahead reduction (already applied by adjust_max_ahead_live)
        // is enough to ease the pressure.
        let batch = store.maybe_take_flush_batch_through(next_height);
        (0u64, protect_evict_ms, batch, false)
    } else if ibd_defer_flush {
        let at_checkpoint = next_height > 0 && next_height % ibd_defer_checkpoint == 0;
        let batch = if at_checkpoint {
            store.take_flush_batch_force_through(next_height)
        } else {
            None
        };
        (0u64, protect_evict_ms, batch, false)
    } else {
        let batch = store.maybe_take_flush_batch_through(next_height);
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
struct IbdRetireWork {
    height: u64,
    blocks_buf: Vec<Arc<Block>>,
    block: Arc<Block>,
}

/// Push a UTXO disk flush from the retire thread; joins older flushes when the in-flight cap is hit.
///
/// Concurrency uses [`memory::utxo_flush_concurrency_cap`]: bounded burst under healthy pressure,
/// strict tier cap under Critical+. Never uses an unbounded ceiling (historically 1024 → OOM).
fn push_utxo_flush_from_retire(
    store: &Arc<IbdUtxoStore>,
    storage_wm: &Arc<Storage>,
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<()>>>>>,
    next_height: u64,
    max_utxo_flushes_in_flight: usize,
    pkg: PendingFlushPackage,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
) -> Result<()> {
    let flush_limit = memory::utxo_flush_concurrency_cap(max_utxo_flushes_in_flight).max(1);
    let mut q = utxo_flush_handles.lock();
    while q.len() >= flush_limit {
        let Some(handle) = q.pop_front() else {
            return Err(anyhow::anyhow!(
                "IBD invariant violated: UTXO flush wait queue empty under backpressure"
            ));
        };
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!("UTXO flush panicked: {:?}", e));
            }
        }
    }
    let store_clone = Arc::clone(store);
    let storage_clone = Arc::clone(storage_wm);
    let mh_acc = Arc::clone(ibd_muhash);
    let batch_size = pkg.ops.len();
    let heights = Arc::clone(&pkg.heights);
    q.push_back(std::thread::spawn(move || {
        let prepared = pkg.prepare_for_disk()?;
        let muhash_running = {
            let mut mh_guard = mh_acc.lock();
            store_clone.flush_prepared_package(&prepared, Some(&mut *mh_guard))?;
            mh_guard.serialize_running_state()
        };
        // `commit_no_wal` leaves rows in the memtable until flush_cf — persist SST first so a
        // crash cannot advance `ibd_utxo_watermark` past durable ibd_utxos rows (see IbdUtxoStore::flush_disk).
        store_clone.flush_disk()?;
        storage_clone.chain().persist_ibd_utxo_flush_checkpoint(
            prepared.max_block_height,
            &muhash_running,
        )?;
        // Release height-based eviction protection now that all ops for these heights are
        // on disk. This makes cache entries at those heights eligible for eviction, which
        // reduces protected_len from O(pipeline_depth) back toward zero after each flush.
        store_clone.release_protected_heights(&heights);
        store_clone.note_utxo_flush_completed(prepared.max_block_height);
        Ok(())
    }));
    debug!(
        "[IBD_DEBUG] Block {}: spawned UTXO flush (retire thread, batch_size={}, in_flight={})",
        next_height,
        batch_size,
        q.len()
    );
    Ok(())
}

/// Drop the work channel (so the retire thread exits `recv`), join it, propagate `retire_err`.
fn retire_thread_shutdown(
    work_tx: mpsc::Sender<IbdRetireWork>,
    retire_thread: JoinHandle<()>,
    retire_err: &Arc<Mutex<Option<anyhow::Error>>>,
) -> Result<()> {
    std::mem::drop(work_tx);
    if let Err(e) = retire_thread.join() {
        return Err(anyhow::anyhow!("IBD retire thread join failed: {:?}", e));
    }
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
/// `max_retire_lag` is a backpressure limit: if this worker's height exceeds the retire thread's
/// last-committed height by more than this value, the worker sleeps until retire catches up.
/// This bounds the size of `pending_shards` (pending UTXO ops awaiting disk flush), which would
/// otherwise grow to thousands of blocks × hundreds of ops/block = hundreds of millions of bytes,
/// causing OOM on 16 GiB hosts. Set to 0 to disable (unlimited, for high-RAM machines).
#[allow(clippy::too_many_arguments)]
fn run_validation_worker_shared(
    rx: crossbeam_channel::Receiver<ValidateJob>,
    tx: mpsc::Sender<ValidateResult>,
    parallel_ibd: Arc<super::ParallelIBD>,
    blockstore: Arc<crate::storage::blockstore::BlockStore>,
    protocol: Arc<blvm_protocol::BitcoinProtocolEngine>,
    store: Arc<IbdUtxoStore>,
    last_retired: Arc<AtomicU64>,
    max_retire_lag: u64,
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
        let _ = last_retired; // not needed in key-driven path; kept on signature for future diagnostics
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
            // Retire-lag backpressure: if this worker is too far ahead of the retire thread,
            // pause before pushing more ops to `pending_shards`. Without this, validation can
            // race tens of thousands of blocks ahead of retire (which drains pending one height
            // at a time), filling pending_shards with N×ops_per_block entries — each ~200 B.
            // At h=200k on a 16 GiB host: 61k-block lag × 380 ops/block × 200 B = ~4.6 GB in
            // pending alone, which pushed RSS to 10.4 GB and triggered Emergency/OOM.
            if max_retire_lag > 0 {
                let backoff = std::time::Duration::from_millis(2);
                loop {
                    let retired = last_retired.load(std::sync::atomic::Ordering::Relaxed);
                    if height.saturating_sub(retired) <= max_retire_lag {
                        break;
                    }
                    std::thread::sleep(backoff);
                }
            }
            store.apply_utxo_delta(delta, height, &mut del_scratch, &mut add_scratch, true);
        }
        let _ = tx.send(ValidateResult {
            height,
            result,
            bip30_post: job.bip30_index,
            elapsed,
            view_build_ms,
        });
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
        std::thread::JoinHandle<Result<()>>,
    >::new()));
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
    let mut spec_adds: std::collections::BTreeMap<u64, Arc<UtxoSet>> = std::collections::BTreeMap::new();

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
    let last_retired: Arc<AtomicU64> = Arc::new(AtomicU64::new(start_height.saturating_sub(1)));
    let retire_err: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    let (work_tx, work_rx) = mpsc::channel::<IbdRetireWork>();
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

    let ibd_muhash_accumulator: Arc<Mutex<blvm_muhash::MuHash3072>> =
        Arc::new(Mutex::new(
            crate::storage::ibd_utxo_muhash::load_ibd_muhash_from_chain(
                storage_clone.chain(),
            )?,
        ));

    // Background retire: applies staged deltas, UTXO store update, UTXO flush spawns, commitment.
    let retire_err_join = retire_err.clone();
    #[cfg(all(feature = "utxo-commitments", feature = "production"))]
    let _retire_thread: JoinHandle<()> = {
        let staged = Arc::clone(&staged);
        let last_retired = Arc::clone(&last_retired);
        let store = Arc::clone(&ibd_store_v2_for_validation);
        let mem_mtx = Arc::clone(&mem_mtx);
        let utxo_flush_handles = Arc::clone(&utxo_flush_handles);
        let max_ahead_live = Arc::clone(&max_ahead_live);
        let blockstore = Arc::clone(&blockstore);
        let ctree = commitment_tree_shared.clone();
        let cst = commitment_store_opt.clone();
        let storage_wm = Arc::clone(&storage_for_retire);
        let ibd_mh = Arc::clone(&ibd_muhash_accumulator);
        std::thread::Builder::new()
            .name("ibd-retire".to_string())
            .spawn(move || {
                run_ibd_retire_loop_with_commitment(
                    work_rx,
                    staged,
                    last_retired,
                    store,
                    storage_wm,
                    mem_mtx,
                    max_ahead_live,
                    nominal_max_ahead,
                    ibd_defer_flush,
                    ibd_defer_checkpoint,
                    max_utxo_flushes_under_pressure,
                    utxo_flush_handles,
                    retire_err_join,
                    blockstore,
                    ctree,
                    cst,
                    ibd_mh,
                );
            })
            .expect("spawn IBD retire thread")
    };
    #[cfg(not(all(feature = "utxo-commitments", feature = "production")))]
    let _retire_thread: JoinHandle<()> = {
        let staged = Arc::clone(&staged);
        let last_retired = Arc::clone(&last_retired);
        let store = Arc::clone(&ibd_store_v2_for_validation);
        let mem_mtx = Arc::clone(&mem_mtx);
        let utxo_flush_handles = Arc::clone(&utxo_flush_handles);
        let max_ahead_live = Arc::clone(&max_ahead_live);
        let storage_wm = Arc::clone(&storage_for_retire);
        let ibd_mh = Arc::clone(&ibd_muhash_accumulator);
        std::thread::Builder::new()
            .name("ibd-retire".to_string())
            .spawn(move || {
                run_ibd_retire_loop_no_commitment(
                    work_rx,
                    staged,
                    last_retired,
                    store,
                    storage_wm,
                    mem_mtx,
                    max_ahead_live,
                    nominal_max_ahead,
                    ibd_defer_flush,
                    ibd_defer_checkpoint,
                    max_utxo_flushes_under_pressure,
                    utxo_flush_handles,
                    retire_err_join,
                    ibd_mh,
                );
            })
            .expect("spawn IBD retire thread")
    };

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
            let total_gb = (system_total_ram_mb + 512) / 1024;
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
    // Default = 4× workers (clamped to [n_workers, 64]). Each in-flight slot holds:
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
    // Retire-lag limit: cap how far (in block heights) any worker can race ahead of the retire
    // thread. The retire thread drains `pending_shards` one height at a time; without a limit,
    // validation can outpace retire by tens of thousands of blocks, accumulating hundreds of
    // millions of pending UTXO ops in RAM (→ OOM). The limit is RAM-tier aware: on 16 GiB a
    // 2 000-block lag × ~400 ops × ~200 B/op = ~160 MB. Override via BLVM_IBD_MAX_RETIRE_LAG.
    let max_retire_lag: u64 = std::env::var("BLVM_IBD_MAX_RETIRE_LAG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            let total_gb = (system_total_ram_mb + 512) / 1024;
            if total_gb >= 32 {
                0 // no limit: retire-lag RSS is small compared to available RAM
            } else if total_gb >= 24 {
                8_000
            } else if total_gb >= 16 {
                4_000
            } else {
                2_000
            }
        });
    if max_retire_lag > 0 {
        info!("IBD: retire-lag backpressure active (max_retire_lag={})", max_retire_lag);
    }

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
        let mrl = max_retire_lag;
        _validate_workers.push(
            std::thread::Builder::new()
                .name(format!("ibd-validate-{}", i))
                .spawn(move || run_validation_worker_shared(rx, tx, pi, bs, pr, st, lr, mrl))
                .expect("spawn IBD validate worker"),
        );
    }
    drop(valjob_rx); // workers hold all live Receiver clones; dropping the prototype lets shutdown propagate
    drop(valres_tx); // workers hold all live Sender clones
    // ────────────────────────────────────────────────────────────────────────

    loop {
        // === DISPATCH PHASE: fill pipeline up to pipeline_depth_live ===
        // BIP30 adjacency guard: the two exceptional heights on mainnet (91722, 91842)
        // require sequential BIP30 state propagation — force depth=1 to serialize through them.
        // Otherwise pipeline_depth controls how far ahead the dispatcher can run, while
        // n_validate_workers controls how many of those in-flight blocks execute concurrently.
        let pipeline_depth_live: usize =
            if next_validation_height >= 91710 && next_validation_height <= 91855 {
                1
            } else {
                n_pipeline_depth
            };

        while in_flight.len() < pipeline_depth_live {
            let is_first = in_flight.is_empty();

            // Get next block: blocking if no in-flight work, non-blocking otherwise.
            let block_tuple_opt = if is_first {
                const FEEDER_WAIT_TIMEOUT: std::time::Duration =
                    std::time::Duration::from_secs(5);
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
                guard.0.remove(next_h).map(|(arc_b, w, ik, u, tx_ids, spec_adds, est_bytes)| {
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

            let witnesses_storage_d: Arc<Vec<Vec<Witness>>> = if witnesses_d.is_empty() {
                shared_empty_witness_stacks(block_arc_d.transactions.len())
            } else if witnesses_d.len() != block_arc_d.transactions.len() {
                return match retire_thread_shutdown(work_tx, _retire_thread, &retire_err) {
                    Ok(()) => Err(anyhow::anyhow!(
                        "Witness count mismatch at height {}: {} witnesses for {} transactions",
                        h,
                        witnesses_d.len(),
                        block_arc_d.transactions.len()
                    )),
                    Err(e) => Err(e),
                };
            } else {
                Arc::new(witnesses_d)
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
                    h, keys_v2_buf.len(), ibd_store_v2_for_validation.len()
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
            spec_adds_snapshot_buf.extend(
                spec_adds.iter().map(|(sh, set)| (*sh, Arc::clone(set)))
            );
            // Move the filled buffer into the job; replace with a fresh pre-sized buf for the
            // next dispatch. Avoids the second Vec alloc + Arc-tuple memcpy from .clone().
            let spec_adds_snapshot = std::mem::replace(
                &mut spec_adds_snapshot_buf,
                Vec::with_capacity(64),
            );

            // Optional debug/profile snapshot (rare, off by default).
            if let Some(ref base) = snapshot_dir_base {
                const SNAPSHOT_HEIGHTS: &[u64] = &[
                    50_000, 90_000, 125_000, 133_000, 145_000, 175_000, 181_000, 190_000,
                    200_000,
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
            let cached_network_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

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

            let job_send = valjob_tx.send(ValidateJob {
                height: h,
                block_arc: Arc::clone(&block_arc_d),
                witnesses_storage: Arc::clone(&witnesses_storage_d),
                bip30_index: bip30_index.clone(),
                recent_headers: recent_snap,
                tx_ids: tx_ids_precomputed_d,
                cached_network_time,
                keys: keys_for_job,
                spec_adds_snapshot,
                prefetched: prefetched_utxos_d,
            });
            if job_send.is_err() {
                return match retire_thread_shutdown(work_tx, _retire_thread, &retire_err) {
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

        // Terminate when feeder is exhausted and the pipeline is empty.
        if in_flight.is_empty() {
            break;
        }

        // === COLLECT PHASE: wait for the next in-order result ===
        let next_process_h = in_flight.front().unwrap().height;
        // Drain any results that arrived out of order.
        while let Ok(vres) = valres_rx.try_recv() {
            // Early spec_adds drop: once worker_cache_put_protected has run (on the worker,
            // before sending the result), this block's outputs are in the DashMap cache.
            // Future workers dispatched at higher heights will find them via cache_get —
            // the spec_adds entry is no longer needed and can be freed now.
            if let Some(set) = spec_adds.remove(&vres.height) {
                let freed = (set.len() as u64).saturating_mul(64);
                spec_adds_bytes.fetch_sub(freed.min(spec_adds_bytes.load(Ordering::Relaxed)), Ordering::Relaxed);
            }
            pending_results.insert(vres.height, vres);
        }
        // Blocking wait until we have the result for the front-of-queue entry.
        while !pending_results.contains_key(&next_process_h) {
            match valres_rx.recv() {
                Ok(vres) => {
                    if let Some(set) = spec_adds.remove(&vres.height) {
                        let freed = (set.len() as u64).saturating_mul(64);
                        spec_adds_bytes.fetch_sub(freed.min(spec_adds_bytes.load(Ordering::Relaxed)), Ordering::Relaxed);
                    }
                    pending_results.insert(vres.height, vres);
                }
                Err(_) => {
                    return match retire_thread_shutdown(work_tx, _retire_thread, &retire_err) {
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
        let mut entry = in_flight.pop_front().unwrap();
        let vres = pending_results.remove(&next_process_h).unwrap();
        // Safety-net: if the early-drop (on result reception) missed an entry, clean it up now.
        // With the early drop path, this should rarely fire (entry is normally already gone).
        while spec_adds
            .first_key_value()
            .map(|(sh, _)| *sh <= next_process_h)
            .unwrap_or(false)
        {
            if let Some((_, set)) = spec_adds.pop_first() {
                let freed = (set.len() as u64).saturating_mul(64);
                spec_adds_bytes.fetch_sub(freed.min(spec_adds_bytes.load(Ordering::Relaxed)), Ordering::Relaxed);
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

        bip30_index = vres.bip30_post;
        let validation_time = vres.elapsed;
        // vres.result carries only Option<UtxoDelta> — tx ids are not propagated.
        let validation_result = vres.result;

        #[cfg(feature = "profile")]
        let ibd_log_this_height = ibd_blocked_log
            && ibd_profile_height_matches_sample(ibd_profile_sample, next_height);
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
                    // Only the dynamic-eviction code path inside the retire helper uses
                    // `blocks_buf` (for `protect_keys_for_next_blocks`). In FIFO/LIFO modes
                    // the cloned Vec<Arc<Block>> is pure waste — one Vec alloc + N Arc bumps
                    // per block on the dispatcher critical path. Send an empty Vec instead.
                    let retire_blocks_buf = if ibd_store_v2_for_validation.is_dynamic_eviction() {
                        blocks_buf.clone()
                    } else {
                        Vec::new()
                    };
                    if work_tx
                        .send(IbdRetireWork {
                            height: next_height,
                            blocks_buf: retire_blocks_buf,
                            block: Arc::clone(&block_arc),
                        })
                        .is_err()
                    {
                        return match retire_thread_shutdown(work_tx, _retire_thread, &retire_err) {
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
                                work_tx,
                                _retire_thread,
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
                                    work_tx,
                                    _retire_thread,
                                    &retire_err,
                                ) {
                                    Ok(()) => Err(e),
                                    Err(e2) => Err(e2),
                                };
                            }
                            Err(e) => {
                                return match retire_thread_shutdown(
                                    work_tx,
                                    _retire_thread,
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
                return match retire_thread_shutdown(work_tx, _retire_thread, &retire_err) {
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
                format!(
                    "{recent_rate:.1} blocks/s (avg since start: {average_rate:.1} blocks/s)"
                )
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
    retire_thread_shutdown(work_tx, _retire_thread, &retire_err)?;

    // Final UTXO flush: drain remaining pending ops, then join all in-flight handles.
    if let Some(pkg) = ibd_store_v2_for_validation.take_remaining_flush_package() {
        let store_clone = Arc::clone(&ibd_store_v2_for_validation);
        let storage_shutdown = Arc::clone(&storage_clone);
        let mh_shutdown = Arc::clone(&ibd_muhash_accumulator);
        let heights = Arc::clone(&pkg.heights);
        utxo_flush_handles.lock().push_back(std::thread::spawn(move || {
            let prepared = pkg.prepare_for_disk()?;
            let muhash_running = {
                let mut mh_guard = mh_shutdown.lock();
                store_clone.flush_prepared_package(&prepared, Some(&mut *mh_guard))?;
                mh_guard.serialize_running_state()
            };
            store_clone.flush_disk()?;
            storage_shutdown.chain().persist_ibd_utxo_flush_checkpoint(
                prepared.max_block_height,
                &muhash_running,
            )?;
            store_clone.release_protected_heights(&heights);
            store_clone.note_utxo_flush_completed(prepared.max_block_height);
            Ok(())
        }));
    }
    for handle in utxo_flush_handles.lock().drain(..) {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!("UTXO flush panicked at shutdown: {:?}", e));
            }
        }
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

    Ok(())
}

#[cfg(all(feature = "utxo-commitments", feature = "production"))]
fn run_ibd_retire_loop_with_commitment(
    work_rx: mpsc::Receiver<IbdRetireWork>,
    staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>,
    last_retired: Arc<AtomicU64>,
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    mem_mtx: Arc<Mutex<MemoryGuard>>,
    max_ahead_live: Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
    max_utxo_flushes_under_pressure: usize,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<()>>>>>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    blockstore: Arc<BlockStore>,
    commitment_tree: Option<Arc<Mutex<blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree>>>,
    commitment_cstore: Option<Arc<crate::storage::commitment_store::CommitmentStore>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
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
                        let flush_cap = last_retired.load(Ordering::Acquire);
                        if let Some(pkg) = store.take_flush_batch_force_through(flush_cap) {
                            if let Err(e) = push_utxo_flush_from_retire(
                                &store,
                                &storage_wm,
                                &utxo_flush_handles,
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
                &mut *mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r)
        };
        drop(mem);
        if let (Some(cref), Some(cstore)) = (
            commitment_tree.as_ref(),
            commitment_cstore.as_ref(),
        ) {
            let block_hash = blockstore.get_block_hash(work.block.as_ref());
            let commitment = { let t = cref.lock(); t.generate_commitment(block_hash, h) };
            if let Err(e) = cstore.store_commitment(&block_hash, h, &commitment) {
                warn!("IBD commitment: store failed at height {}: {}", h, e);
                *retire_err.lock() = Some(e);
                return;
            }
        }
        last_retired.store(h, Ordering::Release);
        // Safe to release staged[h] now: store has the data and last_retired covers it, so the
        // orchestrator's fold check `sh <= lr_now` will succeed for this height.
        staged.lock().remove(&h);
        if let Some(pkg) = opt_pkg {
            if let Err(e) = push_utxo_flush_from_retire(
                &store,
                &storage_wm,
                &utxo_flush_handles,
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
fn run_ibd_retire_loop_no_commitment(
    work_rx: mpsc::Receiver<IbdRetireWork>,
    staged: Arc<Mutex<BTreeMap<u64, Arc<UtxoDelta>>>>,
    last_retired: Arc<AtomicU64>,
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    mem_mtx: Arc<Mutex<MemoryGuard>>,
    max_ahead_live: Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
    max_utxo_flushes_under_pressure: usize,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<()>>>>>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
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
                        let flush_cap = last_retired.load(Ordering::Acquire);
                        if let Some(pkg) = store.take_flush_batch_force_through(flush_cap) {
                            if let Err(e) = push_utxo_flush_from_retire(
                                &store,
                                &storage_wm,
                                &utxo_flush_handles,
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
                &mut *mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r)
        };
        drop(mem);
        last_retired.store(h, Ordering::Release);
        // Safe to release staged[h] now: store has the data and last_retired covers it.
        staged.lock().remove(&h);
        if let Some(pkg) = opt_pkg {
            if let Err(e) = push_utxo_flush_from_retire(
                &store,
                &storage_wm,
                &utxo_flush_handles,
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

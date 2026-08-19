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

/// Wall-clock ms at last `malloc_trim` (lock-free throttle for RSS-pressure path).
///
/// NOTE: `mi_collect` calls have been intentionally removed from all hot IBD paths.
/// With `MIMALLOC_PURGE_DELAY=200ms` and `MIMALLOC_PURGE_DECOMMITS=1`, freed mimalloc
/// pages are returned to the OS automatically within 200ms. Calling `mi_collect` in hot
/// paths causes massive "abandoned page" churn (228K+ pages stuck in the global abandoned
/// queue) which PREVENTS the 200ms purge from firing and inflates RSS by 14+ GB.
static LAST_IBD_HEAP_TRIM_WALL_MS: AtomicU64 = AtomicU64::new(0);
const IBD_HEAP_TRIM_MIN_INTERVAL_MS: u64 = 2_000;

/// Opt out: `BLVM_IBD_ASYNC_ENGINE_APPEND=0`. Default **on** — Phase 1 `SpendSession::append`
/// runs on a dedicated serial thread so the orchestrator can keep taking from the feeder /
/// waiting on validate results while append of prior heights proceeds. Height order is preserved
/// (single consumer); workers still see appends completed for all heights ≤ their job.
fn async_engine_append_enabled() -> bool {
    match std::env::var("BLVM_IBD_ASYNC_ENGINE_APPEND") {
        Ok(v) => {
            let t = v.trim();
            !(t == "0" || t.eq_ignore_ascii_case("false") || t.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    }
}

/// Opt in: `BLVM_IBD_BINDER_LOG=1` (or `true`/`on`). Default **off**.
/// Soak harness (`wan-bench-common.sh`) forces `=1` — emit `[IBD_BINDER]` / `[IBD_SLOW_STRETCH]`.
fn binder_log_enabled() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_BINDER_LOG")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    })
}

/// Classify what is binding the validation tip right now (supply vs engine).
/// Used to attribute wall-BPS tax when recent ≪ median/peak instant.
fn classify_ibd_binder(
    feeder: usize,
    holes: u64,
    contig: u64,
    await_ms: u64,
    gd_ewma_ms: Option<u64>,
    pressure: PressureLevel,
    tip_failover: bool,
) -> &'static str {
    if matches!(pressure, PressureLevel::Critical | PressureLevel::Emergency) {
        return "ENGINE_PRESSURE";
    }
    // Tip missing on the wire / awaiting getdata→body.
    if await_ms >= 200 || (feeder == 0 && holes > 0 && contig == 0) {
        return "SUPPLY_TIP_HOLE";
    }
    if tip_failover {
        return "SUPPLY_FAILOVER";
    }
    if feeder == 0 && contig == 0 {
        return "SUPPLY_EMPTY_TIP";
    }
    if feeder == 0 {
        // H3 C3: feeder drained with healthy tip_gd + contig runway is pipe/engine HOL,
        // not a cold-supply starve (mid-window false FEEDER_STARVE on KEEP green).
        let gd_ok = gd_ewma_ms.map(|ms| ms < 200).unwrap_or(false);
        if holes == 0 && contig > 0 && gd_ok {
            return "PIPE_DRAINED";
        }
        // Tip may be in reorder while feeder drained — classic FEEDER_STARVE shape.
        return "SUPPLY_FEEDER_STARVE";
    }
    if let Some(ms) = gd_ewma_ms {
        if ms >= 200 && feeder < 16 {
            return "SUPPLY_GD_SLOW";
        }
    }
    if contig == 0 && feeder < 8 {
        return "SUPPLY_THIN_RUNWAY";
    }
    // Feeder stocked, pressure calm — scripts/engine/retire path.
    "ENGINE_OR_SCRIPTS"
}

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
    let now_ms = crate::utils::time::current_timestamp_millis();
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
    // mi_collect was removed: it causes page abandonment churn that inflates RSS.
    // MIMALLOC_PURGE_DELAY=200ms + PURGE_DECOMMITS=1 returns freed pages to OS naturally.
}

use super::IbdBlockFlushOpts;
use super::ParallelIBD;
use super::ibd_staging::empty_utxo_delta;

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
/// Pre-lock eviction step for the IBD retire hot path.
///
/// Runs DashMap eviction scans and dynamic-eviction key-protection **before** the caller
/// acquires `mem_mtx`. This keeps the critical section (MemoryGuard pressure + flush
/// decisions) short, since a full 16k-entry DashMap scan+sort can take several
/// milliseconds at h=400k+ and would otherwise block all retire shards and the
/// orchestrator while they wait for the lock.
///
/// Must be called with the same arguments that would have gone to
/// `ibd_v2_retire_apply_utxo_delta`. Safe to call without `mem_mtx` because all work
/// touches only `IbdUtxoStore` (its own internal locking) and the scratch buffers owned
/// by this retire thread.
pub(crate) fn ibd_v2_retire_pre_lock(
    next_height: u64,
    store: &IbdUtxoStore,
    blocks_buf: &[Arc<Block>],
    keys_buf: &mut Vec<OutPointKey>,
    keys_seen: &mut rustc_hash::FxHashSet<OutPointKey>,
    evict_scratch: &mut Vec<(OutPointKey, u64)>,
) {
    // Eviction throttle: scale the interval with cache size. Each `maybe_evict` call iterates
    // DashMap shards, sorts by generation, and evicts entries. At 10M+ entries and only ~8
    // net-new UTXOs per block, the overshoot from a longer interval is negligible (<0.01%),
    // while the shard-lock contention on the retire thread is significant.
    // Interval: 16 blocks at <2M entries, 32 at 2-5M, 64 at >5M. Still runs every 16 blocks
    // when close to cap to prevent overshoot that would require large eviction sweeps.
    let cache_len = store.len() as u64;
    let evict_interval: u64 = if cache_len > 5_000_000 {
        64
    } else if cache_len > 2_000_000 {
        32
    } else {
        16
    };
    if next_height % evict_interval == 0 {
        store.maybe_evict(evict_scratch);
    }
    if store.is_dynamic_eviction() {
        block_input_keys_batch_into_arc(blocks_buf, keys_buf, keys_seen);
        store.protect_keys_for_next_blocks(keys_buf);
        store.evict_if_needed(next_height);
    }
}

/// Post-lock cap-application step for the IBD retire hot path.
///
/// Applies a new UTXO cache cap returned by `ibd_v2_retire_apply_utxo_delta` and
/// triggers heap trimming when a large batch of entries was evicted. Runs **after**
/// `mem_mtx` is dropped so `tune_max_entries_for_pressure` (which may itself scan and
/// shrink the DashMap backing allocation) does not extend the critical section.
///
/// `new_cap`: desired cap from `MemoryGuard::compute_adaptive_cache_cap`.
/// `pre_tune_len`: `store.len()` sampled inside `ibd_v2_retire_apply_utxo_delta` (before
/// `tune_max_entries_for_pressure` was deferred).
pub(crate) fn ibd_v2_retire_post_lock(
    store: &IbdUtxoStore,
    new_cap: usize,
    pre_tune_len: usize,
    current_height: u64,
) {
    store.tune_max_entries_for_pressure(new_cap, current_height);
    let evicted = pre_tune_len.saturating_sub(store.len());
    // Force heap pages back to the OS immediately after a large eviction. Without this,
    // mimalloc holds freed Arc<UTXO> pages resident until ALL objects in each 64 KB page
    // are freed — which with random eviction ordering can take thousands of blocks. The
    // forced mi_collect + malloc_trim bypass the normal 2s throttle when we just dropped
    // a significant number of cache entries, making the adaptive RSS response visible to
    // the kernel within the same ~2s poll cycle.
    if evicted > 32_768 {
        // mi_collect removed — see ibd_maybe_heap_trim comment. Freed pages are
        // returned to OS by mimalloc's purge timer (PURGE_DELAY=200ms, PURGE_DECOMMITS=1).
        let _ = evicted; // suppress unused warning
    }
}

/// Sentinel package for formal checkpoint when `add_shards` are empty but `del_shards` hold tombstones.
fn ibd_empty_checkpoint_package(boundary_height: u64) -> PendingFlushPackage {
    PendingFlushPackage {
        ops: Arc::new(Vec::new()),
        max_block_height: boundary_height,
        heights: Arc::new(FxHashSet::default()),
    }
}

/// Build the capped adds-only package (or DEL-only sentinel) for a formal checkpoint boundary.
fn ibd_formal_checkpoint_flush_batch(
    store: &IbdUtxoStore,
    next_height: u64,
) -> Option<PendingFlushPackage> {
    let pending_before = store.pending_len();
    let batch = store.take_flush_batch_adds_only().or_else(|| {
        if store.pending_len() > 0 {
            Some(ibd_empty_checkpoint_package(next_height))
        } else {
            None
        }
    });
    warn!(
        "[CAPPED_DRAIN] path=at_checkpoint h={next_height} pending_before={pending_before} drained={}",
        batch.as_ref().map(|p| p.ops.len()).unwrap_or(0)
    );
    batch
}

/// Under-lock retire step: MemoryGuard pressure evaluation and flush-batch selection.
///
/// This is the portion that *requires* `mem_mtx` — everything else has been moved to
/// `ibd_v2_retire_pre_lock` (DashMap eviction, pre-lock) and `ibd_v2_retire_post_lock`
/// (cap application + heap trim, post-lock). The goal is to minimise the duration the
/// caller holds `mem_mtx` so that other retire shards and the orchestrator can acquire
/// it promptly.
///
/// Returns `(_, _, flush_pkg, force_durability, cap_change)` where `cap_change` is
/// `Some((new_cap, pre_tune_len))` when `compute_adaptive_cache_cap` produced a new cap
/// that the caller must apply via `ibd_v2_retire_post_lock` after dropping the lock.
pub(crate) fn ibd_v2_retire_apply_utxo_delta(
    next_height: u64,
    store: &IbdUtxoStore,
    mem_guard: &mut MemoryGuard,
    max_ahead_live: &Arc<AtomicU64>,
    nominal_max_ahead: u64,
    ibd_defer_flush: bool,
    ibd_defer_checkpoint: u64,
) -> (
    u64,
    u64,
    Option<PendingFlushPackage>,
    bool,
    Option<(usize, usize)>,
) {
    let pressure_level = mem_guard.should_flush(Some((max_ahead_live, nominal_max_ahead)));
    // Publish for lock-free reads from the dispatcher (avoids contending on `mem_mtx` per
    // block — publish happens inside the lock so the orchestrator never observes a stale
    // pressure level on the same cycle that triggered a flush decision).
    memory::publish_ibd_pressure(pressure_level);
    // Self-adapting UTXO cache cap. Reads ACTUAL process RSS (captures mimalloc
    // fragmentation, RocksDB growth, and every other allocator) and shrinks the cache when
    // we approach the RSS budget; grows it back when memory frees up. Throttled internally
    // to one evaluation per ~2 s. We only snapshot `store.len()` here and defer the actual
    // `tune_max_entries_for_pressure` call to the post-lock phase so that any DashMap
    // eviction+shrink it triggers runs without holding `mem_mtx`.
    let cap_change: Option<(usize, usize)> = mem_guard
        .compute_adaptive_cache_cap()
        .map(|new_cap| (new_cap, store.len()));

    // Formal checkpoint first: must run at boundary heights even under Elevated/Critical.
    // Otherwise RSS pressure branches only drain adds and DEL tombstones accumulate until
    // pending exceeds the worker cap (see zeus wedge @ h≈681k, Elevated after checkpoint).
    if ibd_defer_flush && next_height > 0 && next_height % ibd_defer_checkpoint == 0 {
        let batch = ibd_formal_checkpoint_flush_batch(store, next_height);
        return (0u64, 0u64, batch, true, cap_change);
    }

    // Force-flush ONLY at Critical/Emergency. At Elevated we let `maybe_take_flush_batch` decide
    // (it triggers at the normal threshold). Forcing a flush on every Elevated transition produced
    // a storm of tiny flushes (pending<10k) at h=366k onward, each followed by heap_trim — both
    // ate retire CPU and walloped BPS from 145 → 60. Critical/Emergency still force-flush because
    // those levels mean we're seconds from OOM and need to reclaim aggressively.
    let rss_pressure = pressure_level >= PressureLevel::Critical;
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
        // RSS pressure: capped adds-only drain only — never promote checkpoint here; formal
        // boundaries were handled above before this branch.
        let pending_before = store.pending_len();
        let batch = store.maybe_take_flush_batch_adds_only();
        if batch.is_some() {
            warn!(
                "[CAPPED_DRAIN] path=rss_pressure level={:?} h={next_height} pending_before={pending_before} drained={} force_ckpt=false",
                pressure_level,
                batch.as_ref().map(|p| p.ops.len()).unwrap_or(0),
            );
        }
        ibd_maybe_heap_trim();
        (0u64, 0u64, batch, false, cap_change)
    } else {
        // Elevated / defer / default: same pick-flush. Adds-only drain — never write DEL
        // tombstones on the async path (crash+resume UTXO_TOTAL_MISS).
        let (batch, force_durability) = ibd_retire_pick_flush_batch(store);
        (0u64, 0u64, batch, force_durability, cap_change)
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

/// Shrink validation pipeline depth when engine append is stuck on Arc-contention clones.
///
/// Throttle uses **contention_pct** only (shared outer/tip Arc). Structural freeze-and-new
/// is ~30% of appends at mid-IBD with 25k run caps and must not shrink depth — that was the
/// BPS regression when total `slow_pct≥15%` halved the pipeline.
#[inline]
fn pipeline_depth_for_engine_append(nominal: usize) -> usize {
    let slow_pct = crate::storage::ibd_engine::memory_age::memory_age_throttle_slow_pct();
    if slow_pct == 0 {
        return nominal;
    }
    if slow_pct >= 95 {
        1
    } else if slow_pct >= 50 {
        (nominal / 4).max(4)
    } else if slow_pct >= 25 {
        (nominal / 2).max(8)
    } else {
        nominal
    }
}

/// P1: when tip DiskIndex is hot (`segs≥2` or recent `disk_ms` high), shrink pipeline
/// so fewer workers thrash the page cache (opposite of F5c parallel-pread).
/// Opt-in only (`=1`). C2 default-on under tip-crawl hurt tip30 (`tc220a48h3c21`).
fn tip_concurrency_adapt_from_env() -> bool {
    latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_TIP_CONCURRENCY_ADAPT")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("yes") | Some("on")
        )
    })
}

/// Land E: under tip-crawl with healthy supply, treat Critical like Elevated for
/// pipeline/pending/poll policy (Emergency unchanged). Avoids depth/2 + spill thrash
/// when STREAM/gd already dens-class (A5 Chunk B).
#[inline]
fn effective_pressure_for_tip_crawl(level: PressureLevel) -> PressureLevel {
    if matches!(level, PressureLevel::Critical) && super::tip_stage::tip_crawl_supply_healthy_now()
    {
        PressureLevel::Elevated
    } else {
        level
    }
}

/// Shrink validation pipeline depth under RSS pressure. Engine-mode workers hold
/// `MemoryRun` query snapshots while `MemoryAge::append` falls back to a full run clone
/// when `Arc::strong_count(tip) != 1` — depth 32 at ~800 BPS was measured at ~700 KB anon/block.
///
/// Peak (`57054fba`) + Land E: Critical + healthy supply holds at Elevated depth (24/32).
/// C2v2 extra raw-Critical cap (24→16) is off — it cut the window that printed 246/205.
/// r28 small-anon Elevated→nominal hold is off — peak Elevated is always 3/4.
#[inline]
fn pipeline_depth_for_pressure(level: PressureLevel, nominal: usize) -> usize {
    let mut nominal = pipeline_depth_for_engine_append(nominal);
    if tip_concurrency_adapt_from_env() {
        let segs = crate::storage::ibd_engine::tip_disk_segs_hint();
        let disk_ms = crate::storage::ibd_engine::tip_disk_ms_hint();
        if segs >= 2 || disk_ms >= 8 {
            nominal = (nominal / 2).max(8);
        }
    }
    match effective_pressure_for_tip_crawl(level) {
        PressureLevel::Emergency => (nominal / 4).max(4),
        PressureLevel::Critical => (nominal / 2).max(8),
        PressureLevel::Elevated => (nominal * 3 / 4).max(12),
        PressureLevel::None => nominal,
    }
}

/// How often the orchestrator polls MemoryGuard + engine spill in engine mode.
#[inline]
fn engine_pressure_poll_interval(level: PressureLevel) -> u64 {
    match effective_pressure_for_tip_crawl(level) {
        PressureLevel::Emergency => 1,
        PressureLevel::Critical => 4,
        PressureLevel::Elevated => 16,
        PressureLevel::None => 32,
    }
}

/// Request sent from retire threads to the dedicated durability thread.
///
/// When the retire loop triggers a `force_durability` flush it ships the raw package here
/// instead of running the synchronous LMDB two-phase commit inline. This decouples
/// the 2–20 s flush from the retire loop, preventing `staged_count` from stalling
/// the orchestrator (the primary SegWit-era IBD bottleneck: h=500k–956k saw ~40% of
/// wall-clock spent in synchronous durability stalls with 39 IBD_WATCHDOG freeze events
/// in a single run).
///
/// The durability thread processes requests strictly in-order (single reader on the
/// channel), preserving the monotonic watermark invariant and two-phase crash safety.
pub(super) struct DurabilityRequest {
    pub(super) pkg: PendingFlushPackage,
    /// Retire height that triggered the flush (for debug logging only).
    pub(super) trigger_height: u64,
    /// If true, this request is a durability checkpoint: DELs are written, fdatasync is called,
    /// and the UTXO watermark is advanced. If false, only ADDs are written (no sync, no watermark).
    /// Non-checkpoint batches tolerate a crash restart — the autorepair re-validates from the
    /// last watermark and re-applies the missing ADDs.
    pub(super) is_checkpoint: bool,
}

/// Whether `next` may join an in-progress durability batch. Checkpoint and non-checkpoint
/// requests must not merge — one `is_checkpoint` flag would run `del_backlog` for the whole group.
fn ibd_durability_may_merge_request(batch: &[DurabilityRequest], next: &DurabilityRequest) -> bool {
    if batch.is_empty() {
        return true;
    }
    let batch_is_ckpt = batch.iter().any(|r| r.is_checkpoint);
    next.is_checkpoint == batch_is_ckpt
}

/// Long-lived background thread that owns the synchronous UTXO durability path.
///
/// Receives `DurabilityRequest`s from all retire shards via an ordered channel.
/// For each request it:
/// 1. Drains all in-flight async UTXO flush handles (join + fold sub-MuHash).
/// 2. `prepare_for_disk()` — serialise UTXO bytes into a flat slab.
/// 3. `compute_package_muhash()` — rayon parallel hash over the slab.
/// 4. Folds the hash into `ibd_muhash` (one brief lock).
/// 5. Two-phase LMDB commit: ADD → flush_disk → watermark → DEL → flush_disk.
/// 6. `release_protected_heights` + `note_utxo_flush_completed`.
/// 7. `ibd_flush_del_backlog_through_watermark` — drains leftover DEL tombstones.
///
/// On error the failure is written to `retire_err` (same as the retire threads) and
/// the thread exits; the retire loop detects this on the next tick and shuts down.
#[allow(clippy::too_many_arguments)]
fn run_ibd_durability_loop(
    store: Arc<IbdUtxoStore>,
    storage_wm: Arc<Storage>,
    utxo_flush_handles: Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    ibd_muhash: Arc<Mutex<blvm_muhash::MuHash3072>>,
    retire_err: Arc<Mutex<Option<anyhow::Error>>>,
    in_flight_counter: Arc<AtomicUsize>,
    ibd_defer_checkpoint: u64,
    rx: std::sync::mpsc::Receiver<DurabilityRequest>,
) {
    // Drain up to BATCH_MAX pending requests before issuing fdatasync.
    // The fdatasync is the dominant cost (~200-600ms per call): batching reduces sync count
    // from 2×N to 2 per group of N requests (or 1 with merged ADD+DEL phases).
    // Trade-off: crash recovery replays up to BATCH_MAX×200 blocks instead of 200.
    const BATCH_MAX: usize = 8;
    // Hard cap on total ops merged into a single fdatasync group, independent of BATCH_MAX.
    // Per-block UTXO churn grows with chain height (SegWit-era blocks carry far more
    // inputs/outputs than early blocks), so a fixed 200-block `defer_checkpoint_interval`
    // produces ever-larger packages at higher heights. Without this cap, BATCH_MAX=8
    // requests can merge into multi-million-op commits against a huge (100+ GB) LMDB
    // B-tree, where random-page write amplification drops effective throughput to a few
    // MB/s — turning what should be a sub-second sync into a 10+ minute stall that freezes
    // the entire retire/validation pipeline (observed: 3.6M ops, still running after 9 min).
    // Once accumulated ops from already-drained requests exceed this, stop merging more in
    // (a single oversized request is still processed alone — this only bounds merging).
    // 200 k: SegWit-era blocks (h=481k+) produce 10-20× more UTXOs per block than
    // pre-SegWit blocks (~15k UTXOs/block vs ~1.5k). A single 200-block retire package
    // can exceed 3M ops at those heights. Capping batch merging at 200 k ensures the
    // durability thread never accumulates a single catastrophic commit that takes 10+ seconds
    // of LMDB B-tree work. At pre-SegWit heights this is identical to before; at SegWit
    // heights it prevents the channel from filling with giant merged batches.
    const BATCH_MAX_OPS: usize = 200_000;

    loop {
        // Block until at least one request arrives, then greedily drain additional ones.
        let first = match rx.recv() {
            Ok(r) => r,
            Err(_) => return, // channel closed — sender dropped
        };
        let mut total_ops = first.pkg.ops.len();
        let mut batch: Vec<DurabilityRequest> = Vec::with_capacity(BATCH_MAX);
        batch.push(first);
        while batch.len() < BATCH_MAX && total_ops < BATCH_MAX_OPS {
            match rx.try_recv() {
                Ok(r) => {
                    if !ibd_durability_may_merge_request(&batch, &r) {
                        break;
                    }
                    total_ops += r.pkg.ops.len();
                    batch.push(r);
                }
                Err(_) => break,
            }
        }

        let n_reqs = batch.len();
        let has_checkpoint = batch.iter().any(|r| r.is_checkpoint);
        let total_ops: usize = batch.iter().map(|r| r.pkg.ops.len()).sum();
        let first_h = batch[0].trigger_height;
        let t0 = std::time::Instant::now();
        in_flight_counter.fetch_add(1, Ordering::Relaxed);
        if has_checkpoint {
            let cache_entries = store.cache_len();
            // Each cache entry: ~40B key + 16B Arc pointer + ~160B UTXO heap = ~216B.
            // This is the dominant variable RAM consumer; grows from ~2GB at h=300k to ~3GB at peak.
            let est_cache_mb = (cache_entries as u64 * 216) / (1_024 * 1_024);
            let formal = first_h > 0 && first_h % ibd_defer_checkpoint == 0;
            info!(
                "[IBD_DURABILITY] h={first_h}: checkpoint batch started \
                 (requests={n_reqs}, total_ops={total_ops}, formal={formal}, \
                 cache_entries={cache_entries} ~{est_cache_mb}MB)"
            );
        }

        // Step 1: drain any remaining async handles (legacy; normally empty since all flushes
        // now go through the channel). Kept for safety in case of partial migration.
        {
            let mut combined_sub_mh = blvm_muhash::MuHash3072::new();
            loop {
                let handle = { utxo_flush_handles.lock().pop_front() };
                let Some(handle) = handle else { break };
                match join_utxo_flush_handle_collect_sub_mh(handle) {
                    Ok(sub) => combined_sub_mh = combined_sub_mh.multiply(&sub),
                    Err(e) => {
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
            }
            {
                let mut mh_guard = ibd_muhash.lock();
                *mh_guard = std::mem::take(&mut *mh_guard).multiply(&combined_sub_mh);
            }
        }
        let t_after_handle_join = t0.elapsed().as_millis();

        // Step 2: prepare_for_disk for all packages (parallel encoding via rayon).
        let codec = store.value_codec();
        let mut prepared_pkgs: Vec<(
            Arc<FxHashSet<u32>>,
            crate::storage::ibd_utxo_store::PreparedFlushPackage,
        )> = Vec::with_capacity(n_reqs);
        for req in &batch {
            let heights = Arc::clone(&req.pkg.heights);
            match req.pkg.prepare_for_disk(codec) {
                Ok(p) => prepared_pkgs.push((heights, p)),
                Err(e) => {
                    *retire_err.lock() = Some(e);
                    return;
                }
            }
        }
        let t_after_prepare = t0.elapsed().as_millis();

        // Step 3: compute MuHash for all packages, accumulate into global muhash state.
        let mut local_mh = blvm_muhash::MuHash3072::new();
        for (_, prepared) in &prepared_pkgs {
            if let Err(e) = store.compute_package_muhash(prepared, &mut local_mh) {
                *retire_err.lock() = Some(e);
                return;
            }
        }
        let max_height = prepared_pkgs
            .iter()
            .map(|(_, p)| p.max_block_height)
            .max()
            .or_else(|| batch.iter().map(|r| r.trigger_height).max())
            .unwrap_or(0);
        let total_rows: usize = prepared_pkgs.iter().map(|(_, p)| p.rows.len()).sum();
        let checkpoint_wm = has_checkpoint
            .then(|| ibd_checkpoint_watermark_for_batch(&batch, max_height, ibd_defer_checkpoint));

        // Accumulate into global muhash. For checkpoint batches, also serialize so we can
        // persist with the watermark. For non-checkpoint batches, skip the serialization cost.
        let muhash_running_opt: Option<[u8; blvm_muhash::MUHASH_RUNNING_STATE_BYTES]> = {
            let mut mh_guard = ibd_muhash.lock();
            *mh_guard = std::mem::take(&mut *mh_guard).multiply(&local_mh);
            if has_checkpoint {
                Some(mh_guard.serialize_running_state())
            } else {
                None
            }
        };
        let t_after_muhash = t0.elapsed().as_millis();

        // Step 4a: Write ADDs for ALL requests in the batch.
        //
        // For non-checkpoint batches: LMDB pages are written but not synced. If we crash,
        // autorepair re-applies from the last watermark — safe because ADDs are idempotent.
        // For checkpoint batches: also write DELs, fdatasync, and advance the watermark.
        for (_, prepared) in &prepared_pkgs {
            if let Err(e) = store.flush_prepared_package_adds_only(prepared) {
                *retire_err.lock() = Some(e);
                return;
            }
        }
        // NOTE: madvise(MADV_DONTNEED) on the UTXO store mmap runs inside flush_disk()
        // at every checkpoint. Calling it after every ADD batch (as was tried previously)
        // caused ~10 GB to be evicted and immediately refaulted on the next batch write,
        // producing 1.0–1.7s adds_ms spikes (confirmed: h=277072 adds=1642ms, h=278670
        // adds=1443ms, each preceded by 8–10 GB madvise at the same timestamp).
        // Checkpoint-only madvise gives us bounded file-backed RSS without the refault cost.
        let t_after_adds = t0.elapsed().as_millis();

        // Step 4a-release: Release eviction protection immediately after ADDs.
        //
        // ADDs are now in LMDB (even if not fdatasync'd). Evicting these UTXOs from the
        // DashMap is safe because:
        //   1. Workers doing UTXO lookups can find them in LMDB (they are durably written).
        //   2. DELs (step 4b) use the serialized PreparedFlushPackage bytes — they do NOT
        //      re-read from the DashMap — so eviction does not affect correctness.
        //
        // Releasing early is critical when ADDs take many seconds (large first checkpoint):
        // delayed release keeps `in_flight_insertions` high → MemoryGuard eviction scans
        // block DashMap shard locks → workers stall on UTXO lookups → staged=0 → freeze.
        for (heights, prepared) in &prepared_pkgs {
            store.release_protected_heights(heights);
            store.note_utxo_flush_completed(prepared.max_block_height);
        }
        let t_after_release = t0.elapsed().as_millis();

        // Step 4b: Checkpoint-only: DELs (optional) + fdatasync + watermark.
        //
        // SKIP_DEL_LMDB mode (default false): omit the LMDB DEL phase when set.
        // Unset env → DELs run. Set BLVM_IBD_SKIP_DEL_LMDB=1 to skip.
        //
        // Correctness justification:
        //   A Bitcoin blockchain is append-only: once a UTXO is spent in block H, no
        //   subsequent valid block H' > H will reference that UTXO as an input. Therefore
        //   a "stale" (undeleted) UTXO in the IBD LMDB store is never queried during
        //   validation of later blocks — the false positive is never triggered. The cache
        //   correctly tracks pending-delete entries, so UTXO existence checks against the
        //   cache always return the right answer. LMDB is only consulted on a cache miss;
        //   a cache-miss for a spent UTXO never happens in a valid chain because no future
        //   block references it.
        //
        // On crash+resume from watermark: re-validation from the watermark height will
        //   correctly re-process all blocks after the watermark, marking spent UTXOs
        //   in the cache as it goes. Any stale ADDs in LMDB from before the watermark
        //   are unreachable from subsequent valid blocks, so they don't affect replay.
        //
        // Storage impact: the IBD UTXO LMDB grows larger (no DELs reclaim space). Set
        //   BLVM_IBD_UTXO_MAP_SIZE_MB to a value large enough to hold all ADDs without
        //   DEL reclamation for the full IBD run (256 GB on this machine is safe given
        //   actual live-UTXO-set size ~12 GB at h=956k).
        //
        // Perf impact: at h=400k+, DELs account for 2–7 s of each 100-block checkpoint
        //   (~3× the ADD cost), making the durability thread the global bottleneck and
        //   capping BPS at 25–35. Skipping DELs reduces checkpoint time from ~4 s to
        //   ~1.1 s, enabling ~90 BPS — a 3–4× throughput improvement.
        //
        // Set BLVM_IBD_SKIP_DEL_LMDB=1 to skip DELs (needs a large enough LMDB map;
        //   see BLVM_IBD_UTXO_MAP_SIZE_MB). Set =0 or leave unset to run DELs.
        let skip_del_lmdb: bool = std::env::var("BLVM_IBD_SKIP_DEL_LMDB")
            .map(|v| v != "0")
            .unwrap_or(false);
        let (t_after_dels, t_after_sync) = if has_checkpoint {
            if !skip_del_lmdb {
                for (_, prepared) in &prepared_pkgs {
                    if let Err(e) = store.flush_prepared_package_dels_only(prepared) {
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
            }
            let t_dels = t0.elapsed().as_millis();
            if let Err(e) = store.flush_disk() {
                *retire_err.lock() = Some(e);
                return;
            }
            let t_sync = t0.elapsed().as_millis();
            // Watermark is persisted once after del_backlog / leftover-ADD drain (below),
            // at the formal checkpoint boundary — never at merged-package max_height here.
            (t_dels, t_sync)
        } else {
            (t_after_adds, t_after_adds)
        };

        // Log per-step timing breakdown.
        let prep_ms = t_after_prepare.saturating_sub(t_after_handle_join);
        let release_early_ms = t_after_release.saturating_sub(t_after_adds);
        let adds_ms = t_after_adds.saturating_sub(t_after_muhash);
        let dels_ms = t_after_dels.saturating_sub(t_after_release);
        let sync_ms = t_after_sync.saturating_sub(t_after_dels);
        if has_checkpoint {
            info!(
                "[IBD_DURABILITY_TIMING] h={first_h} \
                 prep={prep_ms}ms release_early={release_early_ms}ms \
                 adds={adds_ms}ms dels={dels_ms}ms sync={sync_ms}ms"
            );
        }

        // Flush (or discard) DEL tombstone backlog only at checkpoints.
        let (t_after_del_backlog, del_batch_n) = if let Some(checkpoint_wm) = checkpoint_wm {
            let mut del_batch_n = 0usize;
            if !skip_del_lmdb {
                match ibd_flush_del_backlog_drain(&store, &ibd_muhash, checkpoint_wm, true) {
                    Ok(n) => del_batch_n = n,
                    Err(e) => {
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
            } else {
                if let Err(e) = ibd_flush_leftover_adds_through_watermark(&store, checkpoint_wm) {
                    *retire_err.lock() = Some(e);
                    return;
                }
                let discarded = store.discard_del_backlog_through_watermark(checkpoint_wm);
                if discarded > 0 {
                    tracing::debug!(
                        "[SKIP_DEL_PURGE] h={first_h} wm={checkpoint_wm} discarded={discarded} DEL tombstones"
                    );
                }
            }
            if store.has_pending_adds_at_or_below(checkpoint_wm) {
                *retire_err.lock() = Some(anyhow::anyhow!(
                    "checkpoint incomplete: ADDs remain at/below formal wm={checkpoint_wm} (h={first_h})"
                ));
                return;
            }
            if let Err(e) =
                ibd_persist_checkpoint_watermark(&storage_wm, &store, &ibd_muhash, checkpoint_wm)
            {
                *retire_err.lock() = Some(e);
                return;
            }
            (t0.elapsed().as_millis(), del_batch_n)
        } else {
            (t_after_release, 0)
        };

        in_flight_counter.fetch_sub(1, Ordering::Relaxed);

        // mi_collect removed — calling it here caused page abandonment churn (228K+
        // abandoned pages, 14 GB stuck RSS). PURGE_DELAY=200ms handles purging naturally.

        let elapsed_ms = t0.elapsed().as_millis();
        if has_checkpoint {
            let del_backlog_ms = t_after_del_backlog.saturating_sub(t_after_sync);
            let rss_kb = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .unwrap_or(0);
            let wm_logged = checkpoint_wm.unwrap_or(max_height);
            let del_path = if ibd_del_backlog_use_collapse_path(wm_logged) {
                "collapse"
            } else {
                "fast"
            };
            info!(
                "[IBD_DURABILITY] h={first_h}: checkpoint batch complete \
                 (requests={n_reqs}, rows={total_rows}, wm={wm_logged}, elapsed={elapsed_ms}ms \
                 del_backlog={del_backlog_ms}ms del_batches={del_batch_n} del_path={del_path} \
                 pending_after={} rss={}MB)",
                store.pending_len(),
                rss_kb / 1024
            );
        }
    }
}

/// One block handed to the background retire thread (after validation enqueues the delta in `staged`).
///
/// Fields are `pub(crate)` so the sibling `retire_dispatcher` module can route on `height`
/// without importing the (mostly internal) validation pipeline. Construction stays
/// inside `validation_loop` (the only producer).
pub(crate) struct IbdRetireWork {
    pub(crate) height: u64,
    /// Only populated in non-engine (legacy UTXO) mode. Engine mode retire thread
    /// ignores blocks immediately (publish + continue), so passing Arc<Block> would
    /// accumulate in the channel when dispatch outruns the retire thread.
    pub(crate) blocks_buf: Vec<Arc<Block>>,
    pub(crate) block: Option<Arc<Block>>,
}

/// Minimum useful adds-only batch. Below this with large remaining pending implies a DEL
/// tombstone backlog; del_backlog runs only at formal boundaries (`at_checkpoint`), not here.
const IBD_MIN_ADDS_ONLY_BATCH: usize = 1_000;

/// Pick a flush package for non-deferred retire ticks under None/Elevated RSS pressure.
///
/// Async adds-only commits are crash-safe, but DEL tombstones stay in `pending_shards` until
/// a formal checkpoint runs `ibd_flush_del_backlog_through_watermark`. When validation outruns
/// retire, pending fills with DELs; adds-only drains return small batches while tombstones
/// remain — workers wedge on the cap unless we wait for the next boundary.
///
/// Returns `(package, force_durability)`: on the channel path only `at_checkpoint` may set
/// `force_durability=true` (see `ibd_v2_retire_apply_utxo_delta`).
fn ibd_retire_pick_flush_batch(store: &IbdUtxoStore) -> (Option<PendingFlushPackage>, bool) {
    let pending = store.pending_len();
    let threshold = store.flush_threshold();

    // Always use the capped adds-only drain (BLVM_IBD_DRAIN_CAP, default 100k).
    // DEL entries accumulate in del_shards and are cleared by
    // `ibd_flush_del_backlog_through_watermark` after each checkpoint — they must
    // not be flushed before the watermark advances past their spend height.
    //
    // The old paths that called `take_flush_batch_force()` (unbounded drain of both
    // add_shards and del_shards) triggered when del_shards pushed `pending` above
    // `2 × threshold`, creating 1.4M-op packages that took 14+ seconds in LMDB,
    // filled the 4-slot sync_channel, and caused add_shards to accumulate 9M+
    // entries while the retire loop blocked → OOM.
    let Some(pkg) = store.maybe_take_flush_batch_adds_only() else {
        // add_shards below threshold (might be del-heavy); del_backlog clears on
        // next checkpoint. Don't force an uncapped drain here.
        return (None, false);
    };

    let remaining = store.pending_len();
    if pkg.ops.len() < IBD_MIN_ADDS_ONLY_BATCH && remaining > threshold {
        return (Some(pkg), false);
    }
    (Some(pkg), false)
}

/// Formal checkpoint boundary height for `trigger_height`, if it aligns to `interval`.
fn ibd_formal_checkpoint_boundary(trigger_height: u64, interval: u64) -> Option<u64> {
    if trigger_height > 0 && interval > 0 && trigger_height % interval == 0 {
        Some(trigger_height)
    } else {
        None
    }
}

/// Durable watermark for a checkpoint batch. Never use merged-package `max_block_height` when
/// a formal boundary is present — that let wm jump 1000+ blocks ahead of validation (zeus
/// h=433800 → wm=435005) and broke cold resume.
fn ibd_checkpoint_watermark_for_batch(
    batch: &[DurabilityRequest],
    batch_max_prepared: u64,
    ibd_defer_checkpoint: u64,
) -> u64 {
    let ckpt_triggers: Vec<u64> = batch
        .iter()
        .filter(|r| r.is_checkpoint)
        .map(|r| r.trigger_height)
        .collect();
    if ckpt_triggers.is_empty() {
        return batch_max_prepared;
    }
    if let Some(formal) = ckpt_triggers
        .iter()
        .filter_map(|&h| ibd_formal_checkpoint_boundary(h, ibd_defer_checkpoint))
        .min()
    {
        return formal;
    }
    let max_trigger = *ckpt_triggers.iter().max().unwrap_or(&0);
    max_trigger.min(batch_max_prepared)
}

fn ibd_persist_checkpoint_watermark(
    storage_wm: &Arc<Storage>,
    store: &Arc<IbdUtxoStore>,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    watermark: u64,
) -> Result<()> {
    let muhash_running = ibd_muhash.lock().serialize_running_state();
    storage_wm
        .chain()
        .persist_ibd_utxo_flush_checkpoint(watermark, &muhash_running)?;
    store.note_utxo_flush_completed(watermark);
    Ok(())
}

/// A1: flush leftover ADD ops at heights `<= watermark` in capped batches (required before wm
/// advance on sentinel checkpoints and when `BLVM_IBD_SKIP_DEL_LMDB=1`).
fn ibd_flush_leftover_adds_through_watermark(
    store: &Arc<IbdUtxoStore>,
    watermark: u64,
) -> Result<usize> {
    let cap = store.adaptive_drain_cap();
    let mut batch_n = 0usize;
    while store.has_pending_adds_at_or_below(watermark) {
        let Some(follow) = store.take_flush_batch_adds_only_through_capped(watermark, cap) else {
            break;
        };
        batch_n += 1;
        let heights = Arc::clone(&follow.heights);
        let prepared = follow.prepare_for_disk(store.value_codec())?;
        store.flush_prepared_package_adds_only(&prepared)?;
        store.flush_disk()?;
        store.release_protected_heights(&heights);
        store.note_utxo_flush_completed(prepared.max_block_height);
    }
    Ok(batch_n)
}

/// Whether del_backlog uses the capped collapse path (A1 + multi-batch DEL) vs a single
/// `take_flush_batch_force_through` fast path. Collapse is required once pending DEL volume
/// grows (observed from h≈350k on zeus); below `BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT`
/// (default **350k**) the fast path avoids +2–4s/checkpoint at low height on a clean store.
fn ibd_del_backlog_use_collapse_path(watermark: u64) -> bool {
    let min_h = std::env::var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(350_000);
    min_h == 0 || watermark >= min_h
}

/// Collapse-path del_backlog drain without watermark persist (caller finalizes wm).
fn ibd_flush_del_backlog_collapse_drain(
    store: &Arc<IbdUtxoStore>,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    watermark: u64,
) -> Result<usize> {
    ibd_flush_leftover_adds_through_watermark(store, watermark)?;

    let cap = store.adaptive_drain_cap();
    let mut batch_n = 0usize;
    loop {
        let Some(follow) = store.take_flush_batch_dels_only_through_capped(watermark, cap) else {
            break;
        };
        batch_n += 1;
        let ops_len = follow.ops.len();
        let heights = Arc::clone(&follow.heights);
        let prepared = follow.prepare_for_disk(store.value_codec())?;

        if crate::storage::ibd_utxo_store::ibd_per_op_muhash_enabled() {
            let mut local_mh = blvm_muhash::MuHash3072::new();
            store.compute_package_muhash(&prepared, &mut local_mh)?;
            let mut mh_guard = ibd_muhash.lock();
            *mh_guard = std::mem::take(&mut *mh_guard).multiply(&local_mh);
        }

        store.flush_prepared_package_dels_only(&prepared)?;
        store.release_protected_heights(&heights);
        debug!("[DEL_BACKLOG] wm={watermark} batch={batch_n} ops={ops_len} cap={cap}");
    }

    if batch_n > 0 {
        store.flush_disk_sync_only()?;
    }
    Ok(batch_n)
}

/// Single-batch del_backlog drain without watermark persist.
fn ibd_flush_del_backlog_fast_drain(
    store: &Arc<IbdUtxoStore>,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    watermark: u64,
) -> Result<usize> {
    let Some(follow) = store.take_flush_batch_force_through(watermark) else {
        return Ok(0);
    };
    if follow.ops.is_empty() {
        return Ok(0);
    }
    let heights = Arc::clone(&follow.heights);
    let prepared = follow.prepare_for_disk(store.value_codec())?;
    let mut local_mh = blvm_muhash::MuHash3072::new();
    store.compute_package_muhash(&prepared, &mut local_mh)?;
    {
        let mut mh_guard = ibd_muhash.lock();
        *mh_guard = std::mem::take(&mut *mh_guard).multiply(&local_mh);
    }
    // Single-fsync combined commit: write adds then dels in the same LMDB epoch, then one
    // fdatasync. Previously two-phase (adds → fsync → dels → fsync = 2 fsyncs). One fsync
    // is equally crash-safe: before the fsync LMDB's on-disk state is unchanged so the
    // del_backlog is replayed from memory on restart; after the fsync both adds and dels are
    // durable. Saves ~700 ms per checkpoint at h<350k, raising BPS ceiling from ~130 to ~230.
    store.flush_prepared_package_adds_only(&prepared)?;
    store.flush_prepared_package_dels_only(&prepared)?;
    store.flush_disk()?;
    store.release_protected_heights(&heights);
    store.note_utxo_flush_completed(watermark);
    Ok(1)
}

/// Drain pending ops at heights `<= watermark`. Checkpoints pass `force_collapse=true`.
fn ibd_flush_del_backlog_drain(
    store: &Arc<IbdUtxoStore>,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    watermark: u64,
    force_collapse: bool,
) -> Result<usize> {
    if force_collapse || ibd_del_backlog_use_collapse_path(watermark) {
        ibd_flush_del_backlog_collapse_drain(store, ibd_muhash, watermark)
    } else {
        ibd_flush_del_backlog_fast_drain(store, ibd_muhash, watermark)
    }
}

/// Retire-loop inline durability helper (persists wm after drain).
fn ibd_flush_del_backlog_through_watermark(
    store: &Arc<IbdUtxoStore>,
    storage_wm: &Arc<Storage>,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    watermark: u64,
) -> Result<usize> {
    let batch_n = ibd_flush_del_backlog_drain(store, ibd_muhash, watermark, false)?;
    if batch_n > 0 || !store.has_pending_adds_at_or_below(watermark) {
        ibd_persist_checkpoint_watermark(storage_wm, store, ibd_muhash, watermark)?;
    }
    Ok(batch_n)
}

fn retire_flush_batch_size() -> usize {
    latch_env!(usize, {
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
    })
}

/// Whether a retire flush should run the checkpoint durability path (wm + del_backlog).
///
/// On the dedicated durability channel, only explicit interval checkpoints (`force_durability`)
/// promote to checkpoint — not every Nth async flush (Phase L).
fn retire_do_durability(
    force_durability: bool,
    batch_count: usize,
    n: usize,
    durability_on_channel: bool,
) -> bool {
    if durability_on_channel {
        force_durability
    } else {
        force_durability || batch_count <= 1 || n % batch_count == 0
    }
}

fn read_mem_available_kb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Durability channel capacity with optional SegWit RAM clamp (Phase D).
fn ibd_durability_channel_cap(retire_height: u64) -> usize {
    let env_cap = std::env::var("BLVM_IBD_DURABILITY_CHANNEL_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .clamp(2, 16);
    if retire_height >= 481_000 {
        let spare_mb = read_mem_available_kb() / 1024;
        let ram_cap = (spare_mb / 1500).max(2) as usize;
        env_cap.min(ram_cap)
    } else {
        env_cap
    }
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
/// the gap. Two-phase commit inside the durability path (ADDs → flush_disk → watermark →
/// DELs → flush_disk) ensures that a SIGKILL can only leave stale (already-spent) UTXOs on
/// disk — never missing UTXOs — so resume always finds what it expects.
/// Join one UTXO flush worker and fold its sub-MuHash into the global accumulator.
fn join_utxo_flush_handle_mul_sub_mh(
    handle: JoinHandle<Result<blvm_muhash::MuHash3072>>,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
) -> Result<()> {
    match handle.join() {
        Ok(Ok(sub_mh)) => {
            let mut mh_guard = ibd_muhash.lock();
            *mh_guard = std::mem::take(&mut *mh_guard).multiply(&sub_mh);
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("UTXO flush panicked: {:?}", e)),
    }
}

/// Join one UTXO flush worker and return its sub-MuHash (for batching before a global fold).
fn join_utxo_flush_handle_collect_sub_mh(
    handle: JoinHandle<Result<blvm_muhash::MuHash3072>>,
) -> Result<blvm_muhash::MuHash3072> {
    match handle.join() {
        Ok(Ok(sub_mh)) => Ok(sub_mh),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("UTXO flush panicked: {:?}", e)),
    }
}

/// RAII guard: joins all pending UTXO flush threads on drop (any exit path — success or error).
///
/// Without this guard, an early-return `Err` from `run_validation_loop` detaches the flush
/// threads (Rust drops `JoinHandle` by detaching, not joining).  The detached threads hold
/// `Arc<IbdUtxoStore>` clones that keep the standalone ibd_utxo_store LMDB environment open.
/// `try_catch_up_ibd` then calls `remove_dir_all` on that directory while the threads are still
/// running, deleting the files out from under the live env (`/proc/self/maps` shows `(deleted)`).
/// The subsequent session's `create_ibd_utxo_standalone_db` fails because the leaked threads
/// still hold `lock.mdb`, forcing a fallback to the slow main-storage LMDB.  Under the extra
/// memory pressure from the leaked ~30 GB mapping, prefetch reads miss single UTXOs → validation
/// failure → infinite retry loop.
struct UtxoFlushGuard(Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>);
impl Drop for UtxoFlushGuard {
    fn drop(&mut self) {
        let handles: Vec<_> = self.0.lock().drain(..).collect();
        if handles.is_empty() {
            return;
        }
        warn!(
            "[IBD_FLUSH_GUARD] joining {} leaked UTXO flush thread(s) on validation-loop exit \
             (prevents ibd_utxo_store LMDB handle leak into next IBD session)",
            handles.len()
        );
        for h in handles {
            if let Err(e) = h.join() {
                warn!(
                    "[IBD_FLUSH_GUARD] UTXO flush thread panicked during cleanup: {:?}",
                    e
                );
            }
        }
    }
}

/// Drain and join all in-flight UTXO flush threads. Never hold `utxo_flush_handles` across `join()`.
fn join_all_utxo_flush_handles(
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    log_label: &str,
) -> Result<blvm_muhash::MuHash3072> {
    let handles: Vec<_> = utxo_flush_handles.lock().drain(..).collect();
    let n = handles.len();
    if n > 0 {
        info!("IBD shutdown: joining {n} in-flight UTXO flush thread(s) ({log_label})");
    }
    let mut combined = blvm_muhash::MuHash3072::new();
    for (i, handle) in handles.into_iter().enumerate() {
        debug!(
            "IBD shutdown: UTXO flush join {}/{} ({log_label})",
            i + 1,
            n
        );
        let sub = join_utxo_flush_handle_collect_sub_mh(handle)?;
        combined = combined.multiply(&sub);
    }
    Ok(combined)
}

fn push_utxo_flush_from_retire(
    store: &Arc<IbdUtxoStore>,
    storage_wm: &Arc<Storage>,
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: &Arc<AtomicUsize>,
    next_height: u64,
    max_utxo_flushes_in_flight: usize,
    pkg: PendingFlushPackage,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    // When true, force the durability path (flush_disk + persist_ibd_utxo_flush_checkpoint)
    // regardless of the Nth-batch counter. Used for deferred-checkpoint flushes where each
    // call is already throttled to once per `defer_checkpoint_interval` blocks — if we waited
    // for the Nth counter to fire it would only trigger at block N*interval (e.g. 160,000 with
    // N=16, interval=10k), making resume accuracy 160k blocks instead of 10k.
    force_durability: bool,
    // When Some, durability flushes are offloaded to the dedicated background thread instead
    // of running synchronously in the retire thread. This eliminates 2–20 s stalls that were
    // blocking `staged_count` from draining and idling all validation workers.
    durability_tx: Option<&std::sync::mpsc::SyncSender<DurabilityRequest>>,
) -> Result<()> {
    let flush_limit = memory::utxo_flush_concurrency_cap(max_utxo_flushes_in_flight).max(1);
    let batch_count = retire_flush_batch_size();
    let n = retire_flush_counter.fetch_add(1, Ordering::Relaxed);
    let do_durability =
        retire_do_durability(force_durability, batch_count, n, durability_tx.is_some());

    // Route ALL flushes through the dedicated durability thread when it's available.
    //
    // Previously, only `do_durability=true` calls used the channel; `do_durability=false` calls
    // spawned async threads that serialized on the LMDB write lock and caused the retire thread
    // to block (via flush_limit join) every few blocks.  With every flush on the channel:
    //
    //   • Non-checkpoint requests (is_checkpoint=false): durability thread writes ADDs only,
    //     no fdatasync, no watermark bump — identical semantics to the old async thread path.
    //   • Checkpoint requests (is_checkpoint=true): durability thread writes ADDs+DELs,
    //     calls fdatasync, and advances the watermark.
    //
    // The retire thread now sends and returns immediately for every block, never blocking on
    // LMDB. It only blocks if the channel fills (default cap=4 via `ibd_durability_channel_cap`),
    // which happens when the durability thread falls behind — still far better than joining every
    // few blocks on the legacy async path.
    if let Some(tx) = durability_tx {
        let ops_len = pkg.ops.len();
        // Each pending entry: 40-byte OutPointKey + Option<Arc<UTXO>> pointer (16B)
        // + UTXO heap (~160B avg). DEL entries omit the UTXO heap (~56B each).
        // Rough but bounded estimate: 200B/op covers the worst-case ADD-heavy package.
        let est_mb = (ops_len * 200) / (1024 * 1024);
        debug!(
            "[IBD_DURABILITY] h={next_height}: queuing flush \
             (ops={ops_len}, ~{est_mb}MB, checkpoint={do_durability})"
        );
        // Time the send to detect when the 4-slot channel is full (back-pressure from
        // slow durability). A blocking send > 500ms means durability is a bottleneck and
        // RSS is at its peak (4 packages × ~est_mb MB each).
        let t_send = std::time::Instant::now();
        if tx
            .send(DurabilityRequest {
                pkg,
                trigger_height: next_height,
                is_checkpoint: do_durability,
            })
            .is_err()
        {
            return Err(anyhow::anyhow!(
                "IBD durability thread disconnected at h={next_height}"
            ));
        }
        let send_ms = t_send.elapsed().as_millis();
        if send_ms > 500 {
            // Read RSS so we can see the actual peak in the log.
            let rss_kb = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .unwrap_or(0);
            warn!(
                "[IBD_DURABILITY] h={next_height}: channel FULL — send blocked {send_ms}ms \
                 (ops={ops_len} ~{est_mb}MB, rss={}MB). \
                 Durability thread is behind; back-pressure is expected but sustained \
                 blocking indicates durability is the IBD bottleneck.",
                rss_kb / 1024
            );
        }
        return Ok(());
    }

    // Never hold `utxo_flush_handles` across `join()`: shutdown and other retire paths need
    // the queue mutex; a slow RocksDB commit inside join would wedge IBD exit.
    loop {
        let handle = {
            let mut q = utxo_flush_handles.lock();
            if q.len() < flush_limit {
                None
            } else {
                q.pop_front()
            }
        };
        let Some(handle) = handle else {
            break;
        };
        join_utxo_flush_handle_mul_sub_mh(handle, ibd_muhash)?;
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
        loop {
            let handle = {
                let mut q = utxo_flush_handles.lock();
                q.pop_front()
            };
            let Some(handle) = handle else {
                break;
            };
            let sub = join_utxo_flush_handle_collect_sub_mh(handle)?;
            combined_sub_mh = combined_sub_mh.multiply(&sub);
        }
        let prepared = pkg.prepare_for_disk(store.value_codec())?;
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
            mh_guard.serialize_running_state()
        };
        // Two-phase crash-safe commit:
        //
        // Phase 1 — ADD ops only → flush_disk():  SST gains new UTXOs; no tombstones yet.
        //   • SIGKILL here: resume re-validates from old watermark, DashMap overlays re-created
        //     UTXOs, safe.
        //
        // Phase 2 — persist_watermark():  advances the resume point.
        //   • Tiny (<1ms) unsafe window between Phase 1 and Phase 3. A crash here leaves
        //     stale UTXOs (ADD'd but never DEL'd) in ibd_utxos; they are harmless because
        //     Bitcoin double-spend prevention guarantees no block after the watermark will
        //     reference them again.
        //
        // Phase 3 — DEL ops only → flush_disk():  tombstones prune the stale entries.
        //   • SIGKILL here: watermark already advanced; stale entries linger but are harmless.
        //
        // This ordering avoids the original bug where DEL tombstones became durable BEFORE the
        // watermark advanced, causing UTXO_TOTAL_MISS on resume for cross-batch spends.
        store.flush_prepared_package_adds_only(&prepared)?;
        store.flush_disk()?;
        storage_wm
            .chain()
            .persist_ibd_utxo_flush_checkpoint(prepared.max_block_height, &muhash_running)?;
        store.flush_prepared_package_dels_only(&prepared)?;
        store.flush_disk()?;
        store.release_protected_heights(&heights);
        store.note_utxo_flush_completed(prepared.max_block_height);
        let watermark = prepared.max_block_height;
        ibd_flush_del_backlog_through_watermark(store, storage_wm, ibd_muhash, watermark)?;
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
        let prepared = pkg.prepare_for_disk(store.value_codec())?;
        drop(pkg);
        let store_clone = Arc::clone(store);
        utxo_flush_handles
            .lock()
            .push_back(std::thread::spawn(move || {
                // Compute muhash in the async thread: concurrent with other threads and with
                // retire loop processing. Retire loop is never blocked by this I/O.
                // Use adds_only: non-durable async flushes must never write DEL tombstones.
                // The caller uses take_flush_batch_adds_only / maybe_take_flush_batch_adds_only
                // so the package should already contain only ADDs; this is a safety net.
                let mut local_mh = blvm_muhash::MuHash3072::new();
                store_clone.compute_package_muhash(&prepared, &mut local_mh)?;
                store_clone.flush_prepared_package_adds_only(&prepared)?;
                store_clone.release_protected_heights(&heights);
                store_clone.note_utxo_flush_completed(prepared.max_block_height);
                Ok(local_mh)
            }));
        debug!(
            "[IBD_DEBUG] Block {}: async commit (batch_size={}, in_flight={}, n={})",
            next_height,
            batch_size,
            utxo_flush_handles.lock().len(),
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
struct EngineValidateJob {
    height: u64,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    bip30_index: Bip30Index,
    recent_headers: Arc<Vec<Arc<BlockHeader>>>,
    tx_ids: Vec<Hash>,
    best_header_chainwork: blvm_consensus::pow::U256,
    cached_network_time: u64,
    partial_session: PartialSpendSession,
    engine_append_ms: u64,
    /// I2: pre-built block output Arc map (built once on dispatch; reused in connect overlay apply).
    ibd_block_outputs:
        Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>>,
}

/// Pre-append payload for the serial engine-append thread (`BLVM_IBD_ASYNC_ENGINE_APPEND`).
struct EngineAppendJob {
    height: u64,
    db: Arc<UtxoDatabase>,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    bip30_index: Bip30Index,
    recent_headers: Arc<Vec<Arc<BlockHeader>>>,
    tx_ids: Vec<Hash>,
    best_header_chainwork: blvm_consensus::pow::U256,
    cached_network_time: u64,
    ibd_block_outputs:
        Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>>,
}

struct LegacyValidateJob {
    height: u64,
    block_arc: Arc<Block>,
    witnesses_storage: Arc<Vec<Vec<Witness>>>,
    bip30_index: Bip30Index,
    recent_headers: Arc<Vec<Arc<BlockHeader>>>,
    tx_ids: Vec<Hash>,
    best_header_chainwork: blvm_consensus::pow::U256,
    cached_network_time: u64,
    keys: Vec<OutPointKey>,
    spec_adds_snapshot: Vec<(u64, Arc<UtxoSet>)>,
    prefetched: PrefetchedUtxoMap,
    ibd_block_outputs:
        Option<Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>>,
}

enum ValidateJob {
    Engine(EngineValidateJob),
    Legacy(LegacyValidateJob),
}

/// Results returned from the validation worker for one block.
struct ValidateResult {
    height: u64,
    /// Tx ids are already available on the job and unused after validation; drop them here to
    /// avoid an `into_owned()` alloc (Vec<Hash> memcpy) per block on the IBD hot path.
    result: Result<Option<UtxoDelta>>,
    undo_log: blvm_consensus::reorganization::BlockUndoLog,
    /// BIP30 index after applying this block's coinbase rules.
    bip30_post: Bip30Index,
    /// Wall time spent inside the worker (view-build + `validate_block_only`).
    elapsed: std::time::Duration,
    /// Wall time spent building the view only (cache + supplement + fold + overlay).
    view_build_ms: u64,
    /// Engine Phase 1 append time (dispatch thread).
    engine_append_ms: u64,
    /// Engine Phase 2 query+fetch time (worker thread).
    engine_complete_ms: u64,
    /// Per-block MuHash contribution (engine mode only; folded in-order by orchestrator).
    block_muhash: Option<blvm_muhash::MuHash3072>,
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
    tx: crossbeam_channel::Sender<ValidateResult>,
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

    // A3 two-phase ECDSA wave: hold ValidateResult until wave hub finishes secp.
    // Worker can take another Engine job while prior wave verify runs.
    struct WavePending {
        vr: ValidateResult,
        rx: std::sync::mpsc::Receiver<std::result::Result<(), blvm_consensus::ConsensusError>>,
    }
    let mut wave_pending: Vec<WavePending> = Vec::new();
    let wave_depth: usize = std::env::var("BLVM_ECDSA_WAVE_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
        .clamp(1, 32);
    let flush_wave_ready =
        |pending: &mut Vec<WavePending>, out: &crossbeam_channel::Sender<ValidateResult>| {
            let mut i = 0;
            while i < pending.len() {
                match pending[i].rx.try_recv() {
                    Ok(Ok(())) => {
                        let WavePending { vr, .. } = pending.swap_remove(i);
                        let _ = out.send(vr);
                    }
                    Ok(Err(e)) => {
                        let WavePending { mut vr, .. } = pending.swap_remove(i);
                        vr.result = Err(anyhow::anyhow!("ECDSA wave: {e:?}"));
                        let _ = out.send(vr);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        let WavePending { mut vr, .. } = pending.swap_remove(i);
                        vr.result = Err(anyhow::anyhow!("ECDSA wave disconnected"));
                        let _ = out.send(vr);
                    }
                }
            }
        };
    let block_wave_oldest =
        |pending: &mut Vec<WavePending>, out: &crossbeam_channel::Sender<ValidateResult>| {
            if pending.is_empty() {
                return;
            }
            let WavePending { mut vr, rx } = pending.remove(0);
            match rx.recv() {
                Ok(Ok(())) => {
                    let _ = out.send(vr);
                }
                Ok(Err(e)) => {
                    vr.result = Err(anyhow::anyhow!("ECDSA wave: {e:?}"));
                    let _ = out.send(vr);
                }
                Err(_) => {
                    vr.result = Err(anyhow::anyhow!("ECDSA wave disconnected"));
                    let _ = out.send(vr);
                }
            }
        };

    loop {
        flush_wave_ready(&mut wave_pending, &tx);
        while wave_pending.len() >= wave_depth {
            block_wave_oldest(&mut wave_pending, &tx);
            flush_wave_ready(&mut wave_pending, &tx);
        }

        let job = match rx.recv() {
            Ok(j) => j,
            Err(_) => {
                while !wave_pending.is_empty() {
                    block_wave_oldest(&mut wave_pending, &tx);
                }
                break;
            }
        };

        match job {
            ValidateJob::Legacy(mut lj) => {
                let height = lj.height;
                let t_view = std::time::Instant::now();
                utxo_base.clear();

                let n_keys = lj.keys.len();
                let n_prefetched = lj.prefetched.len();
                utxo_base.reserve(n_prefetched);
                let still_missing = &mut keys_missing_buf;
                still_missing.clear();

                if n_prefetched > 0 {
                    match Arc::try_unwrap(lj.prefetched) {
                        Ok(mut map) => {
                            for (k, arc) in map.drain() {
                                utxo_base.insert(key_to_outpoint(&k), arc);
                            }
                        }
                        Err(shared) => {
                            for (k, arc) in shared.iter() {
                                utxo_base.insert(key_to_outpoint(k), Arc::clone(arc));
                            }
                        }
                    }
                }

                if n_prefetched < n_keys {
                    for k in lj.keys.iter() {
                        if !utxo_base.contains_key(&key_to_outpoint(k)) {
                            still_missing.push(*k);
                        }
                    }

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

                    if !still_missing.is_empty() && !lj.spec_adds_snapshot.is_empty() {
                        still_missing.retain(|k| {
                            let op = key_to_outpoint(k);
                            for (_sh, set) in lj.spec_adds_snapshot.iter().rev() {
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
                }
                let view_build_ms = t_view.elapsed().as_millis() as u64;

                let recent_opt: Option<&[Arc<BlockHeader>]> = if lj.recent_headers.is_empty() {
                    None
                } else {
                    Some(lj.recent_headers.as_slice())
                };
                let t_val = std::time::Instant::now();
                let raw = parallel_ibd.validate_block_only(
                    &blockstore,
                    protocol.as_ref(),
                    &mut utxo_base,
                    Some(&mut lj.bip30_index),
                    lj.block_arc.as_ref(),
                    Some(Arc::clone(&lj.block_arc)),
                    lj.witnesses_storage.as_slice(),
                    Some(&lj.witnesses_storage),
                    lj.height,
                    recent_opt,
                    lj.cached_network_time,
                    Some(&lj.tx_ids),
                    Some(lj.best_header_chainwork),
                    None,
                    lj.ibd_block_outputs.clone(),
                );
                let elapsed = t_val.elapsed();
                let (result, undo_log) = match raw {
                    Ok((_ids, delta, undo)) => (Ok(delta), undo),
                    Err(e) => (Err(e), blvm_consensus::reorganization::BlockUndoLog::new()),
                };
                if let Ok(Some(delta)) = &result {
                    store.worker_cache_put_protected(&delta.additions, height);
                    store.apply_utxo_delta(delta, height, &mut del_scratch, &mut add_scratch, true);
                }
                let _ = tx.send(ValidateResult {
                    height,
                    result,
                    undo_log,
                    bip30_post: lj.bip30_index,
                    elapsed,
                    view_build_ms,
                    engine_append_ms: 0,
                    engine_complete_ms: 0,
                    block_muhash: None,
                });

                let cap = max_pending_ops.load(Ordering::Relaxed);
                if cap > 0 {
                    let mut spins = 0u32;
                    let spin_start = std::time::Instant::now();
                    const WORKER_SPIN_MAX: std::time::Duration = std::time::Duration::from_secs(60);
                    const WORKER_SPIN_LOG_EVERY: std::time::Duration =
                        std::time::Duration::from_secs(5);
                    let mut last_log = spin_start;
                    loop {
                        let pending_now = store.pending_len();
                        let cap_now = max_pending_ops.load(Ordering::Relaxed);
                        if pending_now <= cap_now {
                            break;
                        }
                        if spin_start.elapsed() >= WORKER_SPIN_MAX {
                            warn!(
                                "IBD worker: pending cap spin timeout at h={height} pending={pending_now} cap={cap_now}"
                            );
                            break;
                        }
                        if last_log.elapsed() >= WORKER_SPIN_LOG_EVERY {
                            warn!(
                                "IBD worker: waiting for pending drain at h={height} pending={pending_now} cap={cap_now} spins={spins}"
                            );
                            last_log = std::time::Instant::now();
                        }
                        spins = spins.saturating_add(1);
                        std::thread::yield_now();
                    }
                }
            }
            ValidateJob::Engine(mut ej) => {
                let height = ej.height;
                let t_view = std::time::Instant::now();
                utxo_base.clear();
                let t_complete = std::time::Instant::now();
                let session = match ej.partial_session.complete() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(ValidateResult {
                            height,
                            result: Err(e.context("IBD engine Phase 2 failed")),
                            undo_log: blvm_consensus::reorganization::BlockUndoLog::new(),
                            bip30_post: ej.bip30_index,
                            elapsed: std::time::Duration::ZERO,
                            view_build_ms: 0,
                            engine_append_ms: ej.engine_append_ms,
                            engine_complete_ms: 0,
                            block_muhash: None,
                        });
                        continue;
                    }
                };
                // N28: SpendSessionLookup opt-in via BLVM_IBD_SPEND_LOOKUP=1 after
                // session_lookup_matches_fill gate. Default = fill (W2-1 resume-safe).
                let use_lookup = crate::storage::ibd_engine::spend_session_lookup_enabled();
                let t_fill = std::time::Instant::now();
                let fill_ms = if use_lookup {
                    0u64
                } else {
                    session_fill_utxo_set(&session, &mut utxo_base);
                    t_fill.elapsed().as_millis() as u64
                };
                let engine_complete_ms = t_complete.elapsed().as_millis() as u64;
                let view_build_ms = t_view.elapsed().as_millis() as u64;
                crate::storage::ibd_engine::note_tip_disk_hints(session.disk_segs, session.disk_ms);
                let hotpath_n = hotpath_timer_sample();
                if hotpath_n > 0 && height % hotpath_n == 0 {
                    info!(
                        "[IBD_HOTPATH] height={} query_ms={} ages_ms={} disk_ms={} preads={} pread_kb={} max_pread_kb={} cands={} segs={} fetch_ms={} map_ms={} fill_ms={} complete_ms={} inputs={}",
                        height,
                        session.query_ms,
                        session.ages_ms,
                        session.disk_ms,
                        session.disk_preads,
                        session.disk_pread_kb,
                        session.disk_max_pread_kb,
                        session.disk_cands,
                        session.disk_segs,
                        session.fetch_ms,
                        session.map_ms,
                        fill_ms,
                        engine_complete_ms,
                        session.details.len() + session.local_spends.len()
                    );
                }

                let recent_opt: Option<&[Arc<BlockHeader>]> = if ej.recent_headers.is_empty() {
                    None
                } else {
                    Some(ej.recent_headers.as_slice())
                };
                let t_val = std::time::Instant::now();
                let lookup = crate::storage::ibd_engine::SpendSessionLookup(&session);
                let raw = if use_lookup {
                    utxo_base.clear();
                    parallel_ibd.validate_block_only(
                        &blockstore,
                        protocol.as_ref(),
                        &mut utxo_base,
                        Some(&mut ej.bip30_index),
                        ej.block_arc.as_ref(),
                        Some(Arc::clone(&ej.block_arc)),
                        ej.witnesses_storage.as_slice(),
                        Some(&ej.witnesses_storage),
                        ej.height,
                        recent_opt,
                        ej.cached_network_time,
                        Some(&ej.tx_ids),
                        Some(ej.best_header_chainwork),
                        Some(&lookup),
                        ej.ibd_block_outputs.clone(),
                    )
                } else {
                    parallel_ibd.validate_block_only(
                        &blockstore,
                        protocol.as_ref(),
                        &mut utxo_base,
                        Some(&mut ej.bip30_index),
                        ej.block_arc.as_ref(),
                        Some(Arc::clone(&ej.block_arc)),
                        ej.witnesses_storage.as_slice(),
                        Some(&ej.witnesses_storage),
                        ej.height,
                        recent_opt,
                        ej.cached_network_time,
                        Some(&ej.tx_ids),
                        Some(ej.best_header_chainwork),
                        None,
                        ej.ibd_block_outputs.clone(),
                    )
                };
                let elapsed = t_val.elapsed();
                let (result, undo_log) = match raw {
                    Ok((_ids, delta, undo)) => (Ok(delta), undo),
                    Err(e) => (Err(e), blvm_consensus::reorganization::BlockUndoLog::new()),
                };
                let block_muhash =
                    if crate::config::ibd::ibd_engine_muhash_enabled() && result.is_ok() {
                        let mut sub = blvm_muhash::MuHash3072::new();
                        crate::storage::ibd_utxo_muhash::fold_block_engine_muhash(
                            ej.block_arc.as_ref(),
                            &ej.tx_ids,
                            height,
                            &session,
                            &mut sub,
                        );
                        Some(sub)
                    } else {
                        None
                    };
                let vr = ValidateResult {
                    height,
                    result,
                    undo_log,
                    bip30_post: ej.bip30_index,
                    elapsed,
                    view_build_ms,
                    engine_append_ms: ej.engine_append_ms,
                    engine_complete_ms,
                    block_muhash,
                };
                // A3: if connect parked ECDSA SoA, submit to wave and continue collecting.
                #[cfg(all(feature = "production"))]
                {
                    if vr.result.is_ok() {
                        if let Some(soa) = blvm_consensus::ecdsa_wave::take_parked() {
                            let wrx = blvm_consensus::ecdsa_wave::submit(height, soa);
                            wave_pending.push(WavePending { vr, rx: wrx });
                            // Join this height before taking more work when pending==1 would
                            // otherwise HOL the orch (in_flight=1 VALRES_STALL). Overlap only
                            // when wave_depth allows multiple pending ECDSA joins.
                            flush_wave_ready(&mut wave_pending, &tx);
                            if wave_pending.len() >= wave_depth.max(1) {
                                block_wave_oldest(&mut wave_pending, &tx);
                            } else if wave_pending.len() == 1 {
                                // Safe default: complete ECDSA before next collect.
                                block_wave_oldest(&mut wave_pending, &tx);
                            }
                            continue;
                        }
                    }
                }
                let _ = tx.send(vr);
                continue;
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
/// **Policy.**
fn adapt_max_pending_ops_tick(
    cap: &AtomicUsize,
    nominal: usize,
    pressure: PressureLevel,
    pending_len: usize,
    last_adapt_ms: &AtomicU64,
) {
    const TICK_INTERVAL_MS: u64 = 500;
    let now_ms = crate::utils::time::current_timestamp_millis();
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
    let pressure = effective_pressure_for_tip_crawl(pressure);
    let new = match pressure {
        PressureLevel::Emergency => {
            // Pending log entries are small (~160 bytes each). Even 8M entries ≈ 1.3 GB,
            // which is small compared to the UTXO cache or pipeline buffers. Aggressive
            // halving (old: /2 per tick, floor=nominal/16) collapsed the cap to ≤500k on
            // large hosts, causing all workers to spin at the 60s safety valve → <1 BPS.
            // The actual memory relief comes from UTXO cache eviction and the coordinator
            // admission pause — not from throttling pending ops.
            //
            // New policy: gentle 10% trim, floor at nominal/2. This keeps workers moving
            // (they wait at most a few seconds before pending drains below the 2M–4M floor)
            // while still providing mild backpressure when retire falls behind.
            (current * 9 / 10).max(nominal / 2).max(1_000_000)
        }
        PressureLevel::Critical => (current * 3 / 4).max(nominal / 4).max(500_000),
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

/// Whether MTP seeding may fall back to the tip `recent_headers` sliding window.
///
/// Deep gap resume (export_h ≪ tip) must **not** use tip timestamps — that fails BIP113 H05.
#[inline]
pub(crate) fn mtp_tip_window_fallback_ok(start_height: u64, tip_height: u64) -> bool {
    tip_height > 0 && start_height.saturating_add(64) > tip_height
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
    pub effective_end_live: Arc<std::sync::atomic::AtomicU64>,
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
    /// Age-tiered UTXO engine (Phase 2). `Some` when `BLVM_IBD_ENGINE=1`; `None` uses
    /// the legacy IbdUtxoStore prefetch/cache/supplement path.
    pub utxo_engine: Option<Arc<UtxoDatabase>>,
    /// Sender for periodic mid-IBD checkpoint heights. The retire loop sends a height here
    /// when it reaches each checkpoint multiple; the background export thread in mod.rs
    /// picks it up and runs `run_checkpoint_export`. `None` in non-engine mode.
    pub checkpoint_tx: Option<std::sync::mpsc::SyncSender<u64>>,
    /// Highest block height already stored in the block store at IBD startup.
    ///
    /// Blocks at heights ≤ this value are already persisted on disk (loaded via
    /// `try_load_local_ibd_block`).  Writing them back during local replay would be
    /// pure redundant I/O — serialise + LMDB-upsert of data that is already there —
    /// taking 37-76 seconds per 600-block flush batch and creating severe sawtooth
    /// BPS stalls.  Set to 0 when there are no pre-stored blocks (fresh IBD from
    /// genesis).
    pub local_replay_max_height: u64,
    /// When `Some(h)`, periodic checkpoint export is deferred until validation passes `h`.
    pub engine_gap_export_defer_until: Option<u64>,
}

/// Join in-flight async block flushes and persist any deferred batch before validation workers
/// retire. Avoids a single giant synchronous flush at process shutdown (observed wedge at ~954k).
fn drain_ibd_pending_blocks_before_shutdown(
    skip_storage: bool,
    parallel_ibd: &ParallelIBD,
    blockstore: &Arc<BlockStore>,
    storage: &Arc<Storage>,
    pending_blocks: &mut Vec<(
        Arc<Block>,
        Arc<Vec<Vec<Witness>>>,
        u64,
        blvm_consensus::reorganization::BlockUndoLog,
    )>,
    pending_storage_bytes: &mut u64,
    flush_handles: &mut VecDeque<std::thread::JoinHandle<Result<()>>>,
) -> Result<()> {
    while let Some(handle) = flush_handles.pop_front() {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Block storage flush thread panicked at IBD completion: {:?}",
                    e
                ));
            }
        }
    }
    if skip_storage {
        pending_blocks.clear();
        *pending_storage_bytes = 0;
        return Ok(());
    }
    if !pending_blocks.is_empty() {
        info!(
            "Flushing {} deferred blocks before IBD shutdown",
            pending_blocks.len()
        );
        parallel_ibd.flush_pending_blocks_with_opts(
            blockstore,
            Some(storage),
            pending_blocks,
            IbdBlockFlushOpts::shutdown_sync(),
        )?;
    }
    Ok(())
}

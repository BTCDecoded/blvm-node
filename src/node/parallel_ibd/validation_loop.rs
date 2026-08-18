//! IBD validation loop — runs on a dedicated `std::thread`.
//!
//! Each block: **connect** (build a `UtxoSet` view from the store, run `validate_block_only` and
//! BIP30 on that view), then **retire** the returned delta on [`IbdUtxoStore`] (apply, protect /
//! evict, flush decision). The connect path does not write canonical in-memory UTXO state; retire
//! does. Reads from the feeder buffer, validates, and flushes to storage in batches.

use super::feeder::FeederState;
use super::latch_env;
use super::memory::{self, MemoryGuard, PressureLevel};
use super::types::PrefetchedUtxoMap;
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use crate::storage::disk_utxo::{
    OutPointKey, block_input_keys_batch_into_arc, block_input_keys_into_filtered,
    block_input_keys_into_filtered_with_tx_ids, key_to_outpoint, outpoint_to_key,
};
use crate::storage::ibd_engine::{
    PartialSpendSession, SpendSession, UtxoDatabase, hotpath_timer_sample, session_fill_utxo_set,
    session_to_utxo_set,
};
use crate::storage::ibd_utxo_store::{IbdUtxoStore, PendingFlushPackage};
use crate::utils::time::current_timestamp;
use anyhow::Result;
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{
    BitcoinProtocolEngine, Block, BlockHeader, Hash, UTXO, UtxoSet, segwit::Witness,
};
use blvm_consensus::utxo_overlay::build_block_output_utxo_cache;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

/// Reuse `Arc<Vec<Vec<Witness>>>` of empty stacks for pre-segwit blocks (same `n` as tx count).
/// Validation runs on one thread — `thread_local` avoids a global mutex on this hot path.
thread_local! {
    static EMPTY_WITNESS_STACKS: RefCell<FxHashMap<usize, Arc<Vec<Vec<Witness>>>>> =
        RefCell::new(FxHashMap::default());
}

/// Weak reference to a sample Arc<Block> captured at h=SAMPLE_HEIGHT.
/// Checked in MEM_REPORT to see if that block has been freed (weak upgrade fails)
/// or is still alive (upgrade succeeds = memory leak).
static SAMPLE_BLOCK_WEAK: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Weak<Block>>>> =
    std::sync::OnceLock::new();
const BLOCK_SAMPLE_HEIGHT: u64 = 150_000;

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
        match std::env::var("BLVM_IBD_BINDER_LOG")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("1") | Some("true") | Some("on") | Some("yes") => true,
            _ => false,
        }
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
    if matches!(
        pressure,
        PressureLevel::Critical | PressureLevel::Emergency
    ) {
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
) -> (u64, u64, Option<PendingFlushPackage>, bool, Option<(usize, usize)>) {
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
    } else if rss_pressure_elevated_only {
        // Elevated: don't disturb the pipeline. Take a normal-threshold flush if it's already due,
        // skip heap_trim. The download-ahead reduction (already applied by adjust_max_ahead_live)
        // is enough to ease the pressure.
        //
        // Use adds-only drain: non-durable async flushes must never write DEL tombstones to SST.
        // If a DEL reaches SST without the watermark having advanced past its spend height, a
        // crash+resume will find "not found" for the UTXO (DEL in newer SST beats ADD in older
        // SST) and fail with UTXO_TOTAL_MISS. DELs stay in the pending_log and are written
        // correctly by Phase 3 of the next durable checkpoint flush.
        let (batch, force_durability) = ibd_retire_pick_flush_batch(store);
        (0u64, 0u64, batch, force_durability, cap_change)
    } else if ibd_defer_flush {
        let (batch, force_durability) = ibd_retire_pick_flush_batch(store);
        (0u64, 0u64, batch, force_durability, cap_change)
    } else {
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
    if matches!(level, PressureLevel::Critical)
        && super::tip_stage::tip_crawl_supply_healthy_now()
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
                *mh_guard =
                    std::mem::take(&mut *mh_guard).multiply(&combined_sub_mh);
            }
        }
        let t_after_handle_join = t0.elapsed().as_millis();

        // Step 2: prepare_for_disk for all packages (parallel encoding via rayon).
        let codec = store.value_codec();
        let mut prepared_pkgs: Vec<(Arc<FxHashSet<u32>>, crate::storage::ibd_utxo_store::PreparedFlushPackage)> =
            Vec::with_capacity(n_reqs);
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
        let checkpoint_wm = has_checkpoint.then(|| {
            ibd_checkpoint_watermark_for_batch(&batch, max_height, ibd_defer_checkpoint)
        });

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
                match ibd_flush_del_backlog_drain(
                    &store,
                    &ibd_muhash,
                    checkpoint_wm,
                    true,
                ) {
                    Ok(n) => del_batch_n = n,
                    Err(e) => {
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
            } else {
                if let Err(e) =
                    ibd_flush_leftover_adds_through_watermark(&store, checkpoint_wm)
                {
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
            if let Err(e) = ibd_persist_checkpoint_watermark(
                &storage_wm,
                &store,
                &ibd_muhash,
                checkpoint_wm,
            ) {
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
                warn!("[IBD_FLUSH_GUARD] UTXO flush thread panicked during cleanup: {:?}", e);
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
    let do_durability = retire_do_durability(
        force_durability,
        batch_count,
        n,
        durability_tx.is_some(),
    );

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
    ibd_block_outputs: Option<
        Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>,
    >,
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
    ibd_block_outputs: Option<
        Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>,
    >,
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
    ibd_block_outputs: Option<
        Arc<rustc_hash::FxHashMap<blvm_consensus::OutPoint, Arc<blvm_consensus::UTXO>>>,
    >,
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
        rx: std::sync::mpsc::Receiver<
            std::result::Result<(), blvm_consensus::ConsensusError>,
        >,
    }
    let mut wave_pending: Vec<WavePending> = Vec::new();
    let wave_depth: usize = std::env::var("BLVM_ECDSA_WAVE_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
        .clamp(1, 32);
    let flush_wave_ready =
        |pending: &mut Vec<WavePending>,
         out: &crossbeam_channel::Sender<ValidateResult>| {
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
        |pending: &mut Vec<WavePending>,
         out: &crossbeam_channel::Sender<ValidateResult>| {
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
                crate::storage::ibd_engine::note_tip_disk_hints(
                    session.disk_segs,
                    session.disk_ms,
                );
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

/// Run the IBD validation loop. Called from std::thread::spawn.
pub fn run_validation_loop(params: ValidationParams) -> Result<()> {
    let feeder_state = params.feeder_state;
    let ibd_store_v2_for_validation = params.ibd_store;
    let blockstore = params.blockstore;
    let storage_clone = params.storage;
    let parallel_ibd = params.parallel_ibd;
    let protocol = params.protocol;
    let _utxo_mutex = params.utxo_mutex;
    let effective_end_live = params.effective_end_live;
    let effective_end_height = || effective_end_live.load(std::sync::atomic::Ordering::Relaxed);
    let start_height = params.start_height;
    let validation_height = params.validation_height;
    let local_replay_max_height = params.local_replay_max_height;
    // When IBD starts from genesis (start_height ≤ 1) with local replay available, the LMDB
    // UTXO store was wiped clean before this run. Skipping LMDB writes during local replay
    // eliminates the channel-fill→worker-spin→freeze loop (3000 BPS >> 97k ops/sec LMDB).
    // A one-shot bulk DashMap→LMDB write fires at the replay→download transition instead.
    let local_replay_no_lmdb_max: u64 = if start_height <= 1 && local_replay_max_height > 0 {
        local_replay_max_height
    } else {
        0 // Normal mode: incremental LMDB durability from the start.
    };
    // Shared CAS flag: exactly ONE retire shard performs the bulk cache→LMDB write at the
    // replay→download transition. Without this, each of the N retire shards (N=4 on 64-core)
    // independently fires its own bulk write — N concurrent 76M-UTXO Vec allocations → OOM.
    let local_replay_transition_done: Arc<std::sync::atomic::AtomicBool> =
        Arc::new(std::sync::atomic::AtomicBool::new(local_replay_no_lmdb_max == 0));
    // Set to `true` only AFTER the winning shard's flush_full_cache_to_lmdb() completes.
    // Losing shards spin-wait on this before processing any block h > local_replay_no_lmdb_max,
    // preventing cache modifications (UTXOs being spent) from racing with the DashMap
    // iteration inside flush_full_cache_to_lmdb — the race that caused UTXO_TOTAL_MISS.
    let local_replay_hydration_done: Arc<std::sync::atomic::AtomicBool> =
        Arc::new(std::sync::atomic::AtomicBool::new(local_replay_no_lmdb_max == 0));
    let mem_guard = params.mem_guard;
    let system_total_ram_mb = mem_guard.system_total_ram_mb();
    let max_pending_ops_nominal = mem_guard.nominal_max_pending_ops_for_guard();
    // Extract the spec_adds_bytes Arc before the guard goes behind a Mutex so the coordinator
    // can update it lock-free. MemoryGuard::memory_snapshot() reads it via Relaxed load.
    let spec_adds_bytes = Arc::clone(&mem_guard.spec_adds_bytes);
    let mem_mtx = Arc::new(Mutex::new(mem_guard));
    let max_ahead_live = params.max_ahead_live;
    let nominal_max_ahead = params.nominal_max_ahead;
    let utxo_nominal_max_entries = params.utxo_nominal_max_entries;
    let stall_tx = params.stall_tx;
    let utxo_engine = params.utxo_engine;
    let engine_durability = utxo_engine
        .as_ref()
        .map(|_| crate::config::ibd::ibd_engine_durability_config(None));
    let engine_gap_export_defer_until = params.engine_gap_export_defer_until;
    let checkpoint_tx = params.checkpoint_tx;
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

    // One-shot struct size dump to identify jemalloc bin sources.
    // jemalloc small classes (LG_QUANTUM=4): 8,16,32,48,64,80,96,112,128,160,192,224,256,...
    // Objects land in the smallest class >= their allocation size.
    {
        use crate::storage::ibd_engine::memory_run::{BloomFilter, Directory, MemoryRun};
        use crate::storage::ibd_engine::types::{OutputKV, OutputDetail};
        use crate::storage::ibd_engine::spend_session::{PartialSpendSession, SpendSession};
        use blvm_protocol::types::{UTXO, SharedByteString, Block, Transaction,
                                   TransactionInput, TransactionOutput, BlockHeader, OutPoint};
        use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
        use blvm_consensus::block::UtxoDelta;
        use blvm_consensus::bip_validation::Bip30Index;
        fn arc_heap_sz<T>() -> usize { 16 + std::mem::size_of::<T>() }
        fn jemalloc_bin(sz: usize) -> usize {
            // Approximate jemalloc small size class (LG_QUANTUM=4, x86_64)
            const CLASSES: &[usize] = &[
                8,16,32,48,64,80,96,112,128,160,192,224,256,320,384,448,512,
                640,768,896,1024,1280,1536,1792,2048,2560,3072,3584,4096,
            ];
            CLASSES.iter().copied().find(|&c| c >= sz).unwrap_or(sz)
        }
        macro_rules! sz {
            ($t:ty) => {{ let s = std::mem::size_of::<$t>(); (s, jemalloc_bin(s)) }};
        }
        macro_rules! arc_sz {
            ($t:ty) => {{ let s = arc_heap_sz::<$t>(); (s, jemalloc_bin(s)) }};
        }
        let (mr, mr_bin)     = sz!(MemoryRun);
        let (arc_mr, arc_mr_bin) = arc_sz!(MemoryRun);
        let (bloom, bl_bin)  = sz!(BloomFilter);
        let (dir, dir_bin)   = sz!(Directory);
        let (kv, kv_bin)     = sz!(OutputKV);
        let (det, det_bin)   = sz!(OutputDetail);
        let (utxo, utxo_bin) = sz!(UTXO);
        let (arc_utxo, arc_utxo_bin) = arc_sz!(UTXO);
        let (sbs, sbs_bin)   = sz!(SharedByteString);
        let (blk, blk_bin)   = sz!(Block);
        let (tx, tx_bin)     = sz!(Transaction);
        let (txin, txin_bin) = sz!(TransactionInput);
        let (txout, txout_bin) = sz!(TransactionOutput);
        let (bh, bh_bin)     = sz!(BlockHeader);
        let (arc_bh, arc_bh_bin) = arc_sz!(BlockHeader);
        let (op, op_bin)     = sz!(OutPoint);
        let (undo, undo_bin) = sz!(BlockUndoLog);
        let (ue, ue_bin)     = sz!(UndoEntry);
        let (ps, ps_bin)     = sz!(PartialSpendSession);
        let (ss, ss_bin)     = sz!(SpendSession);
        let (ife, ife_bin)   = sz!(InFlightEntry);
        let (evj, evj_bin)   = sz!(EngineValidateJob);
        let (vr, vr_bin)     = sz!(ValidateResult);
        let (utxod, utxod_bin) = sz!(UtxoDelta);
        let (bip30, bip30_bin) = sz!(Bip30Index);
        let (arc_ss, arc_ss_bin) = arc_sz!(SpendSession);
        info!("[SIZE_OF_1] MemoryRun={mr}→bin{mr_bin} Arc<MemoryRun>_heap={arc_mr}→bin{arc_mr_bin} BloomFilter={bloom}→bin{bl_bin} Directory={dir}→bin{dir_bin}");
        info!("[SIZE_OF_2] OutputKV={kv}→bin{kv_bin} OutputDetail={det}→bin{det_bin} UTXO={utxo}→bin{utxo_bin} Arc<UTXO>_heap={arc_utxo}→bin{arc_utxo_bin} SharedByteString={sbs}→bin{sbs_bin}");
        info!("[SIZE_OF_3] Block={blk}→bin{blk_bin} Transaction={tx}→bin{tx_bin} TransactionInput={txin}→bin{txin_bin} TransactionOutput={txout}→bin{txout_bin}");
        info!("[SIZE_OF_4] BlockHeader={bh}→bin{bh_bin} Arc<BlockHeader>_heap={arc_bh}→bin{arc_bh_bin} OutPoint={op}→bin{op_bin} BlockUndoLog={undo}→bin{undo_bin} UndoEntry={ue}→bin{ue_bin}");
        info!("[SIZE_OF_5] PartialSpendSession={ps}→bin{ps_bin} SpendSession={ss}→bin{ss_bin} Arc<SpendSession>_heap={arc_ss}→bin{arc_ss_bin} InFlightEntry={ife}→bin{ife_bin} EngineValidateJob={evj}→bin{evj_bin} ValidateResult={vr}→bin{vr_bin}");
        info!("[SIZE_OF_6] UtxoDelta={utxod}→bin{utxod_bin} Bip30Index={bip30}→bin{bip30_bin} (Bip30Index=FxHashMap<Hash,usize> empty_struct_size)");
    }

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
            info!(
                "IBD profiling ENABLED (BLVM_IBD_DEBUG): sample_interval={}, slow_threshold_ms={}, disk_io={}, blocked_log={}",
                sample, slow, disk, blocked_log
            );
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
    // Seed parents of `start_height` via height_index (not tip `recent_headers` window).
    // Live 2026-07-13: gap resume at 880001 with tip ~957k seeded only 4 stale window
    // entries then fell through to tip MTP → H05 "Invalid block header" death loop.
    if start_height > 1 {
        match blockstore.headers_before_height_for_mtp(start_height) {
            Ok(stored) => {
                for header in stored {
                    blvm_protocol::types::ARC_BLOCKHEADER_CREATED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    recent_headers_buf.push_back(Arc::new(header));
                }
            }
            Err(e) => {
                warn!(
                    "Failed to seed BIP113 MTP headers before {}: {e:#}",
                    start_height
                );
            }
        }
        // Tip-window fallback only when validating near tip (parents should already be
        // in the sliding recent_headers index). Never use tip MTP for deep gap replay.
        let tip_h = blockstore.highest_stored_height().ok().flatten().unwrap_or(0);
        let near_tip = mtp_tip_window_fallback_ok(start_height, tip_h);
        if recent_headers_buf.is_empty() && near_tip {
            if let Ok(stored) = blockstore.get_recent_headers(11) {
                for header in stored {
                    blvm_protocol::types::ARC_BLOCKHEADER_CREATED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    recent_headers_buf.push_back(Arc::new(header));
                }
            }
        }
        if !recent_headers_buf.is_empty() {
            info!(
                "Seeded {} recent header(s) from blockstore for BIP113 MTP (before height {})",
                recent_headers_buf.len(),
                start_height
            );
        } else if start_height > 11 {
            warn!(
                "BIP113 MTP seed empty before height {} (tip={:?}) — H05 may reject blocks",
                start_height,
                blockstore.highest_stored_height().ok().flatten()
            );
        }
    }
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
    let mut pending_blocks: Vec<(
        Arc<Block>,
        Arc<Vec<Vec<Witness>>>,
        u64,
        blvm_consensus::reorganization::BlockUndoLog,
    )> = Vec::with_capacity(storage_flush_interval);
    /// Sum of feeder `est_bytes` for entries in `pending_blocks` (same heuristic as [`super::types::estimate_block_bytes`]; pressure-path flush only).
    let mut pending_storage_bytes: u64 = 0;
    let skip_storage = false;
    let initial_buffer_limit = mem_mtx.lock().buffer_limit(start_height);
    super::IBD_FEEDER_BUFFER_CAP.store(
        initial_buffer_limit.max(400),
        std::sync::atomic::Ordering::Relaxed,
    );

    if local_replay_max_height > 0 {
        info!(
            "IBD: local replay mode active — skipping block store writes for heights ≤ {} \
             (blocks already on disk; eliminates redundant LMDB I/O stalls during replay)",
            local_replay_max_height
        );
    }

    super::ms_breakdown::arm();
    info!(
        "Validation loop starting (deferred storage: flush every ~{} blocks [pressure-scaled], extra flush under Critical/Emergency when pending bytes exceed budget cap, initial buffer limit: {}, utxo_prefetch_lookahead_nominal: {})...",
        storage_flush_interval, initial_buffer_limit, nominal_prefetch_lookahead,
    );

    let mut next_validation_height = start_height;
    // Consecutive feeder waits at the same height without receiving a block.
    let mut feeder_stall_count: u32 = 0;
    let mut feeder_stall_at_height: u64 = start_height;
    let mut feeder_stall_started: Option<std::time::Instant> = None;

    use blvm_consensus::pow::{U256, get_block_proof};
    let mut running_header_chainwork = if start_height == 0 {
        U256::zero()
    } else {
        blockstore
            .get_hash_by_height(start_height.saturating_sub(1))
            .ok()
            .flatten()
            .and_then(|prev_hash| {
                storage_clone
                    .chain()
                    .get_chainwork(&prev_hash)
                    .ok()
                    .flatten()
            })
            .unwrap_or(U256::zero())
    };

    // FEEDER BUFFER: Block feeder drains ready_rx into shared state. We read next block and
    // lookahead blocks for protect_keys. Buffer fills while validation runs.

    // Async flush: block batches on std::thread (validation runs off tokio).
    let mut flush_handles: VecDeque<std::thread::JoinHandle<Result<()>>> = VecDeque::new();
    // Atomic mirrors readable by the watchdog without holding a mutex.
    let block_flush_in_flight = Arc::new(AtomicUsize::new(0));
    // Live count and byte estimate of entries in `pending_blocks` (blockstore flush queue).
    // Updated on every push/take; readable by watchdog for MEM_REPORT attribution.
    let pending_blocks_count_atomic: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let pending_blocks_bytes_atomic: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    // Counts durability packages currently queued OR being processed by the durability thread.
    let durability_in_flight = Arc::new(AtomicUsize::new(0));
    let utxo_flush_handles = Arc::new(Mutex::new(VecDeque::<
        std::thread::JoinHandle<Result<blvm_muhash::MuHash3072>>,
    >::new()));
    // Guard: ensures flush threads are joined on ANY exit path (success or early Err return).
    // The normal success path drains utxo_flush_handles explicitly; the guard is then a no-op.
    let _utxo_flush_guard = UtxoFlushGuard(Arc::clone(&utxo_flush_handles));
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
    // Peak recent BPS window — used to detect slow stretches vs good-day bursts.
    let mut peak_recent_rate: f64 = 0.0;
    let mut last_slow_stretch_log: Option<Instant> = None;
    // mi_collect removed from adaptive path (see LAST_IBD_HEAP_TRIM_WALL_MS comment).
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
    // a 16 GiB host we observed 22.5 M ops (~4.6 GB) → OOM; at h=605 k on a 92 GiB shared
    // workstation pending hit 98 M with cap disabled — see docs/IBD_MEMORY_ENVELOPE_FIX.md.
    //
    // Nominal cap is derived from the RSS envelope ([`MemoryGuard::nominal_max_pending_ops`]);
    // the adaptive controller shrinks the live cap under pressure and grows up to ~1.1× nominal
    // when retire keeps up.
    // Live cap that the validation workers read on every backpressure check.
    let max_pending_ops: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(max_pending_ops_nominal));
    let max_pending_ops_last_adapt_ms: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    info!(
        "IBD: pending-ops backpressure active (max_pending_ops={}, adaptive: shrinks under \
         RSS pressure; calm growth up to ~1.1× nominal, Emergency floor max(nominal/16, 100k) — \
         see adapt_max_pending_ops_tick)",
        max_pending_ops_nominal
    );

    // ── Background durability thread ────────────────────────────────────────────────────────
    // Retire threads offload synchronous LMDB durability flushes (2–20 s each) here,
    // eliminating staged_count stalls that idled validation workers for ~40% of SegWit-era
    // IBD (h=500k–956k: 39 IBD_WATCHDOG freeze events observed in one run).
    // Packages arrive in FIFO order; the single reader preserves monotonic watermark + crash safety.
    // Disabled in engine mode (engine bypasses IbdUtxoStore flush path entirely).
    let (durability_tx_opt, _durability_thread_handle) = if !skip_storage && utxo_engine.is_none() {
        // Bounded channel with backpressure. Sizing rules:
        //
        // 1. Cap must be > (BPS × checkpoint_duration / checkpoint_interval) to absorb
        //    one checkpoint batch without filling: at h=300k, 250 BPS × 13s / 200 = 16 req.
        //    Cap=512 gives 32× headroom for the worst case (fast BPS + slow checkpoint).
        //
        // 2. When the channel IS full (LMDB can't keep up with validation rate), retire
        //    briefly blocks, causing staged_count to grow. max_ahead must absorb the block:
        //    staged_growth ≤ BPS × block_duration ≤ 250 × (512 / drain_rate) ≈ 4000 blocks.
        //    BLVM_IBD_MAX_AHEAD must be set ≥ 4096 (see start-ibd-mainnet.sh).
        //
        // 3. Unbounded channel was tried and rejected: with no backpressure, the retire thread
        //    enqueued 83M inflight ops before the durability thread could process them, causing
        //    an inflight-cap stall and internal process recovery from an earlier watermark.
        // Channel cap: 64 requests × ~50-75 MB per package ≈ 3-5 GB max in-flight.
        //
        // The original cap=512 assumed checkpoint_duration ≈ 4s (25 BPS). When the durability
        // thread stalls for minutes (LMDB write amplification at large tree sizes), the channel
        // fills completely: 512 × 75 MB = 38 GB — enough to OOM a 64 GB host. Reduced to 32:
        // each package now carries both ADDs and DELs combined (~500k ops × ~144 B = ~72 MB),
        // so 32 × 72 MB ≈ 2.3 GB peak channel RSS. The prior 64-slot cap with ADD-only packages
        // (~15 MB each) also totalled ~960 MB; with ADDs+DELs per package, 32 slots reproduce
        // that budget while completely eliminating the separate del_backlog flush cycle.
        //
        // If a checkpoint takes longer, retirement blocks on channel-full and staged_count grows.
        // The coordinator's staged_dispatch_cap prevents unbounded validated-but-unretired
        // blocks from accumulating in the feeder.
        // 4 slots default (env override): at SegWit heights each retire package can be large;
        // smaller buffer = correct back-pressure when durability is slow.
        let chan_cap = ibd_durability_channel_cap(start_height);
        info!(
            "[IBD_DURABILITY] durability channel cap={chan_cap} (start_height={start_height})"
        );
        let (tx, rx) = std::sync::mpsc::sync_channel::<DurabilityRequest>(chan_cap);
        let dur_store = Arc::clone(&ibd_store_v2_for_validation);
        let dur_storage_wm = Arc::clone(&storage_for_retire);
        let dur_flush_handles = Arc::clone(&utxo_flush_handles);
        let dur_muhash = Arc::clone(&ibd_muhash_accumulator);
        let dur_err = Arc::clone(&retire_err);
        let dur_inflight = Arc::clone(&durability_in_flight);
        let defer_checkpoint = ibd_defer_checkpoint;
        let handle = std::thread::Builder::new()
            .name("ibd-durability".to_string())
            .spawn(move || {
                run_ibd_durability_loop(
                    dur_store,
                    dur_storage_wm,
                    dur_flush_handles,
                    dur_muhash,
                    dur_err,
                    dur_inflight,
                    defer_checkpoint,
                    rx,
                );
            })
            .expect("spawn IBD durability thread");
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

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
        let utxo_engine_outer = utxo_engine.clone();
        let engine_mode_for_retire = utxo_engine.is_some();
        let durability_tx_outer = durability_tx_opt.clone();
        let replay_transition_done_outer = Arc::clone(&local_replay_transition_done);
        let replay_hydration_done_outer_wc = Arc::clone(&local_replay_hydration_done);
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
                let utxo_engine = utxo_engine_outer.clone();
                let durability_tx = durability_tx_outer.clone();
                let replay_transition_done = Arc::clone(&replay_transition_done_outer);
                let replay_hydration_done = Arc::clone(&replay_hydration_done_outer_wc);
                std::thread::Builder::new()
                    .name(format!("ibd-retire-{i}"))
                    .spawn(move || {
                        run_ibd_retire_loop_with_commitment(
                            work_rx,
                            staged,
                            staged_count,
                            local_last_retired,
                            publisher,
                            i,
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
                            engine_mode_for_retire,
                            utxo_engine,
                            durability_tx,
                            local_replay_no_lmdb_max,
                            replay_transition_done,
                            replay_hydration_done,
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
        let engine_mode_for_retire = utxo_engine.is_some();
        let durability_tx_outer = durability_tx_opt.clone();
        let replay_transition_done_outer = Arc::clone(&local_replay_transition_done);
        let replay_hydration_done_outer = Arc::clone(&local_replay_hydration_done);
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
                let durability_tx = durability_tx_outer.clone();
                let replay_transition_done = Arc::clone(&replay_transition_done_outer);
                let replay_hydration_done = Arc::clone(&replay_hydration_done_outer);
                std::thread::Builder::new()
                    .name(format!("ibd-retire-{i}"))
                    .spawn(move || {
                        run_ibd_retire_loop_no_commitment(
                            work_rx,
                            staged,
                            staged_count,
                            local_last_retired,
                            publisher,
                            i,
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
                            engine_mode_for_retire,
                            durability_tx,
                            local_replay_no_lmdb_max,
                            replay_transition_done,
                            replay_hydration_done,
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
    //
    // Engine mode default 16 (not 32): each in-flight height pins age-0; with 1-height
    // frozen runs, depth 32 made mergeable_prefix 0–1 and stalled the compacter.
    let engine_default_depth = if utxo_engine.is_some() { 16 } else { 32 };
    let n_pipeline_depth: usize = std::env::var("BLVM_IBD_PIPELINE_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| engine_default_depth.clamp(n_validate_workers, 64))
        .max(n_validate_workers);

    info!(
        "IBD: n_validate_workers={} pipeline_depth={} engine_mode={}",
        n_validate_workers,
        n_pipeline_depth,
        utxo_engine.is_some()
    );

    // crossbeam_channel: native multi-consumer recv() with no Mutex — workers can dequeue concurrently.
    // unbounded() so dispatcher never blocks on a slow worker; the in-flight cap enforces backpressure.
    let (valjob_tx, valjob_rx) = crossbeam_channel::unbounded::<ValidateJob>();
    let (valres_tx, valres_rx) = crossbeam_channel::unbounded::<ValidateResult>();
    let use_async_engine_append = utxo_engine.is_some() && async_engine_append_enabled();
    /// Drop closes the append queue first, then joins `ibd-engine-append` (all exit paths).
    struct EngineAppendPipeline {
        tx: Option<crossbeam_channel::Sender<EngineAppendJob>>,
        handle: Option<JoinHandle<()>>,
    }
    impl EngineAppendPipeline {
        fn sender(&self) -> Option<&crossbeam_channel::Sender<EngineAppendJob>> {
            self.tx.as_ref()
        }
        fn close_and_join(&mut self) {
            drop(self.tx.take());
            if let Some(h) = self.handle.take() {
                if let Err(e) = h.join() {
                    warn!("IBD engine append join error: {:?}", e);
                }
            }
        }
    }
    impl Drop for EngineAppendPipeline {
        fn drop(&mut self) {
            self.close_and_join();
        }
    }
    // Append thread needs valjob + valres senders (success → workers; append fail → orchestrator).
    let mut engine_append = if use_async_engine_append {
        let (append_tx, append_rx) = crossbeam_channel::unbounded::<EngineAppendJob>();
        let vjob_tx = valjob_tx.clone();
        let vres_tx = valres_tx.clone();
        let join = std::thread::Builder::new()
            .name("ibd-engine-append".into())
            .spawn(move || {
                while let Ok(job) = append_rx.recv() {
                    let t_append = Instant::now();
                    match SpendSession::append(
                        Arc::clone(&job.db),
                        job.block_arc.as_ref(),
                        job.tx_ids.as_slice(),
                        job.height as i32,
                    ) {
                        Ok(partial) => {
                            let engine_append_ms = t_append.elapsed().as_millis() as u64;
                            if vjob_tx
                                .send(ValidateJob::Engine(EngineValidateJob {
                                    height: job.height,
                                    block_arc: job.block_arc,
                                    witnesses_storage: job.witnesses_storage,
                                    bip30_index: job.bip30_index,
                                    recent_headers: job.recent_headers,
                                    tx_ids: job.tx_ids,
                                    best_header_chainwork: job.best_header_chainwork,
                                    cached_network_time: job.cached_network_time,
                                    partial_session: partial,
                                    engine_append_ms,
                                    ibd_block_outputs: job.ibd_block_outputs,
                                }))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = vres_tx.send(ValidateResult {
                                height: job.height,
                                result: Err(e.context(format!(
                                    "IBD engine SpendSession::append failed at height {}",
                                    job.height
                                ))),
                                undo_log: blvm_consensus::reorganization::BlockUndoLog::new(),
                                bip30_post: job.bip30_index,
                                elapsed: Duration::ZERO,
                                view_build_ms: 0,
                                engine_append_ms: t_append.elapsed().as_millis() as u64,
                                engine_complete_ms: 0,
                                block_muhash: None,
                            });
                            // Stop accepting further appends; orchestrator will see the error
                            // via in-order collect and tear down.
                            break;
                        }
                    }
                }
            })
            .expect("spawn IBD engine append thread");
        info!(
            "IBD: async engine append thread enabled (opt out BLVM_IBD_ASYNC_ENGINE_APPEND=0)"
        );
        EngineAppendPipeline {
            tx: Some(append_tx),
            handle: Some(join),
        }
    } else {
        EngineAppendPipeline {
            tx: None,
            handle: None,
        }
    };
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
    drop(valres_tx); // workers (+ optional append thread) hold live Sender clones
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
    struct IbdValidationWatchdogGuard {
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for IbdValidationWatchdogGuard {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
    let watchdog_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog_handle = {
        let validation_height = Arc::clone(&validation_height);
        let store = Arc::clone(&ibd_store_v2_for_validation);
        let max_pending_ops_w = Arc::clone(&max_pending_ops);
        let staged_count_w = Arc::clone(&staged_count);
        let utxo_flush_handles_w = Arc::clone(&utxo_flush_handles);
        let block_flush_in_flight_w = Arc::clone(&block_flush_in_flight);
        let durability_in_flight_w = Arc::clone(&durability_in_flight);
        let staged_w = Arc::clone(&staged);
        let shutdown = Arc::clone(&watchdog_shutdown);
        let feeder_state_w = Arc::clone(&feeder_state);
        let utxo_engine_w = utxo_engine.clone();
        let pending_blocks_count_w = Arc::clone(&pending_blocks_count_atomic);
        let pending_blocks_bytes_w = Arc::clone(&pending_blocks_bytes_atomic);
        std::thread::Builder::new()
            .name("ibd-validation-watchdog".into())
            .spawn(move || {
                const POLL: std::time::Duration = std::time::Duration::from_secs(10);
                const FREEZE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(30);
                let mut last_height = validation_height.load(Ordering::Relaxed);
                let mut last_change = std::time::Instant::now();
                let mut last_log = std::time::Instant::now() - FREEZE_THRESHOLD;
                let mut watchdog_poll_count = 0u64;
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(POLL);
                    watchdog_poll_count += 1;
                    let h = validation_height.load(Ordering::SeqCst);
                    // Every poll (10s): force-reclaim abandoned mimalloc pages.
                    //
                    // MUST be called frequently when PAGE_RECLAIM_ON_FREE=1. Cross-thread
                    // frees (feeder allocates block data → validator thread frees) produce
                    // a continuous stream of abandoned pages. Each abandoned page stays in
                    // the global abandoned list until reclaimed. Abandoned pages are NOT
                    // eligible for PURGE_DELAY until after reclaim — so they accumulate
                    // indefinitely without periodic mi_collect(true).
                    //
                    // Steady-state abandoned-page RSS ≈ allocation_rate × collect_interval.
                    // At 1-2 GB/s (h=400k-500k): 10s interval → ~10-20 GB. At 30s: 30-60 GB.
                    // Cost: ~0.5ms per call — negligible at 10s intervals.
                    #[cfg(feature = "mimalloc")]
                    {
                        unsafe { libmimalloc_sys::mi_collect(true); }
                    }

                    // Every poll (10s): for jemalloc, purge all arenas to return dirty freed
                    // pages to the OS via MADV_DONTNEED. The background thread does this
                    // automatically after dirty_decay_ms, but an explicit purge here ensures
                    // freed pages are returned promptly during the watchdog window.
                    #[cfg(feature = "jemalloc")]
                    {
                        use std::os::raw::c_void;
                        unsafe extern "C" {
                            fn _rjem_mallctl(name: *const i8, oldp: *mut c_void, oldlenp: *mut usize, newp: *mut c_void, newlen: usize) -> i32;
                        }
                        unsafe {
                            _rjem_mallctl(b"arena.4294967295.purge\0".as_ptr() as *const i8,
                                          std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0);
                        }
                    }

                    // Memory pressure diagnostics (every poll). Kill decisions are handled
                    // externally by scripts/ibd-mem-monitor.sh — not inside the binary.
                    {
                        let (anon_now, swap_now) = memory::read_proc_anon_and_swap_mb();
                        let total_now = anon_now + swap_now;
                        let (swap_total_mb, swap_free_mb) = {
                            use std::io::BufRead;
                            let mut swap_total_kb = 0u64;
                            let mut swap_free_kb = 0u64;
                            if let Ok(f) = std::fs::File::open("/proc/meminfo") {
                                for line in std::io::BufReader::new(f).lines().flatten() {
                                    if line.starts_with("SwapTotal:") {
                                        swap_total_kb = line.split_whitespace().nth(1)
                                            .and_then(|v| v.parse().ok()).unwrap_or(0);
                                    } else if line.starts_with("SwapFree:") {
                                        swap_free_kb = line.split_whitespace().nth(1)
                                            .and_then(|v| v.parse().ok()).unwrap_or(0);
                                    }
                                }
                            }
                            (swap_total_kb / 1024, swap_free_kb / 1024)
                        };
                        let swap_used_mb = swap_total_mb.saturating_sub(swap_free_mb);
                        if swap_total_mb >= 4096 && swap_used_mb * 100 / swap_total_mb >= 70 {
                            warn!(
                                "[IBD_WATCHDOG_SWAP] h={} system swap {}MB/{}MB ({}% used). \
                                 Process anon={}MB proc_swap={}MB total_anon={}MB. \
                                 External monitor (ibd-mem-monitor.sh) should kill before OOM.",
                                h, swap_used_mb, swap_total_mb,
                                swap_used_mb * 100 / swap_total_mb,
                                anon_now, swap_now, total_now
                            );
                        }
                    }
                    // Every 60s: emit heartbeat + full memory attribution report
                    if watchdog_poll_count % 6 == 0 {
                        info!(
                            "[IBD_WATCHDOG_HEARTBEAT] alive: h={} last_h={} elapsed_since_change={}s poll={}",
                            h, last_height, last_change.elapsed().as_secs(), watchdog_poll_count,
                        );
                        // --- Memory attribution report ---
                        // Read live structure sizes first (non-blocking best-effort).
                        let cache_n = store.len();
                        let cache_cap = store.cache_capacity();
                        let inflight_cap = store.inflight_capacity();
                        let pending_n = store.pending_len();
                        let inflight_n = store.in_flight_len();
                        let recently_acc_n = store.recently_accessed_len();
                        let feeder_n = feeder_state_w.0.try_lock().map(|g| g.0.len()).unwrap_or(usize::MAX);
                        let staged_n = staged_w.try_lock().map(|g| g.len()).unwrap_or(usize::MAX);

                        // Exact struct sizes for per-entry accounting.
                        // DashMap bucket = K + V = OutPointKey(40) + UtxoCacheSlot(24) = 64 bytes + 1 ctrl.
                        // Arc<UTXO> heap alloc = 16 (Arc header) + sizeof(UTXO) bytes.
                        use blvm_protocol::types::UTXO;
                        use crate::storage::ibd_utxo_store::UtxoCacheSlot;
                        type OutPointKey = [u8; 40];
                        let sz_kv = std::mem::size_of::<OutPointKey>() + std::mem::size_of::<UtxoCacheSlot>();
                        let sz_utxo = std::mem::size_of::<UTXO>();
                        let sz_arc_utxo = 16 + sz_utxo; // Arc header + UTXO
                        // PendingLogEntry = (OutPointKey, Option<Arc<UTXO>>, u64)
                        let sz_pending_entry = std::mem::size_of::<OutPointKey>() + 8 + 8; // key + ptr + height

                        // DashMap table memory = capacity × (K+V bytes + 1 ctrl byte).
                        let cache_table_mb = (cache_cap as u64 * (sz_kv as u64 + 1)) / (1024 * 1024);
                        let inflight_table_mb = (inflight_cap as u64 * (std::mem::size_of::<OutPointKey>() as u64 + 9)) / (1024 * 1024);
                        // Live Arc<UTXO> allocations on heap (cache entries × Arc<UTXO> size).
                        let arc_heap_mb = (cache_n as u64 * sz_arc_utxo as u64) / (1024 * 1024);
                        let pending_est_mb = (pending_n as u64 * sz_pending_entry as u64) / (1024 * 1024);
                        // Pending blockstore flush queue (Arc<Block> + BlockUndoLog per entry).
                        // This is the DISPATCH THREAD's Vec<...> — tracked via atomic mirrors.
                        let pb_count = pending_blocks_count_w.load(Ordering::Relaxed);
                        let pb_bytes_mb = pending_blocks_bytes_w.load(Ordering::Relaxed) / (1024 * 1024);
                        // Actual byte estimate from feeder: sum estimate_block_bytes for each entry.
                        // The old 400 KB/block fixed estimate was calibrated for pre-SegWit blocks;
                        // post-SegWit blocks can be 1–4 MB, making the estimate off by 4–10×.
                        let feeder_est_kb: u64 = if feeder_n == usize::MAX {
                            0
                        } else {
                            feeder_state_w.0.try_lock()
                                .map(|g| g.0.total_bytes_estimate() as u64 / 1024)
                                .unwrap_or(feeder_n as u64 * 400)
                        };
                        // Read actual RSS from /proc/self/smaps_rollup for ground truth.
                        let (rss_total_mb, anon_mb, file_mb) = std::fs::read_to_string("/proc/self/smaps_rollup")
                            .ok()
                            .map(|s| {
                                let rss = s.lines().find(|l| l.starts_with("Rss:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) / 1024;
                                let anon = s.lines().find(|l| l.starts_with("Anonymous:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) / 1024;
                                let file = s.lines().find(|l| l.starts_with("Private_Clean:"))
                                    .and_then(|l| l.split_whitespace().nth(1))
                                    .and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) / 1024;
                                (rss, anon, file)
                            })
                            .unwrap_or((0, 0, 0));
                        // Read mimalloc virtual vs RSS from /proc/self/smaps.
                        let (mi_virtual_mb, mi_rss_mb) = {
                            use std::io::BufRead;
                            let mut vt = 0u64;
                            let mut rt = 0u64;
                            if let Ok(f) = std::fs::File::open("/proc/self/smaps") {
                                let mut in_mi = false;
                                let mut cur_vsize = 0u64;
                                for line in std::io::BufReader::new(f).lines().flatten() {
                                    if line.ends_with("[anon:mimalloc]") {
                                        in_mi = true;
                                        if let Some(range) = line.split_whitespace().next() {
                                            let mut p = range.splitn(2, '-');
                                            if let (Some(s), Some(e)) = (p.next(), p.next()) {
                                                let start = usize::from_str_radix(s, 16).unwrap_or(0);
                                                let end   = usize::from_str_radix(e, 16).unwrap_or(0);
                                                cur_vsize = (end.saturating_sub(start) / 1024 / 1024) as u64;
                                                vt += cur_vsize;
                                            }
                                        }
                                        continue;
                                    }
                                    if in_mi {
                                        if line.starts_with("Rss:") {
                                            if let Some(v) = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()) {
                                                rt += v / 1024;
                                            }
                                        }
                                        if line.starts_with("VmFlags:") { in_mi = false; }
                                    }
                                }
                            }
                            (vt, rt)
                        };
                        // Top-N anon regions by RSS for fragmentation diagnosis.
                        let top_anon = {
                            use std::io::BufRead;
                            let mut regions: Vec<(u64, String)> = Vec::new();
                            if let Ok(f) = std::fs::File::open("/proc/self/smaps") {
                                let mut cur_label = String::new();
                                let mut cur_rss = 0u64;
                                for line in std::io::BufReader::new(f).lines().flatten() {
                                    if line.starts_with(|c: char| c.is_ascii_hexdigit()) {
                                        if cur_rss > 100 * 1024 {
                                            regions.push((cur_rss, cur_label.clone()));
                                        }
                                        let parts: Vec<&str> = line.splitn(6, ' ').collect();
                                        cur_label = parts.last().cloned().unwrap_or("").trim().to_string();
                                        if cur_label.is_empty() { cur_label = "[anon]".to_string(); }
                                        cur_rss = 0;
                                    } else if line.starts_with("Rss:") {
                                        cur_rss = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                                    }
                                }
                            }
                            regions.sort_by(|a, b| b.0.cmp(&a.0));
                            regions.into_iter().take(6).map(|(r, l)| format!("{}MB:{}", r/1024, l)).collect::<Vec<_>>().join(", ")
                        };
                        // Estimate staged BTreeMap: each entry is Arc<UtxoDelta> which holds
                        // FxHashMap<OutPoint, Arc<UTXO>> (additions) + FxHashSet<[u8;36]> (deletions).
                        // At h=184k: ~1000 adds × (48B slot + 72B arc_utxo) + ~800 dels × 37B slot ≈ 150KB/block.
                        let utxos_per_block_est: u64 = if h < 200_000 { 300 } else if h < 400_000 { 1000 } else { 3000 };
                        let staged_est_kb = if staged_n == usize::MAX { 0u64 } else {
                            staged_n as u64 * utxos_per_block_est * 150 / 1024
                        };
                        // Mimalloc region count.
                        let mi_region_count = {
                            use std::io::BufRead;
                            let mut n = 0usize;
                            if let Ok(f) = std::fs::File::open("/proc/self/smaps") {
                                for line in std::io::BufReader::new(f).lines().flatten() {
                                    if line.ends_with("[anon:mimalloc]") { n += 1; }
                                }
                            }
                            n
                        };
                        // Inflight Arc<UTXO> heap (separate from inflight table in DashMap).
                        let inflight_arc_mb = (inflight_n as u64 * sz_arc_utxo as u64) / (1024 * 1024);
                        // Feeder + staged pipeline bytes (already computed above as KB, convert to MB).
                        let feeder_mb = feeder_est_kb / 1024;
                        let staged_mb = staged_est_kb / 1024;
                        // Accounted anonymous = DashMap tables + live Arc<UTXO> heaps + pending log + inflight + pipeline
                        let accounted_mb = cache_table_mb + arc_heap_mb + pending_est_mb + inflight_table_mb + inflight_arc_mb + feeder_mb + staged_mb;
                        let (engine_in_place, engine_slow, engine_contention) =
                            crate::storage::ibd_engine::memory_age::memory_age_append_stats();
                        let engine_slow_pct = if engine_in_place + engine_slow > 0 {
                            engine_slow * 100 / (engine_in_place + engine_slow)
                        } else {
                            0
                        };
                        let engine_contention_pct = if engine_in_place + engine_slow > 0 {
                            engine_contention * 100 / (engine_in_place + engine_slow)
                        } else {
                            0
                        };
                        // Engine memory + per-age breakdown.
                        // b = compacter_inflight_mb: transient Vec<OutputKV> held by compacter
                        // threads during active merges. These are NOT in index_bytes yet and
                        // show up as UNEXPLAINED_ANON. Now included in accounted total.
                        let (engine_index_mb, engine_compacter_mb, engine_tail_mb, age_detail, disk_detail, table_file_mb) =
                            utxo_engine_w.as_deref().map(|db| {
                                let (a, b, c) = db.mem_usage_bytes();
                                let (ages, disk) = db.age_detail();
                                let file_mb = db.table_file_bytes() / (1024 * 1024);
                                (
                                    a as u64 / (1024 * 1024),
                                    b as u64 / (1024 * 1024),
                                    c as u64 / (1024 * 1024),
                                    ages,
                                    disk,
                                    file_mb,
                                )
                            }).unwrap_or((0, 0, 0, vec![], (0, 0), 0));
                        let engine_total_mb = engine_index_mb + engine_compacter_mb + engine_tail_mb;
                        let accounted_total_mb = accounted_mb + engine_total_mb;

                        // Process VmSwap — how much of our anonymous memory the kernel has pushed to swap.
                        // anon_mb = in-RAM anonymous; vm_swap_mb = same memory paged out.
                        // total_anon = anon_mb + vm_swap_mb is the real committed anonymous footprint.
                        let vm_swap_mb = {
                            use std::io::BufRead;
                            let mut swap_kb = 0u64;
                            if let Ok(f) = std::fs::File::open("/proc/self/status") {
                                for line in std::io::BufReader::new(f).lines().flatten() {
                                    if line.starts_with("VmSwap:") {
                                        swap_kb = line.split_whitespace().nth(1)
                                            .and_then(|v| v.parse::<u64>().ok())
                                            .unwrap_or(0);
                                        break;
                                    }
                                }
                            }
                            swap_kb / 1024
                        };
                        let total_anon_mb = anon_mb + vm_swap_mb;

                        // Format per-age detail string: "a0:2r/4MB a1:3r/18MB a2:7r/201MB a3:8r/2301MB"
                        let age_detail_str = age_detail.iter().enumerate()
                            .map(|(i, (n_runs, mb))| format!("a{}:{}r/{}MB", i, n_runs, mb))
                            .collect::<Vec<_>>().join(" ");
                        let disk_detail_str = format!("disk:{}segs/{}MB", disk_detail.0, disk_detail.1);

                        // Number of OS threads in the current process (from /proc/self/status Threads:).
                        let os_threads = {
                            use std::io::BufRead;
                            let mut n = 0u64;
                            if let Ok(f) = std::fs::File::open("/proc/self/status") {
                                for line in std::io::BufReader::new(f).lines().flatten() {
                                    if line.starts_with("Threads:") {
                                        n = line.split_whitespace().nth(1)
                                            .and_then(|v| v.parse().ok()).unwrap_or(0);
                                        break;
                                    }
                                }
                            }
                            n
                        };
                        // Count Rayon global thread pool size. Each Rayon thread has its own
                        // mimalloc heap; at num_cpus (35) threads each holding ~60-100 MB of
                        // freed page cache, Rayon contributes ~2-3.5 GB UNEXPLAINED_ANON.
                        // BLVM_SCRIPT_THREADS=12 reduces this to ~720 MB-1.2 GB.
                        #[cfg(feature = "rayon")]
                        let rayon_threads = rayon::current_num_threads() as u64;
                        #[cfg(not(feature = "rayon"))]
                        let rayon_threads = 0u64;
                        // Per-thread mimalloc cache estimate: ~60 MB average for active threads
                        // processing large block allocations. This is an estimate; actual is
                        // visible in [MI_STATS] "thread caches" row vs anon=N in MEM_REPORT.
                        let rayon_est_mb = rayon_threads * 60;
                        // Remaining UNEXPLAINED after subtracting rayon estimate helps isolate
                        // other sources (Tokio blocking pool, validation worker stacks, etc.).
                        let unexplained_mb = anon_mb.saturating_sub(accounted_total_mb);
                        let reorder_count = memory::BLOCK_BUFFER_COUNT.load(Ordering::Relaxed);
                        let reorder_bytes_mb =
                            memory::BLOCK_BUFFER_BYTES.load(Ordering::Relaxed) / (1024 * 1024);
                        let bridge_pending = memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
                        let bridge_est_mb = if reorder_count > 0 {
                            bridge_pending * reorder_bytes_mb / reorder_count
                        } else {
                            bridge_pending * 2
                        };
                        let gap_flush_on_abort =
                            memory::GAP_FLUSH_ON_ABORT_BLOCKS.load(Ordering::Relaxed);
                        let pipeline_buffers_mb = reorder_bytes_mb + bridge_est_mb;
                        let adjusted_unexplained_mb =
                            unexplained_mb.saturating_sub(pipeline_buffers_mb);
                        let post_rayon_unexplained_mb =
                            adjusted_unexplained_mb.saturating_sub(rayon_est_mb);

                        // jemalloc stats: allocated = live app bytes, resident = physical pages
                        // in jemalloc arenas (includes dirty freed pages not yet MADV_DONTNEED'd).
                        // If allocated << resident, jemalloc is holding freed pages (purge needed).
                        // If allocated ≈ resident ≈ UNEXPLAINED_ANON, the app has untracked live allocs.
                        #[cfg(feature = "jemalloc")]
                        let (jemalloc_allocated_mb, jemalloc_active_mb, jemalloc_resident_mb, jemalloc_retained_mb) = {
                            use std::os::raw::c_void;
                            unsafe extern "C" {
                                fn _rjem_mallctl(name: *const i8, oldp: *mut c_void, oldlenp: *mut usize, newp: *mut c_void, newlen: usize) -> i32;
                            }
                            unsafe {
                                // Refresh epoch to get up-to-date stats.
                                let mut epoch: u64 = 1;
                                _rjem_mallctl(b"epoch\0".as_ptr() as *const i8, std::ptr::null_mut(), std::ptr::null_mut(),
                                              &mut epoch as *mut u64 as *mut c_void, std::mem::size_of::<u64>());
                                let mut allocated: usize = 0;
                                let mut sz = std::mem::size_of::<usize>();
                                _rjem_mallctl(b"stats.allocated\0".as_ptr() as *const i8,
                                              &mut allocated as *mut usize as *mut c_void, &mut sz, std::ptr::null_mut(), 0);
                                let mut active: usize = 0;
                                let mut sz = std::mem::size_of::<usize>();
                                _rjem_mallctl(b"stats.active\0".as_ptr() as *const i8,
                                              &mut active as *mut usize as *mut c_void, &mut sz, std::ptr::null_mut(), 0);
                                let mut resident: usize = 0;
                                let mut sz = std::mem::size_of::<usize>();
                                _rjem_mallctl(b"stats.resident\0".as_ptr() as *const i8,
                                              &mut resident as *mut usize as *mut c_void, &mut sz, std::ptr::null_mut(), 0);
                                let mut retained: usize = 0;
                                let mut sz = std::mem::size_of::<usize>();
                                _rjem_mallctl(b"stats.retained\0".as_ptr() as *const i8,
                                              &mut retained as *mut usize as *mut c_void, &mut sz, std::ptr::null_mut(), 0);
                                (
                                    allocated as u64 / (1024 * 1024),
                                    active as u64 / (1024 * 1024),
                                    resident as u64 / (1024 * 1024),
                                    retained as u64 / (1024 * 1024),
                                )
                            }
                        };
                        #[cfg(not(feature = "jemalloc"))]
                        let (jemalloc_allocated_mb, jemalloc_active_mb, jemalloc_resident_mb, jemalloc_retained_mb) = (0u64, 0u64, 0u64, 0u64);

                        // Dump jemalloc per-size-class stats to identify where allocations are.
                        // Called once per MEM_REPORT interval. Captures the human-readable
                        // malloc_stats_print output, then extracts the size-class lines
                        // (those containing "reg_size" in the bin breakdown) and logs the
                        // top-10 by bytes allocated.  This lets us identify what object sizes
                        // are consuming the unexplained anonymous memory.
                        #[cfg(feature = "jemalloc")]
                        {
                            use std::os::raw::{c_char, c_void};
                            use std::sync::Mutex;
                            static JEMALLOC_STATS_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());
                            unsafe extern "C" fn je_stats_cb(opaque: *mut c_void, msg: *const c_char) {
                                if msg.is_null() { return; }
                                let bytes = unsafe { std::ffi::CStr::from_ptr(msg).to_bytes() };
                                if let Some(ptr) = unsafe { (opaque as *mut Vec<u8>).as_mut() } {
                                    ptr.extend_from_slice(bytes);
                                }
                            }
                            unsafe extern "C" {
                                fn _rjem_malloc_stats_print(
                                    write_cb: unsafe extern "C" fn(*mut c_void, *const c_char),
                                    cbopaque: *mut c_void,
                                    opts: *const c_char,
                                );
                            }
                            // Track previous curregs per bin to compute per-interval accumulation delta.
                            // Uses curregs (= allocated/sz) which is robust: unlike nmalloc/ndalloc,
                            // it does not overflow jemalloc's fixed-width print fields.
                            static PREV_CURREGS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, u64>>>
                                = std::sync::OnceLock::new();
                            static PREV_HEIGHT: std::sync::atomic::AtomicU64
                                = std::sync::atomic::AtomicU64::new(0);
                            let _ = PREV_CURREGS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));

                            // Capture stats output into our buffer.
                            if let Ok(mut buf) = JEMALLOC_STATS_BUF.lock() {
                                buf.clear();
                                let opts = b"\0".as_ptr() as *const c_char;
                                unsafe {
                                    _rjem_malloc_stats_print(
                                        je_stats_cb,
                                        &mut *buf as *mut Vec<u8> as *mut c_void,
                                        opts,
                                    );
                                }

                                // Write the full stats to a temp file only when explicitly requested.
                                if std::env::var_os("BLVM_IBD_JEMALLOC_DUMP").is_some() {
                                    let stats_path = format!("/tmp/jemalloc_h{h}.txt");
                                    let _ = std::fs::write(&stats_path, &*buf);
                                }

                                let text = String::from_utf8_lossy(&buf);
                                let alloc_line = text.lines()
                                    .find(|l| l.trim_start().starts_with("Allocated:"))
                                    .map(|l| l.trim().to_string())
                                    .unwrap_or_default();

                                // Parse merged-arenas bins. ROBUST PARSING:
                                // jemalloc prints: size ind allocated nmalloc (#/sec) ndalloc (#/sec) ...
                                // When nmalloc/ndalloc are large (>field width), their (#/sec) column
                                // gets concatenated into one token — column offsets shift unpredictably.
                                // Instead, use: curregs = allocated / sz  (always exact, jemalloc guarantees this).
                                // For large extents: curlextents = allocated / sz likewise.
                                struct BinRow { sz: u64, alloc: u64, curregs: u64 }
                                let mut bin_rows: Vec<BinRow> = Vec::new();
                                let mut in_merged = false;
                                let mut in_section = false; // inside bins: or large: block
                                for line in text.lines() {
                                    if line.contains("Merged arenas stats") {
                                        in_merged = true; in_section = false; continue;
                                    }
                                    // Stop at per-arena sections (arena 0:, arenas[0]:, etc.)
                                    if in_merged && (line.starts_with("arenas[") || line.starts_with("Arena ")) {
                                        break;
                                    }
                                    if in_merged && (
                                        (line.contains("bins:") && line.contains("curregs")) ||
                                        (line.contains("large:") && line.contains("curlextents"))
                                    ) {
                                        in_section = true; continue;
                                    }
                                    if in_merged && in_section {
                                        let parts: Vec<&str> = line.split_whitespace().collect();
                                        if parts.len() >= 3 && parts[0].bytes().all(|b| b.is_ascii_digit()) {
                                            if let (Ok(sz), Ok(alloc)) = (
                                                parts[0].parse::<u64>(), parts[2].parse::<u64>(),
                                            ) {
                                                if sz > 0 && alloc > 0 {
                                                    let curregs = alloc / sz;
                                                    bin_rows.push(BinRow { sz, alloc, curregs });
                                                }
                                            }
                                        }
                                    }
                                }

                                // Sort by allocated bytes descending.
                                bin_rows.sort_by(|a, b| b.alloc.cmp(&a.alloc));

                                // Per-bin CURREGS delta vs previous report.
                                // This directly shows accumulation (positive = objects not freed).
                                let prev_h = PREV_HEIGHT.load(Ordering::Relaxed);
                                let blocks_since = (h.saturating_sub(prev_h)).max(1);
                                let mut delta_lines: Vec<String> = Vec::new();
                                if let Ok(mut prev_cr) = PREV_CURREGS.get().unwrap().lock() {
                                    for row in &bin_rows {
                                        let prev = *prev_cr.get(&row.sz).unwrap_or(&0);
                                        let delta_objs = row.curregs as i64 - prev as i64;
                                        let per_blk = delta_objs / blocks_since as i64;
                                        // Log any bin with meaningful per-block accumulation
                                        if delta_objs.abs() > 1000 || row.alloc > 200 * 1024 * 1024 {
                                            delta_lines.push(format!(
                                                "{}B:{}MB/{}obj({:+}/blk)",
                                                row.sz,
                                                row.alloc / (1024 * 1024),
                                                row.curregs,
                                                per_blk
                                            ));
                                        }
                                        prev_cr.insert(row.sz, row.curregs);
                                    }
                                    PREV_HEIGHT.store(h, Ordering::Relaxed);
                                }

                                let cur_map: std::collections::HashMap<u64, u64> =
                                    bin_rows.iter().map(|r| (r.sz, r.curregs)).collect();
                                let alloc_map: std::collections::HashMap<u64, u64> =
                                    bin_rows.iter().map(|r| (r.sz, r.alloc)).collect();

                                let c32   = cur_map.get(&32).copied().unwrap_or(0);
                                let c48   = cur_map.get(&48).copied().unwrap_or(0);
                                let c80   = cur_map.get(&80).copied().unwrap_or(0);
                                let c96   = cur_map.get(&96).copied().unwrap_or(0);
                                let c112  = cur_map.get(&112).copied().unwrap_or(0);
                                let c128  = cur_map.get(&128).copied().unwrap_or(0);
                                let c160  = cur_map.get(&160).copied().unwrap_or(0);
                                let c256  = cur_map.get(&256).copied().unwrap_or(0);
                                let a32   = alloc_map.get(&32).copied().unwrap_or(0)  / (1024*1024);
                                let a48   = alloc_map.get(&48).copied().unwrap_or(0)  / (1024*1024);
                                let a80   = alloc_map.get(&80).copied().unwrap_or(0)  / (1024*1024);
                                let a96   = alloc_map.get(&96).copied().unwrap_or(0)  / (1024*1024);
                                let a112  = alloc_map.get(&112).copied().unwrap_or(0) / (1024*1024);
                                let a128  = alloc_map.get(&128).copied().unwrap_or(0) / (1024*1024);
                                let a160  = alloc_map.get(&160).copied().unwrap_or(0) / (1024*1024);
                                let a256  = alloc_map.get(&256).copied().unwrap_or(0) / (1024*1024);

                                info!(
                                    "[JEMALLOC_BINS] h={h} since_h={prev_h}(+{blocks_since}blk) {alloc_line} \
                                     | live_objs: 32B={c32}({a32}MB) 48B={c48}({a48}MB) \
                                       80B={c80}({a80}MB) 96B={c96}({a96}MB) 112B={c112}({a112}MB) \
                                       128B={c128}({a128}MB) 160B={c160}({a160}MB) 256B={c256}({a256}MB) \
                                     | BIN_DELTAS(+objs/blk): {}",
                                    if delta_lines.is_empty() { "none".to_string() } else { delta_lines.join(" ") }
                                );

                                // Dump jemalloc heap profile only when BLVM_IBD_JEMALLOC_DUMP is set
                                // (requires MALLOC_CONF=prof:true). Produces /tmp/blvm_heap_{h}.jep.
                                static PROF_DUMP_COUNT: std::sync::atomic::AtomicU64
                                    = std::sync::atomic::AtomicU64::new(0);
                                let dump_idx = PROF_DUMP_COUNT.fetch_add(1, Ordering::Relaxed);
                                if std::env::var_os("BLVM_IBD_JEMALLOC_DUMP").is_none() {
                                    let _ = dump_idx;
                                } else {
                                let prof_path = format!("/tmp/blvm_heap_{h}.jep\0");
                                unsafe {
                                    unsafe extern "C" {
                                        fn _rjem_mallctl(
                                            name: *const std::os::raw::c_char,
                                            oldp: *mut std::os::raw::c_void,
                                            oldlenp: *mut usize,
                                            newp: *mut std::os::raw::c_void,
                                            newlen: usize,
                                        ) -> std::os::raw::c_int;
                                    }
                                    let name_cstr = b"prof.dump\0";
                                    let path_ptr: *const std::os::raw::c_char = prof_path.as_ptr() as _;
                                    let ret = _rjem_mallctl(
                                        name_cstr.as_ptr() as _,
                                        std::ptr::null_mut(),
                                        std::ptr::null_mut(),
                                        &path_ptr as *const _ as *mut std::os::raw::c_void,
                                        std::mem::size_of::<*const std::os::raw::c_char>(),
                                    );
                                    if ret == 0 {
                                        info!("[JEMALLOC_PROF] h={h} dump #{dump_idx} written to {}", &prof_path[..prof_path.len()-1]);
                                    } else if ret == 2 {
                                        // ENOENT: profiling not enabled (MALLOC_CONF=prof:true not set) — silent skip
                                    } else {
                                        info!("[JEMALLOC_PROF] h={h} prof.dump failed errno={ret}");
                                    }
                                }
                                }
                            }
                        }

                        // Live-object counters from Drop-instrumented types.
                        let mr_live  = crate::storage::ibd_engine::memory_run::MEMORY_RUN_LIVE.load(Ordering::Relaxed);
                        let mr_total = crate::storage::ibd_engine::memory_run::MEMORY_RUN_TOTAL.load(Ordering::Relaxed);
                        let od_live  = crate::storage::ibd_engine::types::OUTPUT_DETAIL_LIVE.load(Ordering::Relaxed);
                        let od_total = crate::storage::ibd_engine::types::OUTPUT_DETAIL_CREATED.load(Ordering::Relaxed);
                        let bo_live  = crate::storage::ibd_engine::table::BLOCK_OUTPUTS_LIVE.load(Ordering::Relaxed);
                        let sbs_live  = blvm_protocol::types::SBS_SHARED_LIVE.load(Ordering::Relaxed);
                        let sbs_total = blvm_protocol::types::SBS_SHARED_TOTAL.load(Ordering::Relaxed);
                        let arc_blk_created   = blvm_protocol::types::ARC_BLOCK_CREATED.load(Ordering::Relaxed);
                        let arc_bh_created    = blvm_protocol::types::ARC_BLOCKHEADER_CREATED.load(Ordering::Relaxed);
                        let block_flush_inflight = block_flush_in_flight_w.load(Ordering::Relaxed);
                        // Weak ref check: did the sample block at BLOCK_SAMPLE_HEIGHT get freed?
                        let sample_block_alive = SAMPLE_BLOCK_WEAK
                            .get()
                            .and_then(|m| m.lock().ok())
                            .and_then(|g| g.as_ref().map(|w| w.upgrade().is_some()))
                            .unwrap_or(false);
                        // W55: positional `{}` args must match placeholder order. Pre-W55 put
                        // `gap_flush_on_abort` last while its `{}` sat before ENGINE_MEM — so
                        // `table_file` (on-disk GiB) was labeled ENGINE total and index/compacter
                        // were rotated (live: total=10254MB while ages held ~400MB).
                        info!(
                            "[MEM_REPORT] h={h} \
                             rss={}MB anon={}MB swap={}MB total_anon={}MB file_backed={}MB \
                             os_threads={os_threads} rayon_threads={rayon_threads} rayon_est=~{rayon_est_mb}MB | \
                             mimalloc virtual={}MB rss={}MB dead={}MB regions={mi_region_count} | \
                             jemalloc allocated={jemalloc_allocated_mb}MB active={jemalloc_active_mb}MB resident={jemalloc_resident_mb}MB retained={jemalloc_retained_mb}MB | \
                             cache len={cache_n} cap={cache_cap} table~{cache_table_mb}MB arc~{arc_heap_mb}MB | \
                             inflight len={inflight_n} cap={inflight_cap} table~{inflight_table_mb}MB arc~{inflight_arc_mb}MB | \
                             pending={pending_n} ~{pending_est_mb}MB recently_acc={recently_acc_n} | \
                             pending_blocks_flush={pb_count} ~{pb_bytes_mb}MB | \
                             reorder={reorder_count} ~{reorder_bytes_mb}MB bridge_pending={bridge_pending} ~{bridge_est_mb}MB gap_flush_on_abort={} dl_received={} | \
                             staged={staged_n} ~{staged_est_kb}KB feeder={feeder_n} ~{feeder_est_kb}KB | \
                             engine_append in_place={engine_in_place} slow={engine_slow} slow_pct={engine_slow_pct}% contention={engine_contention} contention_pct={engine_contention_pct}% | \
                             ENGINE_MEM index={}MB compacter_inflight={}MB tail={}MB total={}MB table_file={}MB | \
                             AGE_DETAIL {age_detail_str} {disk_detail_str} | \
                             sz_kv={sz_kv} sz_utxo={sz_utxo} sz_arc_utxo={sz_arc_utxo} | \
                             LIVE_COUNTERS MemoryRun={mr_live}(total={mr_total}) OutputDetail={od_live}(total={od_total}) BlockOutputs={bo_live} SBS_Shared_live={sbs_live}(total={sbs_total}) ArcBlock_created={arc_blk_created} ArcBlockHdr_created={arc_bh_created} block_flush_inflight={block_flush_inflight} sample_block_{BLOCK_SAMPLE_HEIGHT}_alive={sample_block_alive} | \
                             accounted~{accounted_total_mb}MB (index={}MB compacter={}MB tail={}MB pipeline={}MB utxocache={}MB) \
                             UNEXPLAINED_ANON={unexplained_mb}MB pipeline_buffers~{pipeline_buffers_mb}MB adjusted_unexplained={adjusted_unexplained_mb}MB (post_rayon={post_rayon_unexplained_mb}MB) | \
                             top_regions: {top_anon}",
                            rss_total_mb, anon_mb, vm_swap_mb, total_anon_mb, file_mb,
                            mi_virtual_mb, mi_rss_mb, mi_virtual_mb.saturating_sub(mi_rss_mb),
                            gap_flush_on_abort,
                            memory::DOWNLOAD_RECEIVED_BLOCKS.load(Ordering::Relaxed),
                            engine_index_mb, engine_compacter_mb, engine_tail_mb, engine_total_mb, table_file_mb,
                            engine_index_mb, engine_compacter_mb, engine_tail_mb, feeder_mb + staged_mb, accounted_mb,
                        );
                        #[cfg(feature = "jemalloc")]
                        {
                            memory::maybe_purge_jemalloc_retained("mem_report");
                        }
                        memory::maybe_madvise_data_mdb_keep_tail(file_mb, "mem_report");
                        // Dump mimalloc per-size-class stats to identify where memory is allocated.
                        // Uses a thread-local buffer + unsafe FFI to call mi_stats_print_out
                        // with a callback that logs each line via tracing.
                        //
                        // Before dumping: force reclaim of all abandoned pages. Without this,
                        // cross-thread alloc/free (32 validation workers) leaves 90%+ of pages
                        // in the global abandoned list, causing ~10 GB of phantom RSS. mi_collect
                        // is cheap here (called once per MEM_REPORT, ~60s interval).
                        #[cfg(feature = "mimalloc")]
                        {
                            use std::sync::Mutex;
                            use std::os::raw::{c_char, c_void};
                            // Collect output lines from mi_stats_print_out via a callback.
                            static MI_STATS_BUF: Mutex<Vec<String>> = Mutex::new(Vec::new());
                            extern "C" fn mi_stats_cb(msg: *const c_char, _arg: *mut c_void) {
                                if msg.is_null() { return; }
                                let s = unsafe { std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned() };
                                if let Ok(mut v) = MI_STATS_BUF.lock() { v.push(s); }
                            }
                            // Snapshot stats BEFORE collect so we can see how many pages are
                            // abandoned, then force-reclaim them.
                            if let Ok(mut v) = MI_STATS_BUF.lock() { v.clear(); }
                            unsafe { libmimalloc_sys::mi_stats_print_out(Some(mi_stats_cb), std::ptr::null_mut()); }
                            // MI_STATS row format: "  label : peak_val peak_unit total_val total_unit current_val current_unit [...]"
                            // Whitespace-split indices:  0        1  2         3          4          5         6             7
                            // We want the "current" column = index 6 (value) + index 7 (unit).
                            // Unit encoding: "K"/"M" are SI (×1000/×1e6) for page counts;
                            //                "GiB"/"MiB" are binary (×1024^3/×1024^2) for byte fields.
                            let parse_mi_current = |text: &str, label: &str, is_bytes: bool| -> u64 {
                                text.lines()
                                    .find(|l| l.trim_start().starts_with(label) && l.contains(':'))
                                    .and_then(|l| {
                                        let parts: Vec<&str> = l.split_whitespace().collect();
                                        let val_str = parts.get(6)?;
                                        let unit = parts.get(7).copied().unwrap_or("");
                                        let n = val_str.parse::<f64>().ok()?;
                                        let mult: f64 = if is_bytes {
                                            // Convert to MB
                                            match unit {
                                                "GiB" | "GB" => 1024.0,       // 1 GiB = 1024 MB
                                                "MiB" | "MB" => 1.0,          // 1 MiB = 1 MB
                                                "KiB" | "KB" => 1.0 / 1024.0, // 1 KiB → MB
                                                _ => 1.0 / (1024.0 * 1024.0), // raw bytes → MB
                                            }
                                        } else {
                                            // Page/object counts: K=1000, M=1e6
                                            match unit {
                                                "K" => 1_000.0,
                                                "M" => 1_000_000.0,
                                                _ => 1.0,
                                            }
                                        };
                                        Some((n * mult) as u64)
                                    }).unwrap_or(0)
                            };
                            let (mi_abandoned_before, mi_pages_before, mi_committed_mb_before) = {
                                let v = MI_STATS_BUF.lock().unwrap_or_else(|e| e.into_inner());
                                let text = v.concat();
                                // "pages" row is in the "pages" section (has ":"); exclude the header line.
                                let pages = parse_mi_current(&text, "pages", false);
                                let abandoned = parse_mi_current(&text, "abandoned", false);
                                let committed_mb = parse_mi_current(&text, "committed", true);
                                (abandoned, pages, committed_mb)
                            };
                            let mi_abandoned_pct = if mi_pages_before > 0 {
                                mi_abandoned_before * 100 / mi_pages_before.max(1)
                            } else { 0 };
                            // Force-reclaim all abandoned pages. This traverses mimalloc's
                            // global abandoned list and returns pages to their arenas so they
                            // can be purged. Called once per MEM_REPORT (~60s), cost is low.
                            unsafe { libmimalloc_sys::mi_collect(true); }
                            // Re-snapshot after collect to see how much was reclaimed.
                            if let Ok(mut v) = MI_STATS_BUF.lock() { v.clear(); }
                            unsafe { libmimalloc_sys::mi_stats_print_out(Some(mi_stats_cb), std::ptr::null_mut()); }
                            let (mi_abandoned_after, mi_committed_mb_after) = {
                                let v = MI_STATS_BUF.lock().unwrap_or_else(|e| e.into_inner());
                                let text = v.concat();
                                let abandoned = parse_mi_current(&text, "abandoned", false);
                                let committed_mb = parse_mi_current(&text, "committed", true);
                                (abandoned, committed_mb)
                            };
                            info!(
                                "[MI_COLLECT] h={h} abandoned_before={mi_abandoned_before} ({mi_abandoned_pct}% of {mi_pages_before} pages) \
                                 committed_before={mi_committed_mb_before}MB → \
                                 abandoned_after={mi_abandoned_after} committed_after={mi_committed_mb_after}MB"
                            );
                            if let Ok(v) = MI_STATS_BUF.lock() {
                                let full = v.concat();
                                info!("[MI_STATS] h={h}\n{full}");
                            }
                        }
                    }
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
                    let frozen_secs = last_change.elapsed().as_secs();
                    let pending = store.pending_len();
                    let cap = max_pending_ops_w.load(Ordering::Relaxed);
                    let staged_n = staged_count_w.load(Ordering::Relaxed);
                    let block_flushes = block_flush_in_flight_w.load(Ordering::Relaxed);
                    let dur_inflight = durability_in_flight_w.load(Ordering::Relaxed);
                    // Emit the watchdog header BEFORE any potentially-blocking calls (DashMap
                    // or mutex acquisitions) so the freeze is always logged even if the
                    // detailed stats below block on a contended lock.
                    warn!(
                        "[IBD_WATCHDOG] validation frozen at h={} for {}s — \
                         pending={}/{} staged={} block_flush={} durability_inflight={} \
                         (gathering stats…)",
                        h, frozen_secs, pending, cap, staged_n, block_flushes, dur_inflight,
                    );
                    // Use try_lock to avoid watchdog deadlocking if these mutexes are held
                    // by a thread stuck in the shutdown/flush path (which would cause the
                    // watchdog to silently hang and never log the freeze diagnostic).
                    let utxo_flushes = utxo_flush_handles_w
                        .try_lock()
                        .map(|g| g.len())
                        .unwrap_or(usize::MAX);
                    // store.len() / in_flight_len() call DashMap::len() which acquires a
                    // read lock on every shard — can block if a shard is write-locked by a
                    // stuck worker. Report usize::MAX (sentinel) in that case.
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
                    // W136: only claim contention when try_lock failed (usize::MAX).
                    // Prior legend on every freeze line false-matched soak "contended"
                    // greps while locks were free (len=0).
                    let any_lock_contended = utxo_flushes == usize::MAX
                        || staged_lo_hi.2 == usize::MAX
                        || feeder_buf_len == usize::MAX;
                    if any_lock_contended {
                        warn!(
                            "[IBD_WATCHDOG] stats: staged_btreemap=(lo={:?},hi={:?},len={}) \
                             feeder_buffer={} utxo_flush_in_flight={} \
                             cache_entries={} inflight_insertions={} \
                             — mutex contended/unavailable (MAX)",
                            staged_lo_hi.0,
                            staged_lo_hi.1,
                            staged_lo_hi.2,
                            feeder_buf_len,
                            utxo_flushes,
                            cache,
                            inflight,
                        );
                    } else {
                        warn!(
                            "[IBD_WATCHDOG] stats: staged_btreemap=(lo={:?},hi={:?},len={}) \
                             feeder_buffer={} utxo_flush_in_flight={} \
                             cache_entries={} inflight_insertions={}",
                            staged_lo_hi.0,
                            staged_lo_hi.1,
                            staged_lo_hi.2,
                            feeder_buf_len,
                            utxo_flushes,
                            cache,
                            inflight,
                        );
                    }
                }
            })
            .expect("spawn ibd-validation-watchdog")
    };
    let _watchdog_guard = IbdValidationWatchdogGuard {
        shutdown: Arc::clone(&watchdog_shutdown),
        handle: Some(watchdog_handle),
    };

    // RSS gate: fires when (anon_rss + vm_swap) = total committed anonymous exceeds the limit.
    // Using total (RAM + swap) catches the case where the kernel is already swapping heavily —
    // swap usage is a direct signal that physical RAM is exhausted and OOM is approaching.
    // Limit = max(92% of rss_budget, 80% of total physical RAM) — whichever is larger,
    // so on a dedicated 91 GB machine the limit is ~73 GB, not the more conservative budget.
    let rss_gate_hard_limit_mb: u64 = {
        let g = mem_mtx.lock();
        // Pause dispatch when total_anon (anon_rss + vm_swap) exceeds 72% of physical RAM.
        // Gives headroom for the allocator purge thread to return freed pages.
        // Kill decisions are external (scripts/ibd-mem-monitor.sh) — binary only pauses.
        g.total_mb * 72 / 100
    };
    info!(
        "[RSS_GATE] pause_limit={}MB (total_anon = anon_rss + vm_swap; pauses dispatch only — external monitor handles kill)",
        rss_gate_hard_limit_mb
    );
    // Block counter for throttling the gate check (~3 µs per /proc read, amortized over 16 blocks).
    let mut rss_gate_check_block: u64 = 0;

    // Compacter backpressure: pause dispatch when age-2 accumulates too many runs.
    //
    // At h=430k+ with 200 BPS and ~500 entries/block, age-2 runs are created at ~1.25/sec
    // while the compacter drains at ~1.4/sec — barely any margin. Any hiccup causes runaway
    // accumulation (observed: 5→77 runs in 60 sec → 14 GB jemalloc growth → OOM).
    //
    // The gate is checked every COMPACTER_GATE_INTERVAL blocks (cheap: one read-lock len()).
    // When a2 run count exceeds COMPACTER_PAUSE_RUNS, block dispatch sleeps until a2 drains
    // below COMPACTER_RESUME_RUNS. This lets the 5 compacter threads catch up without new
    // input, bounding a2 to ≤COMPACTER_PAUSE_RUNS × ~25 MB ≈ 400 MB maximum.
    const COMPACTER_PAUSE_RUNS: usize = 16;   // 2× fan_in — compacter is clearly behind
    const COMPACTER_RESUME_RUNS: usize = 8;   // 1× fan_in — healthy steady state
    const COMPACTER_GATE_INTERVAL: u64 = 32;  // check every 32 blocks (very cheap)
    let mut compacter_gate_check_block: u64 = 0;
    // HP-M5: park validation dispatch while DiskIndex is writing a spill segment so tip
    // query preads do not contend with mega writes. Opt-in via BLVM_IBD_SPILL_IO_GATE=1.
    let spill_io_gate = matches!(
        std::env::var("BLVM_IBD_SPILL_IO_GATE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    );
    if spill_io_gate {
        info!("[SPILL_IO_GATE] enabled — pause validation dispatch during DiskIndex spill writes");
    }

    loop {
        // E1: ladder export isolation — idle validation while checkpoint export owns disk.
        // Do not dispatch new blocks; in-flight ops below still drain on other paths.
        {
            static EXPORT_ISO_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            let iso = !crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED
                .load(std::sync::atomic::Ordering::Acquire)
                && crate::node::parallel_ibd::export_isolation_active();
            if iso {
                if !EXPORT_ISO_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    info!(
                        "[IBD_EXPORT_ISOLATION] validation dispatch paused at h={} (export active)",
                        next_validation_height
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            if EXPORT_ISO_LOGGED.swap(false, std::sync::atomic::Ordering::Relaxed) {
                info!(
                    "[IBD_EXPORT_ISOLATION] validation dispatch resumed at h={}",
                    next_validation_height
                );
            }
        }

        // SIGTERM/SIGINT: stop dispatching immediately and drain in-flight → flush path.
        // Previously shutdown was only observed on feeder-stall timeout; with a full buffer
        // the loop kept validating until the process-wide grace `exit(0)` skipped tip flush.
        let shutdown_now = crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED
            .load(std::sync::atomic::Ordering::Acquire);
        if shutdown_now && in_flight.is_empty() {
            // Nothing left to drain — fall through to the normal empty-pipeline break below.
        } else if shutdown_now {
            // Log once per shutdown (cheap: only when still draining).
            static SHUTDOWN_DRAIN_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !SHUTDOWN_DRAIN_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                warn!(
                    "[IBD] Graceful shutdown: stopping dispatch at h={} (in_flight={}) — \
                     draining pipeline then flushing block store + watermark",
                    next_validation_height,
                    in_flight.len()
                );
            }
        }

        // === RSS HARD GATE ===
        // Checks total committed anonymous memory (in-RAM + swapped) every 16 blocks.
        // Using total rather than just in-RAM catches swap pressure early.
        if !shutdown_now && rss_gate_hard_limit_mb > 0 {
            rss_gate_check_block = rss_gate_check_block.wrapping_add(1);
            if rss_gate_check_block % 16 == 0 {
                let (anon_mb, swap_mb) = memory::read_proc_anon_and_swap_mb();
                let total_anon_mb = anon_mb + swap_mb;
                if total_anon_mb >= rss_gate_hard_limit_mb {
                    warn!(
                        "[RSS_HARD_GATE] h={} anon={}MB swap={}MB total={}MB ≥ pause_limit={}MB — pausing dispatch",
                        next_validation_height, anon_mb, swap_mb, total_anon_mb, rss_gate_hard_limit_mb
                    );
                    let mut paused_ms = 0u32;
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        paused_ms += 500;
                        let (new_anon, new_swap) = memory::read_proc_anon_and_swap_mb();
                        let new_total = new_anon + new_swap;
                        if new_total < rss_gate_hard_limit_mb {
                            info!(
                                "[RSS_HARD_GATE] resumed h={} anon={}MB swap={}MB total={}MB after {}ms",
                                next_validation_height, new_anon, new_swap, new_total, paused_ms
                            );
                            break;
                        }
                        if paused_ms % 5_000 == 0 {
                            #[cfg(feature = "jemalloc")]
                            {
                                use std::os::raw::c_void;
                                unsafe extern "C" {
                                    fn _rjem_mallctl(name: *const i8, oldp: *mut c_void, oldlenp: *mut usize, newp: *mut c_void, newlen: usize) -> i32;
                                }
                                unsafe {
                                    _rjem_mallctl(b"arena.4294967295.purge\0".as_ptr() as *const i8,
                                                  std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0);
                                }
                            }
                            warn!(
                                "[RSS_HARD_GATE] waiting h={} anon={}MB swap={}MB total={}MB pause_limit={}MB paused={}ms \
                                 (external monitor should kill if swap fills)",
                                next_validation_height, new_anon, new_swap, new_total, rss_gate_hard_limit_mb, paused_ms
                            );
                        }
                    }
                }
            }
        }

        // === COMPACTER GATE ===
        // Pause dispatch when age-2 OR the spill tier (eviction_age-1, typically age-3) has
        // too many runs. Age-2 alone was insufficient: with eviction_age=4, a2→a3 merges are
        // cheap while a3→disk takes 20–30s; a3 piled to 58 runs / 12GB with a2 still healthy.
        // Skip while shutting down — we only drain in-flight, not grow the pipeline.
        if !shutdown_now {
            if let Some(db) = utxo_engine.as_deref() {
            compacter_gate_check_block = compacter_gate_check_block.wrapping_add(1);
            if compacter_gate_check_block % COMPACTER_GATE_INTERVAL == 0 {
                let a2_runs = db.age_run_count(2);
                let spill_age = db.eviction_age_live().saturating_sub(1);
                let spill_runs = if spill_age >= 3 {
                    db.age_run_count(spill_age)
                } else {
                    0
                };
                let spill_pause = COMPACTER_PAUSE_RUNS; // same 16-run threshold
                let spill_resume = COMPACTER_RESUME_RUNS;
                let a2_blocked = a2_runs > COMPACTER_PAUSE_RUNS;
                let spill_blocked = spill_runs > spill_pause;
                if a2_blocked || spill_blocked {
                    warn!(
                        "[COMPACTER_GATE] h={} a2_runs={} spill_age={} spill_runs={} \
                         (pause_a2={} pause_spill={}) spill_merging={} disk_compact={} \
                         disk_segs={} — pausing dispatch to let compacter drain",
                        next_validation_height,
                        a2_runs,
                        spill_age,
                        spill_runs,
                        COMPACTER_PAUSE_RUNS,
                        spill_pause,
                        if spill_age >= 3 {
                            db.age_is_merging(spill_age)
                        } else {
                            false
                        },
                        db.disk_is_compacting(),
                        db.disk_segment_count(),
                    );
                    let mut paused_ms = 0u32;
                    loop {
                        if crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            break;
                        }
                        // Explicitly kick the compacter — pausing the dispatcher stops
                        // UtxoIndex::append() calls, which are normally the only source of
                        // compacter enqueue notifications. Without this, compacter threads
                        // would drain the channel and block on recv(), never processing the
                        // stalled runs. memory_pressure_tick(2) enqueues all merge-ready
                        // ages so compacter threads have work even with dispatch paused.
                        db.memory_pressure_tick(2);
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        paused_ms += 200;
                        let current_a2 = db.age_run_count(2);
                        let current_spill = if spill_age >= 3 {
                            db.age_run_count(spill_age)
                        } else {
                            0
                        };
                        let a2_ok = current_a2 <= COMPACTER_RESUME_RUNS;
                        let spill_ok = current_spill <= spill_resume;
                        if a2_ok && spill_ok {
                            info!(
                                "[COMPACTER_GATE] resumed h={} a2_runs={} spill_age={} spill_runs={} \
                                 after {}ms disk_compact={} disk_segs={}",
                                next_validation_height,
                                current_a2,
                                spill_age,
                                current_spill,
                                paused_ms,
                                db.disk_is_compacting(),
                                db.disk_segment_count(),
                            );
                            break;
                        }
                        if paused_ms % 5_000 == 0 {
                            warn!(
                                "[COMPACTER_GATE] still waiting h={} a2_runs={} spill_age={} \
                                 spill_runs={} spill_merging={} disk_compact={} disk_segs={} \
                                 paused={}ms",
                                next_validation_height,
                                current_a2,
                                spill_age,
                                current_spill,
                                if spill_age >= 3 {
                                    db.age_is_merging(spill_age)
                                } else {
                                    false
                                },
                                db.disk_is_compacting(),
                                db.disk_segment_count(),
                                paused_ms
                            );
                        }
                    }
                }
            }
            }
        }

        // === SPILL IO GATE (HP-M5) ===
        // Pause dispatch while a DiskIndex spill file write is in flight so tip complete
        // (query/pread) does not share the disk with ~10–13s mega writes.
        if !shutdown_now && spill_io_gate {
            if let Some(db) = utxo_engine.as_deref() {
                if db.spill_io_busy() {
                    warn!(
                        "[SPILL_IO_GATE] h={} — pausing dispatch (DiskIndex spill write busy)",
                        next_validation_height
                    );
                    let mut paused_ms = 0u32;
                    while db.spill_io_busy() {
                        if crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        paused_ms += 50;
                        if paused_ms % 5_000 == 0 {
                            warn!(
                                "[SPILL_IO_GATE] still waiting h={} paused={}ms",
                                next_validation_height, paused_ms
                            );
                        }
                    }
                    if paused_ms > 0 {
                        info!(
                            "[SPILL_IO_GATE] resumed h={} after {}ms",
                            next_validation_height, paused_ms
                        );
                    }
                }
            }
        }

        // === DISPATCH PHASE: fill pipeline up to pipeline_depth_live ===
        // BIP30 adjacency guard: the two exceptional heights on mainnet (91722, 91842)
        // require sequential BIP30 state propagation — force depth=1 to serialize through them.
        // Otherwise pipeline_depth controls how far ahead the dispatcher can run, while
        // n_validate_workers controls how many of those in-flight blocks execute concurrently.
        let pipeline_depth_live: usize = if (91710..=91855).contains(&next_validation_height) {
            1
        } else {
            pipeline_depth_for_pressure(
                memory::ibd_pressure_level_snapshot(),
                n_pipeline_depth,
            )
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
        //   h=82k  : ~10 KB/entry  → 512 × 10 KB  = 5 MB   (negligible)
        //   h=218k : ~480 KB/entry → 512 × 480 KB  = 246 MB (safe)
        //   h=600k : ~700 KB/entry → 512 × 700 KB  = 358 MB (safe)
        //   h=846k : ~3 MB/entry   → 512 × 3 MB    = 1.5 GB (fine on ≥32 GB hosts)
        //
        // Raised from 256 → 512 because at h>700k the retire thread transiently lags
        // (UTXO eviction scan + storage flush join) causing staged_count to overshoot
        // 256 by pipeline_depth (up to 285+). With the old cap=256, the dispatcher
        // froze for up to 90s while retire drained — validation workers idled and
        // download workers backed up. At 512, retire has twice the headroom to catch
        // up without pausing the dispatcher, eliminating the freeze-thaw cycle.
        //
        // For large-RAM hosts (≥48 GB) we scale up further: the retire lag at
        // h=800k+ is proportional to UTXO cache size (10 M entries × eviction scan).
        // Giving the dispatcher 1024 slots keeps validation continuously fed during
        // retire's periodic flush-join pauses (~2 s every 800 blocks at 8 BPS).
        let staged_dispatch_cap = {
            let base = (n_pipeline_depth * 8).max(512);
            // Scale with available RAM: large hosts tolerate deeper staging.
            let ram_scale = if utxo_nominal_max_entries >= 8_000_000 {
                // ≥48 GB tier (10 M cache entries): double the cap.
                base * 2
            } else if utxo_nominal_max_entries >= 4_000_000 {
                // 32–48 GB tier: 1.5× cap.
                base + base / 2
            } else {
                base
            };
            ram_scale.min(2048) // hard ceiling to prevent unbounded RSS on very large machines
        };
        // Snapshot once per orchestrator iteration (Relaxed load, ~1 atomic read per loop).
        let staged_now = staged_count.load(Ordering::Relaxed);

        while !shutdown_now
            && in_flight.len() < pipeline_depth_live
            && staged_now < staged_dispatch_cap
        {
            // IBD range is [start_height, effective_end_height] inclusive. After validating
            // effective_end_height, next_validation_height advances to end+1 — do not wait on
            // the feeder for a block that was never assigned or downloaded.
            if next_validation_height > effective_end_height() {
                break;
            }

            let is_first = in_flight.is_empty();

            // Get next block: blocking if no in-flight work, non-blocking otherwise.
            let block_tuple_opt = if is_first {
                let feeder_wait_timeout = std::time::Duration::from_secs(
                    parallel_ibd.config.download_timeout_secs.max(5),
                );
                loop {
                    let mut guard = feeder_state.0.lock();
                    if let Some((arc_b, w, input_keys, u, tx_ids, spec_adds, est_bytes)) =
                        guard.0.remove(next_validation_height)
                    {
                        // W59: latch take under feeder lock before unlock (rewind race).
                        super::tip_stage::mark_taken_from_feeder(next_validation_height);
                        guard.2 = guard.2.saturating_sub(est_bytes);
                        // Drop any stale heights below the tip (late channel deliveries).
                        let (_pruned, freed) = guard.0.prune_below(next_validation_height);
                        guard.2 = guard.2.saturating_sub(freed);
                        // Always refresh the atomic. Previously only updated when prune>0,
                        // so a sole tip take left IBD_FEEDER_BUFFER_BLOCKS=1 forever —
                        // TIP_CRAWL/gap_in_pipeline lied (live: feeder=1 while watchdog
                        // feeder_buffer=0 for 40s stalls at tip 218/770).
                        super::IBD_FEEDER_BUFFER_BLOCKS
                            .store(guard.0.len(), std::sync::atomic::Ordering::Relaxed);
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
                    // Tip missing — prune stale junk so backpressure cannot block the tip.
                    {
                        let (pruned, freed) = guard.0.prune_below(next_validation_height);
                        guard.2 = guard.2.saturating_sub(freed);
                        if pruned > 0 {
                            tracing::warn!(
                                "[IBD_FEEDER_PRUNE] dropped {} stale block(s) below {} (feeder now {})",
                                pruned,
                                next_validation_height,
                                guard.0.len()
                            );
                            super::IBD_FEEDER_BUFFER_BLOCKS
                                .store(guard.0.len(), std::sync::atomic::Ordering::Relaxed);
                            feeder_state.1.notify_one();
                        }
                    }
                    if guard.1 && guard.0.is_empty() {
                        break None;
                    }
                    // N13: publish tip so ahead inserts skip Condvar notify.
                    super::IBD_FEEDER_WAIT_TIP
                        .store(next_validation_height, Ordering::Relaxed);
                    let wait_start = std::time::Instant::now();
                    super::ms_breakdown::wall_enter(super::ms_breakdown::WallState::WaitFeeder);
                    let wait = feeder_state.1.wait_for(&mut guard, feeder_wait_timeout);
                    let wait_ms = wait_start.elapsed().as_millis() as u64;
                    if wait_ms > 0 {
                        let feeder_now = guard.0.len();
                        let holes = super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
                        let contig = super::IBD_TIP_CONTIG_RUNWAY.load(Ordering::Relaxed);
                        let await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
                        let gd_ms = super::tip_stage::getdata_body_ewma_ms().map(|(ms, _)| ms);
                        let pressure = memory::ibd_pressure_level_snapshot();
                        let failover = super::tip_stage::tip_failover_armed();
                        let binder = classify_ibd_binder(
                            feeder_now, holes, contig, await_ms, gd_ms, pressure, failover,
                        );
                        super::ms_breakdown::note_wait_feeder_binder(binder, wait_ms);
                    }
                    #[cfg(feature = "profile")]
                    if ibd_profile && wait_ms >= 1 {
                        let buffer_len_after = guard.0.len();
                        let ts_ms = crate::utils::time::current_timestamp_millis();
                        blvm_protocol::profile_log!(
                            "[IBD_STALL_WAIT] next_height={} duration_ms={} buffer_after={} ts_ms={}",
                            next_validation_height,
                            wait_ms,
                            buffer_len_after,
                            ts_ms
                        );
                    }
                    if wait.timed_out() {
                        // Graceful shutdown: signal received while we were waiting for the
                        // next block.  Mark the feeder as done so the dispatch while-loop
                        // breaks with None, the existing in-flight work drains, the retire
                        // thread flushes, and the final watermark checkpoint is persisted
                        // before we return — identical to a normal IBD completion path.
                        if crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            warn!(
                                "[IBD] Graceful shutdown requested at height {} — \
                                 draining in-flight work and flushing watermark before exit",
                                next_validation_height
                            );
                            guard.1 = true; // mark feeder done; dispatch loop will see None
                            break None;
                        }
                        if next_validation_height == feeder_stall_at_height {
                            feeder_stall_count = feeder_stall_count.saturating_add(1);
                        } else {
                            feeder_stall_at_height = next_validation_height;
                            feeder_stall_count = 1;
                            feeder_stall_started = Some(std::time::Instant::now());
                        }
                        const MAX_FEEDER_STALLS: u32 = 24;
                        if feeder_stall_count >= MAX_FEEDER_STALLS {
                            // W36/W73: past local-replay / confirmed body tip, tip-SLA + force
                            // requeue recover owners — do NOT tear down IBD.
                            // Genesis has `local_replay_max_height=0`; the old `> 0 &&` gate
                            // aborted true WAN crawls after ~1080s (live: h=262716).
                            if next_validation_height > local_replay_max_height {
                                warn!(
                                    "[IBD_STALL] WAN tip gap: {} feeder stalls at h={} — \
                                     resetting counter (tip-SLA/force requeue; no IBD abort)",
                                    feeder_stall_count, next_validation_height
                                );
                                feeder_stall_count = 0;
                            } else {
                                let stall_secs =
                                    feeder_wait_timeout.as_secs() * u64::from(MAX_FEEDER_STALLS);
                                return match retire_thread_shutdown(
                                    &mut _retire_dispatcher,
                                    &retire_err,
                                ) {
                                    Ok(()) => Err(anyhow::anyhow!(
                                        "IBD download stalled: no block at height {} after ~{}s \
                                         (coordinator/workers may have exited early)",
                                        next_validation_height,
                                        stall_secs
                                    )),
                                    Err(e) => Err(e),
                                };
                            }
                        }
                        let cur_min = guard.0.min_buffered_height();
                        let stall_wall_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        super::IBD_VALIDATION_STALL_WALL_MS
                            .store(stall_wall_ms, std::sync::atomic::Ordering::Relaxed);
                        super::IBD_FEEDER_BUFFER_BLOCKS
                            .store(guard.0.len(), std::sync::atomic::Ordering::Relaxed);
                        warn!(
                            "[IBD_STALL] Validation waiting for block {} (buffer has {} blocks, min_height={:?}) — coordinator/feeder may be blocked; reorder={} ~{}MB bridge_pending={} bridge_next={:?} gap_flush_on_abort={} gap_admit_drops={}",
                            next_validation_height,
                            guard.0.len(),
                            cur_min,
                            memory::BLOCK_BUFFER_COUNT.load(Ordering::Relaxed),
                            memory::BLOCK_BUFFER_BYTES.load(Ordering::Relaxed) / (1024 * 1024),
                            memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed),
                            {
                                let n = memory::BRIDGE_NEXT_EXPECTED.load(Ordering::Relaxed);
                                if n == u64::MAX {
                                    None
                                } else {
                                    Some(n)
                                }
                            },
                            memory::GAP_FLUSH_ON_ABORT_BLOCKS.load(Ordering::Relaxed),
                            memory::GAP_ADMIT_DROP_BLOCKS.load(Ordering::Relaxed),
                        );
                        // W5/A1: shared floor with coordinator B2 (`gap_ahead_floor`, default 512).
                        // L3: skip clamp when bridge still holds pending work — feeder=0 means
                        // OrderedReadyBridge gap delivery, not download-ahead excess (live 680k:
                        // clamp 4096→1024 while bridge_pending≈100 made recovery worse).
                        let bridge_pending =
                            memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
                        if bridge_pending > 0 {
                            let _ = stall_tx.send(next_validation_height);
                        } else {
                        let gap_cap = parallel_ibd
                            .config
                            .chunk_size
                            .saturating_mul(16)
                            .max(super::gap_ahead_floor())
                            .clamp(super::gap_ahead_floor(), 1024)
                            .min(nominal_max_ahead);
                        let prev_ahead = max_ahead_live.load(Ordering::Relaxed);
                        if prev_ahead > gap_cap {
                            max_ahead_live.store(gap_cap, Ordering::Relaxed);
                            warn!(
                                "[IBD_STALL] clamping max_ahead_live {} → {} (feeder starvation at h={})",
                                prev_ahead,
                                gap_cap,
                                next_validation_height
                            );
                        }
                        let _ = stall_tx.send(next_validation_height);
                        }
                    }
                }
            } else {
                // Non-blocking: grab lookahead block only if already in feeder.
                let next_h = next_validation_height;
                let mut guard = feeder_state.0.lock();
                guard
                    .0
                    .remove(next_h)
                    .map(|(arc_b, w, ik, u, tx_ids, spec_adds, est_bytes)| {
                        // W59: latch take under feeder lock before unlock (rewind race).
                        super::tip_stage::mark_taken_from_feeder(next_h);
                        guard.2 = guard.2.saturating_sub(est_bytes);
                        feeder_state.1.notify_one();
                        super::IBD_FEEDER_BUFFER_BLOCKS.store(
                            guard.0.len(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        (next_h, arc_b, w, ik, u, tx_ids, spec_adds, est_bytes)
                    })
            };

            let (
                h,
                block_arc_d,
                witnesses_d,
                mut input_keys_from_feeder,
                prefetched_utxos_d,
                mut tx_ids_precomputed_d,
                spec_adds_d,
                feeder_est_bytes_d,
            ) = match block_tuple_opt {
                None => break,
                Some(t) => t,
            };
            super::ms_breakdown::wall_enter(super::ms_breakdown::WallState::Dispatch);
            // N15: engine admit defers serial txid SHA — fill before append / output cache.
            if tx_ids_precomputed_d.is_empty() {
                crate::storage::disk_utxo::compute_tx_ids_only(
                    block_arc_d.as_ref(),
                    &mut tx_ids_precomputed_d,
                );
            }
            super::tip_stage::finish_validated(h);
            if blocks_synced == 0 && in_flight.is_empty() {
                info!("Validation: first block received, height {}", h);
            }
            feeder_stall_count = 0;
            feeder_stall_started = None;

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
                let registry =
                    super::local_block::cached_feature_registry(protocol.get_protocol_version());
                let segwit_on =
                    registry.is_feature_active("segwit", h, block_arc_d.header.timestamp);
                if segwit_on {
                    return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                        Ok(()) => Err(anyhow::anyhow!(
                            "missing witness data for block at height {h} (SegWit active)"
                        )),
                        Err(e) => Err(e),
                    };
                }
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
            // In engine mode the worker uses `partial_session` (query+fetch via the age-tiered
            // index) rather than the legacy prefetch/cache key path, so `keys_v2_buf` is dead
            // work — skip the input-key extraction entirely.
            if utxo_engine.is_none() {
                if input_keys_from_feeder.is_empty() {
                    block_input_keys_into_filtered_with_tx_ids(
                        block_arc_d.as_ref(),
                        tx_ids_precomputed_d.as_slice(),
                        &mut keys_v2_buf,
                    );
                } else {
                    std::mem::swap(&mut keys_v2_buf, &mut input_keys_from_feeder);
                }
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
            // Dead in engine mode — workers use `partial_session` (age-tiered query+fetch)
            // and never consult `spec_adds_snapshot`.  Skip the Arc-clone loop entirely.
            let spec_adds_snapshot: Vec<(u64, Arc<UtxoSet>)> = if utxo_engine.is_none() {
                spec_adds_snapshot_buf.clear();
                spec_adds_snapshot_buf
                    .extend(spec_adds.iter().map(|(sh, set)| (*sh, Arc::clone(set))));
                std::mem::replace(&mut spec_adds_snapshot_buf, Vec::with_capacity(64))
            } else {
                Vec::new()
            };

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
            let recent_snap = Arc::new(std::mem::replace(
                &mut recent_snap_buf,
                Vec::with_capacity(12),
            ));
            // Per-job wall clock for header validation (reject future blocks). Cheap vs ECDSA work.
            let cached_network_time = current_timestamp();
            let block_work = get_block_proof(block_arc_d.header.bits).unwrap_or(U256::zero());
            running_header_chainwork = running_header_chainwork.saturating_add(block_work);
            let header_chainwork_for_job = running_header_chainwork;

            // Speculative additions: legacy prefetch path only. Engine workers use
            // `partial_session` and never read `spec_adds_snapshot`.
            if utxo_engine.is_none() {
                let spec_arc: Arc<UtxoSet> = spec_adds_d;
                let spec_entry_bytes = (spec_arc.len() as u64).saturating_mul(64);
                spec_adds_bytes.fetch_add(spec_entry_bytes, Ordering::Relaxed);
                spec_adds.insert(h, Arc::clone(&spec_arc));
            } else {
                drop(spec_adds_d);
            }

            let keys_for_job: Vec<OutPointKey> = if utxo_engine.is_some() {
                Vec::new()
            } else {
                std::mem::take(&mut keys_v2_buf)
            };

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

            // I2: pre-build output Arc map only on the assume-valid fast path (skip_signatures).
            // Above assume_valid_height connect_block builds locally when needed; skipping here
            // avoids N in-flight pipeline jobs each holding a full per-block output cache.
            let ibd_block_outputs = if h < blvm_consensus::block::get_assume_valid_height() {
                Some(Arc::new(build_block_output_utxo_cache(
                    block_arc_d.as_ref(),
                    tx_ids_precomputed_d.as_slice(),
                    h,
                )))
            } else {
                None
            };

            let job_send = if let Some(ref db) = utxo_engine {
                if let Some(append_tx) = engine_append.sender() {
                    // Serial append off orchestrator: queue Phase 1; append thread → valjob_tx.
                    append_tx
                        .send(EngineAppendJob {
                            height: h,
                            db: Arc::clone(db),
                            block_arc: Arc::clone(&block_arc_d),
                            witnesses_storage: Arc::clone(&witnesses_storage_d),
                            bip30_index: bip30_for_job,
                            recent_headers: recent_snap,
                            tx_ids: tx_ids_precomputed_d,
                            best_header_chainwork: header_chainwork_for_job,
                            cached_network_time,
                            ibd_block_outputs: ibd_block_outputs.clone(),
                        })
                        .map_err(|_| ())
                } else {
                    let t_append = std::time::Instant::now();
                    super::ms_breakdown::wall_enter(super::ms_breakdown::WallState::EngineAppend);
                    let partial = match SpendSession::append(
                        Arc::clone(db),
                        block_arc_d.as_ref(),
                        tx_ids_precomputed_d.as_slice(),
                        h as i32,
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            return match retire_thread_shutdown(
                                &mut _retire_dispatcher,
                                &retire_err,
                            ) {
                                Ok(()) => Err(e.context(format!(
                                    "IBD engine SpendSession::append failed at height {h}"
                                ))),
                                Err(shutdown_err) => Err(shutdown_err),
                            };
                        }
                    };
                    super::ms_breakdown::wall_enter(super::ms_breakdown::WallState::Dispatch);
                    let engine_append_ms = t_append.elapsed().as_millis() as u64;
                    valjob_tx
                        .send(ValidateJob::Engine(EngineValidateJob {
                            height: h,
                            block_arc: Arc::clone(&block_arc_d),
                            witnesses_storage: Arc::clone(&witnesses_storage_d),
                            bip30_index: bip30_for_job,
                            recent_headers: recent_snap,
                            tx_ids: tx_ids_precomputed_d,
                            best_header_chainwork: header_chainwork_for_job,
                            cached_network_time,
                            partial_session: partial,
                            engine_append_ms,
                            ibd_block_outputs: ibd_block_outputs.clone(),
                        }))
                        .map_err(|_| ())
                }
            } else {
                valjob_tx
                    .send(ValidateJob::Legacy(LegacyValidateJob {
                        height: h,
                        block_arc: Arc::clone(&block_arc_d),
                        witnesses_storage: Arc::clone(&witnesses_storage_d),
                        bip30_index: bip30_for_job,
                        recent_headers: recent_snap,
                        tx_ids: tx_ids_precomputed_d,
                        best_header_chainwork: header_chainwork_for_job,
                        cached_network_time,
                        keys: keys_for_job,
                        spec_adds_snapshot,
                        prefetched: prefetched_utxos_d,
                        ibd_block_outputs,
                    }))
                    .map_err(|_| ())
            };
            if job_send.is_err() {
                return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                    Ok(()) => Err(anyhow::anyhow!(
                        "IBD validate/append pipeline stopped (failed to send job at height {})",
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
                drain_ibd_pending_blocks_before_shutdown(
                    skip_storage,
                    &parallel_ibd,
                    &blockstore,
                    &storage_clone,
                    &mut pending_blocks,
                    &mut pending_storage_bytes,
                    &mut flush_handles,
                )?;
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
        // Attribute blocking result-wait separately (was previously left in Dispatch).
        super::ms_breakdown::wall_enter(super::ms_breakdown::WallState::CollectWait);
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
        let collect_head_ready = pending_results.contains_key(&next_process_h);
        super::ms_breakdown::note_collect_outcome(collect_head_ready);
        // Blocking wait until we have the result for the front-of-queue entry.
        // Use a timeout so the watchdog and feeder stall counters can observe a frozen pipeline
        // (a plain blocking recv() would wedge the loop indefinitely if the worker for
        // `next_process_h` panicked or the block was never dispatched to a worker).
        const VALRES_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
        const VALRES_STUCK_LIMIT: std::time::Duration = std::time::Duration::from_secs(600); // 10 min
        let mut valres_wait_start = std::time::Instant::now();
        while !pending_results.contains_key(&next_process_h) {
            match valres_rx.recv_timeout(VALRES_POLL_INTERVAL) {
                Ok(vres) => {
                    valres_wait_start = std::time::Instant::now(); // reset on progress
                    if let Some(set) = spec_adds.remove(&vres.height) {
                        let freed = (set.len() as u64).saturating_mul(64);
                        spec_adds_bytes.fetch_sub(
                            freed.min(spec_adds_bytes.load(Ordering::Relaxed)),
                            Ordering::Relaxed,
                        );
                    }
                    pending_results.insert(vres.height, vres);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    let stuck_secs = valres_wait_start.elapsed().as_secs();
                    warn!(
                        "[IBD_VALRES_STALL] No validation result for h={} in {}s \
                         (in_flight={}, pending_results={}, workers may be stuck or block was never dispatched)",
                        next_process_h,
                        stuck_secs,
                        in_flight.len(),
                        pending_results.len(),
                    );
                    if valres_wait_start.elapsed() >= VALRES_STUCK_LIMIT {
                        return match retire_thread_shutdown(&mut _retire_dispatcher, &retire_err) {
                            Ok(()) => Err(anyhow::anyhow!(
                                "IBD validation stuck: no result for h={} after {}s — \
                                 worker may have panicked or block was never delivered to feeder",
                                next_process_h,
                                stuck_secs,
                            )),
                            Err(e) => Err(e),
                        };
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
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

        // Head result is available — end CollectWait here. Extract/BIP30/muhash below used to
        // inflate collect_wait (Phase4 soaks: collect_ready≈84% yet collect_wait≈40%+).
        super::ms_breakdown::wall_enter(super::ms_breakdown::WallState::Drain);

        // === EXTRACT PER-BLOCK VARIABLES FROM IN-FLIGHT ENTRY ===
        let mut entry = in_flight.pop_front().ok_or_else(|| {
            anyhow::anyhow!(
                "IBD coordinator: in_flight drained unexpectedly at height {next_process_h}"
            )
        })?;
        // DIAGNOSTIC: strong_count immediately after pop (worker should have dropped its ref).
        // Expected = 1 (only InFlightEntry). If > 1: something else holds the block before
        // we even clone it here → that holder is accumulating Arc<Block>s.
        if next_process_h % 5000 == 0 {
            let sc_pre = Arc::strong_count(&entry.block_arc);
            info!("[ARC_BLOCK_PRE] h={next_process_h} strong_count_at_pop={sc_pre}");
        }
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
        // Save a Weak ref to a sample block so MEM_REPORT can later confirm it was freed.
        if next_height == BLOCK_SAMPLE_HEIGHT {
            let weak = Arc::downgrade(&block_arc);
            let cell = SAMPLE_BLOCK_WEAK.get_or_init(|| std::sync::Mutex::new(None));
            *cell.lock().unwrap() = Some(weak);
            info!("[SAMPLE_BLOCK_SAVED] h={next_height} strong_count={}", Arc::strong_count(&block_arc));
        }
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
        if let Some(sub) = vres.block_muhash {
            let mut g = ibd_muhash_accumulator.lock();
            *g = std::mem::take(&mut *g).multiply(&sub);
        }
        // Gap replay defers checkpoint export to tip — advance GC fence periodically so
        // disk/memory compactions can cancel spent pairs (otherwise GC'd 0 merges stall BPS).
        if utxo_engine.is_some() {
            if let Some(until) = engine_gap_export_defer_until {
                if next_height < until && next_height > 0 && next_height % 10_000 == 0 {
                    crate::storage::ibd_engine::advance_gc_fence_to(next_height as i32);
                }
            }
        }
        if let Some(ref dur) = engine_durability {
            if next_height > 0 && next_height % dur.muhash_persist_interval == 0 {
                let gap_replay = engine_gap_export_defer_until
                    .is_some_and(|until| next_height < until);
                if !gap_replay {
                    let running = ibd_muhash_accumulator.lock().serialize_running_state();
                    if let Err(e) = storage_clone
                        .chain()
                        .persist_ibd_utxo_muhash_running_only(&running)
                    {
                        warn!(
                            "IBD engine: failed to persist incremental MuHash at h={next_height}: {e:#}"
                        );
                    }
                }
                // Validation tip is informational; persist less often during gap replay.
                let persist_tip = !gap_replay || next_height % 10_000 == 0;
                if persist_tip {
                    if let Err(e) =
                        storage_clone.chain().persist_engine_validation_tip(next_height)
                    {
                        warn!(
                            "IBD engine: failed to persist validation tip at h={next_height}: {e:#}"
                        );
                    }
                }
            }
        }
        let validation_time = vres.elapsed;
        super::ms_breakdown::note_engine(
            vres.engine_append_ms,
            vres.view_build_ms,
            validation_time.as_millis() as u64,
        );
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
                    // Engine mode: UTXO state lives in `UtxoDatabase`; legacy staged deltas and
                    // incremental commitments are skipped (commitments rebuilt post-IBD).
                    if utxo_engine.is_none() {
                        let delta = Arc::new(utxo_delta_opt.unwrap_or_else(empty_utxo_delta));
                        {
                            let mut m = staged.lock();
                            m.insert(next_height, delta);
                        }
                        staged_count.fetch_add(1, Ordering::Relaxed);
                    }
                    // In engine mode: the retire thread immediately calls `continue` (publish
                    // + adapt only), so blocks_buf and block are never accessed. Sending
                    // Arc<Block> in engine mode causes the channel backlog to hold O(blocks)
                    // heap allocations when dispatch outruns the retire thread — the primary
                    // root cause of memory growth during local replay (45K blocks/sec).
                    // In non-engine mode: blocks_buf is needed only for dynamic-eviction.
                    let retire_blocks_buf = if utxo_engine.is_none()
                        && ibd_store_v2_for_validation.is_dynamic_eviction()
                    {
                        blocks_buf.clone()
                    } else {
                        Vec::new()
                    };
                    let retire_block = if utxo_engine.is_none() {
                        Some(Arc::clone(&block_arc))
                    } else {
                        None // engine mode retire thread does not use the block
                    };
                    if _retire_dispatcher
                        .send(IbdRetireWork {
                            height: next_height,
                            blocks_buf: retire_blocks_buf,
                            block: retire_block,
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
        let gap_fill_ms = feeder_stall_started
            .take()
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);

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
                    next_height,
                    apply_pending_ms
                );
            }
            if sync_ms > 5 {
                blvm_protocol::profile_log!(
                    "[IBD_BLOCKED] phase=sync_await height={} duration_ms={} (validation waited for previous block sync+evict)",
                    next_height,
                    sync_ms
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
                blvm_protocol::types::ARC_BLOCKHEADER_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let header_rc = Arc::new(block_arc.header.clone());
                // Skip writing blocks already on disk (contiguous replay cap or sparse per-height
                // body). Re-serializing into heed3 during local gap replay caused redundant heap
                // alloc + LMDB write pressure when probe_confirmed_body_height returned 0.
                let block_hash = blockstore.get_block_hash(block_arc.as_ref());
                let already_persisted = super::local_block::should_skip_block_store_write(
                    blockstore.as_ref(),
                    next_height,
                    &block_hash,
                    local_replay_max_height,
                )
                .unwrap_or(false);
                if next_height == local_replay_max_height + 1 && local_replay_max_height > 0 {
                    info!(
                        "IBD: local replay complete at height {} — resuming block store writes \
                         for network-sourced blocks",
                        next_height
                    );
                }
                if utxo_engine.is_some() {
                    if let Some(until) = engine_gap_export_defer_until {
                        if next_height == until.saturating_add(1) && until >= start_height {
                            info!(
                                "[IBD_ENGINE_REPLAY] validation passed gap target h={until} \
                                 at h={next_height} — download-led sync / checkpoint export active"
                            );
                        }
                    }
                }
                // DIAGNOSTIC: sample Arc strong_count before pushing to pending_blocks.
                // Expected: 2 (entry.block_arc + block_arc) when retire thread has already
                // consumed IbdRetireWork, or 3 if IbdRetireWork is still in-flight.
                // Higher counts mean additional undiscovered holders → log them.
                if next_height % 5000 == 0 {
                    let sc = Arc::strong_count(&block_arc);
                    info!("[ARC_BLOCK_SC] h={next_height} strong_count={sc} skip={skip_storage} already_persisted={already_persisted}");
                }
                if !skip_storage && !already_persisted {
                    pending_storage_bytes =
                        pending_storage_bytes.saturating_add(feeder_est_bytes as u64);
                    pending_blocks.push((
                        block_arc,
                        Arc::clone(&witnesses_storage),
                        next_height,
                        vres.undo_log,
                    ));
                    pending_blocks_count_atomic.store(pending_blocks.len(), Ordering::Relaxed);
                    pending_blocks_bytes_atomic.store(pending_storage_bytes as usize, Ordering::Relaxed);
                } else {
                    // Skip path: body already on disk, so `do_flush_to_storage` (and its
                    // `update_tip`) never runs. Without advancing `chain_info` here, durable tip
                    // stays stuck (observed: tip=236160 while validation reached ~496k with
                    // `already_persisted=true`), watermark refuses past tip, and resume rewinds
                    // to min(tip, watermark). Advance tip periodically on the skip path.
                    // F-C2: also every block in the last 1024 of the IBD range (live: tip stuck
                    // at 957632 with end=957804 — no %1000 boundary in that window).
                    if already_persisted
                        && super::should_advance_tip_on_skip_path(
                            next_height,
                            effective_end_height(),
                        )
                    {
                        let durable = storage_clone.chain().get_height().ok().flatten().unwrap_or(0);
                        if next_height > durable {
                            if let Err(e) = storage_clone.chain().update_tip(
                                &block_hash,
                                &block_arc.header,
                                next_height,
                            ) {
                                warn!(
                                    "[IBD_TIP_SKIP] failed to advance chain_info tip {} → {}: {e:#}",
                                    durable, next_height
                                );
                            } else if let Err(e) = storage_clone.flush() {
                                warn!(
                                    "[IBD_TIP_SKIP] tip advanced to {} but flush failed: {e:#}",
                                    next_height
                                );
                            } else if next_height % 1_000 == 0
                                || next_height.saturating_add(64) > effective_end_height()
                            {
                                // Log every 1k as before; near tip log every 64 to avoid spam.
                                info!(
                                    "[IBD_TIP_SKIP] advanced durable chain_info tip {} → {} \
                                     (body already on disk; flush path skipped)",
                                    durable, next_height
                                );
                            }
                        }
                    }
                    // Skip path: block_arc is not moved; force explicit drop NOW so entry.block_arc
                    // is the sole remaining reference. Confirms no leak from this scope.
                    if next_height % 5000 == 0 {
                        info!("[ARC_BLOCK_SKIP] h={next_height} block_arc dropped (already_persisted={already_persisted} skip={skip_storage})");
                    }
                    drop(block_arc);
                    // After explicit drop: only entry.block_arc (1 ref) should remain.
                    // If strong_count > 1 here: something else still holds the block.
                    if next_height % 5000 == 0 {
                        let sc_after = Arc::strong_count(&entry.block_arc);
                        info!("[ARC_BLOCK_ENTRY_SC] h={next_height} sc_after_block_arc_drop={sc_after}");
                    }
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
                // Non-blocking reap: collect any completed flush handles so we
                // don't join on the hot path. Errors are propagated immediately.
                {
                    let mut i = 0;
                    while i < flush_handles.len() {
                        if flush_handles[i].is_finished() {
                            let handle = flush_handles.remove(i).unwrap();
                            block_flush_in_flight.fetch_sub(1, Ordering::Relaxed);
                            match handle.join() {
                                Ok(Ok(())) => {}
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
                        } else {
                            i += 1;
                        }
                    }
                }

                let (flush_ms, flushed_block_count) = if !skip_storage
                    && (flush_by_interval || flush_by_pressure_bytes)
                {
                    let flush_start = std::time::Instant::now();
                    // Non-blocking slot check: if all flush slots are still in-flight,
                    // skip this flush cycle — pending_blocks accumulates for one more
                    // interval.  The reap above already collected any finished handles.
                    // Hard overflow guard: if pending has grown to 4× the nominal
                    // flush interval (i.e. flushes have been slow for many blocks),
                    // do a single logged join to free a slot rather than letting
                    // pending_blocks grow without bound.
                    if flush_handles.len() >= max_block_flushes_in_flight {
                        let hard_overflow = pending_blocks.len()
                            >= flush_interval_live.saturating_mul(4);
                        if !hard_overflow {
                            // Slot busy, defer flush — validation keeps running.
                            debug!(
                                "[IBD_DEBUG] Block {}: deferring block flush (in_flight={}, pending={})",
                                next_height,
                                flush_handles.len(),
                                pending_blocks.len()
                            );
                            (0u64, 0usize)
                        } else {
                            // Hard overflow: join the oldest in-flight flush so we can
                            // spawn another. Silent defer here left chain_tip hundreds of
                            // thousands of blocks behind validation (resume reset to h=1).
                            warn!(
                                "[IBD] Block flush backpressure at h={}: joining in-flight flush \
                                 (pending={} ≥ 4×interval={})",
                                next_height,
                                pending_blocks.len(),
                                flush_interval_live.saturating_mul(4),
                            );
                            if let Some(handle) = flush_handles.pop_front() {
                                match handle.join() {
                                    Ok(Ok(())) => {}
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
                            // Slot free — fall through by re-checking below would need
                            // restructuring; spawn immediately now.
                            let to_flush = std::mem::take(&mut pending_blocks);
                            pending_storage_bytes = 0;
                            pending_blocks_count_atomic.store(0, Ordering::Relaxed);
                            pending_blocks_bytes_atomic.store(0, Ordering::Relaxed);
                            let to_flush_count = to_flush.len();
                            let blockstore_clone = Arc::clone(&blockstore);
                            let storage_for_flush = storage_clone.clone();
                            flush_handles.push_back(std::thread::spawn(move || {
                                ParallelIBD::do_flush_to_storage(
                                    blockstore_clone.as_ref(),
                                    Some(&storage_for_flush),
                                    to_flush,
                                    IbdBlockFlushOpts::default(),
                                )
                            }));
                            block_flush_in_flight.fetch_add(1, Ordering::Relaxed);
                            (flush_start.elapsed().as_millis() as u64, to_flush_count)
                        }
                    } else {
                    // UTXO flushes run in parallel (fire-and-forget); no barrier here.
                    // On crash, min(chain_tip, watermark) rewinds to the last safe point.
                    let to_flush = std::mem::take(&mut pending_blocks);
                    pending_storage_bytes = 0;
                    pending_blocks_count_atomic.store(0, Ordering::Relaxed);
                    pending_blocks_bytes_atomic.store(0, Ordering::Relaxed);
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
                            IbdBlockFlushOpts::default(),
                        )
                    }));
                    block_flush_in_flight.fetch_add(1, Ordering::Relaxed);
                    let flush_elapsed = flush_start.elapsed().as_millis() as u64;
                    debug!(
                        "[IBD_DEBUG] Block {}: spawned block storage flush (blocks={}, in_flight={}, await_took={}ms)",
                        next_height,
                        to_flush_count,
                        flush_handles.len(),
                        flush_elapsed
                    );
                    (flush_elapsed, to_flush_count)
                    } // close inner else { slot available }
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
                            "[IBD_PROFILE] height={} total_ms={} append_ms={} complete_ms={} prefetch_await={} gap_fill={} prefetch={} utxo_base={} validation={} apply_utxo={} sync={} evict={} flush_coord={} disk_total={} txs={} inputs={}",
                            next_height,
                            total_ms,
                            vres.engine_append_ms,
                            vres.engine_complete_ms,
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
                    let _ = parallel_ibd.flush_pending_blocks_with_opts(
                        &blockstore,
                        Some(&storage_clone),
                        &mut pending_blocks,
                        IbdBlockFlushOpts::shutdown_sync(),
                    );
                }
                error!("Failed to validate block at height {}: {}", next_height, e);
                if crate::storage::ibd_autorepair::validation_error_suggests_utxo_repair(&e) {
                    if let Some(dir) = storage_clone.data_dir() {
                        if let Err(flag_e) =
                            crate::storage::ibd_autorepair::set_ibd_utxo_repair_flag(&dir)
                        {
                            warn!("Could not write IBD UTXO repair marker: {flag_e}");
                        }
                    }
                }
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

        // Engine mode: retire skips legacy UTXO-store pressure; poll RSS + compacter spill on
        // a tiered interval (faster when Critical/Emergency).
        if utxo_engine.is_some() && blocks_synced > 0 {
            let interval = engine_pressure_poll_interval(memory::ibd_pressure_level_snapshot());
            if blocks_synced % interval == 0 {
                memory::ibd_memory_pressure_maintenance(
                    &mem_mtx,
                    &max_ahead_live,
                    nominal_max_ahead,
                    storage_clone.as_ref(),
                    utxo_engine.as_deref(),
                );
            }
        }

        // Adaptive heap-trim block removed — mi_collect causes RSS inflation via page
        // abandonment churn. last_rss_mb / last_collect_block retained for future use.
        let _ = (last_rss_mb, last_collect_block);

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

            let remaining = effective_end_height().saturating_sub(next_height);
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
                effective_end_height(),
                (next_height as f64 / effective_end_height() as f64) * 100.0,
                rate_str,
                buffer_size,
                eta
            );
            super::ms_breakdown::maybe_emit(next_height, false);

            // Binder / slow-stretch attribution (default on; opt out BLVM_IBD_BINDER_LOG=0).
            // Correlates wall-tax dips (median instant ≫ wall) with supply vs engine.
            if binder_log_enabled() && blocks_synced >= 1000 && recent_rate > 0.0 {
                if recent_rate > peak_recent_rate {
                    peak_recent_rate = recent_rate;
                } else {
                    // Slow decay so a single cold window doesn't permanently lower the bar.
                    peak_recent_rate = peak_recent_rate * 0.995 + recent_rate * 0.005;
                }
                let feeder_now = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
                let holes = super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
                let contig = super::IBD_TIP_CONTIG_RUNWAY.load(Ordering::Relaxed);
                let await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
                let gd = super::tip_stage::getdata_body_ewma_ms();
                let gd_ms = gd.map(|(ms, _)| ms);
                let gd_n = gd.map(|(_, n)| n).unwrap_or(0);
                let pressure = memory::ibd_pressure_level_snapshot();
                let failover = super::tip_stage::tip_failover_armed();
                let soft_retries = super::tip_stage::tip_soft_retries();
                let binder = classify_ibd_binder(
                    feeder_now,
                    holes,
                    contig,
                    await_ms,
                    gd_ms,
                    pressure,
                    failover,
                );
                info!(
                    "[IBD_BINDER] h={} recent_bps={:.1} peak_bps={:.1} binder={} feeder={} holes={} contig={} await_ms={} gd_ewma_ms={:?} gd_n={} pressure={:?} failover={} soft_retries={} window_blocks={} window_s={:.2}",
                    next_height,
                    recent_rate,
                    peak_recent_rate,
                    binder,
                    feeder_now,
                    holes,
                    contig,
                    await_ms,
                    gd_ms,
                    gd_n,
                    pressure,
                    failover,
                    soft_retries,
                    blocks_since_last,
                    recent_elapsed
                );
                // Slow stretch: recent ≪ peak (or absolute cold) after warm-up.
                let vs_peak = peak_recent_rate > 180.0
                    && recent_rate < peak_recent_rate * 0.55
                    && recent_rate < 280.0;
                let absolute_cold = recent_rate < 100.0 && peak_recent_rate >= 200.0;
                let throttle_ok = last_slow_stretch_log
                    .map(|t| t.elapsed() >= Duration::from_secs(5))
                    .unwrap_or(true);
                if (vs_peak || absolute_cold) && throttle_ok {
                    last_slow_stretch_log = Some(Instant::now());
                    let covering = super::IBD_TIP_COVERING.load(Ordering::Relaxed);
                    let inflight = super::IBD_TIP_IN_FLIGHT_RANGES.load(Ordering::Relaxed);
                    let tip_reorder = super::IBD_TIP_IN_REORDER.load(Ordering::Relaxed);
                    let pipe_recv0 = super::tip_stage::pipe_fill_recv0_streak();
                    warn!(
                        "[IBD_SLOW_STRETCH] h={} recent_bps={:.1} peak_bps={:.1} drop={:.0}% binder={} feeder={} holes={} contig={} await_ms={} gd_ewma_ms={:?} pressure={:?} failover={} covering={} inflight={} tip_reorder={} pipe_recv0={} — wall tax candidate",
                        next_height,
                        recent_rate,
                        peak_recent_rate,
                        if peak_recent_rate > 0.0 {
                            (1.0 - recent_rate / peak_recent_rate) * 100.0
                        } else {
                            0.0
                        },
                        binder,
                        feeder_now,
                        holes,
                        contig,
                        await_ms,
                        gd_ms,
                        pressure,
                        failover,
                        covering,
                        inflight,
                        tip_reorder,
                        pipe_recv0
                    );
                }
            }

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

    // Signal append thread (if any), then validation workers, then the retire thread.
    info!("IBD shutdown: signaling engine append / validation workers");
    // Close append queue + join before dropping valjob_tx so in-flight appends can enqueue.
    engine_append.close_and_join();
    drop(valjob_tx);
    for worker in _validate_workers {
        if let Err(e) = worker.join() {
            warn!("IBD validate worker join error: {:?}", e);
        }
    }
    info!("IBD shutdown: validation workers joined; stopping retire thread");
    // Signal retire thread to finish, then take any last flush and join UTXO workers.
    retire_thread_shutdown(&mut _retire_dispatcher, &retire_err)?;
    info!("IBD shutdown: retire thread stopped");

    // Join retire-spawned UTXO commits before taking the final package so a slow thread
    // cannot hold the queue mutex while we enqueue shutdown work.
    let mut combined_shutdown_sub_mh =
        join_all_utxo_flush_handles(&utxo_flush_handles, "post-retire")?;

    // Final UTXO flush: drain remaining pending ops, then join all in-flight handles.
    // Collect sub-MuHash accumulators from all async threads (they ran without the global
    // mh_acc mutex) and fold them into the global accumulator before persisting the final
    // checkpoint.
    let mut shutdown_pkg_height: Option<u64> = None;
    // Keep a clone for the DELs phase of the two-phase commit (Arc internals, cheap clone).
    let mut shutdown_prepared_for_dels: Option<
        crate::storage::ibd_utxo_store::PreparedFlushPackage,
    > = None;
    if let Some(pkg) = ibd_store_v2_for_validation.take_remaining_flush_package() {
        shutdown_pkg_height = Some(pkg.max_block_height);
        info!(
            "IBD shutdown: final UTXO package height={} ops={}",
            pkg.max_block_height,
            pkg.ops.len()
        );
        let heights = Arc::clone(&pkg.heights);
        // Pre-compute muhash in the main thread (full rayon pool) before spawning commit thread.
        let prepared = pkg.prepare_for_disk(ibd_store_v2_for_validation.value_codec())?;
        drop(pkg);
        let mut local_mh = blvm_muhash::MuHash3072::new();
        ibd_store_v2_for_validation.compute_package_muhash(&prepared, &mut local_mh)?;
        // Keep a clone for the DELs phase (Phase 3 of two-phase commit).
        shutdown_prepared_for_dels = Some(prepared.clone());
        let store_clone = Arc::clone(&ibd_store_v2_for_validation);
        utxo_flush_handles
            .lock()
            .push_back(std::thread::spawn(move || {
                // Phase 1: ADDs only. DELs are written after watermark is persisted.
                store_clone.flush_prepared_package_adds_only(&prepared)?;
                store_clone.release_protected_heights(&heights);
                store_clone.note_utxo_flush_completed(prepared.max_block_height);
                Ok(local_mh)
            }));
    }
    combined_shutdown_sub_mh = combined_shutdown_sub_mh.multiply(&join_all_utxo_flush_handles(
        &utxo_flush_handles,
        "final-package",
    )?);
    // Fold all async threads' sub-accumulators into the global (multiply by identity is a no-op
    // so this is safe whether or not any handles had real muhash work to contribute).
    {
        let mut mh_guard = ibd_muhash_accumulator.lock();
        *mh_guard = std::mem::take(&mut *mh_guard).multiply(&combined_shutdown_sub_mh);
    }

    // ── Block store first ─────────────────────────────────────────────────────
    // Watermark / engine export must never advance past durable chain_tip. Join
    // in-flight block flushes and drain pending_blocks BEFORE persisting UTXO
    // watermark, including on SIGTERM incomplete-IBD exits.
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
        info!("Flushing final {} pending blocks (before watermark)", pending_blocks.len());
        parallel_ibd.flush_pending_blocks_with_opts(
            &blockstore,
            Some(&storage_clone),
            &mut pending_blocks,
            IbdBlockFlushOpts::shutdown_sync(),
        )?;
    }
    if let Err(e) = storage_clone.flush() {
        warn!("IBD shutdown: storage.flush after block drain failed: {e}");
    }

    // Persist the final checkpoint using two-phase crash-safe commit (same as periodic
    // checkpoints): ADDs flushed before watermark, DELs flushed after.
    // Height is clamped to durable block tip inside persist_ibd_utxo_flush_checkpoint.
    if let Some(max_height) = shutdown_pkg_height {
        let muhash_running = ibd_muhash_accumulator.lock().serialize_running_state();
        info!("IBD shutdown: flush_disk before UTXO watermark advance (height {max_height})");
        // Phase 1 ADDs → disk already written by spawned thread; flush_disk makes them durable.
        ibd_store_v2_for_validation.flush_disk()?;
        info!("IBD shutdown: persisting UTXO checkpoint at height {max_height}");
        // Phase 2: advance watermark (safe point — any crash after this leaves stale UTXOs,
        // not missing UTXOs; stale UTXOs are harmless per Bitcoin double-spend prevention).
        storage_clone
            .chain()
            .persist_ibd_utxo_flush_checkpoint(max_height, &muhash_running)?;
        // Phase 3: DELs → disk, then flush.
        if let Some(ref dels_pkg) = shutdown_prepared_for_dels {
            ibd_store_v2_for_validation.flush_prepared_package_dels_only(dels_pkg)?;
            ibd_store_v2_for_validation.flush_disk()?;
        }
    }
    let last_validated = next_validation_height.saturating_sub(1);
    let shutdown_requested = crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED
        .load(std::sync::atomic::Ordering::Acquire);
    if last_validated < effective_end_height() {
        if shutdown_requested {
            if let Some(ref db) = utxo_engine {
                db.flush_contiguous_length_sidecar();
            }
            if let Some(data_dir) = storage_clone.data_dir() {
                let engine_path =
                    crate::config::ibd::ibd_engine_path(Some(data_dir.as_path()));
                crate::storage::ibd_engine::clear_engine_dirty_flag(&engine_path);
            }
            if utxo_engine.is_some() {
                if let Err(e) = storage_clone
                    .chain()
                    .persist_engine_validation_tip(last_validated)
                {
                    warn!(
                        "IBD shutdown: failed to persist engine validation tip at h={last_validated}: {e:#}"
                    );
                }
                if let Err(e) = storage_clone.flush() {
                    warn!("IBD shutdown: storage flush after engine resume hint failed: {e:#}");
                }
            }
            info!(
                "IBD shutdown: graceful stop at height {} (target {}); block store flushed",
                last_validated, effective_end_height()
            );
            return Ok(());
        }
        return Err(anyhow::anyhow!(
            "IBD incomplete: validated through height {} but need {}",
            last_validated,
            effective_end_height()
        ));
    }
    if let Err(e) = ibd_store_v2_for_validation.flush_disk() {
        warn!(
            "Failed to flush ibd_utxos memtable at final shutdown (height {}): {}",
            last_validated, e
        );
    }

    // Do not advance `ibd_utxo_watermark` from chain tip here. During parallel IBD the block index
    // can reach `chain_tip` before UTXO flush workers persist `ibd_utxos` through that height.
    // Watermark must advance only from flush worker paths after `flush_disk` (see
    // `push_utxo_flush_from_retire`). Bumping from tip caused resume at height H with an empty or
    // partial `ibd_utxos` tree → immediate `UTXO not found for input`.

    info!("IBD shutdown: validation loop complete");
    Ok(())
}

/// Idle retire timeout path: drain adds when possible; when over cap and DEL-heavy, shard 0
/// enqueues a sentinel checkpoint so `del_backlog` can run without a staged block at boundary.
#[allow(clippy::too_many_arguments)]
fn ibd_idle_flush_pending_over_cap(
    path: &'static str,
    retire_shard_index: usize,
    level: PressureLevel,
    pending_len: usize,
    cap: usize,
    over_cap: bool,
    store: &Arc<IbdUtxoStore>,
    storage_wm: &Arc<Storage>,
    utxo_flush_handles: &Arc<Mutex<VecDeque<JoinHandle<Result<blvm_muhash::MuHash3072>>>>>,
    retire_flush_counter: &Arc<AtomicUsize>,
    max_utxo_flushes_under_pressure: usize,
    ibd_muhash: &Arc<Mutex<blvm_muhash::MuHash3072>>,
    durability_tx: Option<&std::sync::mpsc::SyncSender<DurabilityRequest>>,
    publisher: &super::retire_dispatcher::GlobalProgressPublisher,
    local_replay_bulk_write_done: bool,
    retire_err: &Arc<Mutex<Option<anyhow::Error>>>,
) -> bool {
    warn!(
        "[CAPPED_DRAIN] path={path} level={level:?} pending={pending_len} cap={cap} over_cap={over_cap}"
    );
    let push_pkg = |pkg: PendingFlushPackage, trigger_height: u64, force_durability: bool| {
        if !local_replay_bulk_write_done {
            store.drain_in_flight_for_batch(&pkg.ops);
            store.release_protected_heights(&pkg.heights);
            store.note_utxo_flush_completed(pkg.max_block_height);
            return false;
        }
        match push_utxo_flush_from_retire(
            store,
            storage_wm,
            utxo_flush_handles,
            retire_flush_counter,
            trigger_height,
            max_utxo_flushes_under_pressure,
            pkg,
            ibd_muhash,
            force_durability,
            durability_tx,
        ) {
            Ok(()) => false,
            Err(e) => {
                *retire_err.lock() = Some(e);
                true
            }
        }
    };

    if let Some(pkg) = store.take_flush_batch_adds_only() {
        warn!("[CAPPED_DRAIN] path={path} drained={}", pkg.ops.len());
        if over_cap {
            info!(
                "[IBD_IDLE_FLUSH] pending={pending_len} > cap={cap} — draining via idle flush to unblock workers"
            );
        }
        if push_pkg(pkg, 0, false) {
            return true;
        }
    } else if over_cap && pending_len > 0 && retire_shard_index == 0 {
        let wm = publisher.global_floor();
        warn!(
            "[CAPPED_DRAIN] path={path} idle_sentinel wm={wm} pending={pending_len} cap={cap}"
        );
        let pkg = ibd_empty_checkpoint_package(wm);
        if push_pkg(pkg, wm, true) {
            return true;
        }
    }
    ibd_maybe_heap_trim();
    false
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
    retire_shard_index: usize,
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
    engine_mode: bool,
    utxo_engine: Option<Arc<UtxoDatabase>>,
    durability_tx: Option<std::sync::mpsc::SyncSender<DurabilityRequest>>,
    local_replay_no_lmdb_max: u64,
    // Shared across all retire shards. CAS from false→true wins the bulk write.
    local_replay_transition_done: Arc<std::sync::atomic::AtomicBool>,
    // Set to `true` by the winning shard only AFTER flush_full_cache_to_lmdb() completes.
    // Losing shards spin on this before processing blocks h > local_replay_no_lmdb_max.
    local_replay_hydration_done: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen = rustc_hash::FxHashSet::default();
    let mut evict_scratch: Vec<(OutPointKey, u64)> = Vec::new();
    let mut local_replay_bulk_write_done = local_replay_no_lmdb_max == 0 || engine_mode;
    if local_replay_no_lmdb_max > 0 {
        store.set_no_evict_for_local_replay();
    }
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle: no staged blocks arriving because workers are parked on
                // the pending cap. Flush pending to LMDB whenever pending > cap, regardless
                // of pressure level. Without this, defer_flush=true creates a deadlock:
                //   pending > cap -> workers park -> no staged blocks -> timeout fires
                //   -> old code only flushed on Emergency -> pending never drains -> frozen
                // Safety: workers push ops into pending_writes before dispatching retire
                // work (invariant 1), so all in-pending entries are for fully-validated
                // blocks and are safe to flush. Advancing watermark past local_last_retired
                // is correct (invariant 2).
                if engine_mode {
                    continue;
                }
                let level = memory::ibd_memory_pressure_maintenance(
                    &mem_mtx,
                    &max_ahead_live,
                    nominal_max_ahead,
                    storage_wm.as_ref(),
                    utxo_engine.as_deref(),
                );
                let pending_len = store.pending_len();
                let cap = max_pending_ops.load(Ordering::Relaxed);
                let over_cap = pending_len > cap;
                if level >= PressureLevel::Critical || over_cap {
                    let evictable = store.len().saturating_sub(store.protected_len());
                    if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                        store.evict_aggressive_for_rss();
                    }
                    if ibd_idle_flush_pending_over_cap(
                        "idle_flush_A",
                        retire_shard_index,
                        level,
                        pending_len,
                        cap,
                        over_cap,
                        &store,
                        &storage_wm,
                        &utxo_flush_handles,
                        &retire_flush_counter,
                        max_utxo_flushes_under_pressure,
                        &ibd_muhash,
                        durability_tx.as_ref(),
                        publisher.as_ref(),
                        local_replay_bulk_write_done,
                        &retire_err,
                    ) {
                        return;
                    }
                }
                continue;
            }
        };
        let h = work.height;
        // Workers have already mutated cache + pending log for this height;
        // the retire thread no longer needs to read or apply the delta. The commitment tree is
        // the only consumer that still wants the delta, so we look it up under a short lock.
        if !engine_mode {
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
        }

        if engine_mode {
            if h % 64 == 0 {
                memory::ibd_memory_pressure_maintenance(
                    &mem_mtx,
                    &max_ahead_live,
                    nominal_max_ahead,
                    storage_wm.as_ref(),
                    utxo_engine.as_deref(),
                );
            }
            // Skip incremental UTXO commitments during engine IBD: legacy shell store is
            // empty (removes miss), and 2× heed3 inserts/block contend with block flushes.
            publisher.publish(&local_last_retired, h);
            adapt_max_pending_ops_tick(
                &max_pending_ops,
                max_pending_ops_nominal,
                memory::ibd_pressure_level_snapshot(),
                store.pending_len(),
                &max_pending_ops_last_adapt_ms,
            );
            continue;
        }

        // Pre-lock: DashMap eviction scans (can take several ms at h=400k+). Running these
        // before acquiring mem_mtx keeps the critical section to MemoryGuard work only.
        ibd_v2_retire_pre_lock(
            h,
            store.as_ref(),
            &work.blocks_buf,
            &mut keys_buf,
            &mut keys_seen,
            &mut evict_scratch,
        );
        let (opt_pkg, is_defer_checkpoint, cap_change) = {
            let mut mem = mem_mtx.lock();
            let (_s, _e, p, r, cap) = ibd_v2_retire_apply_utxo_delta(
                h,
                store.as_ref(),
                &mut mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r, cap)
        };
        // Post-lock: apply new cap and heap-trim without holding mem_mtx.
        if let Some((new_cap, pre_tune_len)) = cap_change {
            ibd_v2_retire_post_lock(store.as_ref(), new_cap, pre_tune_len, h);
        }
        if let (Some(cref), Some(cstore)) = (commitment_tree.as_ref(), commitment_cstore.as_ref()) {
            // work.block is Some only in non-engine mode (engine mode passes None to
            // avoid holding Arc<Block> in the channel during fast local replay).
            let block_hash = blockstore.get_block_hash(
                work.block.as_deref().expect("commitment tree requires non-engine mode block"),
            );
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
            if !local_replay_bulk_write_done {
                store.drain_in_flight_for_batch(&pkg.ops);
                store.release_protected_heights(&pkg.heights);
                store.note_utxo_flush_completed(pkg.max_block_height);
            } else {
                if let Err(e) = push_utxo_flush_from_retire(
                    &store,
                    &storage_wm,
                    &utxo_flush_handles,
                    &retire_flush_counter,
                    h,
                    max_utxo_flushes_under_pressure,
                    pkg,
                    &ibd_muhash,
                    is_defer_checkpoint,
                    durability_tx.as_ref(),
                ) {
                    *retire_err.lock() = Some(e);
                    return;
                }
            }
        }
        if !engine_mode && !local_replay_bulk_write_done && h > local_replay_no_lmdb_max {
            // CAS: only one shard across all retire threads performs the bulk write.
            let won = local_replay_transition_done
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            local_replay_bulk_write_done = true;
            if won {
                while let Some(tail_pkg) = store.maybe_take_flush_batch_adds_only() {
                    store.drain_in_flight_for_batch(&tail_pkg.ops);
                    store.release_protected_heights(&tail_pkg.heights);
                    store.note_utxo_flush_completed(tail_pkg.max_block_height);
                }
                let n_cache = store.len();
                info!(
                    "[IBD_REPLAY_TRANSITION] h={h}: local replay ended \
                     (replay_max={local_replay_no_lmdb_max}); streaming {n_cache} live UTXOs \
                     to LMDB (one-time bulk hydration); other shards are paused until complete"
                );
                let t0 = std::time::Instant::now();
                match store.flush_full_cache_to_lmdb() {
                    Ok(written) => {
                        let elapsed_s = t0.elapsed().as_secs();
                        info!(
                            "[IBD_REPLAY_TRANSITION] h={h}: bulk LMDB hydration complete \
                             ({written} entries, {elapsed_s}s); signalling losing shards to resume"
                        );
                    }
                    Err(e) => {
                        // Signal losing shards so they don't hang forever.
                        local_replay_hydration_done.store(true, Ordering::Release);
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
                store.restore_evict_after_local_replay(8_000_000);
                if let Err(e) = storage_wm.chain().force_set_ibd_utxo_watermark(local_replay_no_lmdb_max) {
                    warn!("[IBD_REPLAY_TRANSITION] force_set_ibd_utxo_watermark failed: {e}");
                }
                if let Err(e) = storage_wm.flush() {
                    warn!("[IBD_REPLAY_TRANSITION] storage flush after watermark failed: {e}");
                }
                // Release losing shards AFTER hydration and watermark are committed to disk.
                local_replay_hydration_done.store(true, Ordering::Release);
            } else {
                // Losing shard: WAIT for the winning shard to finish flush_full_cache_to_lmdb
                // before processing any block h > local_replay_no_lmdb_max. Without this wait,
                // concurrent cache modifications (spending UTXOs) race with the DashMap iterator
                // inside flush_full_cache_to_lmdb and cause silent UTXO misses → UTXO_TOTAL_MISS.
                info!(
                    "[IBD_REPLAY_TRANSITION] shard pausing at h={h} until bulk hydration \
                     completes (another shard won CAS; replay_max={local_replay_no_lmdb_max})"
                );
                while !local_replay_hydration_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!(
                    "[IBD_REPLAY_TRANSITION] shard resuming at h={h} after bulk hydration"
                );
                store.restore_evict_after_local_replay(8_000_000);
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
    retire_shard_index: usize,
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
    engine_mode: bool,
    durability_tx: Option<std::sync::mpsc::SyncSender<DurabilityRequest>>,
    /// When non-zero: skip all LMDB durability writes while `h <= local_replay_no_lmdb_max`
    /// (safe after a genesis restart where the LMDB UTXO store is empty).  A one-shot bulk
    /// cache→LMDB write fires at `h = local_replay_no_lmdb_max + 1` to hydrate LMDB before
    /// normal incremental durability resumes.  Zero means normal durability from the start.
    local_replay_no_lmdb_max: u64,
    // Shared across all retire shards. CAS from false→true wins the bulk write.
    local_replay_transition_done: Arc<std::sync::atomic::AtomicBool>,
    // Set to `true` by the winning shard only AFTER flush_full_cache_to_lmdb() completes.
    // Losing shards spin on this before processing blocks h > local_replay_no_lmdb_max.
    local_replay_hydration_done: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut keys_buf: Vec<OutPointKey> = Vec::new();
    let mut keys_seen = rustc_hash::FxHashSet::default();
    let mut evict_scratch: Vec<(OutPointKey, u64)> = Vec::new();
    let mut local_replay_bulk_write_done = local_replay_no_lmdb_max == 0 || engine_mode;
    if local_replay_no_lmdb_max > 0 {
        store.set_no_evict_for_local_replay();
        info!(
            "[IBD_REPLAY_NOLMDB] Disabling UTXO cache eviction during local replay \
             (heights 1–{local_replay_no_lmdb_max}): LMDB is empty so eviction would \
             permanently lose UTXOs. Will re-enable at replay→download transition."
        );
    }
    loop {
        let work = match work_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(w) => w,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Retire went idle: flush pending when pending > cap regardless of pressure.
                // Without this, defer_flush=true creates a deadlock: workers park on the
                // cap, no staged blocks arrive, and the old Emergency-only guard never fires.
                // Also re-read /proc so the pressure atomic stays current.
                if engine_mode {
                    continue;
                }
                let level = {
                    let mut mem = mem_mtx.lock();
                    let level = mem.should_flush(Some((&max_ahead_live, nominal_max_ahead)));
                    memory::publish_ibd_pressure(level);
                    level
                };
                let pending_len = store.pending_len();
                let cap = max_pending_ops.load(Ordering::Relaxed);
                let over_cap = pending_len > cap;
                if level >= PressureLevel::Critical || over_cap {
                    let evictable = store.len().saturating_sub(store.protected_len());
                    if evictable >= IBD_EMERGENCY_EVICT_MIN_UNPROTECTED {
                        store.evict_aggressive_for_rss();
                    }
                    if ibd_idle_flush_pending_over_cap(
                        "idle_flush_B",
                        retire_shard_index,
                        level,
                        pending_len,
                        cap,
                        over_cap,
                        &store,
                        &storage_wm,
                        &utxo_flush_handles,
                        &retire_flush_counter,
                        max_utxo_flushes_under_pressure,
                        &ibd_muhash,
                        durability_tx.as_ref(),
                        publisher.as_ref(),
                        local_replay_bulk_write_done,
                        &retire_err,
                    ) {
                        return;
                    }
                }
                continue;
            }
        };
        let h = work.height;

        if engine_mode {
            if h % 64 == 0 {
                memory::ibd_memory_pressure_maintenance(
                    &mem_mtx,
                    &max_ahead_live,
                    nominal_max_ahead,
                    storage_wm.as_ref(),
                    utxo_engine.as_deref(),
                );
            }
            publisher.publish(&local_last_retired, h);
            adapt_max_pending_ops_tick(
                &max_pending_ops,
                max_pending_ops_nominal,
                memory::ibd_pressure_level_snapshot(),
                store.pending_len(),
                &max_pending_ops_last_adapt_ms,
            );
            continue;
        }

        // Workers have already mutated cache + pending log for this height;
        // retire only runs the *coordinated* per-block work (eviction + flush decisions).
        //
        // Per-block timing: log whenever any phase takes >500ms so we can attribute stalls.
        let t_retire_start = std::time::Instant::now();

        // Pre-lock: DashMap eviction scans. Running outside mem_mtx keeps the critical
        // section to only the MemoryGuard pressure evaluation and flush selection.
        ibd_v2_retire_pre_lock(
            h,
            store.as_ref(),
            &work.blocks_buf,
            &mut keys_buf,
            &mut keys_seen,
            &mut evict_scratch,
        );
        let t_after_prelock = t_retire_start.elapsed().as_millis();

        let (opt_pkg, is_defer_checkpoint, cap_change) = {
            let mut mem = mem_mtx.lock();
            let (_s, _e, p, r, cap) = ibd_v2_retire_apply_utxo_delta(
                h,
                store.as_ref(),
                &mut mem,
                &max_ahead_live,
                nominal_max_ahead,
                ibd_defer_flush,
                ibd_defer_checkpoint,
            );
            (p, r, cap)
        };
        let t_after_memlock = t_retire_start.elapsed().as_millis();

        // Post-lock: apply new UTXO cache cap and optional heap trim without holding mem_mtx.
        if let Some((new_cap, pre_tune_len)) = cap_change {
            ibd_v2_retire_post_lock(store.as_ref(), new_cap, pre_tune_len, h);
        }
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

        // Log slow retire cycles to diagnose stalls.
        let t_total_pre_flush = t_retire_start.elapsed().as_millis();
        if t_total_pre_flush > 500 {
            warn!(
                "[IBD_RETIRE_SLOW] h={h} pre_lock={t_after_prelock}ms \
                 mem_lock={t_after_memlock}ms total_pre_flush={t_total_pre_flush}ms \
                 (has_flush={})",
                opt_pkg.is_some()
            );
        }

        if let Some(pkg) = opt_pkg {
            if !local_replay_bulk_write_done {
                // Local-replay no-LMDB mode: drain the pending log but skip all LMDB writes.
                // The LMDB UTXO store is empty (wiped before this genesis restart) so:
                //   • ADD ops: UTXO already lives in the DashMap (workers put it there).
                //   • DEL ops: UTXO already removed from DashMap at spend time.
                // Discarding the package is fully correct and keeps `pending_len` near zero,
                // preventing the worker backpressure spin that otherwise freezes validation
                // for 90 s every 90 s (channel fills in ~43 s at 3000 BPS local replay speed).
                // drain_in_flight_for_batch clears the eagerly-registered in_flight_insertions
                // entries that workers insert for every UTXO add (apply_utxo_delta hot path).
                // Without this, in_flight_insertions grows to 20 M entries / ~5 GB by h=390k.
                store.drain_in_flight_for_batch(&pkg.ops);
                store.release_protected_heights(&pkg.heights);
                store.note_utxo_flush_completed(pkg.max_block_height);
                // pkg dropped here; no LMDB write.
            } else {
                if let Err(e) = push_utxo_flush_from_retire(
                    &store,
                    &storage_wm,
                    &utxo_flush_handles,
                    &retire_flush_counter,
                    h,
                    max_utxo_flushes_under_pressure,
                    pkg,
                    &ibd_muhash,
                    is_defer_checkpoint,
                    durability_tx.as_ref(),
                ) {
                    *retire_err.lock() = Some(e);
                    return;
                }
            }
        }

        // One-time transition: first block past the local-replay window triggers the bulk
        // cache→LMDB write that hydrates the empty LMDB store with all live UTXOs.
        // CAS on the shared atomic ensures exactly ONE shard does the write — previously
        // all N shards fired independently, causing N concurrent ~6 GB Vec allocations → OOM.
        if !engine_mode && !local_replay_bulk_write_done && h > local_replay_no_lmdb_max {
            local_replay_bulk_write_done = true;
            let won = local_replay_transition_done
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if won {
                while let Some(tail_pkg) = store.maybe_take_flush_batch_adds_only() {
                    store.drain_in_flight_for_batch(&tail_pkg.ops);
                    store.release_protected_heights(&tail_pkg.heights);
                    store.note_utxo_flush_completed(tail_pkg.max_block_height);
                }
                let n_cache = store.len();
                info!(
                    "[IBD_REPLAY_TRANSITION] h={h}: local replay ended \
                     (replay_max={local_replay_no_lmdb_max}); streaming {n_cache} live UTXOs \
                     to LMDB (one-time bulk hydration); other shards paused until complete"
                );
                let t0 = std::time::Instant::now();
                match store.flush_full_cache_to_lmdb() {
                    Ok(written) => {
                        let elapsed_s = t0.elapsed().as_secs();
                        info!(
                            "[IBD_REPLAY_TRANSITION] h={h}: bulk LMDB hydration complete \
                             ({written} entries, {elapsed_s}s); signalling losing shards to resume"
                        );
                    }
                    Err(e) => {
                        // Signal losing shards so they don't hang forever.
                        local_replay_hydration_done.store(true, Ordering::Release);
                        *retire_err.lock() = Some(e);
                        return;
                    }
                }
                store.restore_evict_after_local_replay(utxo_nominal_max_entries);
                info!(
                    "[IBD_REPLAY_TRANSITION] h={h}: cache eviction re-enabled \
                     (nominal cap={utxo_nominal_max_entries}); MemoryGuard will tune further."
                );
                if let Err(e) = storage_wm.chain().force_set_ibd_utxo_watermark(local_replay_no_lmdb_max) {
                    warn!("[IBD_REPLAY_TRANSITION] force_set_ibd_utxo_watermark failed: {e}");
                }
                if let Err(e) = storage_wm.flush() {
                    warn!("[IBD_REPLAY_TRANSITION] storage flush after watermark failed: {e}");
                }
                // Release losing shards AFTER hydration and watermark are committed to disk.
                local_replay_hydration_done.store(true, Ordering::Release);
            } else {
                // Losing shard: WAIT for the winning shard to finish flush_full_cache_to_lmdb
                // before processing any block h > local_replay_no_lmdb_max. Without this wait,
                // concurrent cache modifications (spending UTXOs) race with the DashMap iterator
                // inside flush_full_cache_to_lmdb, causing silent UTXO misses → UTXO_TOTAL_MISS.
                info!(
                    "[IBD_REPLAY_TRANSITION] shard pausing at h={h} until bulk hydration \
                     completes (another shard won CAS; replay_max={local_replay_no_lmdb_max})"
                );
                while !local_replay_hydration_done.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                info!(
                    "[IBD_REPLAY_TRANSITION] shard resuming at h={h} after bulk hydration"
                );
                store.restore_evict_after_local_replay(utxo_nominal_max_entries);
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

    #[test]
    fn classify_binder_supply_vs_engine() {
        assert_eq!(
            classify_ibd_binder(0, 12, 0, 0, Some(90), PressureLevel::None, false),
            "SUPPLY_TIP_HOLE"
        );
        assert_eq!(
            classify_ibd_binder(0, 0, 0, 0, None, PressureLevel::None, false),
            "SUPPLY_EMPTY_TIP"
        );
        assert_eq!(
            classify_ibd_binder(0, 0, 8, 0, None, PressureLevel::None, false),
            "SUPPLY_FEEDER_STARVE",
            "no gd sample → still classic starve"
        );
        assert_eq!(
            classify_ibd_binder(0, 0, 66, 0, Some(29), PressureLevel::None, false),
            "PIPE_DRAINED",
            "H3 C3: feeder=0 + healthy gd + contig ≠ supply starve"
        );
        assert_eq!(
            classify_ibd_binder(64, 0, 16, 0, Some(80), PressureLevel::None, false),
            "ENGINE_OR_SCRIPTS"
        );
        assert_eq!(
            classify_ibd_binder(64, 0, 16, 0, None, PressureLevel::Emergency, false),
            "ENGINE_PRESSURE"
        );
        assert_eq!(
            classify_ibd_binder(4, 0, 0, 0, Some(400), PressureLevel::None, false),
            "SUPPLY_GD_SLOW"
        );
    }

    /// Nominal cap is always positive; adaptation runs at every pressure level.
    #[test]
    fn adapt_always_runs_with_positive_nominal() {
        let cap = fresh_cap(5_000_000);
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, 5_000_000, PressureLevel::Emergency, 5_000_000, &last);
        assert!(cap.load(Ordering::Relaxed) < 5_000_000);
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
    ///
    /// Append diagnostics are process-global atomics — keep these cases in one test so
    /// parallel `cargo test` filters cannot race the throttle window.
    #[test]
    fn pipeline_depth_pressure_and_engine_append_throttle() {
        use crate::storage::ibd_engine::memory_age::{
            bump_append_stats_detailed_for_test, reset_append_diagnostics_for_test,
            set_append_window_baseline_for_test,
        };

        reset_append_diagnostics_for_test();
        let _tip = super::super::tip_stage::test_tip_atomics_lock();
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::memory::test_seed_ibd_rss_anon_mb(0);
        assert_eq!(pipeline_depth_for_pressure(PressureLevel::None, 32), 32);
        assert_eq!(pipeline_depth_for_pressure(PressureLevel::Emergency, 32), 8);
        assert_eq!(pipeline_depth_for_pressure(PressureLevel::Critical, 32), 16);
        assert_eq!(pipeline_depth_for_pressure(PressureLevel::Elevated, 32), 24);

        // Peak Land E: tip-crawl healthy holds Critical at Elevated depth (24). No C2v2 cap.
        super::super::tip_stage::publish_wan_body_tip(100);
        super::super::tip_stage::mark_needed(200);
        super::super::tip_stage::test_seed_getdata_body_ewma(40, 32);
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
            24,
            "Elevated must not soft-cap (C2 tip30 regress)"
        );
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Critical, 32),
            24,
            "peak: raw Critical + tip-crawl healthy holds at Elevated depth"
        );
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Emergency, 32),
            8,
            "Emergency depth unchanged (real reclaim)"
        );
        assert_eq!(
            engine_pressure_poll_interval(PressureLevel::Critical),
            16,
            "tip-crawl healthy supply must poll Critical like Elevated"
        );

        // Peak: Elevated is always 3/4. r28 small-anon hold is off.
        super::memory::test_seed_ibd_rss_anon_mb(8377);
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
            24,
            "peak: Elevated stays 24 at anon 8G (r28 hold off)"
        );
        assert_eq!(
            engine_pressure_poll_interval(PressureLevel::Elevated),
            16,
            "must not remap poll — that is r26"
        );
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Critical, 32),
            24,
            "must not lift Critical off Land E 24"
        );
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Emergency, 32),
            8,
            "must not hold Emergency"
        );
        super::memory::test_seed_ibd_rss_anon_mb(17174);
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
            24,
            "real Elevated anon 17G stays depth 24"
        );
        super::memory::test_seed_ibd_rss_anon_mb(8377);
        super::super::tip_stage::test_reset_getdata_body_ewma();
        super::super::tip_stage::publish_wan_body_tip(100);
        super::super::tip_stage::mark_needed(200);
        assert_eq!(
            pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
            24,
            "unhealthy supply still Elevated 24"
        );
        super::memory::test_seed_ibd_rss_anon_mb(0);
        super::super::tip_stage::test_reset_tip_stage();
        super::super::tip_stage::test_reset_getdata_body_ewma();

        // 33% total slow but 0% contention — must keep full depth.
        reset_append_diagnostics_for_test();
        bump_append_stats_detailed_for_test(70_000, 35_000, 0);
        assert_eq!(pipeline_depth_for_engine_append(16), 16);

        // Pure contention spike → collapse depth.
        reset_append_diagnostics_for_test();
        bump_append_stats_detailed_for_test(90_000, 140_600, 140_600);
        set_append_window_baseline_for_test(90_000, 140_600);
        bump_append_stats_detailed_for_test(0, 256, 256);
        assert_eq!(pipeline_depth_for_engine_append(32), 1);
    }

    #[test]
    fn engine_pressure_poll_interval_tightens_with_pressure() {
        assert_eq!(engine_pressure_poll_interval(PressureLevel::None), 32);
        assert_eq!(engine_pressure_poll_interval(PressureLevel::Emergency), 1);
        assert_eq!(engine_pressure_poll_interval(PressureLevel::Critical), 4);
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
        let now_ms = crate::utils::time::current_timestamp_millis();
        let last = Arc::new(AtomicU64::new(now_ms));
        adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Emergency, 8_000_000, &last);
        assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
    }

    /// Repeated Emergency ticks must converge to the floor at max(nominal/2, 1_000_000).
    /// The previous policy used nominal/16 which caused workers to spin at <1 BPS.
    /// Now Emergency uses gentle 10% trim with floor = nominal/2 to keep workers moving.
    #[test]
    fn adapt_emergency_respects_floor_under_repeat() {
        let nominal = 8_000_000;
        let cap = fresh_cap(nominal);
        for _ in 0..50 {
            let last = fresh_last_adapt_zero();
            adapt_max_pending_ops_tick(&cap, nominal, PressureLevel::Emergency, nominal, &last);
        }
        let final_cap = cap.load(Ordering::Relaxed);
        // Floor = max(nominal/2, 1_000_000) — must never go below this
        let expected_floor = (nominal / 2).max(1_000_000);
        assert!(
            final_cap >= expected_floor,
            "must respect floor {expected_floor} (got {final_cap})",
        );
        // Must be at most nominal (never grow under emergency pressure)
        assert!(
            final_cap <= nominal,
            "must not exceed nominal (got {final_cap} for nominal {nominal})",
        );
    }

    /// Regression: holding `utxo_flush_handles` across `join()` wedged IBD shutdown when a
    /// RocksDB commit was slow — other paths could not drain or enqueue flushes.
    #[test]
    fn join_all_utxo_flush_handles_releases_mutex_before_join() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let started_join = Arc::clone(&started);
        let release_join = Arc::clone(&release);
        let slow = thread::spawn(move || {
            started_join.store(true, Ordering::Release);
            while !release_join.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(blvm_muhash::MuHash3072::new())
        });

        let utxo_flush_handles = Arc::new(Mutex::new(VecDeque::new()));
        utxo_flush_handles.lock().push_back(slow);

        let handles_for_join = Arc::clone(&utxo_flush_handles);
        let joiner = thread::spawn(move || join_all_utxo_flush_handles(&handles_for_join, "test"));

        let wait_start = Instant::now();
        while !started.load(Ordering::Acquire) {
            assert!(
                wait_start.elapsed() < Duration::from_secs(2),
                "slow flush worker did not start"
            );
            thread::sleep(Duration::from_millis(1));
        }

        let lock_start = Instant::now();
        {
            let _guard = utxo_flush_handles.lock();
        }
        assert!(
            lock_start.elapsed() < Duration::from_millis(500),
            "utxo_flush_handles mutex still held during join"
        );

        release.store(true, Ordering::Release);
        joiner.join().expect("join thread").expect("join flushes");
        assert!(utxo_flush_handles.lock().is_empty());
    }

    #[test]
    fn join_all_utxo_flush_handles_empty_queue_is_noop() {
        let utxo_flush_handles = Arc::new(Mutex::new(VecDeque::new()));
        let combined =
            join_all_utxo_flush_handles(&utxo_flush_handles, "test").expect("empty join");
        assert_eq!(
            combined.finalize(),
            blvm_muhash::MuHash3072::new().finalize()
        );
    }
}

#[cfg(all(test, feature = "production"))]
mod retire_flush_batch_tests {
    use super::*;
    use crate::storage::database::{BatchWriter, Tree};
    use crate::storage::ibd_utxo_store::IbdUtxoStore;
    use crate::storage::utxo_value_codec::ValueCodec;
    use blvm_protocol::block::UtxoDelta;
    use blvm_protocol::types::{OutPoint, UTXO};
    use rustc_hash::FxHashMap;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MemTree {
        inner: StdMutex<HashMap<Vec<u8>, Vec<u8>>>,
    }

    impl Tree for MemTree {
        fn insert(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
            self.inner
                .lock()
                .unwrap()
                .insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        fn get(&self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self.inner.lock().unwrap().get(key).cloned())
        }
        fn get_many(&self, keys: &[&[u8]]) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
            let g = self.inner.lock().unwrap();
            Ok(keys.iter().map(|k| g.get(*k).cloned()).collect())
        }
        fn remove(&self, key: &[u8]) -> anyhow::Result<()> {
            self.inner.lock().unwrap().remove(key);
            Ok(())
        }
        fn contains_key(&self, key: &[u8]) -> anyhow::Result<bool> {
            Ok(self.inner.lock().unwrap().contains_key(key))
        }
        fn clear(&self) -> anyhow::Result<()> {
            self.inner.lock().unwrap().clear();
            Ok(())
        }
        fn len(&self) -> anyhow::Result<usize> {
            Ok(self.inner.lock().unwrap().len())
        }
        fn iter(&self) -> Box<dyn Iterator<Item = anyhow::Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let entries: Vec<_> = self
                .inner
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| Ok((k.clone(), v.clone())))
                .collect();
            Box::new(entries.into_iter())
        }
        fn batch(&self) -> anyhow::Result<Box<dyn BatchWriter + '_>> {
            Ok(Box::new(MemBatch {
                tree: self,
                ops: Vec::new(),
            }))
        }
    }

    struct MemBatch<'a> {
        tree: &'a MemTree,
        ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    }

    impl<'a> BatchWriter for MemBatch<'a> {
        fn put(&mut self, key: &[u8], value: &[u8]) {
            self.ops.push((key.to_vec(), Some(value.to_vec())));
        }
        fn delete(&mut self, key: &[u8]) {
            self.ops.push((key.to_vec(), None));
        }
        fn commit(self: Box<Self>) -> anyhow::Result<()> {
            let mut g = self.tree.inner.lock().unwrap();
            for (k, v) in self.ops {
                match v {
                    Some(val) => {
                        g.insert(k, val);
                    }
                    None => {
                        g.remove(&k);
                    }
                }
            }
            Ok(())
        }
        fn len(&self) -> usize {
            self.ops.len()
        }
    }

    fn synth_outpoint(seed: u64, idx: u32) -> OutPoint {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&seed.to_le_bytes());
        hash[8..12].copy_from_slice(&idx.to_le_bytes());
        OutPoint { hash, index: idx }
    }

    fn fill_del_backlog(store: &IbdUtxoStore, blocks: u64) {
        let mut alive: FxHashMap<OutPoint, UTXO> = FxHashMap::default();
        let mut del_scratch = Vec::new();
        let mut add_scratch = Vec::new();
        for h in 1..=blocks {
            let mut deletions = Vec::new();
            for op in alive.keys().take(50) {
                deletions.push(*op);
            }
            for op in &deletions {
                alive.remove(op);
            }
            let mut adds = FxHashMap::default();
            for i in 0..50 {
                let op = synth_outpoint(h, i);
                adds.insert(
                    op,
                    Arc::new(UTXO {
                        value: 1,
                        script_pubkey: (&[0u8; 25][..]).into(),
                        height: h,
                        is_coinbase: false,
                    }),
                );
                alive.insert(
                    op,
                    UTXO {
                        value: 1,
                        script_pubkey: (&[0u8; 25][..]).into(),
                        height: h,
                        is_coinbase: false,
                    },
                );
            }
            let delta = UtxoDelta {
                additions: adds,
                deletions: deletions
                    .iter()
                    .map(|op| {
                        let mut k = [0u8; 36];
                        k[..32].copy_from_slice(&op.hash);
                        k[32..36].copy_from_slice(&op.index.to_be_bytes());
                        k
                    })
                    .collect(),
            };
            store.apply_utxo_delta(&delta, h, &mut del_scratch, &mut add_scratch, false);
            while let Some(pkg) = store.maybe_take_flush_batch_adds_only() {
                drop(pkg);
            }
        }
    }

    #[test]
    fn pick_flush_batch_no_force_on_del_backlog() {
        let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
        let store = IbdUtxoStore::new_with_options(
            Arc::clone(&disk),
            100,
            false,
            usize::MAX,
            crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
            0,
            ValueCodec::Bincode,
        );
        // Drain adds each block so DEL tombstones accumulate; leave the last block's ADDs
        // in add_shards so pick returns a small adds-only batch while pending stays del-heavy.
        fill_del_backlog(&store, 119);
        let mut del_scratch = Vec::new();
        let mut add_scratch = Vec::new();
        let mut adds = FxHashMap::default();
        for i in 0..50 {
            let op = synth_outpoint(120, i);
            adds.insert(
                op,
                Arc::new(UTXO {
                    value: 1,
                    script_pubkey: (&[0u8; 25][..]).into(),
                    height: 120,
                    is_coinbase: false,
                }),
            );
        }
        let delta = UtxoDelta {
            additions: adds,
            deletions: FxHashSet::default(),
        };
        store.apply_utxo_delta(&delta, 120, &mut del_scratch, &mut add_scratch, false);
        assert!(store.pending_len() > 1_000);

        let (pkg, force_durability) = ibd_retire_pick_flush_batch(&store);
        assert!(
            !force_durability,
            "del-heavy pick must not promote checkpoint; del_backlog runs at formal boundary only"
        );
        let pkg = pkg.expect("small adds batch should still flush async");
        assert!(
            pkg.ops.len() < IBD_MIN_ADDS_ONLY_BATCH,
            "E2 scenario: small adds drain with large del backlog"
        );
    }

    #[test]
    fn formal_checkpoint_sentinel_when_add_drain_empty() {
        use super::memory::MemoryGuard;

        let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
        let store = IbdUtxoStore::new_with_options(
            Arc::clone(&disk),
            100,
            false,
            usize::MAX,
            crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
            0,
            ValueCodec::Bincode,
        );
        fill_del_backlog(&store, 120);
        assert!(store.pending_len() > 1_000);
        assert!(store.take_flush_batch_adds_only().is_none());

        let mut mem = MemoryGuard::new();
        let max_ahead = Arc::new(AtomicU64::new(2048));
        let (_s, _e, pkg, force, _cap) = ibd_v2_retire_apply_utxo_delta(
            1_000,
            &store,
            &mut mem,
            &max_ahead,
            2048,
            true,
            1_000,
        );
        assert!(force, "formal boundary must set force_durability");
        let pkg = pkg.expect("DEL-only pending at boundary must yield sentinel package");
        assert!(pkg.ops.is_empty());
        assert_eq!(pkg.max_block_height, 1_000);
    }

    #[test]
    fn formal_checkpoint_runs_under_elevated_pressure() {
        use super::memory::MemoryGuard;

        let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
        let store = IbdUtxoStore::new_with_options(
            Arc::clone(&disk),
            100,
            false,
            usize::MAX,
            crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
            0,
            ValueCodec::Bincode,
        );
        fill_del_backlog(&store, 120);
        assert!(store.pending_len() > 1_000);

        let mut mem = MemoryGuard::new();
        mem.test_seed_pressure_level(PressureLevel::Elevated);
        let max_ahead = Arc::new(AtomicU64::new(2048));
        let (_s, _e, pkg, force, _cap) = ibd_v2_retire_apply_utxo_delta(
            681_000,
            &store,
            &mut mem,
            &max_ahead,
            2048,
            true,
            200,
        );
        assert!(
            force,
            "formal boundary must checkpoint even when MemoryGuard is Elevated"
        );
        assert!(pkg.is_some(), "boundary at h=681000 must yield a flush package");
    }

    #[test]
    fn between_checkpoints_under_elevated_stays_async() {
        use super::memory::MemoryGuard;

        let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
        let store = IbdUtxoStore::new_with_options(
            Arc::clone(&disk),
            100,
            false,
            usize::MAX,
            crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
            0,
            ValueCodec::Bincode,
        );
        fill_del_backlog(&store, 119);
        let mut del_scratch = Vec::new();
        let mut add_scratch = Vec::new();
        let mut adds = FxHashMap::default();
        for i in 0..50 {
            let op = synth_outpoint(120, i);
            adds.insert(
                op,
                Arc::new(UTXO {
                    value: 1,
                    script_pubkey: (&[0u8; 25][..]).into(),
                    height: 120,
                    is_coinbase: false,
                }),
            );
        }
        let delta = UtxoDelta {
            additions: adds,
            deletions: FxHashSet::default(),
        };
        store.apply_utxo_delta(&delta, 120, &mut del_scratch, &mut add_scratch, false);

        let mut mem = MemoryGuard::new();
        mem.test_seed_pressure_level(PressureLevel::Elevated);
        let max_ahead = Arc::new(AtomicU64::new(2048));
        let (_s, _e, _pkg, force, _cap) = ibd_v2_retire_apply_utxo_delta(
            681_001,
            &store,
            &mut mem,
            &max_ahead,
            2048,
            true,
            200,
        );
        assert!(
            !force,
            "non-boundary retire under Elevated must stay adds-only async"
        );
    }

    #[test]
    fn retire_do_durability_channel_only_explicit_checkpoints() {
        assert!(!retire_do_durability(false, 16, 15, true));
        assert!(!retire_do_durability(false, 16, 15, false));
        assert!(retire_do_durability(false, 16, 16, false));
        assert!(retire_do_durability(true, 16, 3, true));
    }

    #[test]
    fn ibd_durability_channel_cap_bounds() {
        assert_eq!(ibd_durability_channel_cap(400_000), 4);
        let segwit = ibd_durability_channel_cap(500_000);
        assert!((2..=16).contains(&segwit));
    }

    fn mk_durability_req(is_checkpoint: bool) -> DurabilityRequest {
        DurabilityRequest {
            pkg: PendingFlushPackage {
                ops: Arc::new(Vec::new()),
                max_block_height: 100,
                heights: Arc::new(FxHashSet::default()),
            },
            trigger_height: 100,
            is_checkpoint,
        }
    }

    #[test]
    fn durability_batch_does_not_merge_mixed_checkpoint_flags() {
        let adds_only = mk_durability_req(false);
        let checkpoint = mk_durability_req(true);

        assert!(ibd_durability_may_merge_request(&[], &adds_only));
        assert!(ibd_durability_may_merge_request(
            &[mk_durability_req(false)],
            &adds_only
        ));
        assert!(!ibd_durability_may_merge_request(
            &[mk_durability_req(false)],
            &checkpoint
        ));
        assert!(!ibd_durability_may_merge_request(
            &[mk_durability_req(true)],
            &adds_only
        ));
        assert!(ibd_durability_may_merge_request(
            &[mk_durability_req(true)],
            &checkpoint
        ));
    }

    #[test]
    fn checkpoint_watermark_uses_formal_boundary_not_batch_max() {
        let batch = vec![
            DurabilityRequest {
                pkg: PendingFlushPackage {
                    ops: Arc::new(Vec::new()),
                    max_block_height: 435_005,
                    heights: Arc::new(FxHashSet::default()),
                },
                trigger_height: 433_800,
                is_checkpoint: true,
            },
        ];
        assert_eq!(
            ibd_checkpoint_watermark_for_batch(&batch, 435_005, 200),
            433_800
        );
    }

    #[test]
    fn mtp_tip_fallback_only_near_tip() {
        // Gap resume: start 880001, tip 957850 — must not use tip window.
        assert!(!mtp_tip_window_fallback_ok(880_001, 957_850));
        assert!(!mtp_tip_window_fallback_ok(100, 200_000));
        // Near-tip catch-up: allow sliding-window fallback.
        assert!(mtp_tip_window_fallback_ok(957_820, 957_850));
        assert!(mtp_tip_window_fallback_ok(957_850, 957_850));
        assert!(!mtp_tip_window_fallback_ok(100, 0));
    }

    #[test]
    fn formal_checkpoint_boundary_aligns_to_interval() {
        assert_eq!(ibd_formal_checkpoint_boundary(433_800, 200), Some(433_800));
        assert_eq!(ibd_formal_checkpoint_boundary(433_801, 200), None);
    }

    #[test]
    fn del_backlog_collapse_default_min_height_350k() {
        let _lock = super::retire_flush_batch_tests_env_lock();
        unsafe {
            std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
        }
        assert!(!ibd_del_backlog_use_collapse_path(349_000));
        assert!(ibd_del_backlog_use_collapse_path(350_000));
        assert!(ibd_del_backlog_use_collapse_path(431_000));
    }

    #[test]
    fn del_backlog_collapse_gated_by_min_height_env() {
        let _lock = super::retire_flush_batch_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT", "500000");
        }
        assert!(!ibd_del_backlog_use_collapse_path(300_000));
        assert!(ibd_del_backlog_use_collapse_path(500_000));
        unsafe {
            std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
        }
    }

    #[test]
    fn del_backlog_collapse_forced_when_min_height_zero() {
        let _lock = super::retire_flush_batch_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT", "0");
        }
        assert!(ibd_del_backlog_use_collapse_path(300_000));
        unsafe {
            std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
        }
    }

    #[test]
    fn del_backlog_fast_path_returns_single_batch() {
        let _lock = super::retire_flush_batch_tests_env_lock();
        unsafe {
            std::env::set_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT", "500000");
        }

        let disk: Arc<dyn Tree> = Arc::new(MemTree::default());
        let store = IbdUtxoStore::new_with_options(
            Arc::clone(&disk),
            100,
            false,
            usize::MAX,
            crate::storage::ibd_utxo_store::EvictionStrategy::Fifo,
            0,
            ValueCodec::Bincode,
        );
        fill_del_backlog(&store, 120);
        let store = Arc::new(store);
        let storage = Arc::new(
            crate::storage::Storage::new(tempfile::tempdir().unwrap().path()).unwrap(),
        );
        let mh = Arc::new(Mutex::new(blvm_muhash::MuHash3072::new()));

        let batch_n =
            ibd_flush_del_backlog_drain(&store, &mh, 120, false).unwrap();
        assert_eq!(batch_n, 1, "fast path must use single force-through batch");

        unsafe {
            std::env::remove_var("BLVM_IBD_DEL_COLLAPSE_MIN_HEIGHT");
        }
    }
}

fn retire_flush_batch_tests_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

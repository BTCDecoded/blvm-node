//! Dynamic memory management for IBD.
//!
//! Hardware-aware tuning: derives memory budget from total RAM, allocates across
//! UTXO cache, block buffer, prefetch, and overhead. Flush and download **ahead**
//! depth are driven by **live** `/proc` RSS + MemAvailable + MemTotal — no
//! env-var knobs required. The system must never OOM regardless of host RAM.
//!
//! Graduated pressure response (see `adjust_max_ahead_live`; fractions depend on RAM tier):
//!   None     → recover toward nominal `max_ahead` in steps
//!   Elevated → ~½ nominal (min 128), flush more often
//!   Critical → ~¼–⅓ nominal (mins 64–96), force flush + shed caches
//!   Emergency → ~⅙ nominal on 16 GiB (min 48), minimal pipeline + sync drain
//!
//! Every change in [`PressureLevel`] (including back to `None`) is logged once via
//! `pressure_level_reported` / `should_flush` (`MemoryGuard: pressure transition From -> To`).

#[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
use libmimalloc_sys;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Memory pressure severity. Higher levels trigger more aggressive responses
/// in the validation loop. Ordered so `>=` comparisons work naturally.
/// `repr(u8)` enables sharing with [`IBD_PRESSURE_LEVEL`] for coordinator admission control.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PressureLevel {
    None = 0,
    Elevated = 1,
    Critical = 2,
    Emergency = 3,
}

impl PressureLevel {
    #[inline]
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Elevated,
            2 => Self::Critical,
            3 => Self::Emergency,
            _ => Self::None,
        }
    }
}

/// Latest pressure published by the validation thread (Linux). The coordinator consults
/// [`ibd_pressure_is_emergency`] before draining download queues under memory pressure.
static IBD_PRESSURE_LEVEL: AtomicU8 = AtomicU8::new(0);

/// Last `RssAnon` (MiB) from a MemoryGuard `/proc` sample. `0` = unknown (do not treat as small).
/// Depth-only Land E hold reads this; raw pressure / flush / poll do not.
static IBD_RSS_ANON_MB: AtomicU64 = AtomicU64::new(0);

/// Process-level workload class latch (0xff = not yet set, 0 = Shared, 1 = Dedicated).
///
/// Once the first `MemoryGuard::new_for_ibd` call determines the workload class, it stores
/// the result here.  Every subsequent call in the same process uses the stored value — this
/// prevents the `Dedicated↔Shared` oscillation caused by `MemAvailable` fluctuating around
/// the 70 % threshold as LMDB page-cache is evicted and re-faulted between catch-up cycles.
/// The latch is intentionally never reset within a process lifetime: if we started as
/// `Dedicated` we must stay `Dedicated`; a mid-run flip to `Shared` shrinks the UTXO cache
/// by 12 GB and a subsequent flip back surges it — the pattern that triggers OOM.
static WORKLOAD_CLASS_LATCH: AtomicU8 = AtomicU8::new(0xff);

#[inline]
pub(crate) fn publish_ibd_pressure(level: PressureLevel) {
    IBD_PRESSURE_LEVEL.store(level as u8, Ordering::Relaxed);
}

#[inline]
pub(crate) fn publish_ibd_rss_anon_mb(mb: u64) {
    IBD_RSS_ANON_MB.store(mb, Ordering::Relaxed);
}

#[inline]
pub(crate) fn ibd_rss_anon_mb_snapshot() -> u64 {
    IBD_RSS_ANON_MB.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn test_seed_ibd_rss_anon_mb(mb: u64) {
    publish_ibd_rss_anon_mb(mb);
}

/// Poll `/proc`, publish pressure, throttle RocksDB, and drive engine index spill.
pub(crate) fn ibd_memory_pressure_maintenance(
    mem_mtx: &parking_lot::Mutex<MemoryGuard>,
    max_ahead_live: &AtomicU64,
    nominal_max_ahead: u64,
    storage: &crate::storage::Storage,
    utxo_engine: Option<&crate::storage::ibd_engine::UtxoDatabase>,
) -> PressureLevel {
    let level = {
        let mut mem = mem_mtx.lock();
        let level = mem.should_flush(Some((max_ahead_live, nominal_max_ahead)));
        publish_ibd_pressure(level);
        level
    };
    storage.ibd_memory_pressure_tick(level as u8);
    if let Some(db) = utxo_engine {
        db.memory_pressure_tick(level as u8);
    }
    level
}

#[inline]
pub(crate) fn ibd_pressure_is_emergency() -> bool {
    IBD_PRESSURE_LEVEL.load(Ordering::Relaxed) >= PressureLevel::Emergency as u8
}

#[inline]
pub(crate) fn ibd_pressure_is_critical_or_worse() -> bool {
    IBD_PRESSURE_LEVEL.load(Ordering::Relaxed) >= PressureLevel::Critical as u8
}

/// Clear process-global pressure when a parallel IBD session ends so zombie coordinator
/// threads from a prior session cannot keep new sessions in Emergency admission pause.
pub(crate) fn reset_ibd_pressure_on_session_end() {
    IBD_PRESSURE_LEVEL.store(PressureLevel::None as u8, Ordering::Release);
    IBD_RSS_ANON_MB.store(0, Ordering::Release);
}

/// Thresholds for stepping down a **stale** `Emergency` flag (see
/// [`stale_emergency_step_down_level`]).  Must match [`MemoryGuard::clamp_pressure_to_process_budget`].
fn emergency_entry_anon_mb(rss_budget_mb: u64, no_swap: bool) -> u64 {
    if no_swap {
        rss_budget_mb * 85 / 100
    } else {
        rss_budget_mb
    }
}

/// If anon RSS is still at/above the Emergency **entry** line, return `None` (live pressure).
/// Otherwise return the level to publish for a stale flag left from a prior IBD attempt.
pub(crate) fn stale_emergency_step_down_level(
    anon_mb: u64,
    rss_budget_mb: u64,
    no_swap: bool,
) -> Option<PressureLevel> {
    if rss_budget_mb == 0 {
        return None;
    }
    let emerg_line = emergency_entry_anon_mb(rss_budget_mb, no_swap);
    if anon_mb >= emerg_line {
        return None;
    }
    let elev_line = if no_swap {
        rss_budget_mb * 65 / 100
    } else {
        rss_budget_mb * 82 / 100
    };
    Some(if anon_mb < elev_line {
        PressureLevel::None
    } else {
        PressureLevel::Critical
    })
}

/// Re-read `/proc` and step down a stale `Emergency` level left over from a prior IBD
/// attempt or validation stall.  Call **once at IBD start** — not from the coordinator loop.
/// Without this, catch-up restart can deadlock with `EMERGENCY admission pause` while anon
/// RSS is already below the Emergency entry threshold.
///
/// Prior bug: used the Critical line (92% of budget) as the exit threshold while Emergency
/// enters at 85% on no-swap hosts — so live Emergency at ~32 GiB was cleared every tick,
/// disabling coordinator backpressure and allowing anon to grow to OOM.
#[cfg(target_os = "linux")]
pub(crate) fn refresh_stale_emergency_pressure(rss_budget_mb: u64) {
    if !ibd_pressure_is_emergency() || rss_budget_mb == 0 {
        return;
    }
    let mut snap = MemorySnapshot::default();
    let mut status = String::new();
    let mut meminfo = String::new();
    if proc_read_file("/proc/self/status", &mut status) {
        proc_parse_status_into(&status, &mut snap);
    }
    if proc_read_file("/proc/meminfo", &mut meminfo) {
        proc_parse_meminfo_into(&meminfo, &mut snap);
    }
    if snap.rss_mb == 0 {
        return;
    }
    publish_ibd_rss_anon_mb(snap.rss_anon_mb);
    let r = if snap.rss_anon_mb > 0 {
        snap.rss_anon_mb
    } else {
        snap.rss_mb
    };
    let no_swap = snap.swap_total_mb == 0;
    let Some(level) = stale_emergency_step_down_level(r, rss_budget_mb, no_swap) else {
        return;
    };
    publish_ibd_pressure(level);
    let emerg_line = emergency_entry_anon_mb(rss_budget_mb, no_swap);
    tracing::info!(
        "MemoryGuard: stale Emergency stepped down to {:?} — anon RSS {}MB < emerg {}MB ({})",
        level,
        r,
        emerg_line,
        snap
    );
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn refresh_stale_emergency_pressure(_rss_budget_mb: u64) {}

#[inline]
pub(crate) fn ibd_pressure_level_snapshot() -> PressureLevel {
    PressureLevel::from_u8(IBD_PRESSURE_LEVEL.load(Ordering::Relaxed))
}

/// Concurrent UTXO flush threads allowed **right now**, derived from the RAM tier base
/// ([`MemoryGuard::max_utxo_flushes`]) and [`ibd_pressure_level_snapshot`].
///
/// Under **Critical / Emergency** we never exceed the tier base (crash-safe). Under **None** we
/// allow a bounded burst (`base + base/2`) so RocksDB can overlap writes when RSS is comfortable —
/// avoiding the old `1024` cap without pinning retire at `base` on every host. **Elevated** gets a
/// smaller bump (`base + base/4`) so download-throttle scenarios still pick up some parallelism.
#[inline]
pub(crate) fn utxo_flush_concurrency_cap(base_max_flushes: usize) -> usize {
    let base = base_max_flushes.max(1);
    match ibd_pressure_level_snapshot() {
        PressureLevel::None => {
            let bonus = (base / 2).max(1);
            (base + bonus).min(64)
        }
        PressureLevel::Elevated => {
            let bonus = (base / 4).max(1);
            (base + bonus).min(48)
        }
        PressureLevel::Critical | PressureLevel::Emergency => base,
    }
}

/// Last level from [`MemoryGuard::should_flush`] / pressure hysteresis (validation thread).
#[inline]
pub(crate) fn last_reported_pressure_level(mg: &MemoryGuard) -> PressureLevel {
    PressureLevel::from_u8(mg.last_reported_pressure.load(Ordering::Relaxed))
}

/// Historical name (TidesDB had `TDB_MAX_TXN_OPS=100000`); on RocksDB this is just an
/// upper bound on `flush_threshold` so a single retire→flush batch doesn't grow without
/// bound. Bigger batches → fewer SST flushes → less compaction → higher IBD BPS, at the
/// cost of larger pending memory + longer flush stalls when triggered. 200k × ~250 B per
/// op ≈ 50 MB peak — comfortable on 16 GB hosts.
pub(crate) const TIDESDB_MAX_TXN_OPS: usize = 200_000;

/// Shared counter: total estimated bytes of blocks held in the reorder_buffer + channels.
/// Updated by the coordinator, read by the validation loop for logging.
pub(crate) static BLOCK_BUFFER_BYTES: AtomicU64 = AtomicU64::new(0);
/// Shared counter: number of blocks in the reorder_buffer.
pub(crate) static BLOCK_BUFFER_COUNT: AtomicU64 = AtomicU64::new(0);
/// Heights buffered in OrderedReadyBridge pending BTreeMap (out-of-order prefetch completions).
pub(crate) static BRIDGE_PENDING_COUNT: AtomicU64 = AtomicU64::new(0);
/// Blocks flushed to coordinator on download chunk abort ([IBD_FLUSH_ON_ABORT]).
pub(crate) static GAP_FLUSH_ON_ABORT_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Sum of blocks buffered in per-worker download `received` maps (not yet sent to coordinator).
/// W55: jemalloc 1 MiB class grew to ~4k live objs while reorder≪1k — attribution + trim target.
pub(crate) static DOWNLOAD_RECEIVED_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Far-ahead download `received` entries dropped after GAP_PERSIST (or hard trim).
pub(crate) static DOWNLOAD_RECEIVED_TRIM_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// W27: last validation-gap height successfully streamed to coordinator (dedup multi-worker storms).
pub(crate) static GAP_STREAM_DEDUP_HEIGHT: AtomicU64 = AtomicU64::new(0);

/// Monotonic bump of [`GAP_STREAM_DEDUP_HEIGHT`] (never rewind).
///
/// RESEND used to `.store(gap)` after synth had credited a chunk end — that rewound DEDUP
/// and re-armed tip-owner for the same span (H6d live: TRIM emptied `received`, drain=0,
/// DEDUP stuck at tip → assign on every tip+1).
#[inline]
pub(crate) fn bump_gap_stream_dedup(h: u64) {
    let mut cur = GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
    while h > cur {
        match GAP_STREAM_DEDUP_HEIGHT.compare_exchange(
            cur,
            h,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
}
/// W42b: last GAP_STREAM_RESEND height + wall ms (rate-limit tip re-push storms).
pub(crate) static GAP_STREAM_LAST_RESEND_HEIGHT: AtomicU64 = AtomicU64::new(0);
pub(crate) static GAP_STREAM_LAST_RESEND_MS: AtomicU64 = AtomicU64::new(0);
/// OrderedReadyBridge `next_expected` (u64::MAX = unset). Published for [IBD_STALL] diagnostics.
pub(crate) static BRIDGE_NEXT_EXPECTED: AtomicU64 = AtomicU64::new(u64::MAX);
/// Far-ahead blocks dropped while gap missing ([IBD_GAP_ADMIT_DROP]).
pub(crate) static GAP_ADMIT_DROP_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Reorder entries evicted under S2c gap/bridge pressure ([IBD_REORDER_EVICT]).
pub(crate) static REORDER_EVICT_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Bridge pending entries evicted under GAP-8 gap pressure ([IBD_BRIDGE_EVICT]).
pub(crate) static BRIDGE_EVICT_BLOCKS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock ms at last proactive jemalloc retained purge (rate-limit ≤1/60s).
static LAST_JEMALLOC_RETAINED_PURGE_MS: AtomicU64 = AtomicU64::new(0);
const JEMALLOC_RETAINED_PURGE_MIN_INTERVAL_MS: u64 = 60_000;

/// Update shared reorder-buffer counters for [MEM_REPORT] attribution.
#[cfg(feature = "production")]
pub(crate) fn sync_reorder_buffer_stats(
    reorder_buffer: &std::collections::BTreeMap<
        u64,
        (super::types::SharedBlock, super::types::SharedWitnesses),
    >,
) {
    use super::types::estimate_block_bytes;
    let mut bytes = 0u64;
    for (_, (block, witnesses)) in reorder_buffer {
        bytes += estimate_block_bytes(block.as_ref(), witnesses.as_ref()) as u64;
    }
    BLOCK_BUFFER_COUNT.store(reorder_buffer.len() as u64, Ordering::Relaxed);
    BLOCK_BUFFER_BYTES.store(bytes, Ordering::Relaxed);
}

#[cfg(not(feature = "production"))]
pub(crate) fn sync_reorder_buffer_stats(
    _reorder_buffer: &std::collections::BTreeMap<
        u64,
        (super::types::SharedBlock, super::types::SharedWitnesses),
    >,
) {
}

/// Return jemalloc `stats.retained` in GB (0 when unavailable).
///
/// **W90:** previously `retained − allocated`. Those are different categories —
/// subtracting allocated under-reported sticky VA when allocated dipped after a
/// merge (live W89 soak: purge `per_arena_ok=129` but "excess" stuck 3–7 GB while
/// `stats.retained` alone was 5–8 GB). Threshold compares retained VA held back
/// from the OS.
///
/// Disabled under `cfg(test)`: jemalloc is not the global allocator in tests
/// (`lib.rs` gates it with `not(test)`), so linking `_rjem_mallctl` fails.
#[cfg(all(feature = "jemalloc", not(test)))]
pub(crate) fn jemalloc_retained_excess_gb() -> u64 {
    jemalloc_stats_snapshot()
        .map(|s| s.retained_gb)
        .unwrap_or(0)
}

#[cfg(all(feature = "jemalloc", not(test)))]
#[derive(Clone, Copy, Debug)]
struct JemallocStatsSnap {
    retained_gb: u64,
    retained_mb: u64,
    allocated_mb: u64,
    mapped_mb: u64,
    resident_mb: u64,
    opt_retain: bool,
    background_thread: bool,
    narenas: u32,
}

#[cfg(all(feature = "jemalloc", not(test)))]
fn jemalloc_stats_snapshot() -> Option<JemallocStatsSnap> {
    use std::os::raw::c_void;
    unsafe extern "C" {
        fn _rjem_mallctl(
            name: *const i8,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> i32;
    }
    unsafe {
        let mut sz = std::mem::size_of::<usize>();
        let epoch: usize = 1;
        let _ = _rjem_mallctl(
            b"epoch\0".as_ptr() as *const i8,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &epoch as *const usize as *mut c_void,
            sz,
        );
        let mut allocated: usize = 0;
        let _ = _rjem_mallctl(
            b"stats.allocated\0".as_ptr() as *const i8,
            &mut allocated as *mut usize as *mut c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        );
        let mut retained: usize = 0;
        let _ = _rjem_mallctl(
            b"stats.retained\0".as_ptr() as *const i8,
            &mut retained as *mut usize as *mut c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        );
        let mut mapped: usize = 0;
        let _ = _rjem_mallctl(
            b"stats.mapped\0".as_ptr() as *const i8,
            &mut mapped as *mut usize as *mut c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        );
        let mut resident: usize = 0;
        let _ = _rjem_mallctl(
            b"stats.resident\0".as_ptr() as *const i8,
            &mut resident as *mut usize as *mut c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        );
        let mut opt_retain: bool = false;
        let mut bsz = std::mem::size_of::<bool>();
        let _ = _rjem_mallctl(
            b"opt.retain\0".as_ptr() as *const i8,
            &mut opt_retain as *mut bool as *mut c_void,
            &mut bsz,
            std::ptr::null_mut(),
            0,
        );
        let mut background_thread: bool = false;
        bsz = std::mem::size_of::<bool>();
        let _ = _rjem_mallctl(
            b"background_thread\0".as_ptr() as *const i8,
            &mut background_thread as *mut bool as *mut c_void,
            &mut bsz,
            std::ptr::null_mut(),
            0,
        );
        let mut narenas: u32 = 0;
        let mut nsz = std::mem::size_of::<u32>();
        let _ = _rjem_mallctl(
            b"arenas.narenas\0".as_ptr() as *const i8,
            &mut narenas as *mut u32 as *mut c_void,
            &mut nsz,
            std::ptr::null_mut(),
            0,
        );
        Some(JemallocStatsSnap {
            retained_gb: retained as u64 / (1024 * 1024 * 1024),
            retained_mb: retained as u64 / (1024 * 1024),
            allocated_mb: allocated as u64 / (1024 * 1024),
            mapped_mb: mapped as u64 / (1024 * 1024),
            resident_mb: resident as u64 / (1024 * 1024),
            opt_retain,
            background_thread,
            narenas,
        })
    }
}

#[cfg(any(not(feature = "jemalloc"), test))]
pub(crate) fn jemalloc_retained_excess_gb() -> u64 {
    0
}

/// Purge jemalloc retained pages when `stats.retained` exceeds env threshold (default 16 GB).
/// Returns true if purge ran. Rate-limited to ≤1/60s.
#[cfg(all(feature = "jemalloc", not(test)))]
pub(crate) fn maybe_purge_jemalloc_retained(reason: &str) -> bool {
    let threshold_gb: u64 = std::env::var("BLVM_IBD_JEMALLOC_RETAINED_PURGE_GB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let before = match jemalloc_stats_snapshot() {
        Some(s) if s.retained_gb >= threshold_gb => s,
        _ => return false,
    };
    let now_ms = crate::utils::time::current_timestamp_millis();
    loop {
        let prev = LAST_JEMALLOC_RETAINED_PURGE_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < JEMALLOC_RETAINED_PURGE_MIN_INTERVAL_MS {
            return false;
        }
        if LAST_JEMALLOC_RETAINED_PURGE_MS
            .compare_exchange_weak(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    use std::os::raw::c_void;
    unsafe extern "C" {
        fn _rjem_mallctl(
            name: *const i8,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> i32;
    }
    // arena.ALL (`4294967295`) returns EINVAL (rc=2) on this tikv-jemalloc build — live
    // 2026-07-16: excess 48→48 with only decay_all/purge_all failing. Rely on per-arena
    // decay+purge + malloc_trim. Force dirty/muzzy decay_ms=0 first.
    let mut rcs: Vec<(String, i32)> = Vec::new();
    unsafe {
        let mut decay_ms: isize = 0;
        let sz = std::mem::size_of::<isize>();
        rcs.push((
            "dirty_decay".into(),
            _rjem_mallctl(
                b"arenas.dirty_decay_ms\0".as_ptr() as *const i8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut decay_ms as *mut isize as *mut c_void,
                sz,
            ),
        ));
        rcs.push((
            "muzzy_decay".into(),
            _rjem_mallctl(
                b"arenas.muzzy_decay_ms\0".as_ptr() as *const i8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut decay_ms as *mut isize as *mut c_void,
                sz,
            ),
        ));
        // Still try ALL once (some jemalloc builds accept it); ignore EINVAL.
        let all = u32::MAX;
        let decay_all = format!("arena.{all}.decay\0");
        let purge_all = format!("arena.{all}.purge\0");
        rcs.push((
            "decay_all".into(),
            _rjem_mallctl(
                decay_all.as_ptr() as *const i8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            ),
        ));
        rcs.push((
            "purge_all".into(),
            _rjem_mallctl(
                purge_all.as_ptr() as *const i8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            ),
        ));
        let mut purged_ok = 0u32;
        let mut purged_err = 0u32;
        for i in 0..before.narenas.min(256) {
            let decay = format!("arena.{i}.decay\0");
            let _ = _rjem_mallctl(
                decay.as_ptr() as *const i8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            );
            let purge = format!("arena.{i}.purge\0");
            let rc = _rjem_mallctl(
                purge.as_ptr() as *const i8,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            );
            if rc == 0 {
                purged_ok = purged_ok.saturating_add(1);
            } else {
                purged_err = purged_err.saturating_add(1);
            }
        }
        rcs.push((format!("per_arena_ok={purged_ok}"), 0));
        rcs.push((format!("per_arena_err={purged_err}"), 0));
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
    let after = jemalloc_stats_snapshot().unwrap_or(before);
    if after.retained_mb + 64 < before.retained_mb {
        tracing::info!(
            "[JEMALLOC_RETAINED_PURGE] reason={reason} retained_before_mb={} \
             retained_after_mb={} threshold_gb={threshold_gb} opt_retain={} bg_thread={} \
             narenas={} mapped_mb={}->{} resident_mb={}->{} alloc_mb={}->{} rcs={rcs:?}",
            before.retained_mb,
            after.retained_mb,
            before.opt_retain,
            before.background_thread,
            before.narenas,
            before.mapped_mb,
            after.mapped_mb,
            before.resident_mb,
            after.resident_mb,
            before.allocated_mb,
            after.allocated_mb,
        );
    } else {
        // Live W89: per_arena_ok=129 but retained unchanged — G-M1 still unmet.
        tracing::warn!(
            "[JEMALLOC_RETAINED_PURGE] NO_RECLAIM reason={reason} retained_before_mb={} \
             retained_after_mb={} threshold_gb={threshold_gb} opt_retain={} bg_thread={} \
             narenas={} mapped_mb={} resident_mb={} alloc_mb={} rcs={rcs:?} \
             (if opt_retain=true, MALLOC_CONF retain:false did not apply)",
            before.retained_mb,
            after.retained_mb,
            before.opt_retain,
            before.background_thread,
            before.narenas,
            before.mapped_mb,
            before.resident_mb,
            before.allocated_mb,
        );
    }
    true
}

#[cfg(any(not(feature = "jemalloc"), test))]
pub(crate) fn maybe_purge_jemalloc_retained(_reason: &str) -> bool {
    false
}

/// Wall-clock ms at last chain `data.mdb` keep-tail madvise.
static LAST_MDB_KEEP_TAIL_MADVISE_MS: AtomicU64 = AtomicU64::new(0);
const MDB_KEEP_TAIL_MADVISE_MIN_INTERVAL_MS: u64 = 60_000;

/// When file-backed RSS is high, `madvise(MADV_DONTNEED)` cold prefixes of every
/// `data.mdb` mapping, keeping the last `BLVM_IBD_MDB_KEEP_TAIL_GB` (default **4**)
/// of each mapping hot for tip writes.
///
/// Evidence 2026-07-15: cgroup `inactive_file` ~60 GiB + `file_mapped` ~27 GiB pushed
/// `MemoryCurrent` to ~72 GiB under `MemoryHigh` → reclaim thrash → Cursor SEGV.
/// External `process_madvise` is EPERM; in-process madvise works. `posix_fadvise` is
/// ignored for live mmap pages (see heed3_impl).
///
/// Live 2026-07-15 10:50: `MADVISE_KEEP_TAIL` count=0 — default threshold **12 GiB**
/// skipped while process `file_backed=8495MB`, and default keep_tail **24 GiB** would
/// no-op on an 8.5 GiB mapping (`len <= keep`). Defaults: threshold **4 GiB**, keep **4 GiB**.
#[cfg(all(target_os = "linux", feature = "libc"))]
pub(crate) fn maybe_madvise_data_mdb_keep_tail(file_backed_mb: u64, reason: &str) -> bool {
    let threshold_mb: u64 = std::env::var("BLVM_IBD_MDB_FILE_RSS_MADVISE_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 * 1024);
    if file_backed_mb < threshold_mb {
        return false;
    }
    let now_ms = crate::utils::time::current_timestamp_millis();
    loop {
        let prev = LAST_MDB_KEEP_TAIL_MADVISE_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < MDB_KEEP_TAIL_MADVISE_MIN_INTERVAL_MS {
            return false;
        }
        if LAST_MDB_KEEP_TAIL_MADVISE_MS
            .compare_exchange_weak(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    let keep_gb: u64 = std::env::var("BLVM_IBD_MDB_KEEP_TAIL_GB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let keep_bytes = (keep_gb.max(1) as usize).saturating_mul(1024 * 1024 * 1024);
    // Live 2026-07-15 ~h670k: VMA is the **whole file** (~451 GiB). Advising len−keep
    // (≈447 GiB) every MEM_REPORT caused UTXO fault storms (~11 blk/s). Cap DONTNEED to
    // excess file RSS and a per-cycle max (default **8 GiB**).
    // Live 2026-07-16: skipping huge VMAs entirely (`skipped_huge_vma=1`) left
    // file_backed growing to **45 GiB** → MemoryHigh thrash (high≈15k) → WAN ~8 blk/s.
    // Always include the huge VMA; budget caps how much prefix we drop.
    let keep_mb = keep_gb.saturating_mul(1024);
    let cycle_cap_gb: usize = std::env::var("BLVM_IBD_MDB_MADVISE_MAX_GB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
        .clamp(1, 64);
    let max_advise_bytes = (file_backed_mb.saturating_sub(keep_mb) as usize)
        .saturating_mul(1024 * 1024)
        .min(cycle_cap_gb.saturating_mul(1024 * 1024 * 1024));
    if max_advise_bytes == 0 {
        return false;
    }
    let Ok(maps) = std::fs::File::open("/proc/self/maps") else {
        return false;
    };
    use std::io::BufRead;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for line in std::io::BufReader::new(maps).lines().flatten() {
        if !line.contains("data.mdb") {
            continue;
        }
        let Some(range) = line.split_whitespace().next() else {
            continue;
        };
        let mut parts = range.splitn(2, '-');
        let (Some(s), Some(e)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            usize::from_str_radix(s, 16),
            usize::from_str_radix(e, 16),
        ) else {
            continue;
        };
        if end > start {
            ranges.push((start, end));
        }
    }
    if ranges.is_empty() {
        return false;
    }
    let rss_before = {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0)
            / 1024
    };
    let mut advised_bytes: u64 = 0;
    let mut ranges_touched = 0usize;
    let mut budget = max_advise_bytes;
    for (start, end) in ranges {
        if budget == 0 {
            break;
        }
        let len = end.saturating_sub(start);
        if len <= keep_bytes {
            continue;
        }
        let drop_len = (len - keep_bytes).min(budget);
        unsafe {
            libc::madvise(start as *mut libc::c_void, drop_len, libc::MADV_DONTNEED);
        }
        advised_bytes += drop_len as u64;
        budget = budget.saturating_sub(drop_len);
        ranges_touched += 1;
    }
    let rss_after = {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0)
            / 1024
    };
    tracing::info!(
        "[MADVISE_KEEP_TAIL] reason={reason} file_backed_mb={file_backed_mb} \
         threshold_mb={threshold_mb} keep_gb={keep_gb} cycle_cap_gb={cycle_cap_gb} \
         ranges={ranges_touched} advised_gb={:.1} rss {}MB → {}MB",
        advised_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        rss_before,
        rss_after
    );
    ranges_touched > 0
}

#[cfg(not(all(target_os = "linux", feature = "libc")))]
pub(crate) fn maybe_madvise_data_mdb_keep_tail(_file_backed_mb: u64, _reason: &str) -> bool {
    false
}

#[derive(Default, Clone, Copy)]
pub(crate) struct MemorySnapshot {
    pub rss_mb: u64,
    pub rss_anon_mb: u64,
    pub rss_file_mb: u64,
    pub rss_shmem_mb: u64,
    pub vm_size_mb: u64,
    /// `MemTotal` from `/proc/meminfo` (Linux); 0 if unknown.
    pub mem_total_mb: u64,
    pub sys_avail_mb: u64,
    /// `SwapTotal` from `/proc/meminfo` (Linux); 0 if no swap or unknown.
    pub swap_total_mb: u64,
    /// `SwapFree` from `/proc/meminfo` (Linux); 0 if no swap or unknown.
    pub swap_free_mb: u64,
    /// `VmSwap` from `/proc/self/status` (Linux): bytes of THIS PROCESS that are swapped out.
    /// More accurate than `swap_used_mb() > rss_mb / 4` for detecting our own swap pressure:
    /// the system-wide swap may include leftover pages from a previous OOM-killed process.
    pub vm_swap_mb: u64,
}

impl MemorySnapshot {
    /// Bytes of swap actually consumed (anonymous pages paged out by kernel).
    /// 0 when no swap is configured. Heavy swap usage means the kernel is
    /// thrashing — every "in-RAM" cache hit may actually be a disk read.
    #[inline]
    pub fn swap_used_mb(&self) -> u64 {
        self.swap_total_mb.saturating_sub(self.swap_free_mb)
    }
}

impl std::fmt::Display for MemorySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rss={}MB(anon={}MB file={}MB shm={}MB) vm={}MB mem_total={}MB sys_avail={}MB swap_used={}MB proc_swap={}MB",
            self.rss_mb,
            self.rss_anon_mb,
            self.rss_file_mb,
            self.rss_shmem_mb,
            self.vm_size_mb,
            self.mem_total_mb,
            self.sys_avail_mb,
            self.swap_used_mb(),
            self.vm_swap_mb,
        )
    }
}

#[cfg(target_os = "linux")]
#[inline]
fn proc_field_kb_to_mb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        / 1024
}

#[cfg(target_os = "linux")]
fn proc_read_file(path: &str, buf: &mut String) -> bool {
    buf.clear();
    match std::fs::File::open(path) {
        Ok(mut f) => f.read_to_string(buf).is_ok(),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn proc_parse_status_into(s: &str, snap: &mut MemorySnapshot) {
    for line in s.lines() {
        if line.starts_with("VmRSS:") {
            snap.rss_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("RssAnon:") {
            snap.rss_anon_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("RssFile:") {
            snap.rss_file_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("RssShmem:") {
            snap.rss_shmem_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("VmSize:") {
            snap.vm_size_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("VmSwap:") {
            snap.vm_swap_mb = proc_field_kb_to_mb(line);
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_parse_meminfo_into(s: &str, snap: &mut MemorySnapshot) {
    for line in s.lines() {
        if line.starts_with("MemTotal:") {
            snap.mem_total_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("MemAvailable:") {
            snap.sys_avail_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("SwapTotal:") {
            snap.swap_total_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("SwapFree:") {
            snap.swap_free_mb = proc_field_kb_to_mb(line);
        }
    }
}

#[cfg(target_os = "linux")]
/// Return anonymous RSS (`RssAnon`) from `/proc/self/status`, falling back to `VmRSS`
/// when `RssAnon` is unavailable (Linux < 4.5 or non-Linux).  Callers that budget against
/// anonymous memory (UTXO cache, pending ops) should use this rather than raw `VmRSS` so
/// that file-backed LMDB mmap pages do not trigger spurious pressure escalations.
/// Read the process's anonymous RSS in MiB directly from `/proc/self/status`.
///
/// Uses `RssAnon` (anonymous pages only, excludes file-backed LMDB mmap) if available;
/// falls back to `VmRSS` on older kernels. Returns 0 on non-Linux.
#[cfg(target_os = "linux")]
pub(crate) fn read_proc_anon_rss_mb() -> u64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        proc_anon_rss_mb_from_status(&s)
    } else {
        0
    }
}

/// Read (anon_rss_mb, vm_swap_mb) from /proc/self/status.
///
/// `anon_rss_mb` = anonymous pages in RAM (RssAnon, or VmRSS fallback).
/// `vm_swap_mb`  = anonymous pages the kernel has pushed to swap (VmSwap).
/// Their sum is the process's **total committed anonymous footprint** regardless
/// of whether pages are resident or swapped. This is the right metric for an
/// OOM gate on no-swap hosts: even if the process's in-RAM RSS looks healthy,
/// heavy swapping means the kernel is already under fatal memory pressure.
#[cfg(target_os = "linux")]
pub(crate) fn read_proc_anon_and_swap_mb() -> (u64, u64) {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        let anon = proc_anon_rss_mb_from_status(&s);
        let swap = proc_vm_swap_mb_from_status(&s);
        (anon, swap)
    } else {
        (0, 0)
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_proc_anon_rss_mb() -> u64 {
    0
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_proc_anon_and_swap_mb() -> (u64, u64) {
    (0, 0)
}

#[cfg(target_os = "linux")]
fn proc_anon_rss_mb_from_status(s: &str) -> u64 {
    let mut rss_mb: u64 = 0;
    let mut anon_mb: u64 = 0;
    for line in s.lines() {
        if line.starts_with("VmRSS:") {
            rss_mb = proc_field_kb_to_mb(line);
        } else if line.starts_with("RssAnon:") {
            anon_mb = proc_field_kb_to_mb(line);
        }
    }
    if anon_mb > 0 { anon_mb } else { rss_mb }
}

#[cfg(target_os = "linux")]
fn proc_vm_swap_mb_from_status(s: &str) -> u64 {
    for line in s.lines() {
        if line.starts_with("VmSwap:") {
            return proc_field_kb_to_mb(line);
        }
    }
    0
}

#[cfg(target_os = "linux")]
fn proc_rss_mb_from_status(s: &str) -> u64 {
    for line in s.lines() {
        if line.starts_with("VmRSS:") {
            return proc_field_kb_to_mb(line);
        }
    }
    0
}

/// Cross-platform auto-tuning for IBD memory management.
///
// Probes total/available RAM at startup via sysinfo (Linux, macOS, Windows).
/// Derives budgets from hardware. During IBD the validation loop calls
/// `should_flush()` with live `/proc` snapshots; under memory pressure we force
/// UTXO flush and (via `max_ahead_live`) shrink download-ahead automatically.
pub(crate) struct MemoryGuard {
    /// Total physical RAM in MiB (from MemTotal in /proc/meminfo).
    pub(crate) total_mb: u64,
    /// `MemAvailable` (MiB) at the time `MemoryGuard::new()` ran (boot probe, after env overrides).
    /// Shared with subsystems (e.g. UTXO index eviction age) that need the same hardware snapshot
    /// without re-reading `/proc/meminfo`.
    pub(crate) avail_mb: u64,
    budget_mb: u64,
    /// Derived UTXO cache max in MB (50% of budget).
    utxo_cache_mb: usize,
    /// Nominal UTXO cache cap (entries) at startup. The runtime cap can be shrunk below this
    /// by `compute_adaptive_cache_cap` when actual RSS approaches `rss_budget_mb`, and grown
    /// back up to it when RSS retreats. The static `utxo_cache_mb` derivation is now a *baseline*,
    /// not a hard ceiling; the binary self-adapts to whatever else lives on the host.
    pub(crate) utxo_max_entries: usize,
    /// Hard upper bound on our process RSS in MiB (≈50% of total RAM on default-sized hosts).
    /// When `rss_mb` approaches this number we shrink the UTXO cache automatically — covers
    /// mimalloc fragmentation, RocksDB block cache growth, transient flush buffers, etc.
    /// without requiring a manual env-var retune per host.
    pub(crate) rss_budget_mb: u64,
    /// Last cache cap installed by `compute_adaptive_cache_cap`. Tracked separately from
    /// `utxo_max_entries` so successive callers can converge toward the target without
    /// thrashing on a noisy RSS reading. `0` until the first adaptation runs.
    last_adaptive_cap_entries: AtomicUsize,
    /// Last time we evaluated the adaptive cap. Throttle: at most one adaptation per ~2 s.
    last_adaptive_cap_check: Mutex<Instant>,
    /// Last time we *shrank* the adaptive cap. Used for shrink-cooldown: after a shrink we
    /// wait at least SHRINK_COOLDOWN_SECS before cutting again, giving mimalloc time to
    /// actually return freed pages to the OS and letting RSS stabilise.
    last_adaptive_cap_shrink: Mutex<Instant>,
    /// Number of consecutive `compute_adaptive_cache_cap` polls that saw RSS above the shrink
    /// threshold. We require at least 2 consecutive high-RSS polls before cutting — this
    /// filters out single-sample transient spikes (RocksDB compaction burst, etc.) that
    /// would otherwise trigger an unnecessary shrink.
    above_threshold_consecutive: AtomicU8,
    /// UTXO flush threshold (entries in pending_writes before auto-flush).
    pub(crate) utxo_flush_threshold: usize,
    /// Block buffer limit (blocks in reorder buffer).
    block_buffer_base: usize,
    /// Storage flush interval (blocks between storage flushes).
    pub(crate) storage_flush_interval: usize,
    /// Prefetch cache limit.
    prefetch_limit: usize,
    /// Max items in prefetch channels.
    pub(crate) prefetch_queue_size: usize,
    /// Max blocks download can race ahead of validation.
    pub(crate) max_ahead_blocks: u64,
    /// Defer UTXO flush to checkpoints when RAM is sufficient.
    pub defer_flush: bool,
    /// Checkpoint interval for deferred flushes (blocks).
    pub defer_checkpoint_interval: u64,
    /// Feeder buffer byte cap (alongside count cap).
    pub feeder_buffer_bytes_limit: usize,
    /// Max concurrent UTXO flush threads (replaces old hardcoded 1024).
    pub max_utxo_flushes: usize,
    /// Max concurrent block-storage flush threads.
    pub max_block_flushes: usize,
    /// Chain storage backend at IBD boot — drives pipeline reserve and block flush batching.
    storage_backend: crate::storage::database::DatabaseBackend,
    /// No swap at IBD boot — tighten anon-RSS pressure thresholds (global OOM with no swap reservoir).
    no_swap_at_boot: bool,
    /// Live spec_adds memory usage (bytes). Updated by the coordinator when blocks enter/leave
    /// the spec_adds window. `should_flush` subtracts this from sys_avail_mb so that a large
    /// speculative UTXO window at late heights (h=700k+, ~640 KB/block × 358 ahead = ~229 MB)
    /// is correctly reflected in pressure and `adjust_max_ahead_live`.
    pub spec_adds_bytes: Arc<AtomicU64>,
    #[cfg(feature = "sysinfo")]
    sys: sysinfo::System,
    last_rss_check: Instant,
    last_ahead_adjust: Instant,
    /// Last [`PressureLevel`] we logged (`repr(u8)`). Used to emit a single line on any transition.
    last_reported_pressure: AtomicU8,
    /// <=16 GiB hosts: RSS (MiB) at which we enter `Critical` unless hysteresis holds. Override: `BLVM_IBD_PRESSURE_CRIT_RSS_MB` (800–4000).
    crit_rss_threshold_mb: u64,
    /// Reused buffers for Linux `/proc` reads (avoids allocating two `String`s every `should_flush` poll).
    #[cfg(target_os = "linux")]
    proc_status_buf: String,
    #[cfg(target_os = "linux")]
    proc_meminfo_buf: String,
}

/// Scalars for the feeder thread to recompute buffer / byte caps from live validation height.
#[derive(Clone, Copy)]
pub(crate) struct FeederScaleSnapshot {
    pub block_buffer_base: usize,
    pub total_mb: u64,
    pub feeder_buffer_bytes_limit: usize,
}

/// Host workload at IBD boot — drives RSS envelope caps, not throughput knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkloadClass {
    /// Desktop / IDE / other services already consuming RAM (`MemAvailable` ≪ `MemTotal`).
    Shared,
    /// Headless or lightly loaded node (`MemAvailable` ≈ `MemTotal`). Override: `BLVM_DEDICATED_NODE=1`.
    Dedicated,
}

/// Flat RocksDB block-cache + WBM headroom subtracted from the pipeline slice of the envelope.
pub(crate) const ROCKSDB_PIPELINE_RESERVE_MB: u64 = 2048;

/// Boot-time inputs for IBD memory auto-tuning (backend + workload class).
#[derive(Debug, Clone, Copy)]
pub(crate) struct IbdTuningContext {
    pub storage_backend: crate::storage::database::DatabaseBackend,
    /// From `[ibd] dedicated`, `BLVM_DEDICATED_NODE`, or `BLVM_IBD_EXCLUSIVE`.
    pub ibd_dedicated: bool,
    /// Estimated MiB of file-backed DB pages that will be resident in our RSS.
    /// For Heed3: derived from `data.mdb` file size at IBD start. These pages cannot
    /// be freed by UTXO cache eviction, so they must be subtracted from the RSS budget
    /// to leave room for the anonymous working set.
    pub db_file_size_mb: u64,
}

impl Default for IbdTuningContext {
    fn default() -> Self {
        Self {
            storage_backend: crate::storage::database::default_backend(),
            ibd_dedicated: false,
            db_file_size_mb: 0,
        }
    }
}

impl MemoryGuard {
    /// Laptops marketed as “16 GiB” often report ~17 GiB `MemTotal`; keep one MB cutoff so they
    /// stay on tight tiers (OOM fixes) instead of the 17–31 GiB workstation path.
    pub(crate) const EXTENDED_SIXTEEN_CLASS_MB: u64 = 18 * 1024;

    /// GiB tier label from total RAM (MiB): `(total_mb + 512) / 1024`. Delegates to
    /// [`crate::utils::ram_tier`] (same formula as RocksDB tier sizing).
    #[inline]
    pub(crate) fn total_gb_rounded(total_mb: u64) -> u64 {
        crate::utils::ram_tier::total_gb_rounded(total_mb)
    }

    /// Classify shared vs dedicated from boot-time `/proc` snapshot.
    pub(crate) fn detect_workload_class(
        total_mb: u64,
        avail_mb: u64,
        ibd_dedicated: bool,
    ) -> WorkloadClass {
        let exclusive = std::env::var("BLVM_IBD_EXCLUSIVE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if exclusive {
            return WorkloadClass::Dedicated;
        }
        let avail_pct = if total_mb > 0 {
            avail_mb.saturating_mul(100) / total_mb
        } else {
            100
        };
        let wants_dedicated = ibd_dedicated
            || std::env::var("BLVM_DEDICATED_NODE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        if wants_dedicated {
            // `[ibd] dedicated = true` in the example config must not OOM shared workstations.
            // Only treat as Dedicated when most RAM is actually free at boot.
            const DEDICATED_MIN_AVAIL_PCT: u64 = 70;
            if avail_pct < DEDICATED_MIN_AVAIL_PCT {
                tracing::warn!(
                    "MemoryGuard: dedicated mode requested but MemAvailable is only {avail_pct}% \
                     of MemTotal — using Shared memory envelope (set BLVM_IBD_EXCLUSIVE=1 to force Dedicated)"
                );
                return WorkloadClass::Shared;
            }
            return WorkloadClass::Dedicated;
        }
        // On large-RAM machines (32 GiB+) blvm is typically the dominant workload.
        // MemAvailable includes reclaimable file-backed LMDB pages which fluctuate
        // as the kernel pages in/out the memory-mapped DB — lower the threshold so a
        // temporary drop from 75% → 62% doesn't misclassify a dedicated box as Shared.
        let dedicated_threshold_pct: u64 = if total_mb >= 32 * 1024 { 60 } else { 70 };
        if total_mb == 0 || avail_pct >= dedicated_threshold_pct {
            WorkloadClass::Dedicated
        } else {
            WorkloadClass::Shared
        }
    }

    /// RAM headroom reserved inside the RSS envelope for storage-engine pipeline buffers.
    /// RocksDB block cache + WBM dominate; heed3/LMDB uses mmap and smaller txn buffers.
    pub(crate) fn pipeline_reserve_mb(
        backend: crate::storage::database::DatabaseBackend,
    ) -> u64 {
        use crate::storage::database::DatabaseBackend;
        match backend {
            DatabaseBackend::RocksDB => ROCKSDB_PIPELINE_RESERVE_MB,
            DatabaseBackend::Heed3 => 512,
            DatabaseBackend::TidesDB => 1024,
            DatabaseBackend::Redb | DatabaseBackend::Sled => 768,
        }
    }

    /// Blockstore batch flush interval (blocks) before async write — backend-specific.
    /// RocksDB amortizes L0 SST creation with large batches; heed3 LMDB transactions stall
    /// the orchestrator when batches are huge (see `do_flush_to_storage` backpressure).
    pub(crate) fn storage_flush_interval_base(
        total_gb: u64,
        backend: crate::storage::database::DatabaseBackend,
    ) -> usize {
        use crate::storage::database::DatabaseBackend;
        match backend {
            DatabaseBackend::Heed3 => {
                // LMDB is single-writer: max_block_flushes=1, so join frequency == flush
                // frequency regardless of interval size. The original 800-block interval
                // was chosen to reduce join frequency, but with one flush slot it only
                // increases per-flush latency: 800 blocks × ~300 KB/block = ~240 MB per
                // write transaction into a 300+ GB B-tree, taking 16–57 s at h>300k.
                // During those stalls the retire loop is frozen, durability sends back up
                // for all 32 shards, and channel-full warnings flood the log.
                //
                // 200 blocks × ~300 KB = ~60 MB per transaction → ~10–15 s per flush.
                // Same join frequency, 4× shorter stalls, 4× less channel backpressure.
                // Override via BLVM_IBD_STORAGE_FLUSH_INTERVAL.
                if total_gb >= 16 {
                    200
                } else {
                    100
                }
            }
            DatabaseBackend::RocksDB | DatabaseBackend::TidesDB => {
                if total_gb >= 32 {
                    2000
                } else {
                    300
                }
            }
            DatabaseBackend::Redb | DatabaseBackend::Sled => {
                if total_gb >= 32 {
                    512
                } else {
                    300
                }
            }
        }
    }

    /// Process RSS envelope (MiB). Spare-first on large hosts; legacy clamps preserved on ≤18 GiB.
    pub(crate) fn compute_rss_budget_mb(
        total_mb: u64,
        avail_mb: u64,
        workload: WorkloadClass,
    ) -> u64 {
        if let Some(v) = std::env::var("BLVM_RSS_BUDGET_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n >= 1024)
        {
            return v;
        }
        Self::compute_rss_budget_mb_auto(total_mb, avail_mb, workload)
    }

    /// RSS envelope without `BLVM_RSS_BUDGET_MB` override (for boot diagnostics).
    pub(crate) fn compute_rss_budget_mb_auto(
        total_mb: u64,
        avail_mb: u64,
        workload: WorkloadClass,
    ) -> u64 {
        if total_mb <= 8 * 1024 {
            let from_total = total_mb * 65 / 100;
            let from_avail = avail_mb * 75 / 100;
            return from_total
                .max(from_avail)
                .min(total_mb * 80 / 100)
                .max(2048);
        }
        if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            let from_avail = (avail_mb * 60 / 100).clamp(3000, 7000);
            return from_avail.max(2048);
        }
        // **Dedicated machines** use total RAM as the reference instead of available RAM.
        //
        // Using `avail_mb` for Dedicated creates a negative feedback loop: as blvm consumes
        // RAM, `avail_mb` falls, `from_spare` falls, `rss_budget` falls — Emergency fires
        // earlier on each run, UTXO cache shrinks, BPS collapses. On a machine where blvm
        // owns the RAM, the budget should be a fixed fraction of total RAM, not a moving
        // window that shrinks as the process grows.
        //
        // Shared machines still use `avail_mb` so blvm backs off when heavy co-tenants
        // (LLM inference, IDE) are consuming large amounts of RAM.
        //
        // For large Shared machines (92 GiB Zeus-class) the old 22% OS reserve + 35% cap
        // produced a 31.7 GiB budget that — combined with 33 GiB for vLLM + OS — left only
        // 0.2 GiB of swap headroom before OOM. Larger reserve (25%) + tighter cap (25% vs 35%)
        // keeps blvm well clear of the physical RAM wall even with heavy co-tenants.
        if workload == WorkloadClass::Dedicated {
            // Static budget: total_mb × cap_pct, capped by spare RAM at boot so co-tenants
            // (IDE, LLM) cannot be starved even on "Dedicated" class machines.
            let cap_pct: u64 = if total_mb >= 64 * 1024 {
                50
            } else if total_mb >= 32 * 1024 {
                45
            } else {
                40
            };
            let from_total = total_mb * cap_pct / 100;
            let os_reserve = if total_mb >= 64 * 1024 {
                8192
            } else {
                (total_mb * 10 / 100).max(4096)
            };
            let from_avail = avail_mb.saturating_sub(os_reserve);
            return from_total.min(from_avail.max(2048)).max(2048);
        }
        let os_reserve_pct: u64 = if total_mb >= 32 * 1024 {
            25
        } else {
            22
        };
        let os_reserve = (total_mb * os_reserve_pct / 100).max(2816);
        let from_spare = avail_mb.saturating_sub(os_reserve);
        let cap_pct = match workload {
            // 25% on large Shared machines: leaves room for co-tenants (LLM, IDE, etc.).
            // 35% on smaller machines where blvm is likely the dominant workload.
            WorkloadClass::Shared => {
                if total_mb >= 32 * 1024 { 25 } else { 35 }
            }
            WorkloadClass::Dedicated => unreachable!("handled above"),
        };
        from_spare.min(total_mb * cap_pct / 100).max(2048)
    }

    /// Nominal pending-ops cap — **always > 0**. Pipeline throttle, not optional on large hosts.
    pub(crate) fn nominal_max_pending_ops(
        total_mb: u64,
        rss_budget_mb: u64,
        utxo_cache_mb: usize,
        utxo_flush_threshold: usize,
        storage_backend: crate::storage::database::DatabaseBackend,
    ) -> usize {
        if let Some(v) = std::env::var("BLVM_IBD_MAX_PENDING_OPS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            return v.max(100_000);
        }
        const BYTES_PER_OP: usize = 160;
        const PIPELINE_FRAC_PCT: usize = 6;
        let total_gb = Self::total_gb_rounded(total_mb);
        let tier_ceiling = if total_gb >= 64 {
            8_000_000
        } else if total_gb >= 32 {
            5_000_000
        } else if total_gb >= 24 {
            3_000_000
        } else if total_gb >= 16 {
            1_500_000
        } else {
            1_000_000
        };
        let pipeline_mb = rss_budget_mb
            .saturating_sub(utxo_cache_mb as u64)
            .saturating_sub(Self::pipeline_reserve_mb(storage_backend));
        let from_envelope =
            pipeline_mb as usize * 1024 * 1024 * PIPELINE_FRAC_PCT / 100 / BYTES_PER_OP;
        let floor = 400_000_usize.max(utxo_flush_threshold.saturating_mul(4));
        from_envelope.clamp(floor, tier_ceiling)
    }

    /// Nominal cap from this guard's boot-time sizing.
    pub(crate) fn nominal_max_pending_ops_for_guard(&self) -> usize {
        Self::nominal_max_pending_ops(
            self.total_mb,
            self.rss_budget_mb,
            self.utxo_cache_mb,
            self.utxo_flush_threshold,
            self.storage_backend,
        )
    }

    pub(crate) fn new() -> Self {
        Self::new_for_ibd(IbdTuningContext::default())
    }

    pub(crate) fn new_for_ibd(ctx: IbdTuningContext) -> Self {
        // Prefer /proc/meminfo on Linux — works regardless of feature flags.
        // This prevents the sysinfo-disabled fallback (8192/6144) from starving the UTXO cache
        // when built with --no-default-features.
        #[cfg(target_os = "linux")]
        let (mut total_mb, mut available_mb, startup_swap_total_mb, startup_swap_free_mb) = {
            let mut total = 0u64;
            let mut avail = 0u64;
            let mut swap_total = 0u64;
            let mut swap_free = 0u64;
            if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
                for line in s.lines() {
                    if line.starts_with("MemTotal:") {
                        total = proc_field_kb_to_mb(line);
                    } else if line.starts_with("MemAvailable:") {
                        avail = proc_field_kb_to_mb(line);
                    } else if line.starts_with("SwapTotal:") {
                        swap_total = proc_field_kb_to_mb(line);
                    } else if line.starts_with("SwapFree:") {
                        swap_free = proc_field_kb_to_mb(line);
                    }
                }
            }
            (total, avail, swap_total, swap_free)
        };
        #[cfg(not(target_os = "linux"))]
        let (mut total_mb, mut available_mb, startup_swap_total_mb, startup_swap_free_mb) =
            (0u64, 0u64, 0u64, 0u64);

        // Supplement with sysinfo on non-Linux or if /proc gave nothing.
        #[cfg(feature = "sysinfo")]
        let mut sys = {
            use sysinfo::System;
            let mut s = System::new_all();
            s.refresh_memory();
            if total_mb == 0 {
                total_mb = s.total_memory() / (1024 * 1024);
            }
            if available_mb == 0 {
                available_mb = s.available_memory() / (1024 * 1024);
            }
            s
        };

        // Optional BLVM_* env overrides for A/B testing or constrained environments.
        if let Some(mb) = std::env::var("BLVM_TOTAL_RAM_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&v| v > 0)
        {
            total_mb = mb;
        }
        if let Some(mb) = std::env::var("BLVM_SYS_AVAIL_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&v| v > 0)
        {
            available_mb = mb;
        }

        // Final fallback totals — should only trigger on non-Linux without sysinfo.
        if total_mb == 0 {
            total_mb = 8192;
        }
        if available_mb == 0 {
            available_mb = (total_mb * 60 / 100).max(2048);
        }

        let total_gb = Self::total_gb_rounded(total_mb);

        // Budget: fraction of total RAM.
        // On <=16 GiB use 15% — enough for a ~1 GB UTXO in-memory cache on 16 GiB without
        // OOM risk (leaves 13+ GiB for OS, RocksDB, network, etc.).
        let mut budget_mb = if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            (total_mb * 15 / 100).clamp(512, 2500)
        } else {
            (total_mb * 28 / 100).min(available_mb * 45 / 100).max(512)
        };

        // Spare: how much room we have for pipeline depth. On <=16 GB, cap to 15%
        // of total regardless of what MemAvailable says at boot.
        let effective_avail = if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            available_mb.min(total_mb * 40 / 100)
        } else {
            available_mb
        };
        let os_reserve_mb = (total_mb * 22 / 100).max(2816);
        let spare_mb = effective_avail.saturating_sub(os_reserve_mb).max(256);

        // If swap is nearly full at startup, don't trust MemAvailable as a co-tenant signal.
        // A full swap means the kernel is already under pressure — the "missing" available RAM is
        // from memory pressure / restart transient page-cache retention, not real competing workloads.
        // Treat as Dedicated so the UTXO cache isn't capped at 4 GiB (which would then surge to
        // 16 GiB when the next segment starts and detects Dedicated — causing an anon RSS spike).
        let swap_nearly_full = startup_swap_total_mb > 0
            && startup_swap_free_mb * 100 / startup_swap_total_mb.max(1) < 15; // <15% swap free

        // Workload class latch: determine the class once per process, then lock it in.
        // Each catch-up cycle calls new_for_ibd again; without the latch, transient dips in
        // MemAvailable (LMDB page-cache eviction, network buffer spikes) flip the class between
        // Dedicated (16 GB UTXO cache) and Shared (4 GB), causing 12 GB surges that OOM-kill.
        let latched = WORKLOAD_CLASS_LATCH.load(Ordering::Relaxed);
        let workload = if latched != 0xff {
            // Already determined in this process — reuse it regardless of current MemAvailable.
            let wc = if latched == 1 {
                WorkloadClass::Dedicated
            } else {
                WorkloadClass::Shared
            };
            tracing::debug!(
                "MemoryGuard: reusing latched workload={:?} (available={}MB; latch prevents oscillation)",
                wc, available_mb
            );
            wc
        } else {
            // First call in this process: determine from current conditions.
            let wc = if swap_nearly_full && !ctx.ibd_dedicated {
                tracing::warn!(
                    "MemoryGuard: swap is >85% full at startup ({}/{} MB free) — \
                     overriding workload to Dedicated to avoid cache surge on restart",
                    startup_swap_free_mb, startup_swap_total_mb
                );
                WorkloadClass::Dedicated
            } else {
                Self::detect_workload_class(total_mb, available_mb, ctx.ibd_dedicated)
            };
            // Store in latch so subsequent catch-up cycles reuse this class.
            WORKLOAD_CLASS_LATCH.store(if wc == WorkloadClass::Dedicated { 1 } else { 0 }, Ordering::Relaxed);
            tracing::info!(
                "MemoryGuard: workload class latched as {:?} for this process (available={}MB, swap={}/{}MB free)",
                wc, available_mb, startup_swap_free_mb, startup_swap_total_mb
            );
            wc
        };
        let auto_rss_budget_mb =
            Self::compute_rss_budget_mb_auto(total_mb, available_mb, workload);
        let mut rss_budget_mb_raw = Self::compute_rss_budget_mb(total_mb, available_mb, workload);

        // No swap: kernel cannot page out under pressure — cap envelope to ~42% of MemAvailable
        // so co-tenants + blvm stay within physical RAM (zeus OOM at 31 GiB anon with 61 GiB budget).
        if startup_swap_total_mb == 0 && total_mb >= 32 * 1024 {
            let os_reserve = if total_mb >= 64 * 1024 {
                8192
            } else {
                (total_mb * 10 / 100).max(4096)
            };
            let no_swap_cap = available_mb
                .saturating_sub(os_reserve)
                .min(total_mb * 40 / 100);
            if rss_budget_mb_raw > no_swap_cap && no_swap_cap >= 2048 {
                tracing::info!(
                    "MemoryGuard: no swap configured — capping rss_budget {} -> {} MB \
                     (MemAvailable={}MB)",
                    rss_budget_mb_raw,
                    no_swap_cap,
                    available_mb
                );
                rss_budget_mb_raw = no_swap_cap;
            }
        }

        // Do NOT deduct LMDB file-backed pages from the budget here.  Previously we subtracted
        // 75% of db_file_size_mb from rss_budget_mb to "account" for LMDB pages in VmRSS, but
        // that was conceptually backwards: it made the budget smaller as the database grew,
        // eventually collapsing it below the actual RssFile usage and triggering permanent
        // Emergency even when anonymous RSS (UTXO cache + pending ops) was tiny.
        //
        // The correct fix lives in clamp_pressure_to_process_budget: we now compare RssAnon
        // (anonymous-only RSS from /proc/self/status) against the budget, completely ignoring
        // file-backed mmap pages.  Those pages are kernel-evictable and already accounted for
        // in the system-level MemAvailable estimate, so they pose no OOM risk and need not
        // shrink our anonymous memory budget.
        let rss_budget_mb = rss_budget_mb_raw.max(2048);

        // UTXO cache: dominant post-200k BPS factor. Sized from the RSS **envelope**, not raw
        // MemAvailable×60% (which assumed blvm owns the machine — caused 16 GiB cache + 66 GiB
        // budget on a shared 92 GiB workstation; see docs/IBD_MEMORY_ENVELOPE_FIX.md).
        let envelope_cache_cap_mb = (rss_budget_mb * 45 / 100) as usize;
        let mut utxo_cache_mb = if total_gb >= 32 {
            // Shared tier_max reduced 8192 → 4096 MiB: on a 92 GiB shared machine the
            // 8 GiB UTXO cache plus LMDB file-backed pages + RocksDB easily hit the
            // rss_budget_mb boundary, leaving nothing for Emergency to evict.
            let tier_max = match workload {
                WorkloadClass::Shared => 4096,
                WorkloadClass::Dedicated => 16384,
            };
            envelope_cache_cap_mb.min(tier_max)
        } else if total_gb >= 17 && total_mb > Self::EXTENDED_SIXTEEN_CLASS_MB {
            // Clearly above ~18 GiB physical — larger baseline for mid-tier workstations.
            ((available_mb * 50 / 100) as usize).clamp(2048, 4096)
        } else if total_gb >= 16 {
            // ~16 GiB bucket: desktops share RAM with OS, browser, IDE. mimalloc retains arena
            // pages after eviction so the cache high-water mark sets the RSS floor that the
            // adaptive shrinker can never recover below. 2560 MB → ~10 GB RSS at peak + desktop
            // workload → OOM (observed). Cap at 1400 MB so the mimalloc high-water mark stays
            // below ~2 GB, keeping total RSS well under OOM territory.
            ((available_mb * 30 / 100) as usize).clamp(1024, 1400)
        } else if total_gb >= 12 {
            ((available_mb * 25 / 100) as usize).clamp(768, 1536)
        } else if total_gb >= 8 {
            // ≤8 GiB: budget VPS / old laptop.  OS + LMDB mmap + TCP buffers already
            // consume 1–2 GiB; keep the UTXO cache small to avoid Emergency pressure stalls.
            ((available_mb * 20 / 100) as usize).clamp(256, 512)
        } else if total_gb >= 4 {
            // ≤4 GiB: Raspberry Pi 4 / tiny VPS.  256 MiB UTXO cache with aggressive
            // flushing is still much faster than no cache.
            ((available_mb * 15 / 100) as usize).clamp(128, 256)
        } else {
            // <4 GiB: very constrained.  Minimal cache; rely on incremental flushes.
            ((budget_mb * 30 / 100) as usize).clamp(64, 128)
        };
        // On tight hosts (< 12 GiB total or < 6 GiB available at boot), keep the cache
        // conservative to avoid OOM with other workloads. A flat 192 MiB ceiling was stable
        // but crippled BPS on 16 GiB laptops with temporarily low MemAvailable; cap instead
        // at 7% of total RAM (192–384 MiB) so tiny machines stay near 192 MiB while larger
        // tight boxes retain a bit of working set.
        if total_gb < 12 || available_mb < 6144 {
            let tight_cap_mb = (total_mb.saturating_mul(7) / 100).clamp(128, 384) as usize;
            utxo_cache_mb = utxo_cache_mb.min(tight_cap_mb);
        }
        if let Some(mb) = std::env::var("BLVM_UTXO_CACHE_MAX_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            if mb > 0 {
                utxo_cache_mb = utxo_cache_mb.min(mb);
            }
        }
        utxo_cache_mb = utxo_cache_mb.min(envelope_cache_cap_mb.max(128));
        // Empirical ~1600 B/entry actual cost (DashMap table + Arc<UTXO> heap + mimalloc
        // fragmented arena + RocksDB compaction/cache growth). The old 560 B/entry estimate was
        // the *marginal* cost per entry in isolation, which underestimated:
        //   • DashMap backing array (64B per slot / 0.875 load factor ≈ 73B, not freed on remove)
        //   • Mimalloc arena fragmentation: freed Arc<UTXO> objects don't immediately return pages
        //     to OS — the allocator retains the segment until ALL objects in it are freed
        //   • RocksDB memory growing with the DB: block cache fills, compaction buffers accumulate
        // At 1600B/entry: utxo_cache_mb=1400 → ~875k entries → cache RSS ≈ 1.4 GB.
        // mimalloc high-water mark (arenas retained after eviction) matches the cap, so
        // total stays ~1.4 (cache) + 1.5 (RocksDB) + 0.9 (download queue) + 0.5 (other)
        // ≈ 4.3 GB on 16 GB — well clear of OOM even with a 6 GB desktop workload.
        let utxo_max_entries = utxo_cache_mb * 1024 * 1024 / 1600;

        // UTXO flush threshold — larger batches reduce L0 SST creation rate and compaction churn.
        // At h=360k each block has ~8k ops; at 100k threshold we emit a flush every ~12 blocks
        // UTXO flush threshold: how many pending ops to accumulate before flushing.
        // Derived proportionally from spare_mb so that intermediate RAM sizes (24 GB, 20 GB)
        // are not penalized by a coarse step function. Each pending UTXO op is ~160 B
        // (key 40B + Arc<UTXO> ptr 8B + value ~64B + DashMap slot overhead ~48B).
        // Target ≤ 6% of spare_mb for pending-op buffer; clamp to tier max to avoid excessive
        // L0-SST accumulation on constrained hosts.
        let utxo_flush_threshold = {
            const BYTES_PER_OP: usize = 160;
            let target = (spare_mb as usize).saturating_mul(1024 * 1024) * 6 / 100 / BYTES_PER_OP;
            let max_ops: usize = if total_gb >= 48 {
                2_000_000
            } else if total_gb >= 32 {
                1_200_000
            } else if total_gb >= 24 {
                800_000
            } else if total_gb >= 17 && total_mb > Self::EXTENDED_SIXTEEN_CLASS_MB {
                480_000
            } else if total_gb >= 16 {
                320_000
            } else {
                120_000
            };
            target.clamp(40_000, max_ops)
        };

        let crit_rss_threshold_mb = std::env::var("BLVM_IBD_PRESSURE_CRIT_RSS_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| (800..=8000).contains(&n))
            .unwrap_or_else(|| {
                // Scale with total RAM so a 16 GiB host with 3 GiB RSS doesn't trigger Critical.
                // 8 GiB → ~1800 MB, 12 GiB → ~2700 MB, 16 GiB → ~3600 MB.
                (total_mb * 22 / 100).clamp(1200, 6000)
            });

        // Defer flush on 32+ GiB only. The earlier 16 GiB tier ran with defer=true (every 500
        // blocks) on the theory that fewer L0 SSTs meant less compaction churn — but the cost
        // was 500 blocks × ~5k UTXOs = ~2.5M entries pinned in `worker_preinserted` between
        // flushes, which (combined with the 3 GB DashMap cache cap) drove RSS to 8–9 GB and
        // triggered EMERGENCY admission pauses in a tight loop. Below 32 GiB threshold-based
        // flushing wins: pending caps at `flush_threshold` (500k ops ≈ 100 blocks of pins),
        // so the protected set is 5× smaller and the cache stays well below its cap.
        //
        // Shared workstations (MemAvailable ≪ MemTotal, or BLVM_DEDICATED_NODE unset) use
        // pending-ops backpressure — deferring flushes to 10k checkpoints wedges pending at
        // the cap until RSS hits Critical (observed h≈600k resume, 2026-06-27).
        let defer_flush = if std::env::var("BLVM_IBD_DEFER_FLUSH")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
            .unwrap_or(false)
        {
            false
        } else if std::env::var("BLVM_IBD_DEFER_FLUSH")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            true
        } else if matches!(workload, WorkloadClass::Shared) {
            false
        } else {
            total_gb >= 32
        };
        // UTXO flush cadence: how many blocks between durable DEL-inclusive flushes.
        //
        // RocksDB: large intervals reduce L0 SST churn (write buffer amortises the cost).
        //   50k was too large — a single 130M-entry flush triggers RocksDB's write stall.
        //   10k keeps each batch to ~26M ops (~2.6 GB) within the write buffer limit.
        //
        // LMDB/Heed3: large intervals cause freeDB B-tree explosion. When millions of DELs
        //   are deferred across thousands of txn IDs, LMDB's freeDB grows so large that
        //   updating it requires more pages than LMDB can allocate (circular dependency)
        //   → MDB_MAP_FULL even with headroom in the map. A 200-block interval applies
        //   ~1M DELs per checkpoint (≈300 k freed pages) — small enough for LMDB to handle
        //   freeDB updates atomically without hitting the page-reclamation cliff.
        //
        // Overrideable via BLVM_IBD_DEFER_CHECKPOINT_INTERVAL for benchmarking.
        let defer_checkpoint_interval_base =
            if matches!(ctx.storage_backend, crate::storage::database::DatabaseBackend::Heed3) {
                200u64
            } else if total_gb >= 64 {
                10_000
            } else if total_gb >= 32 {
                2_000
            } else {
                25_000
            };
        // Floor lowered from 100 to 20: per-block UTXO churn grows substantially with chain
        // height (SegWit-era blocks carry far more ops than early blocks), so a fixed
        // 200-block interval that was safe at low heights can produce multi-million-op
        // single packages at h>400k. Smaller intervals only reduce freeDB churn risk
        // (the constraint above is about intervals being too LARGE), so lowering the floor
        // is safe and lets us bound worst-case package/commit size at high heights.
        let defer_checkpoint_interval = std::env::var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| (20..=500_000).contains(&n))
            .unwrap_or(defer_checkpoint_interval_base);

        // Block buffer: 10% of budget. 16GB caps lower (500KB estimate from early blocks
        // doesn't hold at h>300k where blocks average ~1MB).
        let block_buffer_base = {
            let buffer_mb = budget_mb * 10 / 100;
            let blocks = buffer_mb * 1024 / 500;
            (blocks as usize).clamp(100, 800)
        };

        // Storage flush interval (blocks buffered before async blockstore flush).
        // Backend-aware: heed3 uses smaller batches to avoid orchestrator stalls on LMDB writes.
        let mut storage_flush_interval =
            Self::storage_flush_interval_base(total_gb, ctx.storage_backend);
        if let Ok(s) = std::env::var("BLVM_IBD_STORAGE_FLUSH_INTERVAL") {
            if let Ok(n) = s.parse::<usize>() {
                // Same bounds as chunk_size-style knobs: avoid tiny flushes or OOM-sized buffers.
                storage_flush_interval = n.clamp(16, 4000);
            }
        }

        // Prefetch queue: scales with **spare** RAM at boot (pipeline depth without env).
        // Each slot holds a block body (~2 MB at h>700k) plus UTXO prefetch results
        // (~0.5 MB) — roughly 2.5 MB per slot.  On tight machines the queue must stay
        // small to avoid OOM when the coordinator stalls and the pipeline fills end-to-end.
        let prefetch_queue_size = {
            // Each slot ≈ 2.5 MB (block body + UTXO prefetch results at h>700k).
            // On large-RAM hosts the prefetch ready-channel (OrderedReadyBridge output)
            // can fill up under the bridge mutex, serializing all 24 prefetch workers.
            // A deeper channel reduces bridge-mutex contention by letting workers deposit
            // completed items without blocking on the feeder consuming them.
            let hi: u64 = if total_gb <= 3 {
                // <4 GiB: 16 slots × 2.5 MB = 40 MB max; OS + LMDB leave almost nothing.
                16
            } else if total_gb <= 7 {
                // 4–7 GiB: 32 slots × 2.5 MB = 80 MB.
                32
            } else if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
                160
            } else if total_gb <= 24 {
                1024
            } else {
                // ≥25 GiB: 2048 slots × 2.5 MB = 5 GB max.
                // Previously raised to 4096 for >48 GiB hosts but that added up to
                // 10 GB of anonymous RSS that pushed RssAnon over the rss_budget_mb
                // ceiling on 94 GiB machines, triggering permanent Emergency pressure
                // and collapsing the UTXO cache to ~335k entries. Keep at 2048.
                2048
            };
            (spare_mb / 10).clamp(16, hi) as usize
        };

        // Max blocks download can race ahead — derived from spare MB, capped by tier (parity with
        // stable mainline: under 16 GiB 256, 16–31 GiB 512, 32+ GiB 1024) so low spare still throttles.
        let max_ahead_blocks = {
            let mut v = (spare_mb / 8).clamp(64, 8192);
            if total_gb < 32 {
                v = v.min(4096);
            }
            let tier_cap = Self::tier_max_download_ahead_blocks(total_mb);
            v.min(tier_cap)
        };

        // Prefetch cache (entries); upper bound scales down on 16GB-class machines.
        let prefetch_limit = {
            let cache_mb = budget_mb * 3 / 100;
            let hi = if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
                8000
            } else if total_gb <= 24 {
                35_000
            } else {
                50_000
            };
            let spare_boost = ((spare_mb / 1024) as usize).saturating_mul(800);
            (((cache_mb * 1024 * 1024 / 400) as usize).saturating_add(spare_boost)).clamp(5_000, hi)
        };

        // Feeder buffer byte cap — tighter on 16GB to avoid holding too many ~1MB blocks.
        let feeder_pct = if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            2
        } else {
            5
        };
        let feeder_buffer_bytes_limit = (budget_mb * feeder_pct / 100 * 1024 * 1024) as usize;

        // Flush concurrency: each std::thread::spawn takes ~8MB stack + RocksDB WriteBatch
        // internal buffers. Retire scales concurrency down automatically from [`PressureLevel`]
        // (`utxo_flush_concurrency_cap`); tier sets the **floor** used under Critical+.
        //
        // For single-writer backends (Heed3/LMDB): all UTXO flush threads and block flush
        // threads share the same `write_lock`. Having N>4 concurrent UTXO flush threads only
        // queues N write batches behind the lock — each holding its entire serialized batch
        // (50–200 MB) in memory while waiting. At 32 threads this wastes 1–6 GB of RSS for
        // batches that are serialized anyway. Cap at 4 for Heed3/LMDB: allows one thread
        // writing while two have pre-sorted their batch and one is being prepared, with minimal
        // memory waste (~200 MB total for 4 × 50 MB batches vs ~1.6 GB for 32).
        let max_utxo_flushes_auto: usize = {
            use crate::storage::database::DatabaseBackend;
            match ctx.storage_backend {
                // Single-writer: cap UTXO flush concurrency to bound memory waste from
                // queued-but-not-yet-writing batches.
                DatabaseBackend::Heed3 | DatabaseBackend::Redb | DatabaseBackend::Sled => 4,
                // Multi-writer / WAL-based: more parallelism helps.
                _ => {
                    if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
                        8
                    } else if total_gb <= 24 {
                        12
                    } else if total_gb <= 32 {
                        16
                    } else {
                        32
                    }
                }
            }
        };
        let max_utxo_flushes: usize = std::env::var("BLVM_IBD_MAX_UTXO_FLUSHES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .map(|n| n.clamp(1, 64))
            .unwrap_or(max_utxo_flushes_auto);
        // Blockstore async flushes are a separate pool from UTXO commits.
        //
        // LMDB (Heed3) is a SINGLE-WRITER database: only one write transaction can be active at a
        // time across both the blockstore and the UTXO store (they share the same `write_lock` +
        // environment). Having N concurrent block-flush handles provides zero additional I/O
        // parallelism for LMDB — it only creates a serialization pile-up:
        //
        //   UTXO checkpoint flush (every 10k blocks) holds `write_lock` for ~2-4 s while writing
        //   ~50k UTXOs. With 48 concurrent handles, 48×8=384 block-store write transactions are
        //   queued behind it. Each write txn takes ~200ms (serialize + pwrite + fdatasync), so
        //   the queue drains in ~77 s. The oldest pending handle finishes ~77s after the UTXO
        //   flush started; the orchestrator's `handle.join()` blocks for 30–90s at every 400-block
        //   boundary near a UTXO checkpoint — confirmed by watchdog events at h=24000, 32800, etc.
        //
        // Fix: cap concurrent block-flush handles at 2 for single-writer backends. At 80 BPS and
        // 400-block flush intervals the orchestrator spawns a handle every 5s. With 2 slots, it
        // joins the oldest handle that completed 10s ago in <1ms. The UTXO flush stall shrinks
        // from ~77s to ≤2×8×200ms = 3.2s — well below the 30s watchdog threshold.
        let max_block_flushes_auto: usize = {
            use crate::storage::database::DatabaseBackend;
            match ctx.storage_backend {
                // Single-writer engines: LMDB (Heed3) and similar backends enforce a
                // single write transaction at a time via an internal write_lock.  A second
                // concurrent flush thread adds no parallelism — it just blocks on write_lock
                // for the full duration of the first flush, preventing the non-blocking
                // handle reaper from seeing a "finished" handle.  With max=1 the dispatcher
                // knows there is at most one pending flush; when it finishes it will be
                // reaped at the top of the next iteration and a new flush spawned immediately
                // without any join latency.
                DatabaseBackend::Heed3 | DatabaseBackend::Redb | DatabaseBackend::Sled => 1,
                // Multi-writer / WAL-based engines: extra parallelism helps.
                DatabaseBackend::RocksDB | DatabaseBackend::TidesDB => {
                    if total_gb <= 24 {
                        max_utxo_flushes
                    } else if total_gb <= 32 {
                        max_utxo_flushes + max_utxo_flushes / 2
                    } else {
                        (max_utxo_flushes + max_utxo_flushes / 2).min(48)
                    }
                }
            }
        };
        let max_block_flushes: usize = std::env::var("BLVM_IBD_MAX_BLOCK_FLUSHES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .map(|n| n.clamp(1, 64))
            .unwrap_or(max_block_flushes_auto);

        tracing::info!(
            "MemoryGuard: total={}MB available={}MB workload={:?} backend={:?} spare≈{}MB budget={}MB \
             rss_budget={}MB (live /proc pressure) utxo_cache={}MB ({}entries) flush_threshold={} \
             defer_flush={} defer_checkpoint={} buffer={} prefetch={} prefetch_queue={} \
             max_ahead={} storage_flush={} pipeline_reserve={}MB feeder_bytes={}MB max_utxo_flush={} max_block_flush={}",
            total_mb,
            available_mb,
            workload,
            ctx.storage_backend,
            spare_mb,
            budget_mb,
            rss_budget_mb,
            utxo_cache_mb,
            utxo_max_entries,
            utxo_flush_threshold,
            defer_flush,
            defer_checkpoint_interval,
            block_buffer_base,
            prefetch_limit,
            prefetch_queue_size,
            max_ahead_blocks,
            storage_flush_interval,
            Self::pipeline_reserve_mb(ctx.storage_backend),
            feeder_buffer_bytes_limit / (1024 * 1024),
            max_utxo_flushes,
            max_block_flushes,
        );

        if std::env::var("BLVM_RSS_BUDGET_MB").is_ok() {
            tracing::warn!(
                "MemoryGuard: BLVM_RSS_BUDGET_MB overrides auto envelope (auto would be {}MB); \
                 remove for backend-aware auto-tuning unless debugging OOM",
                auto_rss_budget_mb
            );
        }
        if std::env::var("BLVM_IBD_MAX_PARALLEL").is_ok() {
            tracing::warn!(
                "MemoryGuard: BLVM_IBD_MAX_PARALLEL set — validation worker count is pinned; \
                 auto scales to logical CPUs on 32+ GiB hosts"
            );
        }
        if std::env::var("BLVM_IBD_MAX_BLOCK_FLUSHES").is_ok() {
            tracing::warn!(
                "MemoryGuard: BLVM_IBD_MAX_BLOCK_FLUSHES set — blockstore flush pool capped; \
                 auto allows up to {} on this host",
                max_block_flushes_auto
            );
        }

        Self {
            total_mb,
            avail_mb: available_mb,
            budget_mb,
            utxo_cache_mb,
            utxo_max_entries,
            rss_budget_mb,
            last_adaptive_cap_entries: AtomicUsize::new(0),
            last_adaptive_cap_check: Mutex::new(Instant::now() - Duration::from_secs(60)),
            last_adaptive_cap_shrink: Mutex::new(Instant::now() - Duration::from_secs(120)),
            above_threshold_consecutive: AtomicU8::new(0),
            utxo_flush_threshold,
            block_buffer_base,
            storage_flush_interval,
            prefetch_limit,
            prefetch_queue_size,
            max_ahead_blocks,
            defer_flush,
            defer_checkpoint_interval,
            feeder_buffer_bytes_limit,
            max_utxo_flushes,
            max_block_flushes,
            storage_backend: ctx.storage_backend,
            no_swap_at_boot: startup_swap_total_mb == 0,
            #[cfg(feature = "sysinfo")]
            sys,
            last_rss_check: Instant::now(),
            last_ahead_adjust: Instant::now() - Duration::from_secs(1),
            last_reported_pressure: AtomicU8::new(PressureLevel::None as u8),
            crit_rss_threshold_mb,
            #[cfg(target_os = "linux")]
            proc_status_buf: String::with_capacity(4096),
            #[cfg(target_os = "linux")]
            proc_meminfo_buf: String::with_capacity(8192),
            spec_adds_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn feeder_scale_snapshot(&self) -> FeederScaleSnapshot {
        FeederScaleSnapshot {
            block_buffer_base: self.block_buffer_base,
            total_mb: self.total_mb,
            feeder_buffer_bytes_limit: self.feeder_buffer_bytes_limit,
        }
    }

    /// Blockstore flush interval: `storage_flush_interval` (RAM-tier base from init) scaled by pressure.
    /// Under memory pressure we flush sooner (fewer blocks buffered), but never below a safe floor.
    #[inline]
    pub(crate) fn storage_flush_interval_live(&self, pressure: PressureLevel) -> usize {
        Self::storage_flush_interval_live_for(self.storage_flush_interval, pressure)
    }

    /// Pure-function variant — lets the dispatcher capture `storage_flush_interval` once and
    /// avoid acquiring `mem_mtx` on the per-block hot path.
    #[inline]
    pub(crate) fn storage_flush_interval_live_for(base: usize, pressure: PressureLevel) -> usize {
        match pressure {
            PressureLevel::None => base,
            PressureLevel::Elevated => (base * 3 / 4).max(200),
            PressureLevel::Critical => (base / 2).max(128),
            PressureLevel::Emergency => (base / 4).max(64),
        }
    }

    /// When pressure is Critical or Emergency, cap estimated bytes of validated block+witness data
    /// held in `pending_blocks` before forcing a blockstore flush. Tied to IBD RAM budget, not chain height.
    /// `None` at None/Elevated: only [`storage_flush_interval_live`] applies (avoids tiny-batch flushes).
    #[inline]
    pub(crate) fn storage_flush_pending_bytes_pressure_cap(
        &self,
        pressure: PressureLevel,
    ) -> Option<u64> {
        Self::storage_flush_pending_bytes_pressure_cap_for(self.budget_mb, pressure)
    }

    /// Pure-function variant — see [`Self::storage_flush_interval_live_for`].
    ///
    /// At `PressureLevel::None` / `Elevated` we now apply a byte cap (20% / 12% of budget)
    /// instead of returning `None`. Without this, `pending_blocks` could accumulate up to
    /// the full `storage_flush_interval` × block-size — at h=700k that is ~450 MB (300 ×
    /// 1.5 MB) with the new 300-block interval, or up to 3 GB on 32+ GiB hosts with the
    /// 2000-block interval. The cap ensures blocks are flushed before their Arcs pin that
    /// memory too long. The `pressure_min_blocks` floor (≥ 40% of the live interval, min 96)
    /// prevents very-small-block heights from triggering spurious micro-flushes.
    #[inline]
    pub(crate) fn storage_flush_pending_bytes_pressure_cap_for(
        budget_mb: u64,
        pressure: PressureLevel,
    ) -> Option<u64> {
        let pct: u64 = match pressure {
            PressureLevel::None => 20,
            PressureLevel::Elevated => 12,
            PressureLevel::Critical => 6,
            PressureLevel::Emergency => 4,
        };
        let raw = budget_mb.saturating_mul(1024 * 1024).saturating_mul(pct) / 100;
        // 64 MiB hard floor: avoids micro-flushes on tiny-budget or very-early-chain scenarios.
        Some(raw.max(64 * 1024 * 1024))
    }

    /// Minimum pending block count before a pressure byte cap can trigger a flush.
    #[inline]
    pub(crate) fn storage_flush_pressure_min_blocks(flush_interval_live: usize) -> usize {
        flush_interval_live
            .saturating_mul(2)
            .saturating_div(5)
            .max(96)
    }

    /// Total system RAM (MB) at init — for IBD caps that need a host tier without re-probing.
    #[inline]
    pub(crate) fn system_total_ram_mb(&self) -> u64 {
        self.total_mb
    }

    /// IBD memory budget (MB) at init — constant after construction.
    /// Exposed so the dispatcher can capture it once and avoid taking `mem_mtx` every block
    /// just to recompute pressure-scaled byte caps.
    #[inline]
    pub(crate) fn budget_mb(&self) -> u64 {
        self.budget_mb
    }

    /// Upper bound on download-ahead for this host tier (blocks). Spare-derived nominal is always
    /// `min(spare_formula, this)` so RAM-tight machines stay bounded.
    #[inline]
    pub(crate) fn tier_max_download_ahead_blocks(total_mb: u64) -> u64 {
        let total_gb = Self::total_gb_rounded(total_mb);
        if total_gb <= 3 {
            // <4 GiB: 64 blocks in-flight keeps heap usage ~64 MB — safe on 3-4 GiB machines.
            64
        } else if total_gb <= 7 {
            // 4-7 GiB budget tier (VPS, Pi 5).
            128
        } else if total_gb < 16 {
            256
        } else if total_gb <= 16 || total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            // ~16 GiB class + BIOS‑reported “17 GiB” laptops (~≤18 GiB MemTotal)
            320
        } else if total_gb < 32 {
            512
        } else if total_gb < 64 {
            1024
        } else {
            // ≥64 GiB: large RAM hosts can absorb a deeper download pipeline. The adaptive
            // chunk-size formula uses `ceil(num_peers/2)` as the denominator; at 25 peers and
            // max_ahead=1024 this yields chunk_size=64 (25→13 active, 1024/13=78→64). With
            // max_ahead=2048 the same formula yields chunk_size=128, halving the number of
            // chunk round-trips and improving download pipeline fill. RAM cost is bounded by
            // `feeder_buffer_bytes_limit` (≈5% of rss_budget), not by max_ahead directly.
            2048
        }
    }

    /// Default depth for UTXO flush `sync_channel`(s). Larger values reduce validation blocking when
    /// the single committer falls behind; bounded and tiered so 16 GiB hosts stay conservative.
    #[inline]
    pub(crate) fn ibd_utxo_flush_queue_depth_default(&self) -> usize {
        let total_gb = Self::total_gb_rounded(self.total_mb);
        if self.total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            128
        } else if total_gb <= 24 {
            160
        } else if total_gb <= 32 {
            224
        } else {
            288
        }
    }

    #[inline]
    fn pressure_level_name(v: u8) -> &'static str {
        match v {
            x if x == PressureLevel::None as u8 => "None",
            x if x == PressureLevel::Elevated as u8 => "Elevated",
            x if x == PressureLevel::Critical as u8 => "Critical",
            x if x == PressureLevel::Emergency as u8 => "Emergency",
            _ => "?",
        }
    }

    /// Like [`pressure_level`](Self::pressure_level), but logs `MemoryGuard: pressure transition A -> B (snapshot)`
    /// whenever the level **changes** in any direction (including recovery to `None`).
    pub(crate) fn pressure_level_reported(&self, snap: &MemorySnapshot) -> PressureLevel {
        let level = self.pressure_level(snap);
        self.log_pressure_transition_if_changed(level, snap);
        level
    }

    fn log_pressure_transition_if_changed(&self, level: PressureLevel, snap: &MemorySnapshot) {
        let new = level as u8;
        let prev = self.last_reported_pressure.swap(new, Ordering::Relaxed);
        if prev == new {
            return;
        }
        tracing::info!(
            "MemoryGuard: pressure transition {} -> {} ({})",
            Self::pressure_level_name(prev),
            Self::pressure_level_name(new),
            snap
        );
        // On first Critical/Emergency transition, dump mimalloc allocation stats to stderr so we
        // can identify what is consuming memory. Gated on feature="mimalloc" so it compiles away
        // in non-production builds. The output goes to stderr — redirect with 2>/tmp/mi-stats.log.
        if new >= (PressureLevel::Critical as u8) && prev < (PressureLevel::Critical as u8) {
            #[cfg(all(not(target_os = "windows"), feature = "mimalloc"))]
            unsafe {
                libmimalloc_sys::mi_stats_print(std::ptr::null_mut());
            }
        }
    }

    /// Graduated pressure assessment with hysteresis to prevent rapid oscillation.
    ///
    /// Reads `last_reported_pressure` as the current level. Entry thresholds are unchanged;
    /// exit thresholds are 150-200 MB lower on <=16 GiB. This eliminates the
    /// Emergency<->Critical thrashing seen at h=264k (244 transitions in 8 min) where RSS
    /// bounced +/-15 MB around the 2000 MB boundary, triggering repeated
    /// `cancel_all_background_work` calls in the hot validation path.
    pub(crate) fn pressure_level(&self, snap: &MemorySnapshot) -> PressureLevel {
        let current = PressureLevel::from_u8(self.last_reported_pressure.load(Ordering::Relaxed));
        let level = self.clamp_pressure_to_process_budget(
            self.pressure_level_for(snap, current),
            snap,
            current,
        );
        Self::clamp_pressure_to_swap_state(level, snap)
    }

    /// Raise pressure when system-wide swap is near exhaustion AND RAM is tight, OR when our
    /// own process pages are actually on disk.
    ///
    /// ## The two signals
    ///
    /// 1. **`vm_swap_mb` (proc_swap)** — MiB of *our* pages the kernel has evicted to swap.
    ///    `>1024` is real paging (KEEP C0 Emergency @415k) and stays unconditional.
    ///    64 / 256 still raise Elevated/Critical regardless of free RAM (r26 gated those
    ///    and tip90 194→183 — reverted). False Emergency on vLLM-filled zram is the
    ///    [`Self::large_host_our_swap_counts`] gate in `pressure_level_for`, not this clamp.
    ///
    /// 2. **system-wide swap exhaustion** — only dangerous when RAM is *also* tight
    ///    (`sys_avail_mb < 8 GiB`). When 73 GiB of RAM is free, exhausted swap from a
    ///    previous OOM-killed process poses no immediate threat: the kernel will satisfy our
    ///    allocations from RAM without touching swap at all.
    ///
    ///    Prior bug: we raised Emergency unconditionally on `swap_free_pct < 5`, which fired
    ///    immediately after an IBD restart on a machine whose swap was still 99% full from the
    ///    previous run's OOM kill — despite `proc_swap=0` and 73 GiB available. The result was
    ///    an infinite Emergency admission pause at height 1, blocking the entire pipeline.
    fn clamp_pressure_to_swap_state(level: PressureLevel, snap: &MemorySnapshot) -> PressureLevel {
        if snap.swap_total_mb == 0 {
            return level; // No swap configured: skip.
        }
        let our_swap_mb = snap.vm_swap_mb;
        // Our own pages are on disk: act immediately regardless of free RAM.
        if our_swap_mb > 1024 {
            return PressureLevel::Emergency;
        }
        if our_swap_mb > 256 {
            return level.max(PressureLevel::Critical);
        }
        if our_swap_mb > 64 {
            return level.max(PressureLevel::Elevated);
        }
        // System-wide swap checks: only apply when RAM is also tight.
        // If sys_avail is large, the kernel can serve allocations from RAM and the fact
        // that swap is full (likely from a prior process) doesn't threaten us right now.
        let ram_is_tight = snap.sys_avail_mb > 0 && snap.sys_avail_mb < 8192;
        if !ram_is_tight {
            return level;
        }
        let swap_free_pct = snap.swap_free_mb * 100 / snap.swap_total_mb.max(1);
        if swap_free_pct < 5 {
            PressureLevel::Emergency
        } else if swap_free_pct < 15 {
            level.max(PressureLevel::Critical)
        } else if swap_free_pct < 35 {
            level.max(PressureLevel::Elevated)
        } else {
            level
        }
    }

    /// Large-host (>16 GiB) process-swap pressure only counts when MemAvailable is
    /// under 32 GiB. Matches the `sys_swap_*` gate written for vLLM-filled zram.
    #[inline]
    pub(crate) fn large_host_our_swap_counts(sys_avail_mb: u64) -> bool {
        sys_avail_mb > 0 && sys_avail_mb < 32 * 1024
    }

    /// Raise pressure when **process anonymous RSS** nears `rss_budget_mb`, even if host
    /// MemAvailable still looks ample (shared RAM with vLLM, IDE, etc.).
    ///
    /// We compare `RssAnon` (heap, stack, anonymous mmap) against the budget rather than
    /// raw `VmRSS`.  `VmRSS` includes file-backed mmap pages (primarily the LMDB data.mdb
    /// mapping) which the kernel can evict freely — they are already accounted for by the
    /// kernel's own `MemAvailable` estimate.  At h≈400k the LMDB file grows to 80+ GiB;
    /// 10–15 GiB of hot pages appear in `VmRSS` but not in `RssAnon`.  Comparing raw
    /// `VmRSS` against a budget sized for *anonymous* use caused spurious Emergency triggers
    /// that nuked the UTXO cache to ~170k entries and created cascading write-lock contention
    /// across block-flush threads.
    fn clamp_pressure_to_process_budget(
        &self,
        mut level: PressureLevel,
        snap: &MemorySnapshot,
        current: PressureLevel,
    ) -> PressureLevel {
        let b = self.rss_budget_mb;
        // Prefer RssAnon (anonymous RSS only); fall back to total VmRSS if not available.
        let r = if snap.rss_anon_mb > 0 {
            snap.rss_anon_mb
        } else {
            snap.rss_mb
        };
        if b == 0 || r == 0 {
            return level;
        }
        // Without swap the kernel cannot absorb anon growth — enter Emergency earlier.
        let emerg_line = if self.no_swap_at_boot {
            b * 85 / 100
        } else {
            b
        };
        // Hysteresis: anon RSS jitters ±500 MB around the Emergency line at h≈490k,
        // causing Emergency↔Critical flips 4×/second (observed 08:56 UTC). Stay in
        // Emergency until anon drops 5% below the entry line.
        let emerg_exit = if self.no_swap_at_boot {
            b * 80 / 100
        } else {
            b * 95 / 100
        };
        let crit_line = if self.no_swap_at_boot {
            b * 75 / 100
        } else {
            b * 92 / 100
        };
        let elev_line = if self.no_swap_at_boot {
            b * 65 / 100
        } else {
            b * 82 / 100
        };
        if r >= emerg_line {
            return PressureLevel::Emergency;
        }
        if current == PressureLevel::Emergency && r >= emerg_exit {
            return PressureLevel::Emergency;
        }
        if r >= crit_line {
            level = level.max(PressureLevel::Critical);
        } else if r >= elev_line {
            level = level.max(PressureLevel::Elevated);
        }
        level
    }

    /// RAM budget (MiB) for sizing the IBD engine index at open — derived from `rss_budget_mb`,
    /// not raw MemAvailable (which ignores other workloads on the host).
    pub(crate) fn engine_avail_mb(&self) -> u64 {
        const PIPELINE_RESERVE_MB: u64 = 6144;
        const LEGACY_CACHE_CAP_MB: u64 = 4096;
        let legacy = (self.utxo_cache_mb as u64).min(LEGACY_CACHE_CAP_MB);
        let engine_mb = self
            .rss_budget_mb
            .saturating_sub(PIPELINE_RESERVE_MB)
            .saturating_sub(legacy);
        // Size the index from the IBD RSS envelope, not boot-time MemAvailable. On Zeus with
        // vLLM running, MemAvailable at IBD start can be ~10 GiB while rss_budget is ~37 GiB;
        // clamping to avail_mb forced eviction age 3 and left memory_pressure_tick with no
        // room to spill (Emergency also maps to age 3). engine_mb already reserves pipeline +
        // legacy cache headroom within the budget.
        engine_mb.max(2048)
    }

    fn pressure_level_for(&self, snap: &MemorySnapshot, current: PressureLevel) -> PressureLevel {
        let t = if snap.mem_total_mb > 0 {
            snap.mem_total_mb
        } else {
            self.total_mb
        };
        let r = snap.rss_mb;
        let a = snap.sys_avail_mb;
        if r == 0 {
            return PressureLevel::None;
        }

        if t <= 16 * 1024 {
            // <=16 GiB: pressure follows MemAvailable AND active swap-thrash. The kernel's
            // MemAvailable does NOT account for swapped-out anonymous pages — at h=375k
            // we observed `MemAvailable=5067MB` while 3.7 GB of our cache was on disk
            // in swap, killing BPS to 60. So we additionally watch swap, but only when
            // memory is also tight (otherwise leftover swap from a previous process
            // would falsely trip Critical at startup with 12 GB free).
            //
            // Swap pressure ONLY counts when:
            //   - swap usage is large relative to OUR RSS (kernel had to evict our pages, not
            //     just leftover from another process), AND
            //   - sys_avail is tight enough that we'd actually swap more if we grew the cache.
            let swap_used = snap.swap_used_mb();
            // Use our process's actual VmSwap (from /proc/self/status) instead of the
            // system-wide swap heuristic. The old `swap_used > rss / 4` incorrectly
            // triggered on leftover system swap from previous OOM-killed blvm processes,
            // causing 67k false Critical events per IBD run. VmSwap is exactly what we need:
            // it is non-zero only when our pages are actually on disk.
            let our_swap = snap.vm_swap_mb > 256; // >256MB of OUR pages are in swap
            let crit_rss = self.crit_rss_threshold_mb; // default = t*22/100
            let rss_elev = (t * 30 / 100).max(2000); // e.g. 4776 MB on 16 GiB
            let rss_emerg = (t * 50 / 100).max(4000); // e.g. 8192 MB on 16 GiB
            // Hysteresis on avail thresholds: enter at A_up, require A_up + 512 MB to deactivate
            // the swap_X flag (and thus permit downward transitions). The 512 MB gap absorbs the
            // observed sys_avail swing (3015–3555 MB at h=315k = 540 MB swing under steady IBD
            // load). Without this gap, sys_avail oscillating ±300 MB around the entry boundary
            // caused 24+ Elevated↔Critical transitions per minute, each one running
            // `adjust_max_ahead_live` and clobbering the prefetch lookahead.
            let swap_elev_up = swap_used >= t * 5 / 100 && a > 0 && a < 4096 && our_swap;
            let swap_crit_up = swap_used >= t * 12 / 100 && a > 0 && a < 3072 && our_swap;
            let swap_emerg_up = swap_used >= t * 20 / 100 && a > 0 && a < 2048 && our_swap;
            // _dn variants: same swap/our_swap requirements but a higher avail ceiling. We treat
            // a swap_X_dn=true value as "swap pressure persists" and gate exit on it being false.
            let swap_elev_dn = swap_used >= t * 5 / 100 && a > 0 && a < 4608 && our_swap;
            let swap_crit_dn = swap_used >= t * 12 / 100 && a > 0 && a < 3584 && our_swap;
            let swap_emerg_dn = swap_used >= t * 20 / 100 && a > 0 && a < 2560 && our_swap;
            // Entry: pure sys_avail at tight thresholds, OR true swap thrash.
            let emerg_up =
                (r >= rss_emerg && a > 0 && a < 1024) || (a > 0 && a < 512) || swap_emerg_up;
            let crit_up =
                (r >= crit_rss && a > 0 && a < 1536) || (a > 0 && a < 768) || swap_crit_up;
            let elev_up =
                (r >= rss_elev && a > 0 && a < 2048) || (a > 0 && a < 1024) || swap_elev_up;
            // Exit: hysteresis on avail AND no active swap pressure (swap_X_dn=false). If swap
            // pages have not drained, we stay in the higher level. Swap pages only page-in lazily
            // on access, so we cannot drive swap_X_dn to false from here — but we can stop
            // oscillating around the entry boundary by requiring more headroom for exit.
            let emerg_dn = (a == 0 || a >= 768) && !swap_emerg_dn;
            let crit_dn = (a == 0 || a >= 1024) && !swap_crit_dn;
            let elev_dn = (a == 0 || a >= 1280) && !swap_elev_dn;

            return match current {
                PressureLevel::Emergency => {
                    if emerg_dn {
                        // Re-evaluate downward without hysteresis so rapid large drops work.
                        if crit_up {
                            PressureLevel::Critical
                        } else if elev_up {
                            PressureLevel::Elevated
                        } else {
                            PressureLevel::None
                        }
                    } else {
                        PressureLevel::Emergency
                    }
                }
                PressureLevel::Critical => {
                    if emerg_up {
                        PressureLevel::Emergency
                    } else if crit_dn {
                        if elev_up {
                            PressureLevel::Elevated
                        } else {
                            PressureLevel::None
                        }
                    } else {
                        PressureLevel::Critical
                    }
                }
                PressureLevel::Elevated => {
                    if emerg_up {
                        PressureLevel::Emergency
                    } else if crit_up {
                        PressureLevel::Critical
                    } else if elev_dn {
                        PressureLevel::None
                    } else {
                        PressureLevel::Elevated
                    }
                }
                PressureLevel::None => {
                    if emerg_up {
                        PressureLevel::Emergency
                    } else if crit_up {
                        PressureLevel::Critical
                    } else if elev_up {
                        PressureLevel::Elevated
                    } else {
                        PressureLevel::None
                    }
                }
            };
        }

        // >16 GiB: percentage-based thresholds with a 5% hysteresis gap on exit.
        let avail_emerg_up: u64 = if t <= 24 * 1024 { 1536 } else { 768 };
        let rss_emerg_pct_up: u64 = if t <= 24 * 1024 { 60 } else { 72 };
        let avail_crit_up: u64 = if t <= 24 * 1024 { 1792 } else { 1024 };
        let rss_crit_pct_up: u64 = if t <= 24 * 1024 { 55 } else { 65 };
        let avail_elev_up: u64 = if t <= 24 * 1024 { 2048 } else { 1536 };
        let rss_elev_pct_up: u64 = if t <= 24 * 1024 { 45 } else { 55 };
        let avail_emerg_dn: u64 = avail_emerg_up + avail_emerg_up / 4;
        let avail_crit_dn: u64 = avail_crit_up + avail_crit_up / 4;
        let avail_elev_dn: u64 = avail_elev_up + avail_elev_up / 4;
        let rss_emerg_pct_dn: u64 = rss_emerg_pct_up.saturating_sub(5);
        let rss_crit_pct_dn: u64 = rss_crit_pct_up.saturating_sub(5);
        let rss_elev_pct_dn: u64 = rss_elev_pct_up.saturating_sub(5);

        // On large-RAM machines with large LMDB memory-maps (file-backed RSS, e.g. 50+ GiB for a
        // full-node data.mdb), total VmRSS can exceed 65% of RAM even when MemAvailable is ~60 GiB
        // and the system is under no real pressure. File-backed pages are freely evictable by the
        // kernel and are already reflected in MemAvailable — counting them against the RSS threshold
        // double-penalises them and triggers spurious Critical/Emergency states that kill BPS.
        //
        // Fix: on large-RAM machines (>16 GiB) where RssAnon is available, use RssAnon (anonymous
        // heap + stack) for the percentage check. Anonymous RSS is the only kind that actually
        // competes for RAM against other processes (it cannot be evicted without swap).
        //
        // Safety: the MemAvailable checks above still catch true system-wide pressure. The anon-RSS
        // check is a process-specific signal to detect runaway heap growth that MemAvailable might
        // not show immediately (e.g. if other processes are evicted first to make room).
        let r_for_pct = if snap.rss_anon_mb > 0 { snap.rss_anon_mb } else { r };

        // On large-RAM machines, full swap is still an OOM signal: the kernel has exhausted
        // its eviction reservoir and any further anonymous allocation will trigger OOM kill.
        // The ≤16 GiB path already has swap logic; mirror the essential checks here.
        let swap_pct = if snap.swap_total_mb > 0 {
            snap.swap_used_mb() * 100 / snap.swap_total_mb
        } else {
            0
        };
        // Process-specific: count pressure only when OUR pages are in swap.
        let our_swap_active = snap.vm_swap_mb > 256;
        // Same 32 GiB avail gate as sys_swap_crit below. Zeus 4G zram is often ≥98%
        // full from vLLM while MemAvailable is 50–70 GiB. Ungated swap_emerg_up then
        // tripped Emergency on 650 MB VmSwap and cut validation depth 32→8
        // (r24b @411k, 65 BPS, sys_avail=63G). Real OOM still hits via
        // clamp_pressure_to_swap_state (vm_swap>1024 → Emergency).
        let our_swap_counts = our_swap_active && Self::large_host_our_swap_counts(a);
        // Critical: swap ≥90% full and we have pages in swap.
        let swap_crit_up = swap_pct >= 90 && our_swap_counts;
        // Emergency: swap ≥98% full (kernel about to OOM-kill) *and* RAM is tight.
        let swap_emerg_up = swap_pct >= 98 && our_swap_counts;
        // Allow exit once swap is substantially drained.
        let swap_crit_dn = swap_pct >= 85 && our_swap_counts;
        let swap_emerg_dn = swap_pct >= 95 && our_swap_counts;

        // System-wide swap exhaustion: even if OUR pages aren't in swap yet, a full system
        // swap (≥95% used) means the kernel's eviction reservoir is gone. Any RSS growth that
        // forces OTHER processes' cold pages to page-in — when swap is full — triggers OOM.
        // Raise minimum pressure to Elevated when system swap is critically low AND available
        // RAM is not extremely large (i.e., we're still close to the RAM boundary).
        // Only apply when swap is meaningful (≥1 GiB total) to avoid false triggers on
        // machines with tiny swap or no swap at all.
        let sys_swap_full = snap.swap_total_mb >= 1024 && swap_pct >= 95;
        // Exit sys_swap pressure once swap is below 85%.
        let sys_swap_full_dn = snap.swap_total_mb >= 1024 && swap_pct >= 85;

        let emerg_up = (a > 0 && a < avail_emerg_up) || r_for_pct > t * rss_emerg_pct_up / 100 || swap_emerg_up;
        let crit_up = (a > 0 && a < avail_crit_up) || r_for_pct > t * rss_crit_pct_up / 100 || swap_crit_up;
        // sys_swap_full elevates minimum to Critical so blvm doesn't grow RSS further when
        // the system has no eviction reservoir. If available RAM is very large (>32 GB
        // headroom) only Elevated is applied — the machine has enough RAM even without swap.
        //
        // Gate sys_swap_full for elev_up on available RAM: if sys_avail > 32 GiB there is no
        // OOM risk even with swap exhausted (kernel has RAM to satisfy new allocations). Swap
        // may be full from a co-tenant (vLLM, a prior OOM-killed process, etc.) while blvm
        // itself uses zero swap (proc_swap=0). Without this gate, a full system swap
        // permanently locks max_ahead at 512 and cuts download throughput by 50%.
        // Gate all sys_swap_full pressure on available RAM. When sys_avail > 32 GiB the kernel
        // has ample RAM even without swap — no OOM risk from co-tenant (vLLM, etc.) swap usage.
        // Without this gate a vLLM-filled system swap permanently raises pressure even when blvm
        // itself uses 0 swap (proc_swap=0) and 57 GiB of RAM is available.
        let sys_swap_elev = sys_swap_full && a < 32 * 1024;
        let sys_swap_elev_dn = sys_swap_full_dn && a < 32 * 1024;
        let sys_swap_crit = sys_swap_full && a < 32 * 1024;
        let sys_swap_crit_dn = sys_swap_full_dn && a < 32 * 1024;
        let elev_up = (a > 0 && a < avail_elev_up) || r_for_pct > t * rss_elev_pct_up / 100 || sys_swap_elev;
        let crit_up = crit_up || sys_swap_crit;
        let emerg_dn = (a == 0 || a >= avail_emerg_dn) && r_for_pct <= t * rss_emerg_pct_dn / 100 && !swap_emerg_dn;
        let crit_dn = (a == 0 || a >= avail_crit_dn) && r_for_pct <= t * rss_crit_pct_dn / 100 && !swap_crit_dn && !sys_swap_crit_dn;
        let elev_dn = (a == 0 || a >= avail_elev_dn) && r_for_pct <= t * rss_elev_pct_dn / 100 && !sys_swap_elev_dn;

        match current {
            PressureLevel::Emergency => {
                if emerg_dn {
                    if crit_up {
                        PressureLevel::Critical
                    } else if elev_up {
                        PressureLevel::Elevated
                    } else {
                        PressureLevel::None
                    }
                } else {
                    PressureLevel::Emergency
                }
            }
            PressureLevel::Critical => {
                if emerg_up {
                    PressureLevel::Emergency
                } else if crit_dn {
                    if elev_up {
                        PressureLevel::Elevated
                    } else {
                        PressureLevel::None
                    }
                } else {
                    PressureLevel::Critical
                }
            }
            PressureLevel::Elevated => {
                if emerg_up {
                    PressureLevel::Emergency
                } else if crit_up {
                    PressureLevel::Critical
                } else if elev_dn {
                    PressureLevel::None
                } else {
                    PressureLevel::Elevated
                }
            }
            PressureLevel::None => {
                if emerg_up {
                    PressureLevel::Emergency
                } else if crit_up {
                    PressureLevel::Critical
                } else if elev_up {
                    PressureLevel::Elevated
                } else {
                    PressureLevel::None
                }
            }
        }
    }

    fn adjust_max_ahead_live(&self, snap: &MemorySnapshot, live: &AtomicU64, nominal: u64) {
        let cur = live.load(Ordering::Relaxed);
        let nominal = nominal.max(64);
        let level = self.pressure_level(snap);

        let tight_ahead = self.total_mb <= 16 * 1024;
        match level {
            PressureLevel::Emergency => {
                let target = if tight_ahead {
                    (nominal / 6).max(48)
                } else {
                    (nominal / 4).max(64)
                };
                if cur > target {
                    tracing::warn!(
                        "MemoryGuard: EMERGENCY — download ahead {} → {} ({})",
                        cur,
                        target,
                        snap
                    );
                    live.store(target, Ordering::Relaxed);
                }
            }
            PressureLevel::Critical => {
                let target = if tight_ahead {
                    (nominal / 4).max(64)
                } else {
                    (nominal / 3).max(96)
                };
                if cur > target {
                    tracing::warn!(
                        "MemoryGuard: CRITICAL — download ahead {} → {} ({})",
                        cur,
                        target,
                        snap
                    );
                    live.store(target, Ordering::Relaxed);
                }
            }
            PressureLevel::Elevated => {
                // Gradual step-down: jumping live max_ahead 4096→1024 while validation runs
                // at 240 BPS drained the feeder in <1s (cliff at h≈482k). Reduce by 25% per
                // tick until nominal/2 floor instead of an instant cut to nominal/2.
                let floor = (nominal / 2).max(128);
                let target = (cur * 3 / 4).max(floor);
                if cur > target {
                    tracing::info!(
                        "MemoryGuard: elevated — download ahead {} → {} ({})",
                        cur,
                        target,
                        snap
                    );
                    live.store(target, Ordering::Relaxed);
                }
            }
            PressureLevel::None => {
                // When pressure is absent, allow max_ahead to grow above the boot-time
                // nominal when free memory is ample. More pipeline depth increases prefetch
                // parallelism and hides per-block multi_get latency variance.
                // On <=16 GiB, growth ceilings must track tier cap (nominal can be 256+, not legacy 64).
                let tier_cap = Self::tier_max_download_ahead_blocks(self.total_mb);
                let ceil = if self.total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
                    if snap.sys_avail_mb > 7_000 {
                        nominal.saturating_mul(2).min(tier_cap)
                    } else if snap.sys_avail_mb > 5_000 {
                        (nominal * 3 / 2).min(tier_cap.saturating_mul(3) / 4)
                    } else {
                        nominal
                    }
                } else {
                    // Larger hosts: allow up to 2x nominal freely.
                    nominal.saturating_mul(2)
                };
                if cur < ceil {
                    // Small steps (16) to avoid sudden memory spikes from large blocks.
                    let nxt = cur.saturating_add(16).min(ceil);
                    live.store(nxt, Ordering::Relaxed);
                }
            }
        }
    }

    /// Assess live memory pressure, adjust download-ahead, and return the severity level.
    /// The validation loop uses the returned level to decide flush strategy:
    ///   Elevated → async flush, reduce in-flight cap
    ///   Critical → force flush, drain most in-flight handles
    ///   Emergency → drain ALL handles synchronously, minimal download pipeline
    ///
    /// Throttled to avoid reading /proc every block (except under Emergency).
    pub(crate) fn should_flush(
        &mut self,
        max_ahead_live: Option<(&AtomicU64, u64)>,
    ) -> PressureLevel {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_rss_check);
        let cached = PressureLevel::from_u8(self.last_reported_pressure.load(Ordering::Relaxed));
        // Skip /proc between samples, but keep returning the last level (not `None`) so UTXO
        // flush pressure and callers stay consistent. Emergency always re-polls.
        if elapsed < Duration::from_millis(150) && cached < PressureLevel::Emergency {
            return cached;
        }
        self.last_rss_check = now;

        let snap = self.memory_snapshot();
        publish_ibd_rss_anon_mb(snap.rss_anon_mb);
        if let Some((live, nominal)) = max_ahead_live {
            self.adjust_max_ahead_live(&snap, live, nominal);
        }

        if snap.rss_mb == 0 {
            return PressureLevel::None;
        }

        let level = self.pressure_level(&snap);
        self.log_pressure_transition_if_changed(level, &snap);
        level
    }

    /// Pin cached pressure for unit tests (`should_flush` returns this for ~150ms).
    #[cfg(test)]
    pub(crate) fn test_seed_pressure_level(&mut self, level: PressureLevel) {
        self.last_reported_pressure
            .store(level as u8, Ordering::Relaxed);
        self.last_rss_check = Instant::now();
    }

    /// Self-adapting cache cap: returns the desired UTXO cache cap (in entries) based on
    /// **actual current RSS**, not theoretical entry size. Throttled to one evaluation per ~2 s
    /// to avoid thrashing.
    ///
    /// The contract is simple: keep our own RSS under `rss_budget_mb`. If we're approaching
    /// the budget we shrink the cache; if we're well below it we allow the cap to grow back
    /// toward the nominal baseline (`utxo_max_entries`).
    ///
    /// This handles every memory-bloat source uniformly:
    ///   - mimalloc arena fragmentation (Arc<UTXO> churn leaving freed pages resident),
    ///   - RocksDB block cache + WBM growth as the DB matures,
    ///   - per-flush transient allocations,
    ///   - any other allocator that doesn't return memory to the OS promptly.
    ///
    /// Returns `Some(new_cap)` when the cap should change (caller must apply it via
    /// `IbdUtxoStore::tune_max_entries_for_pressure`); `None` when the current cap is still
    /// appropriate or when the throttle interval hasn't elapsed.
    pub(crate) fn compute_adaptive_cache_cap(&mut self) -> Option<usize> {
        let nominal = self.utxo_max_entries;
        if nominal == usize::MAX {
            return None;
        }
        let rss_mb = self.current_rss_mb();
        if rss_mb == 0 || self.rss_budget_mb == 0 {
            return None;
        }
        let budget = self.rss_budget_mb;
        // BUG FIX: read the previously-applied cap, NOT max(prev, nominal).
        // The old `.max(nominal)` meant `current` was always `nominal`, so every call
        // recomputed the shrink from the same starting point and the log always showed
        // "2684354 -> ..." — the cap never compounded downward across calls.
        let stored = self.last_adaptive_cap_entries.load(Ordering::Relaxed);
        let current = if stored == 0 { nominal } else { stored };
        // Ratio of current RSS to budget. >1.0 = over budget, <0.85 = comfortable headroom.
        // Compute in fixed-point (×1000) to avoid float in a hot path.
        let ratio_x1000 = (rss_mb as u128 * 1000 / budget.max(1) as u128) as u64;
        // Emergency (over budget): bypass all throttling and cooldown — shrink immediately.
        let is_emergency = ratio_x1000 >= 1000;
        // Throttle: at most one adaptation every 2 s. Prevents thrashing on noisy RSS reads.
        // Bypassed under Emergency so the response is immediate.
        if !is_emergency {
            let mut last = self
                .last_adaptive_cap_check
                .lock()
                .expect("adaptive_cap_check");
            if last.elapsed() < Duration::from_secs(2) {
                return None;
            }
            *last = Instant::now();
        }
        // Hard floor: never drop below 1/4 of nominal or 256k entries.
        // Rationale: the cache saves ~250 MB at nominal (2.7M × ~90 bytes/entry) vs ~90 MB at
        // nominal/4. The RSS savings from going below nominal/4 are marginal (~160 MB) but the
        // cache miss performance hit is severe (forced disk reads for every old UTXO). At h=400k+,
        // the non-cache RSS is ~7 GB — the cache is <5% of total RSS so shrinking it to 1/8
        // (the old floor) saves only ~80 MB while causing cache thrashing that hurts BPS.
        let hard_floor = (nominal / 4).max(256 * 1024);
        // Whether this poll sees pressure above the shrink threshold (≥ 80% of budget).
        let is_above_shrink_threshold = ratio_x1000 >= 800;
        // Maintain consecutive-high-RSS counter. We require at least 2 back-to-back
        // above-threshold polls (= ≥4 s at the 2 s poll rate) before cutting the cap.
        // This filters single-sample transient spikes (RocksDB flush burst, etc.) that
        // resolve within one poll interval and would otherwise trigger an unnecessary shrink.
        // Emergency bypasses this gate (act immediately, no false-spike concern when over budget).
        if is_above_shrink_threshold && !is_emergency {
            let prev = self
                .above_threshold_consecutive
                .fetch_add(1, Ordering::Relaxed);
            if prev < 1 {
                // First high-RSS poll: record but don't act yet.
                return None;
            }
        } else if !is_emergency {
            self.above_threshold_consecutive.store(0, Ordering::Relaxed);
        }
        let target = if is_emergency {
            // Over budget: shrink hard toward (budget * 0.65 / rss) fraction of current.
            // 0.65 coefficient targets 65% of budget so the next eviction batch actually
            // brings RSS below the budget threshold before the next poll.
            let scaled =
                (current as u128 * (budget as u128 * 650 / 1000) / rss_mb.max(1) as u128) as usize;
            scaled.max(hard_floor)
        } else if ratio_x1000 >= 900 {
            // Approaching budget (90-100%): cut 30%.
            (current * 7 / 10).max(hard_floor)
        } else if ratio_x1000 >= 800 {
            // Mild pressure (80-90%): cut 10%.
            // Check shrink cooldown: after any shrink we wait SHRINK_COOLDOWN_SECS before cutting
            // again. This gives mimalloc time to return freed pages to the OS and lets RSS
            // stabilise, breaking the rapid oscillation where we cut every 2 s indefinitely.
            const SHRINK_COOLDOWN_SECS: u64 = 20;
            {
                let last_shrink = self
                    .last_adaptive_cap_shrink
                    .lock()
                    .expect("last_shrink lock");
                if last_shrink.elapsed().as_secs() < SHRINK_COOLDOWN_SECS {
                    return None;
                }
            }
            (current * 90 / 100).max(hard_floor)
        } else if ratio_x1000 < 600 && current < nominal {
            // Comfortable headroom (<60%) and we're below the baseline cap: grow fast (25%).
            // This recovers the cache quickly after a transient RSS spike (e.g. RocksDB compaction).
            ((current * 125 / 100).min(nominal)).max(hard_floor)
        } else if ratio_x1000 < 700 && current < nominal {
            // Moderate headroom (60-70%) with cache below baseline: grow 15%.
            // Fixes the stuck-at-floor bug: with RSS at ~62% we never crossed the <60% threshold,
            // so the cache sat at the hard floor (335k) indefinitely after a compaction spike.
            ((current * 115 / 100).min(nominal)).max(hard_floor)
        } else if ratio_x1000 < 800 && current < nominal {
            // Light growth zone (70-80%) below baseline: grow 8% to slowly recover.
            ((current * 108 / 100).min(nominal)).max(hard_floor)
        } else {
            // 80%+ without hitting the shrink threshold above (cooldown active), or already at
            // nominal: stable — no change.
            return None;
        };
        if target == current {
            return None;
        }
        // Hysteresis: only emit an adjustment if it moves at least 3% of current cap —
        // small jiggles waste the eviction-walk CPU when shrinking.
        let delta = target.abs_diff(current);
        if delta < (current / 33).max(8 * 1024) {
            return None;
        }
        // Record shrink timestamp when the cap decreases.
        if target < current {
            let mut last_shrink = self
                .last_adaptive_cap_shrink
                .lock()
                .expect("last_shrink lock");
            *last_shrink = Instant::now();
            // Reset consecutive counter — the shrink consumed the accumulated pressure signal.
            self.above_threshold_consecutive.store(0, Ordering::Relaxed);
        }
        self.last_adaptive_cap_entries
            .store(target, Ordering::Relaxed);
        tracing::info!(
            "MemoryGuard: adaptive cache cap {} -> {} entries (rss={}MB / budget={}MB = {}.{}%, nominal={})",
            current,
            target,
            rss_mb,
            budget,
            ratio_x1000 / 10,
            ratio_x1000 % 10,
            nominal,
        );
        Some(target)
    }

    /// Current **anonymous** process RSS in MB.
    ///
    /// Returns `RssAnon` from `/proc/self/status` (Linux 4.5+), falling back to full `VmRSS`
    /// when `RssAnon` is not available.  File-backed mmap pages (e.g. LMDB data.mdb) are
    /// excluded from `RssAnon` and should not count against our anonymous-memory budget.
    pub(crate) fn current_rss_mb(&mut self) -> u64 {
        #[cfg(target_os = "linux")]
        {
            if proc_read_file("/proc/self/status", &mut self.proc_status_buf) {
                return proc_anon_rss_mb_from_status(&self.proc_status_buf);
            }
            0
        }
        #[cfg(all(not(target_os = "linux"), feature = "sysinfo"))]
        {
            use sysinfo::Pid;
            let pid = Pid::from(std::process::id() as usize);
            self.sys.refresh_process(pid);
            self.sys
                .process(pid)
                .map(|p| p.memory() / (1024 * 1024))
                .unwrap_or(0)
        }
        #[cfg(all(not(target_os = "linux"), not(feature = "sysinfo")))]
        0u64
    }

    /// Detailed memory snapshot for diagnostics. Returns (rss_mb, rss_anon_mb, rss_file_mb, vm_size_mb, sys_avail_mb).
    /// All values from /proc on Linux; zeros on other platforms.
    #[cfg(target_os = "linux")]
    pub(crate) fn memory_snapshot(&mut self) -> MemorySnapshot {
        let mut snap = MemorySnapshot::default();
        if proc_read_file("/proc/self/status", &mut self.proc_status_buf) {
            proc_parse_status_into(&self.proc_status_buf, &mut snap);
        }
        if proc_read_file("/proc/meminfo", &mut self.proc_meminfo_buf) {
            proc_parse_meminfo_into(&self.proc_meminfo_buf, &mut snap);
        }
        // NOTE: Do NOT subtract spec_adds_bytes from sys_avail_mb here.
        // sys_avail_mb comes from /proc/meminfo MemAvailable, which the kernel already
        // reduces to reflect our process RSS (including spec_adds heap). Double-counting
        // spec_adds caused artificial Critical pressure oscillation (67k events vs 1.8k)
        // because each deduction triggered max_ahead reduction → spec_adds shrinks →
        // pressure exits → max_ahead grows → spec_adds grows → Critical again.
        // spec_adds_bytes is retained for `adjust_max_ahead_live` capacity planning only.
        snap
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn memory_snapshot(&self) -> MemorySnapshot {
        MemorySnapshot::default()
    }

    /// Dynamic block buffer limit adjusted for current height.
    /// Blocks at h>300k average ~1MB; lower caps prevent OOM on 16GB boxes.
    pub(crate) fn buffer_limit(&self, current_height: u64) -> usize {
        Self::buffer_limit_for(self.block_buffer_base, self.total_mb, current_height)
    }

    /// Same as [`buffer_limit`](Self::buffer_limit) but usable from the feeder thread (no `&self` beyond scalars).
    pub(crate) fn buffer_limit_for(
        block_buffer_base: usize,
        total_mb: u64,
        current_height: u64,
    ) -> usize {
        let scale = match current_height {
            0..=100_000 => 100,
            100_001..=300_000 => 50,
            300_001..=480_000 => 33,
            480_001..=700_000 => 20,
            _ => 12,
        };
        let min_buf = if total_mb <= 16 * 1024 { 50 } else { 200 };
        (block_buffer_base * scale / 100).clamp(min_buf, 2_000)
    }

    /// Feeder RAM cap scales down with height (large blocks) and is bounded by buffer × ~900KB estimate.
    pub(crate) fn feeder_bytes_limit_for_height(&self, current_height: u64) -> usize {
        Self::feeder_bytes_for(
            self.feeder_buffer_bytes_limit,
            self.block_buffer_base,
            self.total_mb,
            current_height,
        )
    }

    pub(crate) fn feeder_bytes_for(
        feeder_buffer_bytes_limit: usize,
        block_buffer_base: usize,
        total_mb: u64,
        current_height: u64,
    ) -> usize {
        let tier = match current_height {
            0..=100_000 => 100u64,
            100_001..=300_000 => 72,
            300_001..=480_000 => 58,
            480_001..=700_000 => 48,
            _ => 40,
        };
        let scaled = (feeder_buffer_bytes_limit as u64 * tier / 100) as usize;
        let buf = Self::buffer_limit_for(block_buffer_base, total_mb, current_height);
        let cap_by_est_blocks = buf.saturating_mul(900_000);
        scaled.min(cap_by_est_blocks).max(32 * 1024 * 1024)
    }

    /// Diagnostic: current RSS and available memory (MB).
    pub(crate) fn memory_diag(&mut self) -> Option<(u64, u64)> {
        #[cfg(feature = "sysinfo")]
        {
            use sysinfo::Pid;
            let pid = Pid::from(std::process::id() as usize);
            self.sys.refresh_memory();
            self.sys.refresh_process(pid);
            let rss_mb = self
                .sys
                .process(pid)
                .map(|p| p.memory() / (1024 * 1024))
                .unwrap_or(0);
            let avail_mb = self.sys.available_memory() / (1024 * 1024);
            Some((rss_mb, avail_mb))
        }
        #[cfg(not(feature = "sysinfo"))]
        None
    }
}

/// MEM_REPORT accounted-vs-residual verdict (Phase 0b.4 / C2 gate).
///
/// H3 KEEP signature: ENGINE_MEM dominates, `UNEXPLAINED_ANON≈0` → working set is
/// **accounted engine**, not a mystery leak — C2 targets `eng_view` / residency.
/// High `post_rayon` residual → leak / allocator hunt before engine A/B.
/// See docs/RBITCOIN_VS_BLVM_IBD_ARCHITECTURE.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemReportAccountedVerdict {
    /// Engine owns most accounted anon; residual after pipeline/rayon is small.
    AccountableEngine,
    /// Large post-rayon unexplained — do not treat as pure eng_view working-set.
    ResidualMystery,
    /// Soft middle / empty sample — not enough to pick C2 vs leak track.
    Inconclusive,
}

/// Classify a single MEM_REPORT snapshot for C2 acceptance.
///
/// Inputs are the greppable fields from `[MEM_REPORT]`:
/// `anon`, `ENGINE_MEM … total=`, `accounted~`, `post_rayon=`, `file_backed`.
///
/// `file_backed` (RssFile page cache) is **not** residual anon — callers must not
/// fold it into unexplained (H3 glossary / 0b.7).
#[inline]
pub(crate) fn classify_mem_report_accounted(
    anon_mb: u64,
    engine_total_mb: u64,
    accounted_total_mb: u64,
    post_rayon_unexplained_mb: u64,
) -> MemReportAccountedVerdict {
    if anon_mb == 0 || accounted_total_mb == 0 {
        return MemReportAccountedVerdict::Inconclusive;
    }
    // Residual mystery: ≥1 GiB or ≥10% of anon still unexplained after pipeline+rayon.
    let mystery_floor = 1024u64.max(anon_mb / 10);
    if post_rayon_unexplained_mb > mystery_floor {
        return MemReportAccountedVerdict::ResidualMystery;
    }
    // Accountable engine: engine ≥ half of accounted, and residual ≤5% anon (min 256 MiB).
    let residual_ok = post_rayon_unexplained_mb <= 256u64.max(anon_mb / 20);
    let engine_dominant = engine_total_mb.saturating_mul(2) >= accounted_total_mb;
    if residual_ok && engine_dominant {
        return MemReportAccountedVerdict::AccountableEngine;
    }
    MemReportAccountedVerdict::Inconclusive
}

/// C2 working-set / `eng_view` A/B is fair only under accounted-engine MEM_REPORT.
#[inline]
pub(crate) fn c2_working_set_track_ok(verdict: MemReportAccountedVerdict) -> bool {
    matches!(verdict, MemReportAccountedVerdict::AccountableEngine)
}

#[cfg(test)]
mod memory_tier_tests {
    use super::{
        c2_working_set_track_ok, classify_mem_report_accounted, ibd_pressure_is_emergency,
        ibd_pressure_level_snapshot, publish_ibd_pressure, reset_ibd_pressure_on_session_end,
        MemoryGuard, MemReportAccountedVerdict, WorkloadClass, ROCKSDB_PIPELINE_RESERVE_MB,
        emergency_entry_anon_mb, stale_emergency_step_down_level, PressureLevel,
    };
    use crate::storage::database::DatabaseBackend;

    /// Phase 0b.4: H3-like accounted engine vs residual mystery.
    #[test]
    fn mem_report_accounted_vs_residual_c2_gate() {
        // H3 @414419-ish: ENGINE_MEM≈15.1G, unexplained≈0 → C2 working-set track.
        let h3 = classify_mem_report_accounted(
            15_500, // anon
            15_100, // engine total
            15_400, // accounted total
            0,      // post_rayon
        );
        assert_eq!(h3, MemReportAccountedVerdict::AccountableEngine);
        assert!(c2_working_set_track_ok(h3));

        // Large post-rayon residual → mystery; do not lead with eng_view A/B.
        let mystery = classify_mem_report_accounted(12_000, 4_000, 5_000, 3_500);
        assert_eq!(mystery, MemReportAccountedVerdict::ResidualMystery);
        assert!(!c2_working_set_track_ok(mystery));

        // Soft middle (engine not dominant, residual modest).
        let mid = classify_mem_report_accounted(8_000, 1_000, 4_000, 200);
        assert_eq!(mid, MemReportAccountedVerdict::Inconclusive);
        assert!(!c2_working_set_track_ok(mid));
    }

    /// zeus OOM replay: 92 GiB Shared box. cap_pct reduced 35->25 to avoid OOM alongside vLLM.
    /// Old budget was ~31 GiB; new budget ~23 GiB leaves room for 30+ GiB other workloads.
    #[test]
    fn shared_ninety_two_gb_envelope_and_pending_cap() {
        let total = 94_162_u64;
        let avail = 52_449_u64;
        let workload = MemoryGuard::detect_workload_class(total, avail, false);
        assert_eq!(workload, WorkloadClass::Shared);
        let rss = MemoryGuard::compute_rss_budget_mb(total, avail, workload);
        // Reduced from ~31 GiB (old 35% cap_pct) to ~23 GiB (new 25% cap_pct).
        assert!(rss >= 18_000 && rss <= 26_000, "rss_budget={rss}");
        // utxo_cache tier_max for Shared workloads reduced 8192 -> 4096 MB.
        let utxo_cache = ((rss * 45 / 100) as usize).min(4096);
        assert_eq!(utxo_cache, 4096);
        let pending = MemoryGuard::nominal_max_pending_ops(
            total,
            rss,
            utxo_cache,
            2_000_000,
            DatabaseBackend::RocksDB,
        );
        assert!(
            pending >= 4_000_000 && pending <= 8_000_000,
            "pending={pending}"
        );
    }

    #[test]
    fn heed3_pipeline_reserve_and_flush_interval() {
        assert_eq!(MemoryGuard::pipeline_reserve_mb(DatabaseBackend::Heed3), 512);
        assert_eq!(
            MemoryGuard::pipeline_reserve_mb(DatabaseBackend::RocksDB),
            ROCKSDB_PIPELINE_RESERVE_MB
        );
        // 64 GB host → Heed3 uses 200-block batches (LMDB single-writer; see storage_flush_interval_base)
        assert_eq!(
            MemoryGuard::storage_flush_interval_base(64, DatabaseBackend::Heed3),
            200
        );
        assert_eq!(
            MemoryGuard::storage_flush_interval_base(64, DatabaseBackend::RocksDB),
            2000
        );
        assert!(
            MemoryGuard::pipeline_reserve_mb(DatabaseBackend::Heed3)
                < MemoryGuard::pipeline_reserve_mb(DatabaseBackend::RocksDB)
        );
    }

    #[test]
    fn dedicated_workload_gets_higher_envelope_than_shared() {
        let total = 94_162_u64;
        let rss_shared = MemoryGuard::compute_rss_budget_mb(total, 52_449, WorkloadClass::Shared);
        let rss_dedicated =
            MemoryGuard::compute_rss_budget_mb(total, 90_000, WorkloadClass::Dedicated);
        assert!(rss_dedicated > rss_shared);
        assert_eq!(
            MemoryGuard::detect_workload_class(total, 90_000, false),
            WorkloadClass::Dedicated
        );
        // dedicated=true with low MemAvailable → Shared (avoid OOM on workstations)
        assert_eq!(
            MemoryGuard::detect_workload_class(total, 52_449, true),
            WorkloadClass::Shared
        );
    }

    #[test]
    fn engine_avail_mb_formula_not_clamped_to_boot_avail() {
        let rss_budget_mb = 37_664_u64;
        let boot_avail_mb = 10_300_u64;
        let engine_mb = rss_budget_mb.saturating_sub(6144).saturating_sub(4096);
        assert!(engine_mb > 20_000, "engine_mb={engine_mb}");
        let old_capped = (rss_budget_mb * 28 / 100)
            .max(2048)
            .min(engine_mb)
            .min(boot_avail_mb);
        assert_eq!(old_capped, boot_avail_mb, "old formula forced age-3 sizing");
        let new_hint = engine_mb.max(2048);
        assert_eq!(new_hint, 27_424, "new formula uses RSS budget headroom");
    }

    #[test]
    fn reset_ibd_pressure_on_session_end_clears_emergency_latch() {
        publish_ibd_pressure(PressureLevel::Emergency);
        assert!(ibd_pressure_is_emergency());
        reset_ibd_pressure_on_session_end();
        assert!(!ibd_pressure_is_emergency());
        assert_eq!(ibd_pressure_level_snapshot(), PressureLevel::None);
    }

    #[test]
    fn stale_emergency_not_cleared_at_live_entry_threshold_no_swap() {
        let budget = 37_664_u64;
        let emerg = emergency_entry_anon_mb(budget, true);
        assert_eq!(emerg, 32_014);
        // Live Emergency at entry: must not step down (old bug used 92% crit line = 34650).
        assert!(stale_emergency_step_down_level(32_074, budget, true).is_none());
        assert!(stale_emergency_step_down_level(32_014, budget, true).is_none());
        // Stale flag after memory dropped below entry (catch-up restart).
        assert_eq!(
            stale_emergency_step_down_level(31_000, budget, true),
            Some(PressureLevel::Critical)
        );
        assert_eq!(
            stale_emergency_step_down_level(20_000, budget, true),
            Some(PressureLevel::None)
        );
    }

    #[test]
    fn zeus_boot_with_dedicated_config_uses_shared_envelope() {
        let total = 94_162_u64;
        let avail = 64_865_u64; // ~69% — zeus with vLLM + IDE
        assert_eq!(
            MemoryGuard::detect_workload_class(total, avail, true),
            WorkloadClass::Shared
        );
        let rss = MemoryGuard::compute_rss_budget_mb(total, avail, WorkloadClass::Shared);
        assert!(
            rss >= 18_000 && rss <= 28_000,
            "rss_budget={rss} should be ~23 GiB Shared envelope"
        );
    }

    #[test]
    fn no_swap_caps_dedicated_budget() {
        let total = 94_162_u64;
        let avail = 64_865_u64;
        let dedicated = MemoryGuard::compute_rss_budget_mb(total, avail, WorkloadClass::Dedicated);
        // min(50% total, avail-8GiB) ≈ 47 GiB before no-swap cap in new_for_ibd
        assert!(dedicated <= 47_081, "dedicated={dedicated}");
        let no_swap_cap = avail.saturating_sub(8192).min(total * 40 / 100);
        assert_eq!(no_swap_cap, 37_664);
    }

    #[test]
    fn extended_sixteen_class_gets_tight_download_ahead_cap() {
        assert_eq!(MemoryGuard::tier_max_download_ahead_blocks(15921), 320);
        assert_eq!(MemoryGuard::tier_max_download_ahead_blocks(18 * 1024), 320);
        assert_eq!(
            MemoryGuard::tier_max_download_ahead_blocks(18 * 1024 + 1),
            512
        );
    }

    /// r24b @411k: sys_avail=63G, zram 99% full, proc_swap=650. Must not count as
    /// Emergency-class our-swap (that cut depth 32→8). Tight RAM still counts.
    #[test]
    fn r24b_ample_avail_does_not_count_process_swap_as_emergency() {
        assert!(
            !MemoryGuard::large_host_our_swap_counts(63_509),
            "r24b sys_avail=63G — vLLM-filled zram is not OOM"
        );
        assert!(
            !MemoryGuard::large_host_our_swap_counts(32 * 1024),
            "32 GiB avail is the gate, not under it"
        );
        assert!(
            MemoryGuard::large_host_our_swap_counts(32 * 1024 - 1),
            "just under 32 GiB still counts"
        );
        assert!(
            MemoryGuard::large_host_our_swap_counts(8_192),
            "tight RAM + our swap still Emergency-eligible"
        );
        assert!(
            !MemoryGuard::large_host_our_swap_counts(0),
            "unknown avail must not trip"
        );
    }
}

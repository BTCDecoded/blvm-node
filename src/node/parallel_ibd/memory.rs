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
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
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

#[inline]
pub(crate) fn publish_ibd_pressure(level: PressureLevel) {
    IBD_PRESSURE_LEVEL.store(level as u8, Ordering::Relaxed);
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
    total_mb: u64,
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

    pub(crate) fn new() -> Self {
        // Prefer /proc/meminfo on Linux — works regardless of feature flags.
        // This prevents the sysinfo-disabled fallback (8192/6144) from starving the UTXO cache
        // when built with --no-default-features.
        #[cfg(target_os = "linux")]
        let (mut total_mb, mut available_mb) = {
            let mut total = 0u64;
            let mut avail = 0u64;
            if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
                for line in s.lines() {
                    if line.starts_with("MemTotal:") {
                        total = proc_field_kb_to_mb(line);
                    } else if line.starts_with("MemAvailable:") {
                        avail = proc_field_kb_to_mb(line);
                    }
                }
            }
            (total, avail)
        };
        #[cfg(not(target_os = "linux"))]
        let (mut total_mb, mut available_mb) = (0u64, 0u64);

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

        // UTXO cache: this is the dominant factor in post-200k BPS. Each entry ≈ 560 B.
        //   2 GB ≈ 3.7M entries (cap hit at h≈220k, then every miss = disk read)
        //   4 GB ≈ 7.5M entries (covers UTXO churn deep into 500k+)
        //   6 GB ≈ 11M entries  (covers near 800k)
        // We size from `available_mb` (real free RAM at boot), not `budget_mb`, because the cache
        // IS the budget in steady state; everything else (RocksDB caches, write buffers,
        // validation thread stacks) is bounded and small relative to the cache.
        // BLVM_UTXO_CACHE_MAX_MB still caps when set (e.g. shared/memory-constrained hosts).
        let mut utxo_cache_mb = if total_gb >= 32 {
            ((available_mb * 60 / 100) as usize).clamp(4096, 16384)
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
        } else {
            ((budget_mb * 40 / 100) as usize).clamp(128, 768)
        };
        // On tight hosts (< 12 GiB total or < 6 GiB available at boot), keep the cache
        // conservative to avoid OOM with other workloads. A flat 192 MiB ceiling was stable
        // but crippled BPS on 16 GiB laptops with temporarily low MemAvailable; cap instead
        // at 7% of total RAM (192–384 MiB) so tiny machines stay near 192 MiB while larger
        // tight boxes retain a bit of working set.
        if total_gb < 12 || available_mb < 6144 {
            let tight_cap_mb = (total_mb.saturating_mul(7) / 100).clamp(192, 384) as usize;
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
        let defer_flush = total_gb >= 32
            || std::env::var("BLVM_IBD_DEFER_FLUSH")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        // UTXO flush cadence: how many blocks between pending-log flushes to RocksDB.
        // 50k on 64+ GB was too large — a single 130M-entry flush stalls RocksDB (write stall).
        // 10k keeps each batch to ~26M ops (~2.6 GB) which the write buffer handles without stall.
        // Overrideable via BLVM_IBD_DEFER_CHECKPOINT_INTERVAL for benchmarking.
        let defer_checkpoint_interval_base = if total_gb >= 64 {
            10_000
        } else if total_gb >= 32 {
            2_000
        } else {
            25_000
        };
        let defer_checkpoint_interval = std::env::var("BLVM_IBD_DEFER_CHECKPOINT_INTERVAL")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| (100..=500_000).contains(&n))
            .unwrap_or(defer_checkpoint_interval_base);

        // Block buffer: 10% of budget. 16GB caps lower (500KB estimate from early blocks
        // doesn't hold at h>300k where blocks average ~1MB).
        let block_buffer_base = {
            let buffer_mb = budget_mb * 10 / 100;
            let blocks = buffer_mb * 1024 / 500;
            (blocks as usize).clamp(100, 800)
        };

        // Storage flush interval (blocks buffered before async blockstore flush).
        // The byte-cap in storage_flush_pending_bytes_pressure_cap_for bounds block RAM
        // at late heights (where each block ≈ 1–1.5 MB) even under PressureLevel::None;
        // this count-based interval is the primary trigger at early heights (small blocks).
        // 300 on <32 GiB (was 500): reduces the peak "non-pressure" accumulation window
        // from ~750 MB to ~450 MB at h=700k (300 × 1.5 MB).
        let mut storage_flush_interval = if total_gb >= 32 { 2000 } else { 300 };
        if let Ok(s) = std::env::var("BLVM_IBD_STORAGE_FLUSH_INTERVAL") {
            if let Ok(n) = s.parse::<usize>() {
                // Same bounds as chunk_size-style knobs: avoid tiny flushes or OOM-sized buffers.
                storage_flush_interval = n.clamp(16, 4000);
            }
        }

        // Prefetch queue: scales with **spare** RAM at boot (pipeline depth without env).
        let prefetch_queue_size = {
            let hi: u64 = if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
                160
            } else if total_gb <= 24 {
                1024
            } else {
                2048
            };
            (spare_mb / 10).clamp(64, hi) as usize
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
        let max_utxo_flushes_auto: usize = if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            8
        } else if total_gb <= 24 {
            12
        } else if total_gb <= 32 {
            16
        } else {
            32
        };
        let max_utxo_flushes: usize = std::env::var("BLVM_IBD_MAX_UTXO_FLUSHES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .map(|n| n.clamp(1, 64))
            .unwrap_or(max_utxo_flushes_auto);
        // Blockstore async flushes are a separate pool from UTXO commits; modest extra overlap on
        // larger hosts improves post-cliff BPS without multiplying UTXO write-batch memory.
        let max_block_flushes_auto: usize = if total_gb <= 24 {
            max_utxo_flushes
        } else if total_gb <= 32 {
            max_utxo_flushes + max_utxo_flushes / 2
        } else {
            (max_utxo_flushes + max_utxo_flushes / 2).min(48)
        };
        let max_block_flushes: usize = std::env::var("BLVM_IBD_MAX_BLOCK_FLUSHES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .map(|n| n.clamp(1, 64))
            .unwrap_or(max_block_flushes_auto);

        tracing::info!(
            "MemoryGuard: total={}MB available={}MB spare≈{}MB budget={}MB (live /proc pressure) \
             utxo_cache={}MB ({}entries) flush_threshold={} defer_flush={} defer_checkpoint={} \
             buffer={} prefetch={} prefetch_queue={} max_ahead={} storage_flush={} feeder_bytes={}MB \
             max_utxo_flush={} max_block_flush={}",
            total_mb,
            available_mb,
            spare_mb,
            budget_mb,
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
            feeder_buffer_bytes_limit / (1024 * 1024),
            max_utxo_flushes,
            max_block_flushes,
        );

        // RSS budget: the binary self-shrinks the UTXO cache when actual process RSS
        // approaches this number. ≈50% of total RAM by default leaves ample headroom for
        // the OS, the IDE, and any other apps. Override via BLVM_RSS_BUDGET_MB only if you
        // know exactly what other RAM consumers exist on the host.
        let rss_budget_mb = std::env::var("BLVM_RSS_BUDGET_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&v| v >= 1024)
            .unwrap_or_else(|| {
                let pct = if total_mb >= 64 * 1024 {
                    70
                } else if total_mb >= 32 * 1024 {
                    60
                } else if total_mb >= 24 * 1024 {
                    57
                } else if total_mb <= 8 * 1024 {
                    // ≤8 GiB: small dedicated node (Raspberry Pi 5, etc.).
                    // On a headless node the OS uses ≤1–2 GB; blvm can use the rest.
                    // Base on total_mb × 60% (= 4915 MB for 8 GB) so a lightly-loaded Pi 5
                    // doesn’t trip EMERGENCY during normal IBD.
                    // Also respect available_mb × 75% so a genuinely-loaded machine still adapts.
                    let from_total = total_mb * 65 / 100;
                    let from_avail = available_mb * 75 / 100;
                    return from_total.max(from_avail).min(total_mb * 80 / 100).max(2048);
                } else if total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
                    // 8–18 GiB physical. Budget from available_mb (not total_mb): a desktop
                    // workload already consumes ~6 GB, so total_mb×54% = 8.6 GB on a 15.9 GB
                    // machine left only ~1.4 GB system headroom → OOM at h≈64k (swap-full).
                    // available_mb×60% accounts for what the OS can actually spare at boot.
                    let from_avail = (available_mb * 60 / 100).clamp(3000, 7000);
                    return from_avail.max(2048);
                } else {
                    // ~17–23 GiB
                    60
                };
                (total_mb * pct / 100).max(2048)
            });
        tracing::info!(
            "MemoryGuard: rss_budget={}MB ({}% of {}MB) — adaptive cache cap shrinks when RSS exceeds this",
            rss_budget_mb,
            rss_budget_mb * 100 / total_mb.max(1),
            total_mb,
        );

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
        if total_gb < 16 {
            256
        } else if total_gb <= 16 || total_mb <= Self::EXTENDED_SIXTEEN_CLASS_MB {
            // ~16 GiB class + BIOS‑reported “17 GiB” laptops (~≤18 GiB MemTotal)
            320
        } else if total_gb < 32 {
            512
        } else {
            1024
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
        self.clamp_pressure_to_process_budget(self.pressure_level_for(snap, current), snap)
    }

    /// Raise pressure when **process RSS** nears `rss_budget_mb`, even if host MemAvailable
    /// still looks ample (shared RAM with vLLM, IDE, etc.). Engine-mode IBD ignores the
    /// legacy UTXO cache; without this, only the age-tiered index grows toward OOM.
    fn clamp_pressure_to_process_budget(
        &self,
        mut level: PressureLevel,
        snap: &MemorySnapshot,
    ) -> PressureLevel {
        let b = self.rss_budget_mb;
        let r = snap.rss_mb;
        if b == 0 || r == 0 {
            return level;
        }
        if r >= b {
            return PressureLevel::Emergency;
        }
        if r >= b * 92 / 100 {
            level = level.max(PressureLevel::Critical);
        } else if r >= b * 82 / 100 {
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
        // ~28% of RSS budget → eviction age 5 on 64+ GiB hosts (age 6 caused ~67 GiB RSS OOM).
        let capped = (self.rss_budget_mb * 28 / 100).max(2048);
        capped.min(engine_mb).min(self.avail_mb)
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

        let emerg_up = (a > 0 && a < avail_emerg_up) || r > t * rss_emerg_pct_up / 100;
        let crit_up = (a > 0 && a < avail_crit_up) || r > t * rss_crit_pct_up / 100;
        let elev_up = (a > 0 && a < avail_elev_up) || r > t * rss_elev_pct_up / 100;
        let emerg_dn = (a == 0 || a >= avail_emerg_dn) && r <= t * rss_emerg_pct_dn / 100;
        let crit_dn = (a == 0 || a >= avail_crit_dn) && r <= t * rss_crit_pct_dn / 100;
        let elev_dn = (a == 0 || a >= avail_elev_dn) && r <= t * rss_elev_pct_dn / 100;

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
                let target = (nominal / 2).max(128);
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
        // Throttle: at most one adaptation every 2 s. Prevents thrashing on noisy RSS reads.
        {
            let mut last = self
                .last_adaptive_cap_check
                .lock()
                .expect("adaptive_cap_check");
            if last.elapsed() < Duration::from_secs(2) {
                return None;
            }
            *last = Instant::now();
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
        if is_above_shrink_threshold {
            let prev = self
                .above_threshold_consecutive
                .fetch_add(1, Ordering::Relaxed);
            if prev < 1 {
                // First high-RSS poll: record but don't act yet.
                return None;
            }
        } else {
            self.above_threshold_consecutive.store(0, Ordering::Relaxed);
        }
        let target = if ratio_x1000 >= 1000 {
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

    /// Current process RSS in MB.
    pub(crate) fn current_rss_mb(&mut self) -> u64 {
        #[cfg(target_os = "linux")]
        {
            if proc_read_file("/proc/self/status", &mut self.proc_status_buf) {
                return proc_rss_mb_from_status(&self.proc_status_buf);
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

#[cfg(test)]
mod memory_tier_tests {
    use super::MemoryGuard;

    #[test]
    fn extended_sixteen_class_gets_tight_download_ahead_cap() {
        assert_eq!(MemoryGuard::tier_max_download_ahead_blocks(15921), 320);
        assert_eq!(MemoryGuard::tier_max_download_ahead_blocks(18 * 1024), 320);
        assert_eq!(
            MemoryGuard::tier_max_download_ahead_blocks(18 * 1024 + 1),
            512
        );
    }
}

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

#[cfg(target_os = "linux")]
use std::io::Read;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
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

/// Latest pressure published by the validation thread (Linux). Coordinator reads this for §9 L3.
static IBD_PRESSURE_LEVEL: AtomicU8 = AtomicU8::new(0);

#[inline]
pub(crate) fn publish_ibd_pressure(level: PressureLevel) {
    IBD_PRESSURE_LEVEL.store(level as u8, Ordering::Relaxed);
}

#[inline]
pub(crate) fn ibd_pressure_is_emergency() -> bool {
    IBD_PRESSURE_LEVEL.load(Ordering::Relaxed) >= PressureLevel::Emergency as u8
}

#[inline]
pub(crate) fn ibd_pressure_level_snapshot() -> PressureLevel {
    PressureLevel::from_u8(IBD_PRESSURE_LEVEL.load(Ordering::Relaxed))
}

/// Last level from [`MemoryGuard::should_flush`] / pressure hysteresis (validation thread).
#[inline]
pub(crate) fn last_reported_pressure_level(mg: &MemoryGuard) -> PressureLevel {
    PressureLevel::from_u8(mg.last_reported_pressure.load(Ordering::Relaxed))
}

/// TidesDB hard limit: max ops per transaction (TDB_MAX_TXN_OPS=100000 in tidesdb.c).
pub(crate) const TIDESDB_MAX_TXN_OPS: usize = 50_000;

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
}

impl std::fmt::Display for MemorySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rss={}MB(anon={}MB file={}MB shm={}MB) vm={}MB mem_total={}MB sys_avail={}MB",
            self.rss_mb,
            self.rss_anon_mb,
            self.rss_file_mb,
            self.rss_shmem_mb,
            self.vm_size_mb,
            self.mem_total_mb,
            self.sys_avail_mb,
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
    budget_mb: u64,
    /// Derived UTXO cache max in MB (50% of budget).
    utxo_cache_mb: usize,
    /// Max UTXO cache entries (`utxo_cache_mb` / ~560 B per entry empirical).
    pub(crate) utxo_max_entries: usize,
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
    pub(crate) fn new() -> Self {
        #[cfg(feature = "sysinfo")]
        let (total_mb, available_mb, mut sys) = {
            use sysinfo::System;
            let mut s = System::new_all();
            s.refresh_memory();
            let t = s.total_memory() / (1024 * 1024);
            let a = s.available_memory() / (1024 * 1024);
            (t, a, s)
        };
        #[cfg(not(feature = "sysinfo"))]
        let (total_mb, available_mb) = (8192u64, 6144u64);

        let total_gb = (total_mb + 512) / 1024;

        // Budget: fraction of total RAM. On <=16 GB, use a FIXED 10% of total
        // (about 1.6 GB) so the budget never inflates when boot has lots of free RAM.
        // The old formula scaled with available_mb, giving budget=2755 when 6 GB was
        // free at boot, then OOM when cargo check ate the headroom later.
        let mut budget_mb = if total_gb <= 16 {
            (total_mb * 10 / 100).clamp(512, 1600)
        } else {
            (total_mb * 28 / 100).min(available_mb * 45 / 100).max(512)
        };

        // Spare: how much room we have for pipeline depth. On <=16 GB, cap to 15%
        // of total regardless of what MemAvailable says at boot.
        let effective_avail = if total_gb <= 16 {
            available_mb.min(total_mb * 40 / 100)
        } else {
            available_mb
        };
        let os_reserve_mb = (total_mb * 22 / 100).max(2816);
        let spare_mb = effective_avail.saturating_sub(os_reserve_mb).max(256);

        // UTXO cache: 40% of budget. BLVM_UTXO_CACHE_MAX_MB caps when set (e.g. memory-constrained systems).
        let mut utxo_cache_mb = ((budget_mb * 40 / 100) as usize).clamp(128, 3072);
        // 16 GiB: per-entry DashMap overhead is ~500-600 bytes — `utxo_max_entries` uses /560 below.
        // 128 MB label → ~240k entries → ~130–150 MB real RSS; reduces cache thrash post-cliff vs 64 MB.
        if total_gb <= 16 {
            utxo_cache_mb = utxo_cache_mb.min(128);
        }
        if let Some(mb) = std::env::var("BLVM_UTXO_CACHE_MAX_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        {
            if mb > 0 {
                utxo_cache_mb = utxo_cache_mb.min(mb);
            }
        }
        // Empirical ~550 B/entry (DashMap + Arc<UTXO> + fragmentation); /320 underestimated real RSS.
        let utxo_max_entries = utxo_cache_mb * 1024 * 1024 / 560;

        // UTXO flush threshold — smaller batches on low-RAM hosts reduce peak pending_writes RSS.
        let utxo_flush_threshold = if total_gb >= 32 {
            400_000
        } else if total_gb >= 24 {
            200_000
        } else if total_gb >= 16 {
            50_000
        } else {
            40_000
        }
        .min(TIDESDB_MAX_TXN_OPS);

        let crit_rss_threshold_mb = std::env::var("BLVM_IBD_PRESSURE_CRIT_RSS_MB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| (800..=4000).contains(&n))
            .unwrap_or(1600);

        // Defer flush (UTXO packages mostly at checkpoints) only on very large hosts by default.
        // On 32–63 GiB it was auto-on and suppressed steady `maybe_take_flush_batch` work, pooling
        // hundreds/thousands of blocks into rare huge batches at blockstore barriers — bad overlap
        // with the single committer and higher barrier wait. Opt-in below 64 GiB: `BLVM_IBD_DEFER_FLUSH=1`.
        let defer_flush = total_gb >= 64
            || ((16..64).contains(&total_gb)
                && std::env::var("BLVM_IBD_DEFER_FLUSH")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false));
        let defer_checkpoint_interval = if total_gb >= 64 { 50_000 } else { 25_000 };

        // Block buffer: 10% of budget. 16GB caps lower (500KB estimate from early blocks
        // doesn't hold at h>300k where blocks average ~1MB).
        let block_buffer_base = {
            let buffer_mb = budget_mb * 10 / 100;
            let blocks = buffer_mb * 1024 / 500;
            (blocks as usize).clamp(100, 800)
        };

        // Storage flush interval (blocks buffered before async blockstore flush).
        let mut storage_flush_interval = if total_gb >= 32 { 2000 } else { 500 };
        if let Ok(s) = std::env::var("BLVM_IBD_STORAGE_FLUSH_INTERVAL") {
            if let Ok(n) = s.parse::<usize>() {
                // Same bounds as chunk_size-style knobs: avoid tiny flushes or OOM-sized buffers.
                storage_flush_interval = n.clamp(16, 4000);
            }
        }

        // Prefetch queue: scales with **spare** RAM at boot (pipeline depth without env).
        let prefetch_queue_size = {
            let hi: u64 = if total_gb <= 16 {
                256
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
            let tier_cap = Self::tier_max_download_ahead_blocks_from_gb(total_gb);
            v.min(tier_cap)
        };

        // Prefetch cache (entries); upper bound scales down on 16GB-class machines.
        let prefetch_limit = {
            let cache_mb = budget_mb * 3 / 100;
            let hi = if total_gb <= 16 {
                12_000
            } else if total_gb <= 24 {
                35_000
            } else {
                50_000
            };
            let spare_boost = ((spare_mb / 1024) as usize).saturating_mul(800);
            (((cache_mb * 1024 * 1024 / 400) as usize).saturating_add(spare_boost)).clamp(5_000, hi)
        };

        // Feeder buffer byte cap — tighter on 16GB to avoid holding too many ~1MB blocks.
        let feeder_pct = if total_gb <= 16 { 2 } else { 5 };
        let feeder_buffer_bytes_limit = (budget_mb * feeder_pct / 100 * 1024 * 1024) as usize;

        // Flush concurrency: each std::thread::spawn takes ~8MB stack + RocksDB WriteBatch
        // internal buffers. On 16GiB boxes the old 1024 limit was the OOM root cause.
        let max_utxo_flushes: usize = if total_gb <= 16 {
            4
        } else if total_gb <= 24 {
            8
        } else if total_gb <= 32 {
            16
        } else {
            32
        };
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
             utxo_cache={}MB ({}entries) flush_threshold={} defer_flush={} buffer={} \
             prefetch={} prefetch_queue={} max_ahead={} storage_flush={} feeder_bytes={}MB \
             max_utxo_flush={} max_block_flush={}",
            total_mb,
            available_mb,
            spare_mb,
            budget_mb,
            utxo_cache_mb,
            utxo_max_entries,
            utxo_flush_threshold,
            defer_flush,
            block_buffer_base,
            prefetch_limit,
            prefetch_queue_size,
            max_ahead_blocks,
            storage_flush_interval,
            feeder_buffer_bytes_limit / (1024 * 1024),
            max_utxo_flushes,
            max_block_flushes,
        );

        Self {
            total_mb,
            budget_mb,
            utxo_cache_mb,
            utxo_max_entries,
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
        let base = self.storage_flush_interval;
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
        let pct: u64 = match pressure {
            PressureLevel::None | PressureLevel::Elevated => return None,
            PressureLevel::Critical => 6,
            PressureLevel::Emergency => 4,
        };
        let raw = self
            .budget_mb
            .saturating_mul(1024 * 1024)
            .saturating_mul(pct)
            / 100;
        Some(raw.max(32 * 1024 * 1024))
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

    /// Upper bound on download-ahead for this host tier (blocks). Spare-derived nominal is always
    /// `min(spare_formula, this)` so RAM-tight machines stay bounded.
    #[inline]
    fn tier_max_download_ahead_blocks_from_gb(total_gb: u64) -> u64 {
        if total_gb < 16 {
            256
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
        let total_gb = (self.total_mb + 512) / 1024;
        if total_gb <= 16 {
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
        self.pressure_level_for(snap, current)
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
            // <=16 GiB: absolute RSS ceilings.
            // Entry thresholds (unchanged -- trigger when crossing upward):
            let crit_rss = self.crit_rss_threshold_mb;
            let emerg_up = r >= 2000 || (a > 0 && a < 2048);
            let crit_up = r >= crit_rss || (a > 0 && a < 2560);
            let elev_up = r >= 1200 || (a > 0 && a < 3072);
            // Exit thresholds (150-200 MB lower -- must clear before descending):
            let emerg_dn = r < 1800 && (a == 0 || a >= 2200);
            let crit_dn = r < 1450 && (a == 0 || a >= 2700);
            let elev_dn = r < 1050 && (a == 0 || a >= 3200);

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
                let tier_cap =
                    Self::tier_max_download_ahead_blocks_from_gb((self.total_mb + 512) / 1024);
                let ceil = if self.total_mb <= 16 * 1024 {
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

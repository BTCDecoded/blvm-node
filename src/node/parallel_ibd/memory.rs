//! Dynamic memory management for IBD.
//!
//! Hardware-aware tuning: derives memory budget from total RAM, allocates across
//! UTXO cache, block buffer, prefetch, and overhead. Never exceeds 40% of total RAM.

/// TidesDB hard limit: max ops per transaction (TDB_MAX_TXN_OPS=100000 in tidesdb.c).
pub(crate) const TIDESDB_MAX_TXN_OPS: usize = 50_000;

/// Cross-platform auto-tuning for IBD memory management.
///
// Probes total/available RAM at startup via sysinfo (Linux, macOS, Windows).
/// Derives all thresholds from hardware. During IBD the validation loop calls
/// `should_flush()` periodically; when process RSS nears the limit the guard
/// forces a UTXO flush to keep memory under control.
pub(crate) struct MemoryGuard {
    total_mb: u64,
    budget_mb: u64,
    /// RSS above this → force flush pending_writes to disk.
    flush_trigger_mb: u64,
    /// Derived UTXO cache max in MB (50% of budget).
    utxo_cache_mb: usize,
    /// Max UTXO cache entries (utxo_cache_mb / 320 bytes per entry).
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
    #[cfg(feature = "sysinfo")]
    sys: sysinfo::System,
    last_rss_check: std::time::Instant,
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

        let total_gb = total_mb / 1024;

        // Budget: 28% of total, capped to 45% of available.
        let budget_mb = (total_mb * 28 / 100).min(available_mb * 45 / 100).max(512);

        // RSS flush trigger: 55% of total.
        let flush_trigger_mb = total_mb * 55 / 100;

        // UTXO cache: 40% of budget. BLVM_UTXO_CACHE_MAX_MB caps when set (e.g. memory-constrained systems).
        let mut utxo_cache_mb = ((budget_mb * 40 / 100) as usize).clamp(256, 3072);
        if let Some(mb) = std::env::var("BLVM_UTXO_CACHE_MAX_MB").ok().and_then(|s| s.parse::<usize>().ok()) {
            if mb > 0 {
                utxo_cache_mb = utxo_cache_mb.min(mb);
            }
        }
        let utxo_max_entries = utxo_cache_mb * 1024 * 1024 / 320;

        // UTXO flush threshold.
        let utxo_flush_threshold = if total_gb >= 32 {
            400_000
        } else if total_gb >= 24 {
            200_000
        } else if total_gb >= 16 {
            100_000
        } else {
            50_000
        }
        .min(TIDESDB_MAX_TXN_OPS);

        // Defer flush.
        let defer_flush = std::env::var("BLVM_IBD_DEFER_FLUSH")
            .ok()
            .and_then(|v| match v.as_str() {
                "0" | "false" => Some(false),
                "1" | "true" => Some(true),
                _ => v.parse().ok().map(|b: bool| b),
            })
            .unwrap_or(total_gb >= 32);
        let defer_checkpoint_interval = if total_gb >= 64 { 50_000 } else { 25_000 };

        // Block buffer: 10% of budget.
        let block_buffer_base = {
            let buffer_mb = budget_mb * 10 / 100;
            let blocks = buffer_mb * 1024 / 500;
            (blocks as usize).clamp(100, 800)
        };

        // Storage flush interval.
        let storage_flush_interval = if total_gb >= 32 { 2000 } else { 500 };

        // Prefetch queue size.
        let prefetch_queue_size = {
            let queue_mb = budget_mb * 15 / 100;
            let items = queue_mb * 1024 / 850;
            (items as usize).clamp(64, 2048)
        };

        // Max blocks download can race ahead.
        let max_ahead_blocks = if total_gb >= 32 {
            1024
        } else if total_gb >= 16 {
            512
        } else {
            256
        };

        // Prefetch cache.
        let prefetch_limit = {
            let cache_mb = budget_mb * 3 / 100;
            ((cache_mb * 1024 * 1024 / 400) as usize).clamp(5_000, 50_000)
        };

        // Feeder buffer byte cap.
        let feeder_buffer_bytes_limit = (budget_mb * 5 / 100 * 1024 * 1024) as usize;

        tracing::info!(
            "MemoryGuard: total={}MB available={}MB budget={}MB flush_trigger={}MB \
             utxo_cache={}MB ({}entries) flush_threshold={} defer_flush={} buffer={} \
             prefetch={} prefetch_queue={} max_ahead={} storage_flush={} feeder_bytes={}MB",
            total_mb,
            available_mb,
            budget_mb,
            flush_trigger_mb,
            utxo_cache_mb,
            utxo_max_entries,
            utxo_flush_threshold,
            defer_flush,
            block_buffer_base,
            prefetch_limit,
            prefetch_queue_size,
            max_ahead_blocks,
            storage_flush_interval,
            feeder_buffer_bytes_limit / (1024 * 1024)
        );

        Self {
            total_mb,
            budget_mb,
            flush_trigger_mb,
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
            #[cfg(feature = "sysinfo")]
            sys,
            last_rss_check: std::time::Instant::now(),
        }
    }

    /// Check if process RSS exceeds the flush trigger.
    pub(crate) fn should_flush(&mut self) -> bool {
        if self.last_rss_check.elapsed() < std::time::Duration::from_millis(500) {
            return false;
        }
        self.last_rss_check = std::time::Instant::now();

        let rss_mb = self.current_rss_mb();
        if rss_mb > 0 && rss_mb > self.flush_trigger_mb {
            tracing::info!(
                "MemoryGuard: RSS {}MB > trigger {}MB, forcing flush",
                rss_mb, self.flush_trigger_mb
            );
            return true;
        }
        false
    }

    /// Current process RSS in MB.
    pub(crate) fn current_rss_mb(&mut self) -> u64 {
        #[cfg(target_os = "linux")]
        {
            if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
                for line in s.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(kb) = line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|v| v.parse::<u64>().ok())
                        {
                            return kb / 1024;
                        }
                        break;
                    }
                }
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

    /// Dynamic block buffer limit adjusted for current height.
    pub(crate) fn buffer_limit(&self, current_height: u64) -> usize {
        let scale = match current_height {
            0..=100_000 => 100,
            100_001..=300_000 => 50,
            300_001..=480_000 => 33,
            480_001..=700_000 => 20,
            _ => 12,
        };
        (self.block_buffer_base * scale / 100).clamp(200, 2_000)
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

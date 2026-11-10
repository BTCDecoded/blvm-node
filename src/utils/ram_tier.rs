//! Shared host RAM **MiB → GiB tier** helpers for [`MemoryGuard`] and RocksDB sizing.
//!
//! `BLVM_TOTAL_RAM_MB` / `/proc/meminfo` / `BLVM_RAM_GB` handling here matches
//! [`crate::node::parallel_ibd::memory::MemoryGuard::new`] for **`total_mb`**.
//! `BLVM_SYS_AVAIL_MB` handling matches MemoryGuard for **`avail_mb`**.

/// GiB label for tier tables: `(total_ram_mib + 512) / 1024`.
#[inline]
pub(crate) fn total_gb_rounded(total_ram_mib: u64) -> u64 {
    (total_ram_mib + 512) / 1024
}

/// Parse a single field from `/proc/meminfo` (Linux). Returns the value in MiB, or 0.
#[cfg(target_os = "linux")]
fn parse_proc_meminfo_field(content: &str, field: &str) -> u64 {
    for line in content.lines() {
        if line.starts_with(field) {
            let kib = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            return kib / 1024;
        }
    }
    0
}

/// Total RAM MiB for tier ladders (`MemoryGuard` total_mb through `BLVM_TOTAL_RAM_MB`; then `8192` if unknown).
pub(crate) fn probe_total_ram_mib() -> u64 {
    #[cfg(target_os = "linux")]
    let mut total_mb = {
        let content = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        parse_proc_meminfo_field(&content, "MemTotal:")
    };
    #[cfg(not(target_os = "linux"))]
    let mut total_mb = 0u64;

    #[cfg(feature = "sysinfo")]
    if total_mb == 0 {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        total_mb = sys.total_memory() / (1024 * 1024);
    }

    if let Some(mb) = std::env::var("BLVM_TOTAL_RAM_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
    {
        total_mb = mb;
    }

    if total_mb == 0 {
        total_mb = std::env::var("BLVM_RAM_GB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|g| g.saturating_mul(1024))
            .unwrap_or(8192);
    }

    total_mb
}

/// Available RAM MiB at probe time (`MemAvailable` from `/proc/meminfo`).
///
/// Honors `BLVM_SYS_AVAIL_MB` override (same env var as [`MemoryGuard`]).
/// Falls back to `total_mb / 2` (a conservative estimate) when `/proc/meminfo`
/// and sysinfo both fail.  Returns 0 only when called with an explicit override
/// of 0 (disabling auto-detection).
pub(crate) fn probe_avail_ram_mib() -> u64 {
    // Env override wins — same semantic as MemoryGuard::new.
    if let Some(mb) = std::env::var("BLVM_SYS_AVAIL_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
    {
        return mb;
    }

    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let avail = parse_proc_meminfo_field(&content, "MemAvailable:");
        if avail > 0 {
            return avail;
        }
    }

    #[cfg(feature = "sysinfo")]
    {
        let mut sys = sysinfo::System::new_all();
        sys.refresh_memory();
        let avail = sys.available_memory() / (1024 * 1024);
        if avail > 0 {
            return avail;
        }
    }

    // Conservative fallback: half of total RAM.
    probe_total_ram_mib() / 2
}

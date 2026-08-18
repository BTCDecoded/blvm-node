//! IBD (Initial Block Download), bandwidth protection, background tasks, and replay protection config.

use serde::{Deserialize, Serialize};

/// Process-latched env parse (prod OnceLock / test re-read). Local copy so config
/// does not depend on `parallel_ibd`.
macro_rules! latch_env {
    ($t:ty, $body:block) => {{
        #[cfg(test)]
        {
            $body
        }
        #[cfg(not(test))]
        {
            static CACHED: ::std::sync::OnceLock<$t> = ::std::sync::OnceLock::new();
            *CACHED.get_or_init(|| $body)
        }
    }};
}

/// IBD bandwidth protection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbdProtectionConfig {
    #[serde(default = "default_ibd_max_bandwidth_per_peer_per_day")]
    pub max_bandwidth_per_peer_per_day_gb: u64,

    #[serde(default = "default_ibd_max_bandwidth_per_peer_per_hour")]
    pub max_bandwidth_per_peer_per_hour_gb: u64,

    #[serde(default = "default_ibd_max_bandwidth_per_ip_per_day")]
    pub max_bandwidth_per_ip_per_day_gb: u64,

    #[serde(default = "default_ibd_max_bandwidth_per_ip_per_hour")]
    pub max_bandwidth_per_ip_per_hour_gb: u64,

    #[serde(default = "default_ibd_max_bandwidth_per_subnet_per_day")]
    pub max_bandwidth_per_subnet_per_day_gb: u64,

    #[serde(default = "default_ibd_max_bandwidth_per_subnet_per_hour")]
    pub max_bandwidth_per_subnet_per_hour_gb: u64,

    #[serde(default = "default_ibd_max_concurrent_serving")]
    pub max_concurrent_ibd_serving: usize,

    #[serde(default = "default_ibd_request_cooldown")]
    pub ibd_request_cooldown_seconds: u64,

    #[serde(default = "default_ibd_suspicious_reconnection_threshold")]
    pub suspicious_reconnection_threshold: u32,

    #[serde(default = "default_ibd_reputation_ban_threshold")]
    pub reputation_ban_threshold: i32,

    #[serde(default = "crate::config::default_false")]
    pub enable_emergency_throttle: bool,

    #[serde(default = "default_ibd_emergency_throttle_percent")]
    pub emergency_throttle_percent: u8,
}

fn default_ibd_max_bandwidth_per_peer_per_day() -> u64 {
    50
}
fn default_ibd_max_bandwidth_per_peer_per_hour() -> u64 {
    10
}
fn default_ibd_max_bandwidth_per_ip_per_day() -> u64 {
    100
}
fn default_ibd_max_bandwidth_per_ip_per_hour() -> u64 {
    20
}
fn default_ibd_max_bandwidth_per_subnet_per_day() -> u64 {
    500
}
fn default_ibd_max_bandwidth_per_subnet_per_hour() -> u64 {
    100
}
fn default_ibd_max_concurrent_serving() -> usize {
    3
}
fn default_ibd_request_cooldown() -> u64 {
    3600
}
fn default_ibd_suspicious_reconnection_threshold() -> u32 {
    3
}
fn default_ibd_reputation_ban_threshold() -> i32 {
    -100
}
fn default_ibd_emergency_throttle_percent() -> u8 {
    50
}

impl Default for IbdProtectionConfig {
    fn default() -> Self {
        Self {
            max_bandwidth_per_peer_per_day_gb: 50,
            max_bandwidth_per_peer_per_hour_gb: 10,
            max_bandwidth_per_ip_per_day_gb: 100,
            max_bandwidth_per_ip_per_hour_gb: 20,
            max_bandwidth_per_subnet_per_day_gb: 500,
            max_bandwidth_per_subnet_per_hour_gb: 100,
            max_concurrent_ibd_serving: 3,
            ibd_request_cooldown_seconds: 3600,
            suspicious_reconnection_threshold: 3,
            reputation_ban_threshold: -100,
            enable_emergency_throttle: false,
            emergency_throttle_percent: 50,
        }
    }
}

/// Parallel IBD download configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbdConfig {
    #[serde(default = "default_ibd_chunk_size")]
    pub chunk_size: u64,

    #[serde(default = "default_ibd_download_timeout")]
    pub download_timeout_secs: u64,

    #[serde(default = "default_ibd_mode")]
    pub mode: String,

    #[serde(default)]
    pub preferred_peers: Vec<String>,

    #[serde(default)]
    pub max_ahead_blocks: Option<u64>,

    #[serde(default)]
    pub memory_only: bool,

    #[serde(default)]
    pub dump_dir: Option<String>,

    #[serde(default)]
    pub snapshot_dir: Option<String>,

    #[serde(default = "default_ibd_yield_interval")]
    pub yield_interval: u64,

    #[serde(default = "default_ibd_eviction")]
    pub eviction: String,

    #[serde(default)]
    pub earliest_first: bool,

    #[serde(default)]
    pub prefetch_workers: Option<usize>,

    #[serde(default)]
    pub prefetch_queue_size: Option<usize>,

    #[serde(default = "default_ibd_utxo_prefetch_lookahead")]
    pub utxo_prefetch_lookahead: u64,

    #[serde(default = "default_ibd_max_blocks_in_transit")]
    pub max_blocks_in_transit_per_peer: usize,

    #[serde(default = "default_ibd_headers_timeout")]
    pub headers_timeout_secs: u64,

    #[serde(default = "default_ibd_headers_max_failures")]
    pub headers_max_failures: u32,

    /// Treat this node as dedicated to IBD (headless sync): use the larger RSS envelope even
    /// when MemAvailable/MemTotal < 70%. Also set via `BLVM_DEDICATED_NODE=1` or `BLVM_IBD_EXCLUSIVE=1`.
    #[serde(default)]
    pub dedicated: bool,

    /// Age-tiered UTXO engine (default on). Opt out with `BLVM_IBD_ENGINE=0` or `utxo_engine = false`.
    #[serde(default = "default_utxo_engine")]
    pub utxo_engine: bool,
}

fn default_utxo_engine() -> bool {
    true
}

/// Mode T / archive: listen + answer `getdata`, skip outbound discovery and IBD.
///
/// Env `BLVM_SERVE_ONLY=1` (aliases: `true`/`on`/`yes`). Default **off**.
/// Used by loopback/LAN blvm archive peers so startup does not LAN-scan, DNS-dial,
/// or rewind UTXO watermarks via zero-peer local replay.
pub fn ibd_serve_only() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_SERVE_ONLY")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("1") | Some("true") | Some("on") | Some("yes") => true,
            _ => false,
        }
    })
}

/// Mode T A4: how many block `getdata` loads may run ahead on the blocking pool
/// while sends stay strictly in inventory order.
///
/// - Env `BLVM_GETDATA_SERVE_PIPE` (1..=8) overrides when set.
/// - Else: **4** under `BLVM_SERVE_ONLY=1`, **1** (sequential) otherwise.
///
/// Cap is intentional — historical `BLVM_GETDATA_SERVE_CONCURRENCY=64` full-fanout
/// flooded LMDB and is not wired. Sync-on-async (tc285) stays banned.
pub fn getdata_serve_pipe_depth() -> usize {
    latch_env!(usize, {
        if let Ok(v) = std::env::var("BLVM_GETDATA_SERVE_PIPE") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return n.clamp(1, 8);
            }
        }
        if ibd_serve_only() {
            4
        } else {
            1
        }
    })
}

/// Whether parallel IBD uses the age-tiered UTXO engine (default **true**).
///
/// Precedence: `BLVM_IBD_ENGINE` env > `[ibd].utxo_engine` > default true.
pub fn ibd_engine_enabled(config: Option<&IbdConfig>) -> bool {
    if let Ok(v) = std::env::var("BLVM_IBD_ENGINE") {
        let v = v.trim();
        if v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("no") {
            return false;
        }
        if v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes") {
            return true;
        }
    }
    config.map(|c| c.utxo_engine).unwrap_or(true)
}

/// Engine IBD durability tuning (checkpoint export + incremental MuHash persist).
#[derive(Debug, Clone, Copy)]
pub struct IbdEngineDurabilityConfig {
    /// Fixed checkpoint interval in blocks (`None` = adaptive).
    pub checkpoint_interval: Option<i32>,
    /// Adaptive interval lower bound (blocks between full ckpt exports).
    pub checkpoint_min_interval: i32,
    /// Adaptive interval upper bound.
    pub checkpoint_max_interval: i32,
    /// Target wall time between checkpoint exports when adaptive (seconds). Scales block
    /// interval down when validation BPS is low (`interval ≈ bps × target_secs`, capped by
    /// UTXO/export-cost ceiling). Also scales interval up when a single export exceeds this.
    pub checkpoint_target_secs: u64,
    /// Persist rolling MuHash + validation tip every N blocks (no UTXO snapshot; cheap metadata).
    pub muhash_persist_interval: u64,
}

impl Default for IbdEngineDurabilityConfig {
    fn default() -> Self {
        Self {
            checkpoint_interval: None,
            checkpoint_min_interval: 500,
            // Mid-chain piggyback exports are 90–200s @ 30–60M UTXOs (W173). A 10k
            // ceiling forced ~5k-block cadence once UTXO scaling + BPS min() interacted;
            // 50k matches the high-UTXO default and E1 "fewer, longer-spaced" policy.
            checkpoint_max_interval: 50_000,
            checkpoint_target_secs: 60,
            muhash_persist_interval: 200,
        }
    }
}

/// Resolved engine durability knobs: env > defaults (toml fields may be added later).
pub fn ibd_engine_durability_config(_config: Option<&IbdConfig>) -> IbdEngineDurabilityConfig {
    let mut d = IbdEngineDurabilityConfig::default();
    if let Ok(v) = std::env::var("BLVM_IBD_CHECKPOINT_INTERVAL") {
        if let Ok(n) = v.trim().parse::<i32>() {
            if n > 0 {
                d.checkpoint_interval = Some(n);
            }
        }
    }
    if let Ok(v) = std::env::var("BLVM_IBD_CHECKPOINT_MIN_INTERVAL") {
        if let Ok(n) = v.trim().parse::<i32>() {
            if n > 0 {
                d.checkpoint_min_interval = n;
            }
        }
    }
    if let Ok(v) = std::env::var("BLVM_IBD_CHECKPOINT_MAX_INTERVAL") {
        if let Ok(n) = v.trim().parse::<i32>() {
            if n > 0 {
                d.checkpoint_max_interval = n.max(d.checkpoint_min_interval);
            }
        }
    }
    if let Ok(v) = std::env::var("BLVM_IBD_CHECKPOINT_TARGET_SECS") {
        if let Ok(n) = v.trim().parse::<u64>() {
            if n > 0 {
                d.checkpoint_target_secs = n;
            }
        }
    }
    if let Ok(v) = std::env::var("BLVM_IBD_ENGINE_MUHASH_INTERVAL") {
        if let Ok(n) = v.trim().parse::<u64>() {
            if n > 0 {
                d.muhash_persist_interval = n;
            }
        }
    }
    d
}

/// Whether engine IBD folds per-block MuHash in workers (`BLVM_IBD_ENGINE_MUHASH=0` disables).
pub fn ibd_engine_muhash_enabled() -> bool {
    match std::env::var("BLVM_IBD_ENGINE_MUHASH") {
        Ok(v) => !(v.trim() == "0" || v.eq_ignore_ascii_case("false")),
        Err(_) => true,
    }
}

/// Resolve durable engine export height for IBD resume / engine seed.
///
/// The active ping-pong checkpoint tree is an exact UTXO snapshot **at**
/// `stored_export`. Resuming validation below that height while seeding from
/// the current tree causes immediate `UTXO not found` (outputs spent between
/// the forced tip and `stored_export` are absent from the snapshot).
///
/// `BLVM_IBD_EXPORT_HEIGHT_OVERRIDE` is only honored when it is **≥** the stored
/// export height — the historical recovery case where soft repair rolled
/// metadata *below* checkpoint content. An override **below** stored export is
/// ignored (live 2026-07-12: `OVERRIDE=640000` with `export_h=720000` seeded the
/// 720k set, labeled it 640k, and death-looped on block 640001).
///
/// Returns `(resolved_height, ignored_override)` where `ignored_override` is set
/// when a below-stored override was discarded.
pub fn resolve_engine_export_height(
    stored_export: Option<u64>,
    override_height: Option<u64>,
) -> (Option<u64>, Option<u64>) {
    let stored = stored_export.filter(|&h| h > 0);
    let over = override_height.filter(|&h| h > 0);
    match (stored, over) {
        (Some(eh), Some(oh)) if oh < eh => (Some(eh), Some(oh)),
        (Some(eh), Some(oh)) => (Some(oh.max(eh)), None),
        (Some(eh), None) => (Some(eh), None),
        (None, Some(oh)) => (Some(oh), None),
        (None, None) => (None, None),
    }
}

/// Read `BLVM_IBD_EXPORT_HEIGHT_OVERRIDE` (positive height only).
pub fn export_height_override_from_env() -> Option<u64> {
    std::env::var("BLVM_IBD_EXPORT_HEIGHT_OVERRIDE")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&h| h > 0)
}

/// Checkpoint interval for engine-mode soft autorepair rollback.
pub fn ibd_engine_checkpoint_interval_for_repair() -> u64 {
    let d = ibd_engine_durability_config(None);
    d.checkpoint_interval
        .map(|n| n as u64)
        .unwrap_or(d.checkpoint_min_interval as u64)
        .max(1)
}

/// Stable on-disk path for the engine flat table (not `/tmp` when `data_dir` is known).
pub fn ibd_engine_path(data_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Ok(p) = std::env::var("BLVM_IBD_ENGINE_PATH") {
        return std::path::PathBuf::from(p);
    }
    if let Some(dd) = data_dir {
        return dd.join("ibd_engine").join("utxo_table.bin");
    }
    std::env::temp_dir().join(format!(
        "blvm_ibd_engine_{}.bin",
        std::process::id()
    ))
}

fn default_ibd_chunk_size() -> u64 {
    // 128 blocks per request round-trip. With a single WAN peer and per-peer serial
    // chunk assignment, each round-trip fetches exactly one chunk; raising this from 16
    // cuts RTT overhead 8×. Memory is bounded independently by MemoryGuard::max_ahead_blocks
    // (RAM-adaptive), so this is safe across all hardware tiers.
    128
}
fn default_ibd_download_timeout() -> u64 {
    30
}
fn default_ibd_mode() -> String {
    "parallel".to_string()
}
fn default_ibd_yield_interval() -> u64 {
    1000
}
fn default_ibd_eviction() -> String {
    "fifo".to_string()
}
fn default_ibd_utxo_prefetch_lookahead() -> u64 {
    64
}
fn default_ibd_max_blocks_in_transit() -> usize {
    // Must stay in sync with chunk_size default: the per-peer blocks semaphore must have
    // at least as many permits as blocks in a chunk or workers stall mid-chunk waiting
    // for permits that can never be freed until the chunk completes.
    128
}
fn default_ibd_headers_timeout() -> u64 {
    // Must match IbdConfig::default() — serde `#[serde(default = ...)]` is used when
    // the field is omitted from TOML. A 5s helper vs 30s Default made omitted-field
    // loads silently more aggressive than `IbdConfig::default()`.
    30
}
fn default_ibd_headers_max_failures() -> u32 {
    10
}

impl Default for IbdConfig {
    fn default() -> Self {
        Self {
            chunk_size: 128,
            download_timeout_secs: 30,
            mode: "parallel".to_string(),
            preferred_peers: Vec::new(),
            max_ahead_blocks: None,
            memory_only: false,
            dump_dir: None,
            snapshot_dir: None,
            yield_interval: 1000,
            eviction: "fifo".to_string(),
            earliest_first: false,
            prefetch_workers: None,
            prefetch_queue_size: None,
            utxo_prefetch_lookahead: 64,
            max_blocks_in_transit_per_peer: 128,
            headers_timeout_secs: 30,
            headers_max_failures: 10,
            dedicated: false,
            utxo_engine: true,
        }
    }
}

/// Background task interval configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTaskConfig {
    #[serde(default = "default_bg_dos_cleanup_interval")]
    pub dos_cleanup_interval_secs: u64,

    #[serde(default = "default_bg_ban_cleanup_interval")]
    pub ban_cleanup_interval_secs: u64,

    #[serde(default = "default_bg_ban_cleanup_outer_interval")]
    pub ban_cleanup_outer_interval_secs: u64,

    #[serde(default = "default_bg_chain_sync_check_interval")]
    pub chain_sync_check_interval_secs: u64,

    #[serde(default = "default_bg_chain_sync_timeout")]
    pub chain_sync_timeout_secs: u64,

    #[serde(default = "default_bg_peer_eviction_interval")]
    pub peer_eviction_interval_secs: u64,

    #[serde(default = "default_bg_ping_timeout_check_interval")]
    pub ping_timeout_check_interval_secs: u64,

    #[serde(default = "default_bg_ping_interval")]
    pub ping_interval_secs: u64,

    #[serde(default = "default_bg_peer_reconnection_interval")]
    pub peer_reconnection_interval_secs: u64,
}

fn default_bg_dos_cleanup_interval() -> u64 {
    300
}
fn default_bg_ban_cleanup_interval() -> u64 {
    60
}
fn default_bg_ban_cleanup_outer_interval() -> u64 {
    300
}
fn default_bg_chain_sync_check_interval() -> u64 {
    60
}
fn default_bg_chain_sync_timeout() -> u64 {
    1200
}
fn default_bg_peer_eviction_interval() -> u64 {
    300
}
fn default_bg_ping_timeout_check_interval() -> u64 {
    30
}
fn default_bg_ping_interval() -> u64 {
    120
}
fn default_bg_peer_reconnection_interval() -> u64 {
    10
}

impl Default for BackgroundTaskConfig {
    fn default() -> Self {
        Self {
            dos_cleanup_interval_secs: 300,
            ban_cleanup_interval_secs: 60,
            ban_cleanup_outer_interval_secs: 300,
            chain_sync_check_interval_secs: 60,
            chain_sync_timeout_secs: 1200,
            peer_eviction_interval_secs: 300,
            ping_timeout_check_interval_secs: 30,
            ping_interval_secs: 120,
            peer_reconnection_interval_secs: 10,
        }
    }
}

/// Replay protection configuration for custom protocol messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayProtectionConfig {
    #[serde(default = "default_replay_cleanup_interval")]
    pub cleanup_interval_secs: u64,

    #[serde(default = "default_replay_message_id_expiration")]
    pub message_id_expiration_secs: u64,

    #[serde(default = "default_replay_request_id_expiration")]
    pub request_id_expiration_secs: u64,

    #[serde(default = "default_replay_future_tolerance")]
    pub future_tolerance_secs: u64,
}

fn default_replay_cleanup_interval() -> u64 {
    300
}
fn default_replay_message_id_expiration() -> u64 {
    3600
}
fn default_replay_request_id_expiration() -> u64 {
    300
}
fn default_replay_future_tolerance() -> u64 {
    300
}

impl Default for ReplayProtectionConfig {
    fn default() -> Self {
        Self {
            cleanup_interval_secs: 300,
            message_id_expiration_secs: 3600,
            request_id_expiration_secs: 300,
            future_tolerance_secs: 300,
        }
    }
}

#[cfg(test)]
mod ibd_engine_config_tests {
    use super::*;

    fn with_env<F: FnOnce()>(key: &str, val: Option<&str>, f: F) {
        let prev = std::env::var(key).ok();
        unsafe {
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn ibd_serve_only_env() {
        with_env("BLVM_SERVE_ONLY", None, || {
            assert!(!ibd_serve_only());
        });
        with_env("BLVM_SERVE_ONLY", Some("1"), || {
            assert!(ibd_serve_only());
        });
        with_env("BLVM_SERVE_ONLY", Some("true"), || {
            assert!(ibd_serve_only());
        });
        with_env("BLVM_SERVE_ONLY", Some("0"), || {
            assert!(!ibd_serve_only());
        });
    }

    #[test]
    fn getdata_serve_pipe_depth_serve_only_and_cap() {
        with_env("BLVM_GETDATA_SERVE_PIPE", None, || {
            with_env("BLVM_SERVE_ONLY", None, || {
                assert_eq!(getdata_serve_pipe_depth(), 1);
            });
            with_env("BLVM_SERVE_ONLY", Some("1"), || {
                assert_eq!(getdata_serve_pipe_depth(), 4);
            });
        });
        with_env("BLVM_GETDATA_SERVE_PIPE", Some("2"), || {
            assert_eq!(getdata_serve_pipe_depth(), 2);
        });
        with_env("BLVM_GETDATA_SERVE_PIPE", Some("64"), || {
            assert_eq!(getdata_serve_pipe_depth(), 8, "A4 cap — never honor 64 fanout");
        });
    }

    #[test]
    fn ibd_engine_enabled_config_default_true() {
        assert!(IbdConfig::default().utxo_engine);
        assert!(!ibd_engine_enabled(Some(&IbdConfig {
            utxo_engine: false,
            ..Default::default()
        })));
    }

    #[test]
    fn ibd_engine_enabled_env_overrides_config() {
        with_env("BLVM_IBD_ENGINE", Some("0"), || {
            assert!(!ibd_engine_enabled(Some(&IbdConfig {
                utxo_engine: true,
                ..Default::default()
            })));
        });
        with_env("BLVM_IBD_ENGINE", Some("1"), || {
            assert!(ibd_engine_enabled(Some(&IbdConfig {
                utxo_engine: false,
                ..Default::default()
            })));
        });
    }

    #[test]
    fn ibd_engine_durability_defaults() {
        with_env("BLVM_IBD_CHECKPOINT_MIN_INTERVAL", None, || {
            let d = ibd_engine_durability_config(None);
            assert_eq!(d.checkpoint_min_interval, 500);
            assert_eq!(d.muhash_persist_interval, 200);
        });
    }

    #[test]
    fn ibd_engine_durability_env_overrides() {
        with_env("BLVM_IBD_CHECKPOINT_INTERVAL", Some("200"), || {
            with_env("BLVM_IBD_ENGINE_MUHASH_INTERVAL", Some("100"), || {
                let d = ibd_engine_durability_config(None);
                assert_eq!(d.checkpoint_interval, Some(200));
                assert_eq!(d.muhash_persist_interval, 100);
            });
        });
    }

    #[test]
    fn ibd_engine_path_uses_data_dir() {
        with_env("BLVM_IBD_ENGINE_PATH", None, || {
            let p = ibd_engine_path(Some(std::path::Path::new("/data/mainnet")));
            assert_eq!(
                p,
                std::path::PathBuf::from("/data/mainnet/ibd_engine/utxo_table.bin")
            );
        });
    }

    #[test]
    fn resolve_export_ignores_override_below_stored() {
        // Live poison: stale shell OVERRIDE=640000 while durable ckpt is at 720000.
        let (h, ignored) = resolve_engine_export_height(Some(720_000), Some(640_000));
        assert_eq!(h, Some(720_000));
        assert_eq!(ignored, Some(640_000));
    }

    #[test]
    fn resolve_export_honors_override_at_or_above_stored() {
        // Pin when metadata matches ckpt content.
        let (h, ignored) = resolve_engine_export_height(Some(640_000), Some(640_000));
        assert_eq!(h, Some(640_000));
        assert_eq!(ignored, None);
        // Historical recovery: metadata rolled below ckpt content.
        let (h, ignored) = resolve_engine_export_height(Some(639_500), Some(640_000));
        assert_eq!(h, Some(640_000));
        assert_eq!(ignored, None);
    }

    #[test]
    fn resolve_export_override_only_when_no_stored() {
        let (h, ignored) = resolve_engine_export_height(None, Some(640_000));
        assert_eq!(h, Some(640_000));
        assert_eq!(ignored, None);
        let (h, ignored) = resolve_engine_export_height(Some(0), Some(640_000));
        assert_eq!(h, Some(640_000));
        assert_eq!(ignored, None);
    }
}

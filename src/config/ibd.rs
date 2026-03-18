//! IBD (Initial Block Download), bandwidth protection, background tasks, and replay protection config.

use serde::{Deserialize, Serialize};

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
}

fn default_ibd_chunk_size() -> u64 {
    16
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
    16
}
fn default_ibd_headers_timeout() -> u64 {
    30
}
fn default_ibd_headers_max_failures() -> u32 {
    10
}

impl Default for IbdConfig {
    fn default() -> Self {
        Self {
            chunk_size: 16,
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
            max_blocks_in_transit_per_peer: 16,
            headers_timeout_secs: 30,
            headers_max_failures: 10,
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

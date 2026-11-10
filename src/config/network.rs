//! Network configuration: DoS protection, spam ban, relay, address DB, Dandelion, peer rate limiting.

use serde::{Deserialize, Serialize};

/// DoS protection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DosProtectionConfig {
    #[serde(default = "default_dos_max_connections_per_window")]
    pub max_connections_per_window: usize,

    #[serde(default = "default_dos_window_seconds")]
    pub window_seconds: u64,

    #[serde(default = "default_dos_max_message_queue_size")]
    pub max_message_queue_size: usize,

    #[serde(default = "default_dos_max_active_connections")]
    pub max_active_connections: usize,

    #[serde(default = "default_dos_auto_ban_threshold")]
    pub auto_ban_threshold: usize,

    #[serde(default = "default_dos_ban_duration")]
    pub ban_duration_seconds: u64,
}

fn default_dos_max_connections_per_window() -> usize {
    10
}
fn default_dos_window_seconds() -> u64 {
    60
}
fn default_dos_max_message_queue_size() -> usize {
    10000
}
fn default_dos_max_active_connections() -> usize {
    200
}
fn default_dos_auto_ban_threshold() -> usize {
    3
}
fn default_dos_ban_duration() -> u64 {
    3600
}

impl Default for DosProtectionConfig {
    fn default() -> Self {
        Self {
            max_connections_per_window: 10,
            window_seconds: 60,
            max_message_queue_size: 10000,
            max_active_connections: 200,
            auto_ban_threshold: 3,
            ban_duration_seconds: 3600,
        }
    }
}

/// Spam-specific peer banning configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpamBanConfig {
    #[serde(default = "default_spam_ban_threshold")]
    pub spam_ban_threshold: usize,

    #[serde(default = "default_spam_ban_duration")]
    pub spam_ban_duration_seconds: u64,
}

fn default_spam_ban_threshold() -> usize {
    10
}
fn default_spam_ban_duration() -> u64 {
    3600
}

impl Default for SpamBanConfig {
    fn default() -> Self {
        Self {
            spam_ban_threshold: 10,
            spam_ban_duration_seconds: 3600,
        }
    }
}

/// Network relay configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    #[serde(default = "default_relay_max_age")]
    pub max_relay_age: u64,

    #[serde(default = "default_relay_max_tracked_items")]
    pub max_tracked_items: usize,

    #[serde(default = "crate::config::default_true")]
    pub enable_block_relay: bool,

    #[serde(default = "crate::config::default_true")]
    pub enable_tx_relay: bool,

    #[serde(default = "crate::config::default_false")]
    pub enable_dandelion: bool,
}

fn default_relay_max_age() -> u64 {
    3600
}
fn default_relay_max_tracked_items() -> usize {
    10000
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            max_relay_age: 3600,
            max_tracked_items: 10000,
            enable_block_relay: true,
            enable_tx_relay: true,
            enable_dandelion: false,
        }
    }
}

/// Address database configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressDatabaseConfig {
    #[serde(default = "default_address_db_max_addresses")]
    pub max_addresses: usize,

    #[serde(default = "default_address_db_expiration")]
    pub expiration_seconds: u64,
}

fn default_address_db_max_addresses() -> usize {
    10000
}
fn default_address_db_expiration() -> u64 {
    24 * 60 * 60
}

impl Default for AddressDatabaseConfig {
    fn default() -> Self {
        Self {
            max_addresses: 10000,
            expiration_seconds: 24 * 60 * 60,
        }
    }
}

/// Dandelion++ privacy relay configuration
#[cfg(feature = "dandelion")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DandelionConfig {
    #[serde(default = "default_dandelion_stem_timeout")]
    pub stem_timeout_seconds: u64,

    #[serde(default = "default_dandelion_fluff_probability")]
    pub fluff_probability: f64,

    #[serde(default = "default_dandelion_max_stem_hops")]
    pub max_stem_hops: u8,
}

#[cfg(feature = "dandelion")]
fn default_dandelion_stem_timeout() -> u64 {
    10
}
#[cfg(feature = "dandelion")]
fn default_dandelion_fluff_probability() -> f64 {
    0.1
}
#[cfg(feature = "dandelion")]
fn default_dandelion_max_stem_hops() -> u8 {
    2
}

#[cfg(feature = "dandelion")]
impl Default for DandelionConfig {
    fn default() -> Self {
        Self {
            stem_timeout_seconds: 10,
            fluff_probability: 0.1,
            max_stem_hops: 2,
        }
    }
}

/// Peer rate limiting configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRateLimitingConfig {
    #[serde(default = "default_peer_rate_burst")]
    pub default_burst: u32,

    #[serde(default = "default_peer_rate_rate")]
    pub default_rate: u32,
}

fn default_peer_rate_burst() -> u32 {
    100
}
fn default_peer_rate_rate() -> u32 {
    10
}

impl Default for PeerRateLimitingConfig {
    fn default() -> Self {
        Self {
            default_burst: 100,
            default_rate: 10,
        }
    }
}

//! Governance and ban-list sharing configuration.

use serde::{Deserialize, Serialize};

/// Governance message relay configuration
#[cfg(feature = "governance")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GovernanceConfig {
    /// Enable governance message relay (default: false)
    /// If false, governance messages are gossiped but not forwarded to blvm-commons
    #[serde(default = "crate::config::default_false")]
    pub enabled: bool,

    /// URL for blvm-commons internal API (e.g., "http://10.0.0.2:8080")
    /// Used for forwarding P2P governance messages via VPN
    #[serde(default)]
    pub commons_url: Option<String>,

    /// API key for authenticating with blvm-commons internal API
    /// Can also be set via COMMONS_API_KEY environment variable
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Ban list sharing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanListSharingConfig {
    /// Enable ban list sharing
    #[serde(default = "crate::config::default_true")]
    pub enabled: bool,

    /// Share mode: Immediate, Periodic, or Disabled
    #[serde(default = "default_ban_share_mode")]
    pub share_mode: BanShareMode,

    /// Periodic sharing interval in seconds (only used if share_mode is Periodic)
    #[serde(default = "default_periodic_interval")]
    pub periodic_interval_seconds: u64,

    /// Minimum ban duration to share (seconds, 0 = all)
    #[serde(default = "default_min_ban_duration")]
    pub min_ban_duration_to_share: u64,
}

/// Ban share mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BanShareMode {
    /// Share immediately when auto-ban occurs
    Immediate,
    /// Share periodically (default)
    Periodic,
    /// Disabled
    Disabled,
}

fn default_ban_share_mode() -> BanShareMode {
    BanShareMode::Periodic
}

fn default_periodic_interval() -> u64 {
    300 // 5 minutes
}

fn default_min_ban_duration() -> u64 {
    3600 // 1 hour
}

impl Default for BanListSharingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            share_mode: BanShareMode::Periodic,
            periodic_interval_seconds: 300,
            min_ban_duration_to_share: 3600,
        }
    }
}

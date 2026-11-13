//! Governance and ban-list sharing configuration.

use serde::{Deserialize, Serialize};

/// Governance message relay configuration
#[cfg(feature = "governance")]
#[derive(Clone, Default, Serialize, Deserialize)]
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

#[cfg(feature = "governance")]
impl std::fmt::Debug for GovernanceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = self.commons_url.as_deref().map(redact_url_if_secret);
        f.debug_struct("GovernanceConfig")
            .field("enabled", &self.enabled)
            .field("commons_url", &url)
            .field("api_key", &self.api_key.as_ref().map(|_| "redacted"))
            .finish()
    }
}

#[cfg(feature = "governance")]
fn redact_url_if_secret(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if url.contains('@')
        || lower.contains("key=")
        || lower.contains("token=")
        || lower.contains("password=")
    {
        "redacted".to_string()
    } else {
        url.to_string()
    }
}

#[cfg(feature = "governance")]
impl GovernanceConfig {
    /// Configured key, or `COMMONS_API_KEY` when the config value is empty.
    pub fn resolved_api_key(&self) -> Option<String> {
        self.api_key
            .clone()
            .filter(|key| !key.is_empty())
            .or_else(|| {
                std::env::var("COMMONS_API_KEY")
                    .ok()
                    .filter(|key| !key.is_empty())
            })
    }
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

    /// Peers allowed to install bans on this node. Empty means this node
    /// sends its own list and applies none. An entry is an IP or `ip:port`.
    #[serde(default)]
    pub trusted_peers: Vec<String>,

    /// Longest peer-supplied ban this node will install, measured from now.
    #[serde(default = "default_max_accepted_ban_seconds")]
    pub max_accepted_ban_seconds: u64,
}

fn default_max_accepted_ban_seconds() -> u64 {
    7 * 24 * 3600
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
            trusted_peers: Vec::new(),
            max_accepted_ban_seconds: default_max_accepted_ban_seconds(),
        }
    }
}

impl BanListSharingConfig {
    /// Sharing is on when the section is enabled and the mode is not `disabled`.
    pub fn shares(&self) -> bool {
        self.enabled && !matches!(self.share_mode, BanShareMode::Disabled)
    }

    /// True when `peer` is listed by IP or by full socket address.
    pub fn trusts_peer(&self, peer: &std::net::SocketAddr) -> bool {
        self.trusted_peers.iter().any(|entry| {
            let entry = entry.trim();
            if let Ok(addr) = entry.parse::<std::net::SocketAddr>() {
                addr == *peer || (addr.port() == 0 && addr.ip() == peer.ip())
            } else if let Ok(ip) = entry.parse::<std::net::IpAddr>() {
                ip == peer.ip()
            } else {
                false
            }
        })
    }
}

#[cfg(all(test, feature = "governance"))]
mod secret_debug {
    use super::GovernanceConfig;

    #[test]
    fn debug_output_does_not_contain_the_api_key_or_a_url_secret() {
        let config = GovernanceConfig {
            enabled: true,
            commons_url: Some(
                "https://commons.example/internal?api_key=url-secret-should-not-leak".to_string(),
            ),
            api_key: Some("commons-api-key-should-not-leak".to_string()),
        };
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("commons-api-key-should-not-leak"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("url-secret-should-not-leak"),
            "{rendered}"
        );
        assert!(rendered.contains("redacted"));
    }
}

//! RPC server, auth, Stratum V2 and merge-mining configuration.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

fn default_max_request_size_bytes() -> usize {
    1_048_576
}

fn default_rate_limit_when_auth_disabled() -> bool {
    true
}

fn default_ip_rate_limit_burst() -> u32 {
    50
}

fn default_ip_rate_limit_rate() -> u32 {
    5
}

fn default_max_connections_per_ip_per_minute() -> u32 {
    10
}

fn default_batch_rate_multiplier_cap() -> u32 {
    10
}

fn default_connection_rate_limit_window_seconds() -> u64 {
    60
}

/// RPC server configuration (limits, timeouts)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Maximum request body size in bytes (default: 1MB)
    #[serde(default = "default_max_request_size_bytes")]
    pub max_request_size_bytes: usize,

    /// When auth is disabled, still apply IP rate limiting (default: true)
    #[serde(default = "default_rate_limit_when_auth_disabled")]
    pub rate_limit_when_auth_disabled: bool,

    /// IP rate limit burst when auth disabled (default: 50)
    #[serde(default = "default_ip_rate_limit_burst")]
    pub ip_rate_limit_burst: u32,

    /// IP rate limit per second when auth disabled (default: 5)
    #[serde(default = "default_ip_rate_limit_rate")]
    pub ip_rate_limit_rate: u32,

    /// Max connections per IP per minute for RPC (default: 10)
    #[serde(default = "default_max_connections_per_ip_per_minute")]
    pub max_connections_per_ip_per_minute: u32,

    /// Batch rate limit multiplier cap (default: 10)
    #[serde(default = "default_batch_rate_multiplier_cap")]
    pub batch_rate_multiplier_cap: u32,

    /// Connection rate limit window in seconds (default: 60)
    #[serde(default = "default_connection_rate_limit_window_seconds")]
    pub connection_rate_limit_window_seconds: u64,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            max_request_size_bytes: 1_048_576,
            rate_limit_when_auth_disabled: true,
            ip_rate_limit_burst: 50,
            ip_rate_limit_rate: 5,
            max_connections_per_ip_per_minute: 10,
            batch_rate_multiplier_cap: 10,
            connection_rate_limit_window_seconds: 60,
        }
    }
}

fn default_rate_limit_burst() -> u32 {
    100
}

fn default_rate_limit_rate() -> u32 {
    10
}

/// RPC authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcAuthConfig {
    /// Require authentication for RPC requests
    #[serde(default)]
    pub required: bool,

    /// Valid authentication tokens (env RPC_AUTH_TOKENS, token_file, or this field)
    #[serde(default)]
    pub tokens: Vec<String>,

    /// Path to file containing tokens (one per line)
    #[serde(default)]
    pub token_file: Option<String>,

    /// Valid certificate fingerprints (for certificate-based auth)
    #[serde(default)]
    pub certificates: Vec<String>,

    /// Default rate limit (burst, requests per second)
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,

    #[serde(default = "default_rate_limit_rate")]
    pub rate_limit_rate: u32,

    #[serde(default = "default_rate_limit_when_auth_disabled")]
    pub rate_limit_when_auth_disabled: bool,

    #[serde(default = "default_ip_rate_limit_burst")]
    pub ip_rate_limit_burst: u32,

    #[serde(default = "default_ip_rate_limit_rate")]
    pub ip_rate_limit_rate: u32,
}

impl Default for RpcAuthConfig {
    fn default() -> Self {
        Self {
            required: false,
            tokens: Vec::new(),
            token_file: None,
            certificates: Vec::new(),
            rate_limit_burst: 100,
            rate_limit_rate: 10,
            rate_limit_when_auth_disabled: true,
            ip_rate_limit_burst: 50,
            ip_rate_limit_rate: 5,
        }
    }
}

impl RpcAuthConfig {
    /// Create a minimal config for rate-limiting-only mode (no auth, no tokens)
    pub fn rate_limit_only(burst: u32, rate: u32) -> Self {
        Self {
            required: false,
            tokens: Vec::new(),
            token_file: None,
            certificates: Vec::new(),
            rate_limit_burst: burst,
            rate_limit_rate: rate,
            rate_limit_when_auth_disabled: true,
            ip_rate_limit_burst: burst,
            ip_rate_limit_rate: rate,
        }
    }

    /// Load tokens from environment variable, token file, or config file
    pub fn load_tokens(&self) -> anyhow::Result<Vec<String>> {
        use std::env;

        if let Ok(env_tokens) = env::var("RPC_AUTH_TOKENS") {
            let tokens: Vec<String> = env_tokens
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !tokens.is_empty() {
                return Ok(tokens);
            }
        }

        if let Some(ref token_file) = self.token_file {
            let content = std::fs::read_to_string(token_file).map_err(|e| {
                anyhow::anyhow!("Failed to read token file {:?}: {}", token_file, e)
            })?;
            let tokens: Vec<String> = content
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && !s.starts_with('#'))
                .collect();
            if !tokens.is_empty() {
                return Ok(tokens);
            }
        }

        Ok(self.tokens.clone())
    }
}

/// Stratum V2 mining configuration
#[cfg(feature = "stratum-v2")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StratumV2Config {
    pub enabled: bool,
    pub pool_url: Option<String>,
    pub listen_addr: Option<SocketAddr>,
    pub transport_preference: crate::config::TransportPreferenceConfig,
    pub merge_mining_enabled: bool,
    pub secondary_chains: Vec<String>,
    pub merge_mining_fee: Option<MergeMiningFeeConfig>,
}

/// Merge mining fee configuration
#[cfg(feature = "stratum-v2")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeMiningFeeConfig {
    #[serde(default = "crate::config::default_false")]
    pub enabled: bool,

    #[serde(default = "default_merge_mining_fee_percentage")]
    pub fee_percentage: u8,

    pub commons_address: Option<String>,
    pub contributor_id: Option<String>,

    #[serde(default = "crate::config::default_false")]
    pub auto_distribute: bool,
}

#[cfg(feature = "stratum-v2")]
fn default_merge_mining_fee_percentage() -> u8 {
    1
}

#[cfg(feature = "stratum-v2")]
impl Default for StratumV2Config {
    fn default() -> Self {
        Self {
            enabled: false,
            pool_url: None,
            listen_addr: None,
            transport_preference: crate::config::TransportPreferenceConfig::TcpOnly,
            merge_mining_enabled: false,
            secondary_chains: Vec::new(),
            merge_mining_fee: None,
        }
    }
}

#[cfg(feature = "stratum-v2")]
impl Default for MergeMiningFeeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fee_percentage: 1,
            commons_address: None,
            contributor_id: None,
            auto_distribute: false,
        }
    }
}

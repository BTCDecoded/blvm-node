//! RPC server, auth, and the node Stratum V2 demux switch.

use serde::{Deserialize, Serialize};

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

    /// Valid authentication tokens (env RPC_AUTH_TOKENS, token_file, or this field).
    /// Bearer tokens are read-only unless also listed in `admin_tokens`.
    #[serde(default)]
    pub tokens: Vec<String>,

    /// Path to file containing tokens (one per line)
    #[serde(default)]
    pub token_file: Option<String>,

    /// Tokens with admin privileges (mining: getblocktemplate, submitblock, generatetoaddress;
    /// destructive: stop, loadmodule, invalidateblock, etc.). Bearer tokens in `tokens` /
    /// `token_file` are read-only unless listed here. `[rpc_auth].password` is always
    /// registered as admin when HTTP Basic auth is enabled (ckpool / Bitcoin Core compat).
    #[serde(default)]
    pub admin_tokens: Vec<String>,

    /// Valid certificate fingerprints (for certificate-based auth)
    #[serde(default)]
    pub certificates: Vec<String>,

    /// Bitcoin Core-compatible RPC username for HTTP Basic auth (e.g. ckpool `auth`).
    #[serde(default)]
    pub username: Option<String>,

    /// Bitcoin Core-compatible RPC password for HTTP Basic auth (e.g. ckpool `pass`).
    #[serde(default)]
    pub password: Option<String>,

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
            required: true,
            tokens: Vec::new(),
            token_file: None,
            admin_tokens: Vec::new(),
            certificates: Vec::new(),
            username: None,
            password: None,
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
            admin_tokens: Vec::new(),
            certificates: Vec::new(),
            username: None,
            password: None,
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

/// Node-side Stratum V2 switch. Miner TCP belongs to the `blvm-stratum-v2` module file.
#[cfg(feature = "stratum-v2")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StratumV2Config {
    /// When true (default), inbound P2P payloads matching the Stratum V2 TLV
    /// heuristic are routed to `StratumV2MessageReceived`.
    #[serde(default = "crate::config::default_true")]
    pub p2p_stratum_demux: bool,
}

#[cfg(feature = "stratum-v2")]
impl Default for StratumV2Config {
    fn default() -> Self {
        Self {
            p2p_stratum_demux: true,
        }
    }
}

/// Absent `[stratum_v2]` keeps the P2P TLV path on.
#[cfg(feature = "stratum-v2")]
pub fn p2p_stratum_demux_enabled(stratum: Option<&StratumV2Config>) -> bool {
    stratum
        .map(|section| section.p2p_stratum_demux)
        .unwrap_or(true)
}

#[cfg(all(test, feature = "stratum-v2"))]
mod stratum_v2_config_tests {
    use super::{StratumV2Config, p2p_stratum_demux_enabled};
    use crate::config::NodeConfig;

    #[test]
    fn stratum_v2_config_demux_defaults_on() {
        let c = StratumV2Config::default();
        assert!(c.p2p_stratum_demux);
        assert!(p2p_stratum_demux_enabled(None));
    }

    #[test]
    fn old_stratum_keys_parse_and_demux_false_disables_the_tlv_path() {
        let config: NodeConfig = toml::from_str(
            r#"
            [stratum_v2]
            listen_addr = "0.0.0.0:3333"
            pool_url = "tcp://pool.example.com:3333"
            enabled = true
            p2p_stratum_demux = false
            "#,
        )
        .expect("old stratum keys are ignored");
        let section = config.stratum_v2.as_ref().expect("section present");
        assert!(!section.p2p_stratum_demux);
        assert!(!p2p_stratum_demux_enabled(Some(section)));
    }
}

//! Configuration management for blvm-node
//!
//! Handles configuration loading, validation, and transport selection.

pub mod storage;
pub use storage::*;

pub mod bitcoin_core_convert;
pub use bitcoin_core_convert::*;

pub mod governance;
pub use governance::*;

pub mod rpc;
pub use rpc::*;

pub mod mempool;
pub use mempool::*;

pub mod ibd;
pub use ibd::*;

pub mod network;
pub use network::*;

#[cfg(feature = "fibre")]
use crate::network::fibre;
use crate::network::transport::TransportPreference;
use serde::de::{Deserializer, Error, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;

// TOML support for configuration files

fn flatten_toml_to_string_map(value: &toml::Value) -> HashMap<String, String> {
    use toml::Value;
    let mut result = HashMap::new();
    fn flatten(prefix: &str, v: &Value, out: &mut HashMap<String, String>) {
        match v {
            Value::String(s) => {
                if !prefix.is_empty() {
                    out.insert(prefix.to_string(), s.clone());
                }
            }
            Value::Integer(i) => {
                out.insert(prefix.to_string(), i.to_string());
            }
            Value::Float(f) => {
                out.insert(prefix.to_string(), f.to_string());
            }
            Value::Boolean(b) => {
                out.insert(prefix.to_string(), b.to_string());
            }
            Value::Array(arr) => {
                let s: Vec<String> = arr
                    .iter()
                    .map(|x| match x {
                        Value::String(s) => s.clone(),
                        _ => x.to_string(),
                    })
                    .collect();
                out.insert(prefix.to_string(), s.join(","));
            }
            Value::Table(t) => {
                for (k, val) in t {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    flatten(&p, val, out);
                }
            }
            Value::Datetime(dt) => {
                out.insert(prefix.to_string(), dt.to_string());
            }
        }
    }
    if let Value::Table(t) = value {
        for (k, v) in t {
            flatten(k, v, &mut result);
        }
    }
    result
}

fn deserialize_module_config<'de, D>(deserializer: D) -> Result<ModuleConfig, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModuleConfigVisitor;
    impl<'de> Visitor<'de> for ModuleConfigVisitor {
        type Value = ModuleConfig;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("modules config table")
        }

        fn visit_map<A>(self, mut map: A) -> Result<ModuleConfig, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut enabled = true;
            let mut modules_dir = "modules".to_string();
            let mut data_dir = "data/modules".to_string();
            let mut socket_dir = "data/modules/sockets".to_string();
            let mut enabled_modules = Vec::new();
            let mut module_configs = HashMap::new();
            let mut watch_enabled = true;
            let mut watch_auto_load = false;
            let mut watch_auto_unload = false;

            while let Some(key) = map.next_key::<String>()? {
                if MODULE_CONFIG_KNOWN_KEYS.contains(&key.as_str()) {
                    match key.as_str() {
                        "enabled" => enabled = map.next_value()?,
                        "modules_dir" => modules_dir = map.next_value().unwrap_or(modules_dir),
                        "data_dir" => data_dir = map.next_value().unwrap_or(data_dir),
                        "socket_dir" => socket_dir = map.next_value().unwrap_or(socket_dir),
                        "enabled_modules" => enabled_modules = map.next_value().unwrap_or_default(),
                        "module_configs" => module_configs = map.next_value().unwrap_or_default(),
                        "watch_enabled" => watch_enabled = map.next_value().unwrap_or(true),
                        "watch_auto_load" => watch_auto_load = map.next_value().unwrap_or(false),
                        "watch_auto_unload" => {
                            watch_auto_unload = map.next_value().unwrap_or(false)
                        }
                        _ => {
                            let _: toml::Value = map.next_value()?;
                        }
                    }
                } else {
                    // [modules.<name>] - per-module override table (e.g. [modules.selective-sync])
                    let value: toml::Value = map.next_value()?;
                    if let toml::Value::Table(_) = &value {
                        let flat = flatten_toml_to_string_map(&value);
                        if !flat.is_empty() {
                            module_configs.insert(key, flat);
                        }
                    }
                }
            }

            Ok(ModuleConfig {
                enabled,
                modules_dir,
                data_dir,
                socket_dir,
                enabled_modules,
                module_configs,
                watch_enabled,
                watch_auto_load,
                watch_auto_unload,
            })
        }
    }
    deserializer.deserialize_map(ModuleConfigVisitor)
}

/// Known keys in [modules] section (not per-module config tables)
const MODULE_CONFIG_KNOWN_KEYS: &[&str] = &[
    "enabled",
    "modules_dir",
    "data_dir",
    "socket_dir",
    "enabled_modules",
    "module_configs",
    "watch_enabled",
    "watch_auto_load",
    "watch_auto_unload",
];

/// Module system configuration
#[derive(Debug, Clone, Serialize)]
pub struct ModuleConfig {
    /// Enable module system
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Directory containing module binaries
    #[serde(default = "default_modules_dir")]
    pub modules_dir: String,

    /// Directory for module data (state, configs)
    #[serde(default = "default_modules_data_dir")]
    pub data_dir: String,

    /// Directory for IPC sockets
    #[serde(default = "default_modules_socket_dir")]
    pub socket_dir: String,

    /// List of enabled modules (empty = auto-discover all)
    #[serde(default)]
    pub enabled_modules: Vec<String>,

    /// Module-specific configuration overrides
    #[serde(default)]
    pub module_configs:
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,

    /// Enable file watcher for hot-reload on module.toml/config.toml change (requires module-watcher feature)
    #[serde(default = "default_true")]
    pub watch_enabled: bool,

    /// When a new module appears (module.toml created), auto-load it
    #[serde(default)]
    pub watch_auto_load: bool,

    /// When a module directory is removed, auto-unload it
    #[serde(default)]
    pub watch_auto_unload: bool,
}

pub(crate) fn default_true() -> bool {
    true
}

pub(crate) fn default_false() -> bool {
    false
}

fn default_modules_dir() -> String {
    "modules".to_string()
}

fn default_modules_data_dir() -> String {
    "data/modules".to_string()
}

fn default_modules_socket_dir() -> String {
    "data/modules/sockets".to_string()
}

impl<'de> Deserialize<'de> for ModuleConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_module_config(deserializer)
    }
}

impl Default for ModuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            modules_dir: "modules".to_string(),
            data_dir: "data/modules".to_string(),
            socket_dir: "data/modules/sockets".to_string(),
            enabled_modules: Vec::new(),
            module_configs: std::collections::HashMap::new(),
            watch_enabled: true,
            watch_auto_load: false,
            watch_auto_unload: false,
        }
    }
}

/// Network timing and connection behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkTimingConfig {
    /// Target number of outbound peers to connect to (desired connections; reference: 8-125).
    /// Distinct from max_outbound_peers which is the PeerManager cap.
    #[serde(default = "default_target_peer_count", rename = "target_peer_count")]
    pub target_outbound_peers: usize,

    /// Wait time before connecting to peers from database (after persistent peers)
    #[serde(default = "default_peer_connection_delay")]
    pub peer_connection_delay_seconds: u64,

    /// Minimum interval between addr message broadcasts (prevents spam)
    #[serde(default = "default_addr_relay_min_interval")]
    pub addr_relay_min_interval_seconds: u64,

    /// Maximum addresses to include in a single addr message
    #[serde(default = "default_max_addresses_per_addr_message")]
    pub max_addresses_per_addr_message: usize,

    /// Maximum addresses to fetch from DNS seeds
    #[serde(default = "default_max_addresses_from_dns")]
    pub max_addresses_from_dns: usize,
}

fn default_target_peer_count() -> usize {
    8
}

fn default_peer_connection_delay() -> u64 {
    2
}

fn default_addr_relay_min_interval() -> u64 {
    8640 // 2.4 hours
}

fn default_max_addresses_per_addr_message() -> usize {
    1000
}

fn default_max_addresses_from_dns() -> usize {
    100
}

impl Default for NetworkTimingConfig {
    fn default() -> Self {
        Self {
            target_outbound_peers: 8,
            peer_connection_delay_seconds: 2,
            addr_relay_min_interval_seconds: 8640,
            max_addresses_per_addr_message: 1000,
            max_addresses_from_dns: 100,
        }
    }
}

/// Request timeout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestTimeoutConfig {
    /// Timeout for async request-response patterns (getheaders, getdata, etc.)
    #[serde(default = "default_async_request_timeout")]
    pub async_request_timeout_seconds: u64,

    /// Timeout for UTXO commitment requests
    #[serde(default = "default_utxo_commitment_timeout")]
    pub utxo_commitment_request_timeout_seconds: u64,

    /// Cleanup interval for expired pending requests
    #[serde(default = "default_request_cleanup_interval")]
    pub request_cleanup_interval_seconds: u64,

    /// Maximum age for pending requests before cleanup
    #[serde(default = "default_pending_request_max_age")]
    pub pending_request_max_age_seconds: u64,

    /// Timeout for storage operations (seconds)
    #[serde(default = "default_storage_timeout")]
    pub storage_timeout_seconds: u64,

    /// Timeout for network operations (seconds)
    #[serde(default = "default_network_timeout")]
    pub network_timeout_seconds: u64,

    /// Timeout for RPC operations (seconds)
    #[serde(default = "default_rpc_timeout")]
    pub rpc_timeout_seconds: u64,

    /// Handshake timeout (Version/VerAck exchange)
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_secs: u64,

    /// TCP connect timeout
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,

    /// Checkpoint request timeout (LAN security)
    #[serde(default = "default_checkpoint_request_timeout")]
    pub checkpoint_request_timeout_secs: u64,

    /// Protocol verification timeout (LAN discovery handshake)
    #[serde(default = "default_protocol_verify_timeout")]
    pub protocol_verify_timeout_secs: u64,

    /// Headers verification timeout (LAN security)
    #[serde(default = "default_headers_verify_timeout")]
    pub headers_verify_timeout_secs: u64,
}

fn default_async_request_timeout() -> u64 {
    300 // 5 minutes
}

fn default_utxo_commitment_timeout() -> u64 {
    30
}

fn default_request_cleanup_interval() -> u64 {
    60
}

fn default_pending_request_max_age() -> u64 {
    300 // 5 minutes
}

fn default_storage_timeout() -> u64 {
    10 // 10 seconds
}

fn default_network_timeout() -> u64 {
    30 // 30 seconds
}

fn default_rpc_timeout() -> u64 {
    60 // 60 seconds
}

fn default_handshake_timeout() -> u64 {
    10
}

fn default_connect_timeout() -> u64 {
    10
}

fn default_checkpoint_request_timeout() -> u64 {
    5
}

fn default_protocol_verify_timeout() -> u64 {
    5
}

fn default_headers_verify_timeout() -> u64 {
    10
}

impl Default for RequestTimeoutConfig {
    fn default() -> Self {
        Self {
            async_request_timeout_seconds: 300,
            utxo_commitment_request_timeout_seconds: 30,
            request_cleanup_interval_seconds: 60,
            pending_request_max_age_seconds: 300,
            storage_timeout_seconds: 10,
            network_timeout_seconds: 30,
            rpc_timeout_seconds: 60,
            handshake_timeout_secs: 10,
            connect_timeout_secs: 10,
            checkpoint_request_timeout_secs: 5,
            protocol_verify_timeout_secs: 5,
            headers_verify_timeout_secs: 10,
        }
    }
}

/// Module resource limits configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleResourceLimitsConfig {
    /// Default CPU limit for modules (percentage, 0-100)
    #[serde(default = "default_module_max_cpu_percent")]
    pub default_max_cpu_percent: u32,

    /// Default memory limit for modules (bytes)
    #[serde(default = "default_module_max_memory_bytes")]
    pub default_max_memory_bytes: u64,

    /// Default file descriptor limit
    #[serde(default = "default_module_max_file_descriptors")]
    pub default_max_file_descriptors: u32,

    /// Default child process limit
    #[serde(default = "default_module_max_child_processes")]
    pub default_max_child_processes: u32,

    /// Module startup wait time (milliseconds)
    #[serde(default = "default_module_startup_wait_millis")]
    pub module_startup_wait_millis: u64,

    /// Timeout for module socket to appear (seconds)
    #[serde(default = "default_module_socket_timeout")]
    pub module_socket_timeout_seconds: u64,

    /// Interval between socket existence checks (milliseconds)
    #[serde(default = "default_module_socket_check_interval")]
    pub module_socket_check_interval_millis: u64,

    /// Maximum attempts to check for socket
    #[serde(default = "default_module_socket_max_attempts")]
    pub module_socket_max_attempts: usize,
}

fn default_module_max_cpu_percent() -> u32 {
    50
}

fn default_module_max_memory_bytes() -> u64 {
    512 * 1024 * 1024 // 512 MB
}

fn default_module_max_file_descriptors() -> u32 {
    256
}

fn default_module_max_child_processes() -> u32 {
    10
}

fn default_module_startup_wait_millis() -> u64 {
    100
}

fn default_module_socket_timeout() -> u64 {
    5
}

fn default_module_socket_check_interval() -> u64 {
    100
}

fn default_module_socket_max_attempts() -> usize {
    50
}

impl Default for ModuleResourceLimitsConfig {
    fn default() -> Self {
        Self {
            default_max_cpu_percent: 50,
            default_max_memory_bytes: 512 * 1024 * 1024,
            default_max_file_descriptors: 256,
            default_max_child_processes: 10,
            module_startup_wait_millis: 100,
            module_socket_timeout_seconds: 5,
            module_socket_check_interval_millis: 100,
            module_socket_max_attempts: 50,
        }
    }
}

/// Protocol message limits (for constrained networks)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolLimitsConfig {
    /// Maximum protocol message size (bytes). Default 32MB.
    #[serde(default = "default_max_protocol_message_length")]
    pub max_protocol_message_length: usize,

    /// Maximum addresses in addr message. Default 1000.
    #[serde(default = "default_max_addr_to_send")]
    pub max_addr_to_send: usize,

    /// Maximum inventory items in inv/getdata. Default 50000.
    #[serde(default = "default_max_inv_sz")]
    pub max_inv_sz: usize,

    /// Maximum headers in headers message. Default 2000.
    #[serde(default = "default_max_headers_results")]
    pub max_headers_results: usize,
}

fn default_max_protocol_message_length() -> usize {
    32 * 1024 * 1024
}

fn default_max_addr_to_send() -> usize {
    1000
}

fn default_max_inv_sz() -> usize {
    50000
}

fn default_max_headers_results() -> usize {
    2000
}

impl Default for ProtocolLimitsConfig {
    fn default() -> Self {
        Self {
            max_protocol_message_length: default_max_protocol_message_length(),
            max_addr_to_send: default_max_addr_to_send(),
            max_inv_sz: default_max_inv_sz(),
            max_headers_results: default_max_headers_results(),
        }
    }
}

/// Per-network default assume-valid height (Core chainparams defaultAssumeValid)
pub fn default_assume_valid_height_for_network(network: &str) -> u64 {
    match network.to_lowercase().as_str() {
        "mainnet" | "bitcoinv1" => 912_683,  // Core mainnet default
        "testnet" | "testnet3" => 4_550_000, // Core testnet default
        "signet" => 267_665,                 // Core signet default
        _ => 0,                              // Regtest and unknown: validate all
    }
}

/// Block validation configuration (maps to blvm-consensus BlockValidationConfig)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockValidationNodeConfig {
    /// Assume-valid height: blocks before this height skip signature verification (-assumevalid equivalent)
    #[serde(default)]
    pub assume_valid_height: u64,

    /// Assume-valid block hash: when set, verify block at assume_valid_height matches; skip verification for ancestors.
    /// Takes precedence over assume_valid_height when both set.
    #[serde(default)]
    pub assume_valid_hash: Option<[u8; 32]>,
}

/// Node configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Network listening address
    pub listen_addr: Option<SocketAddr>,

    /// Block validation configuration (assume-valid height, etc.)
    pub block_validation: Option<BlockValidationNodeConfig>,

    /// AssumeUTXO: block hash of snapshot to load for fast sync (-assumeutxo=<blockhash>)
    #[serde(default)]
    pub assumeutxo_blockhash: Option<[u8; 32]>,

    /// Transport preference
    pub transport_preference: TransportPreferenceConfig,

    /// Maximum outbound peers (PeerManager cap). Distinct from target_outbound_peers (desired connections).
    #[serde(rename = "max_peers")]
    pub max_outbound_peers: Option<usize>,

    /// Protocol version
    pub protocol_version: Option<String>,

    /// Module system configuration
    pub modules: Option<ModuleConfig>,

    /// Stratum V2 mining configuration
    #[cfg(feature = "stratum-v2")]
    pub stratum_v2: Option<StratumV2Config>,

    /// RPC server configuration (limits, etc.)
    pub rpc: Option<RpcConfig>,

    /// RPC authentication configuration
    pub rpc_auth: Option<RpcAuthConfig>,

    /// Ban list sharing configuration
    pub ban_list_sharing: Option<BanListSharingConfig>,

    /// Governance message relay configuration
    #[cfg(feature = "governance")]
    pub governance: Option<GovernanceConfig>,

    /// Storage and pruning configuration
    pub storage: Option<StorageConfig>,

    /// Persistent peers (peers to connect to on startup)
    #[serde(default)]
    pub persistent_peers: Vec<SocketAddr>,

    /// Enable self-advertisement (send own address to peers)
    #[serde(default = "default_true")]
    pub enable_self_advertisement: bool,

    /// DoS protection configuration
    pub dos_protection: Option<DosProtectionConfig>,

    /// IBD bandwidth protection configuration
    pub ibd_protection: Option<IbdProtectionConfig>,

    /// Parallel IBD download configuration (chunk_size, mode, prefetch, etc.)
    pub ibd: Option<IbdConfig>,

    /// Spam-specific peer banning configuration
    pub spam_ban: Option<SpamBanConfig>,

    /// Network relay configuration
    pub relay: Option<RelayConfig>,

    /// FIBRE (Fast Internet Bitcoin Relay Engine) configuration
    #[cfg(feature = "fibre")]
    pub fibre: Option<fibre::FibreConfig>,

    /// Address database configuration
    pub address_database: Option<AddressDatabaseConfig>,

    /// Dandelion++ privacy relay configuration
    #[cfg(feature = "dandelion")]
    pub dandelion: Option<DandelionConfig>,

    /// Peer rate limiting configuration
    pub peer_rate_limiting: Option<PeerRateLimitingConfig>,

    /// Network timing and connection behavior
    pub network_timing: Option<NetworkTimingConfig>,

    /// Request timeout configuration
    pub request_timeouts: Option<RequestTimeoutConfig>,

    /// Protocol message limits (for constrained networks)
    pub protocol_limits: Option<ProtocolLimitsConfig>,

    /// Background task interval configuration (DoS cleanup, ban cleanup, ping, etc.)
    pub background_tasks: Option<BackgroundTaskConfig>,

    /// Replay protection for custom protocol messages (governance, ban list, etc.)
    pub replay_protection: Option<ReplayProtectionConfig>,

    /// Module resource limits configuration
    pub module_resource_limits: Option<ModuleResourceLimitsConfig>,

    /// ZMQ notification configuration
    #[cfg(feature = "zmq")]
    pub zmq: Option<ZmqConfig>,

    /// Logging configuration
    pub logging: Option<LoggingConfig>,

    /// Mempool configuration
    pub mempool: Option<MempoolPolicyConfig>,

    /// RBF (Replace-By-Fee) configuration
    pub rbf: Option<RbfConfig>,

    /// Payment configuration (BIP70)
    pub payment: Option<PaymentConfig>,

    /// REST API configuration
    pub rest_api: Option<RestApiConfig>,
}

/// Transport preference configuration (serializable)
///
/// Note: This is a simplified enum for serialization. The actual TransportPreference
/// uses bitflags for all combinations. Use From trait for conversion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportPreferenceConfig {
    /// TCP-only mode (Bitcoin P2P compatible, default)
    TcpOnly,
    /// Quinn-only mode (direct QUIC)
    #[cfg(feature = "quinn")]
    QuinnOnly,
    /// Iroh-only mode (QUIC-based with NAT traversal)
    #[cfg(feature = "iroh")]
    IrohOnly,
    /// Hybrid mode (TCP + Iroh)
    #[cfg(feature = "iroh")]
    Hybrid,
    /// All transports (TCP + Quinn + Iroh)
    #[cfg(all(feature = "quinn", feature = "iroh"))]
    All,
}

impl Default for TransportPreferenceConfig {
    fn default() -> Self {
        Self::TcpOnly
    }
}

impl From<TransportPreferenceConfig> for TransportPreference {
    fn from(config: TransportPreferenceConfig) -> Self {
        match config {
            TransportPreferenceConfig::TcpOnly => TransportPreference::TCP_ONLY,
            #[cfg(feature = "quinn")]
            TransportPreferenceConfig::QuinnOnly => TransportPreference::QUINN_ONLY,
            #[cfg(feature = "iroh")]
            TransportPreferenceConfig::IrohOnly => TransportPreference::IROH_ONLY,
            #[cfg(feature = "iroh")]
            TransportPreferenceConfig::Hybrid => TransportPreference::hybrid(),
            #[cfg(all(feature = "quinn", feature = "iroh"))]
            TransportPreferenceConfig::All => TransportPreference::all_transports(),
        }
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen_addr: Some("127.0.0.1:8333".parse().unwrap()),
            block_validation: None,
            assumeutxo_blockhash: None,
            transport_preference: TransportPreferenceConfig::TcpOnly,
            max_outbound_peers: Some(100),
            protocol_version: Some("BitcoinV1".to_string()),
            modules: Some(ModuleConfig::default()),
            #[cfg(feature = "stratum-v2")]
            stratum_v2: None,
            rpc: None,
            rpc_auth: None,
            ban_list_sharing: None,
            #[cfg(feature = "governance")]
            governance: None,
            storage: None,
            persistent_peers: Vec::new(),
            enable_self_advertisement: true,
            dos_protection: None,
            ibd_protection: None,
            ibd: None,
            relay: None,
            #[cfg(feature = "fibre")]
            fibre: None,
            address_database: None,
            #[cfg(feature = "dandelion")]
            dandelion: None,
            peer_rate_limiting: None,
            network_timing: None,
            request_timeouts: None,
            protocol_limits: None,
            background_tasks: None,
            replay_protection: None,
            module_resource_limits: None,
            spam_ban: None,
            #[cfg(feature = "zmq")]
            zmq: None,
            logging: None,
            mempool: None,
            rbf: None,
            payment: None,
            rest_api: None,
        }
    }
}

/// Expand leading `~` to home directory. Leaves other paths unchanged.
fn expand_tilde_path(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return s.to_string();
    }
    if s == "~" {
        return dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "~".to_string());
    }
    if let (Some(home), Some(rest)) = (
        dirs::home_dir(),
        s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")),
    ) {
        return home.join(rest).to_string_lossy().into_owned();
    }
    s.to_string()
}

impl NodeConfig {
    /// Expand `~` to home directory in all path fields. Idempotent.
    pub fn expand_paths(&mut self) {
        if let Some(ref mut s) = self.storage {
            s.data_dir = expand_tilde_path(&s.data_dir);
        }
        if let Some(ref mut m) = self.modules {
            m.modules_dir = expand_tilde_path(&m.modules_dir);
            m.data_dir = expand_tilde_path(&m.data_dir);
            m.socket_dir = expand_tilde_path(&m.socket_dir);
        }
        if let Some(ref mut ibd) = self.ibd {
            if let Some(ref d) = ibd.dump_dir {
                ibd.dump_dir = Some(expand_tilde_path(d));
            }
            if let Some(ref d) = ibd.snapshot_dir {
                ibd.snapshot_dir = Some(expand_tilde_path(d));
            }
        }
    }

    /// Load configuration from file (supports JSON and TOML)
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        // Validate file permissions (warn if world-readable)
        #[cfg(unix)]
        {
            if let Ok(metadata) = std::fs::metadata(path) {
                use std::os::unix::fs::PermissionsExt;
                let permissions = metadata.permissions();
                let mode = permissions.mode();
                // Check if file is readable by others (group or world)
                if mode & 0o077 != 0 {
                    tracing::warn!(
                        "SECURITY WARNING: Configuration file {:?} is readable by others (mode: {:o}). \
                         Consider setting permissions to 600 for security. \
                         Run: chmod 600 {:?}",
                        path, mode, path
                    );
                }
            }
        }

        let content = std::fs::read_to_string(path)?;

        let mut config: NodeConfig = if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse TOML config: {}", e))?
        } else {
            serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse JSON config: {}", e))?
        };
        config.expand_paths();
        Ok(config)
    }

    /// Load configuration from JSON file
    pub fn from_json_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: NodeConfig = serde_json::from_str(&content)?;
        config.expand_paths();
        Ok(config)
    }

    /// Load configuration from TOML file
    pub fn from_toml_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: NodeConfig = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse TOML config: {}", e))?;
        config.expand_paths();
        Ok(config)
    }

    /// Save configuration to JSON file
    pub fn to_json_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Save configuration to TOML file
    pub fn to_toml_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize TOML config: {}", e))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Get transport preference
    pub fn get_transport_preference(&self) -> TransportPreference {
        self.transport_preference.into()
    }

    /// Auto-detect governance server and enable if available
    ///
    /// If `governance.commons_url` is set but `governance.enabled` is false,
    /// this will check if the server responds to a health check and auto-enable governance.
    #[cfg(feature = "governance")]
    pub async fn auto_detect_governance(&mut self) -> Result<(), anyhow::Error> {
        use reqwest::Client;
        use tracing::{debug, info};

        // Only auto-detect if governance config exists but is disabled
        if let Some(ref mut gov_config) = self.governance {
            if !gov_config.enabled {
                if let Some(ref commons_url) = gov_config.commons_url {
                    let client = Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()?;

                    let health_url = format!("{commons_url}/internal/health");
                    debug!("Auto-detecting governance server at {}", health_url);

                    match client.get(&health_url).send().await {
                        Ok(response) if response.status().is_success() => {
                            info!("Governance server detected and responding at {}, auto-enabling governance", commons_url);
                            gov_config.enabled = true;
                        }
                        Ok(response) => {
                            debug!("Governance server at {} responded with status {}, keeping governance disabled", commons_url, response.status());
                        }
                        Err(e) => {
                            debug!("Governance server at {} not reachable: {}, keeping governance disabled", commons_url, e);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl NodeConfig {
    /// Validate configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate pruning configuration
        if let Some(ref storage) = self.storage {
            if let Some(ref pruning) = storage.pruning {
                pruning.validate()?;
            }
        }

        Ok(())
    }

    /// Validate security configuration and return warnings
    ///
    /// Checks for insecure configurations that could expose the node to unauthorized access.
    /// Returns a list of warning messages that should be logged but don't prevent startup.
    ///
    /// # Arguments
    /// * `rpc_addr` - The RPC server bind address
    /// * `rest_api_addr` - Optional REST API server bind address
    ///
    /// # Returns
    /// Vector of warning messages (empty if configuration is secure)
    pub fn validate_security(
        &self,
        rpc_addr: SocketAddr,
        rest_api_addr: Option<SocketAddr>,
    ) -> Vec<String> {
        let mut warnings = Vec::new();

        // Helper to check if address is localhost
        let is_localhost =
            |addr: &SocketAddr| addr.ip().is_loopback() || addr.ip().to_string() == "127.0.0.1";

        // Check RPC authentication
        if let Some(ref rpc_auth) = self.rpc_auth {
            if !rpc_auth.required {
                // Check if binding to non-localhost
                if !is_localhost(&rpc_addr) {
                    warnings.push(format!(
                        "SECURITY WARNING: RPC server is binding to {rpc_addr} (non-localhost) but authentication is not required. \
                        This exposes your node to unauthorized access. Consider setting rpc_auth.required = true"
                    ));
                }
            } else if rpc_auth.tokens.is_empty() && rpc_auth.certificates.is_empty() {
                warnings.push(
                    "SECURITY WARNING: RPC authentication is required but no tokens or certificates are configured. \
                    RPC requests will be rejected. Add tokens or certificates to rpc_auth configuration."
                        .to_string(),
                );
            }
        } else {
            // No auth config at all
            if !is_localhost(&rpc_addr) {
                warnings.push(format!(
                    "SECURITY WARNING: RPC server is binding to {rpc_addr} (non-localhost) without authentication. \
                    This exposes your node to unauthorized access. Consider configuring rpc_auth with required = true"
                ));
            }
        }

        // Check REST API authentication
        if let Some(rest_addr) = rest_api_addr {
            if !is_localhost(&rest_addr) {
                if let Some(ref rpc_auth) = self.rpc_auth {
                    if !rpc_auth.required {
                        warnings.push(format!(
                            "SECURITY WARNING: REST API is binding to {rest_addr} (non-localhost) but authentication is not required. \
                            This exposes your node to unauthorized access. Consider setting rpc_auth.required = true"
                        ));
                    }
                } else {
                    warnings.push(format!(
                        "SECURITY WARNING: REST API is binding to {rest_addr} (non-localhost) without authentication. \
                        This exposes your node to unauthorized access. Consider configuring rpc_auth with required = true"
                    ));
                }
            }
        }

        warnings
    }

    /// Maximum RPC/REST request body size in bytes.
    /// ENV BLVM_RPC_MAX_REQUEST_SIZE_BYTES overrides config.
    pub fn max_request_size_bytes(&self) -> usize {
        crate::utils::env_int::<usize>("BLVM_RPC_MAX_REQUEST_SIZE_BYTES").unwrap_or_else(|| {
            self.rpc
                .as_ref()
                .map(|r| r.max_request_size_bytes)
                .unwrap_or(1_048_576)
        })
    }

    /// Max RPC connections per IP per minute.
    /// ENV BLVM_RPC_MAX_CONNECTIONS_PER_IP_PER_MINUTE overrides config.
    pub fn max_connections_per_ip_per_minute(&self) -> u32 {
        crate::utils::env_int::<u32>("BLVM_RPC_MAX_CONNECTIONS_PER_IP_PER_MINUTE").unwrap_or_else(
            || {
                self.rpc
                    .as_ref()
                    .map(|r| r.max_connections_per_ip_per_minute)
                    .unwrap_or(10)
            },
        )
    }

    /// When auth disabled, still apply IP rate limiting.
    /// ENV BLVM_RPC_RATE_LIMIT_WHEN_AUTH_DISABLED (true/1/yes/on) overrides config.
    pub fn rpc_rate_limit_when_auth_disabled(&self) -> bool {
        if std::env::var("BLVM_RPC_RATE_LIMIT_WHEN_AUTH_DISABLED").is_ok() {
            crate::utils::env_bool("BLVM_RPC_RATE_LIMIT_WHEN_AUTH_DISABLED")
        } else {
            self.rpc
                .as_ref()
                .map(|r| r.rate_limit_when_auth_disabled)
                .unwrap_or(true)
        }
    }

    /// IP rate limit burst when auth disabled.
    /// ENV BLVM_RPC_IP_RATE_LIMIT_BURST overrides config.
    pub fn rpc_ip_rate_limit_burst(&self) -> u32 {
        crate::utils::env_int::<u32>("BLVM_RPC_IP_RATE_LIMIT_BURST").unwrap_or_else(|| {
            self.rpc
                .as_ref()
                .map(|r| r.ip_rate_limit_burst)
                .unwrap_or(50)
        })
    }

    /// IP rate limit per second when auth disabled.
    /// ENV BLVM_RPC_IP_RATE_LIMIT_RATE overrides config.
    pub fn rpc_ip_rate_limit_rate(&self) -> u32 {
        crate::utils::env_int::<u32>("BLVM_RPC_IP_RATE_LIMIT_RATE")
            .unwrap_or_else(|| self.rpc.as_ref().map(|r| r.ip_rate_limit_rate).unwrap_or(5))
    }

    /// Batch rate limit multiplier cap: min(batch_len, this) tokens consumed.
    /// ENV BLVM_RPC_BATCH_RATE_MULTIPLIER_CAP overrides config.
    pub fn rpc_batch_rate_multiplier_cap(&self) -> u32 {
        crate::utils::env_int::<u32>("BLVM_RPC_BATCH_RATE_MULTIPLIER_CAP").unwrap_or_else(|| {
            self.rpc
                .as_ref()
                .map(|r| r.batch_rate_multiplier_cap)
                .unwrap_or(10)
        })
    }

    /// Connection rate limit window in seconds.
    /// ENV BLVM_RPC_CONNECTION_RATE_LIMIT_WINDOW_SECS overrides config.
    pub fn rpc_connection_rate_limit_window_seconds(&self) -> u64 {
        crate::utils::env_int::<u64>("BLVM_RPC_CONNECTION_RATE_LIMIT_WINDOW_SECS").unwrap_or_else(
            || {
                self.rpc
                    .as_ref()
                    .map(|r| r.connection_rate_limit_window_seconds)
                    .unwrap_or(60)
            },
        )
    }
}

/// ZMQ notification configuration
#[cfg(feature = "zmq")]
pub use crate::zmq::ZmqConfig;

/// Payment configuration (BIP70)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentConfig {
    /// Enable P2P BIP70 (default: true)
    #[serde(default = "default_true")]
    pub p2p_enabled: bool,

    /// Enable HTTP BIP70 (default: false, requires bip70-http feature)
    #[serde(default = "default_false")]
    pub http_enabled: bool,

    /// Network (mainnet, testnet, regtest, signet) - defaults to mainnet
    #[serde(default = "default_network")]
    pub network: Option<String>,

    /// Merchant private key for signing (optional, hex-encoded)
    #[serde(default)]
    pub merchant_key: Option<String>,

    /// Node payment address/script for module downloads (optional)
    /// Used when serving modules that require payment
    #[serde(default)]
    pub node_payment_address: Option<String>,

    /// Payment request storage path
    #[serde(default = "default_payment_store_path")]
    pub payment_store_path: String,

    /// Enable module payment integration (default: true)
    #[serde(default = "default_true")]
    pub module_payments_enabled: bool,

    /// Minimum confirmations before payment is "safe for release" (default: 6)
    /// Merchants should wait for this many confirmations before releasing high-value goods.
    #[serde(default = "default_safe_confirmation_depth")]
    pub safe_confirmation_depth: u32,
}

fn default_safe_confirmation_depth() -> u32 {
    6
}

fn default_payment_store_path() -> String {
    "data/payments".to_string()
}

fn default_network() -> Option<String> {
    Some("mainnet".to_string())
}

impl Default for PaymentConfig {
    fn default() -> Self {
        Self {
            p2p_enabled: true,
            http_enabled: false,
            network: default_network(),
            merchant_key: None,
            node_payment_address: None,
            payment_store_path: "data/payments".to_string(),
            module_payments_enabled: true,
            safe_confirmation_depth: default_safe_confirmation_depth(),
        }
    }
}

/// REST API configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RestApiConfig {
    /// Enable REST API (default: false, requires rest-api feature)
    #[serde(default = "default_false")]
    pub enabled: bool,

    /// Enable payment endpoints (default: false, requires bip70-http feature)
    #[serde(default = "default_false")]
    pub payment_endpoints_enabled: bool,
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggingConfig {
    /// Log level filter (e.g., "info", "debug", "blvm_node=debug,network=trace")
    /// If not set, uses RUST_LOG environment variable or defaults to "info"
    /// Config key "level" is accepted as alias for "filter" (e.g. level = "info")
    #[serde(default, alias = "level")]
    pub filter: Option<String>,

    /// Enable JSON logging format (for log aggregation systems)
    #[serde(default)]
    pub json_format: bool,
}

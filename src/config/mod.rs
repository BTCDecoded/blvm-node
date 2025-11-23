//! Configuration management for reference-node
//!
//! Handles configuration loading, validation, and transport selection.

use crate::network::transport::TransportPreference;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

// TOML support for configuration files

/// Module system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn default_true() -> bool {
    true
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

impl Default for ModuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            modules_dir: "modules".to_string(),
            data_dir: "data/modules".to_string(),
            socket_dir: "data/modules/sockets".to_string(),
            enabled_modules: Vec::new(),
            module_configs: std::collections::HashMap::new(),
        }
    }
}

/// Network timing and connection behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkTimingConfig {
    /// Target number of peers to connect to (Bitcoin Core uses 8-125)
    #[serde(default = "default_target_peer_count")]
    pub target_peer_count: usize,

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
            target_peer_count: 8,
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

/// Node configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Network listening address
    pub listen_addr: Option<SocketAddr>,

    /// Transport preference
    pub transport_preference: TransportPreferenceConfig,

    /// Maximum number of peers
    pub max_peers: Option<usize>,

    /// Protocol version
    pub protocol_version: Option<String>,

    /// Module system configuration
    pub modules: Option<ModuleConfig>,

    /// Stratum V2 mining configuration
    #[cfg(feature = "stratum-v2")]
    pub stratum_v2: Option<StratumV2Config>,

    /// RPC authentication configuration
    pub rpc_auth: Option<RpcAuthConfig>,

    /// Ban list sharing configuration
    pub ban_list_sharing: Option<BanListSharingConfig>,

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

    /// Network relay configuration
    pub relay: Option<RelayConfig>,

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

    /// Module resource limits configuration
    pub module_resource_limits: Option<ModuleResourceLimitsConfig>,

    /// Fee forwarding configuration (for governance contributions)
    pub fee_forwarding: Option<FeeForwardingConfig>,

    /// ZMQ notification configuration
    #[cfg(feature = "zmq")]
    pub zmq: Option<ZmqConfig>,

    /// Logging configuration
    pub logging: Option<LoggingConfig>,
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
            transport_preference: TransportPreferenceConfig::TcpOnly,
            max_peers: Some(100),
            protocol_version: Some("BitcoinV1".to_string()),
            modules: Some(ModuleConfig::default()),
            #[cfg(feature = "stratum-v2")]
            stratum_v2: None,
            rpc_auth: None,
            ban_list_sharing: None,
            storage: None,
            persistent_peers: Vec::new(),
            enable_self_advertisement: true,
            dos_protection: None,
            relay: None,
            address_database: None,
            #[cfg(feature = "dandelion")]
            dandelion: None,
            peer_rate_limiting: None,
            network_timing: None,
            request_timeouts: None,
            module_resource_limits: None,
            fee_forwarding: None,
            #[cfg(feature = "zmq")]
            zmq: None,
            logging: None,
        }
    }
}

/// Fee forwarding configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct FeeForwardingConfig {
    /// Enable fee forwarding to Commons address
    #[serde(default = "default_false")]
    pub enabled: bool,

    /// Commons address to forward fees to
    pub commons_address: Option<String>,

    /// Percentage of block reward to forward (0-100)
    #[serde(default = "default_fee_forwarding_percentage")]
    pub forwarding_percentage: u8,

    /// Contributor identifier (for tracking)
    pub contributor_id: Option<String>,
}

fn default_false() -> bool {
    false
}

fn default_fee_forwarding_percentage() -> u8 {
    0 // Default: no forwarding (opt-in)
}


/// Ban list sharing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanListSharingConfig {
    /// Enable ban list sharing
    #[serde(default = "default_true")]
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

/// RPC authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcAuthConfig {
    /// Require authentication for RPC requests
    #[serde(default)]
    pub required: bool,

    /// Valid authentication tokens
    #[serde(default)]
    pub tokens: Vec<String>,

    /// Valid certificate fingerprints (for certificate-based auth)
    #[serde(default)]
    pub certificates: Vec<String>,

    /// Default rate limit (burst, requests per second)
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,

    #[serde(default = "default_rate_limit_rate")]
    pub rate_limit_rate: u32,
}

fn default_rate_limit_burst() -> u32 {
    100
}

fn default_rate_limit_rate() -> u32 {
    10
}

impl Default for RpcAuthConfig {
    fn default() -> Self {
        Self {
            required: false,
            tokens: Vec::new(),
            certificates: Vec::new(),
            rate_limit_burst: 100,
            rate_limit_rate: 10,
        }
    }
}

impl NodeConfig {
    /// Load configuration from file (supports JSON and TOML)
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;

        if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            // Try TOML
            let config: NodeConfig = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse TOML config: {}", e))?;
            Ok(config)
        } else {
            // Default to JSON
            let config: NodeConfig = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse JSON config: {}", e))?;
            Ok(config)
        }
    }

    /// Load configuration from JSON file
    pub fn from_json_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: NodeConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Load configuration from TOML file
    pub fn from_toml_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: NodeConfig = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse TOML config: {}", e))?;
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
}

/// Stratum V2 mining configuration
#[cfg(feature = "stratum-v2")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StratumV2Config {
    /// Enable Stratum V2 mining
    pub enabled: bool,

    /// Pool URL for miner mode (format: "tcp://pool.example.com:3333" or "iroh://<nodeid>")
    pub pool_url: Option<String>,

    /// Listen address for server mode
    pub listen_addr: Option<SocketAddr>,

    /// Transport preference for Stratum V2 connections
    pub transport_preference: TransportPreferenceConfig,

    /// Enable merge mining
    pub merge_mining_enabled: bool,

    /// Secondary chains for merge mining
    pub secondary_chains: Vec<String>,
}

#[cfg(feature = "stratum-v2")]
impl Default for StratumV2Config {
    fn default() -> Self {
        Self {
            enabled: false,
            pool_url: None,
            listen_addr: None,
            transport_preference: TransportPreferenceConfig::TcpOnly,
            merge_mining_enabled: false,
            secondary_chains: Vec::new(),
        }
    }
}

/// Pruning mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PruningMode {
    /// No pruning (keep all blocks)
    Disabled,

    /// Normal pruning (keep recent blocks for verification)
    Normal {
        /// Keep blocks from this height onwards
        #[serde(default = "default_zero")]
        keep_from_height: u64,
        /// Keep at least this many recent blocks
        #[serde(default = "default_min_recent_blocks")]
        min_recent_blocks: u64,
    },

    /// Aggressive pruning with UTXO commitments
    /// Requires: utxo-commitments feature enabled
    Aggressive {
        /// Keep blocks from this height onwards
        #[serde(default = "default_zero")]
        keep_from_height: u64,
        /// Keep UTXO commitments for all pruned blocks
        #[serde(default = "default_true")]
        keep_commitments: bool,
        /// Keep filtered blocks (spam-filtered) for pruned range
        #[serde(default = "default_false")]
        keep_filtered_blocks: bool,
        /// Minimum blocks to keep (safety margin)
        #[serde(default = "default_min_blocks")]
        min_blocks: u64,
    },

    /// Custom pruning configuration
    Custom {
        /// Keep block headers (always required for PoW verification)
        #[serde(default = "default_true")]
        keep_headers: bool,
        /// Keep block bodies from this height onwards
        #[serde(default = "default_zero")]
        keep_bodies_from_height: u64,
        /// Keep UTXO commitments (if utxo-commitments feature enabled)
        #[serde(default = "default_false")]
        keep_commitments: bool,
        /// Keep BIP158 filters (if BIP157/158 enabled)
        #[serde(default = "default_false")]
        keep_filters: bool,
        /// Keep filtered blocks (spam-filtered)
        #[serde(default = "default_false")]
        keep_filtered_blocks: bool,
        /// Keep witness data (for SegWit verification)
        #[serde(default = "default_false")]
        keep_witnesses: bool,
        /// Keep transaction index
        #[serde(default = "default_false")]
        keep_tx_index: bool,
    },
}

fn default_zero() -> u64 {
    0
}

fn default_min_recent_blocks() -> u64 {
    288 // ~2 days at 10 min/block
}

fn default_min_blocks() -> u64 {
    144 // ~1 day at 10 min/block
}

/// Pruning configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruningConfig {
    /// Pruning mode
    #[serde(default = "default_pruning_mode")]
    pub mode: PruningMode,

    /// Automatic pruning (prune periodically as chain grows)
    #[serde(default = "default_false")]
    pub auto_prune: bool,

    /// Automatic pruning interval (blocks)
    /// Prune every N blocks if auto_prune is enabled
    #[serde(default = "default_auto_prune_interval")]
    pub auto_prune_interval: u64,

    /// Minimum blocks to keep (safety margin)
    /// Even with aggressive pruning, keep at least this many blocks
    #[serde(default = "default_min_blocks_to_keep")]
    pub min_blocks_to_keep: u64,

    /// Prune on startup (prune old blocks when node starts)
    #[serde(default = "default_false")]
    pub prune_on_startup: bool,

    /// Allow incremental pruning during IBD (requires UTXO commitments + aggressive mode)
    /// When enabled, old blocks are pruned incrementally during sync, keeping only a window
    /// of recent blocks. This prevents the need to download the full blockchain before pruning.
    #[serde(default = "default_false")]
    pub incremental_prune_during_ibd: bool,

    /// Block window size for incremental pruning (number of recent blocks to keep)
    /// Only used when incremental_prune_during_ibd is true
    #[serde(default = "default_prune_window_size")]
    pub prune_window_size: u64,

    /// Minimum blocks before starting incremental pruning during IBD
    /// Prevents pruning too early in the sync process
    #[serde(default = "default_min_blocks_for_incremental_prune")]
    pub min_blocks_for_incremental_prune: u64,

    /// UTXO commitments integration
    #[cfg(feature = "utxo-commitments")]
    pub utxo_commitments: Option<UtxoCommitmentsPruningConfig>,

    /// BIP158 filter integration
    #[cfg(feature = "bip158")]
    pub bip158_filters: Option<Bip158PruningConfig>,
}

fn default_pruning_mode() -> PruningMode {
    PruningMode::Aggressive {
        keep_from_height: 0,
        keep_commitments: true,
        keep_filtered_blocks: false,
        min_blocks: 144, // ~1 day at 10 min/block
    }
}

fn default_auto_prune_interval() -> u64 {
    144 // Prune every ~1 day at 10 min/block
}

fn default_min_blocks_to_keep() -> u64 {
    144 // ~1 day at 10 min/block
}

fn default_prune_window_size() -> u64 {
    144 // Keep last 144 blocks (~1 day) during incremental pruning
}

fn default_min_blocks_for_incremental_prune() -> u64 {
    288 // Start incremental pruning after 288 blocks (~2 days) to ensure stability
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            mode: PruningMode::Aggressive {
                keep_from_height: 0,
                keep_commitments: true,
                keep_filtered_blocks: false,
                min_blocks: 144, // ~1 day at 10 min/block
            },
            auto_prune: true,         // Enable automatic pruning
            auto_prune_interval: 144, // Prune every ~1 day
            min_blocks_to_keep: 144,
            prune_on_startup: false,               // Still false for safety
            incremental_prune_during_ibd: true,    // Enable incremental pruning during IBD
            prune_window_size: 144,                // Keep sliding window of 144 blocks during IBD
            min_blocks_for_incremental_prune: 288, // Start pruning after 288 blocks (~2 days)
            #[cfg(feature = "utxo-commitments")]
            utxo_commitments: None,
            #[cfg(feature = "bip158")]
            bip158_filters: None,
        }
    }
}

/// UTXO commitments pruning configuration
#[cfg(feature = "utxo-commitments")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoCommitmentsPruningConfig {
    /// Keep UTXO commitments for pruned blocks
    #[serde(default = "default_true")]
    pub keep_commitments: bool,

    /// Keep filtered blocks (spam-filtered) for pruned range
    #[serde(default = "default_false")]
    pub keep_filtered_blocks: bool,

    /// Generate commitments before pruning (if not already generated)
    #[serde(default = "default_true")]
    pub generate_before_prune: bool,

    /// Maximum age for commitments (days, 0 = keep forever)
    #[serde(default = "default_commitment_max_age")]
    pub max_commitment_age_days: u32,
}

#[cfg(feature = "utxo-commitments")]
fn default_commitment_max_age() -> u32 {
    0 // Keep forever by default
}

#[cfg(feature = "utxo-commitments")]
impl Default for UtxoCommitmentsPruningConfig {
    fn default() -> Self {
        Self {
            keep_commitments: true,
            keep_filtered_blocks: false,
            generate_before_prune: true,
            max_commitment_age_days: 0,
        }
    }
}

/// BIP158 filter pruning configuration
#[cfg(feature = "bip158")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bip158PruningConfig {
    /// Keep BIP158 filters for pruned blocks
    #[serde(default = "default_true")]
    pub keep_filters: bool,

    /// Keep filter header chain (always required for verification)
    #[serde(default = "default_true")]
    pub keep_filter_headers: bool,

    /// Maximum age for filters (days, 0 = keep forever)
    #[serde(default = "default_filter_max_age")]
    pub max_filter_age_days: u32,
}

#[cfg(feature = "bip158")]
fn default_filter_max_age() -> u32 {
    0 // Keep forever by default
}

#[cfg(feature = "bip158")]
impl Default for Bip158PruningConfig {
    fn default() -> Self {
        Self {
            keep_filters: true,
            keep_filter_headers: true,
            max_filter_age_days: 0,
        }
    }
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Database backend selection
    #[serde(default = "default_database_backend")]
    pub database_backend: DatabaseBackendConfig,

    /// Storage path
    #[serde(default = "default_storage_path")]
    pub data_dir: String,

    /// Pruning configuration
    pub pruning: Option<PruningConfig>,

    /// Cache sizes
    pub cache: Option<StorageCacheConfig>,
}

/// Database backend configuration
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseBackendConfig {
    /// Use sled database (beta, fallback)
    Sled,
    /// Use redb database (default, recommended)
    Redb,
    /// Auto-select based on availability
    Auto,
}

fn default_database_backend() -> DatabaseBackendConfig {
    DatabaseBackendConfig::Auto
}

fn default_storage_path() -> String {
    "data".to_string()
}

/// Storage cache configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCacheConfig {
    /// Block cache size (MB)
    #[serde(default = "default_block_cache_mb")]
    pub block_cache_mb: usize,

    /// UTXO cache size (MB)
    #[serde(default = "default_utxo_cache_mb")]
    pub utxo_cache_mb: usize,

    /// Header cache size (MB)
    #[serde(default = "default_header_cache_mb")]
    pub header_cache_mb: usize,
}

fn default_block_cache_mb() -> usize {
    100
}

fn default_utxo_cache_mb() -> usize {
    50
}

fn default_header_cache_mb() -> usize {
    10
}

impl Default for StorageCacheConfig {
    fn default() -> Self {
        Self {
            block_cache_mb: 100,
            utxo_cache_mb: 50,
            header_cache_mb: 10,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_backend: DatabaseBackendConfig::Auto,
            data_dir: "data".to_string(),
            pruning: None,
            cache: None,
        }
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
}

impl PruningConfig {
    /// Validate pruning configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate aggressive mode requires utxo-commitments feature
        if let PruningMode::Aggressive { .. } = self.mode {
            #[cfg(not(feature = "utxo-commitments"))]
            {
                return Err(anyhow::anyhow!(
                    "Aggressive pruning mode requires the 'utxo-commitments' feature to be enabled. \
                    Please enable it in Cargo.toml or use Normal pruning mode instead."
                ));
            }
        }

        // Validate min_blocks_to_keep is reasonable
        if self.min_blocks_to_keep == 0 {
            return Err(anyhow::anyhow!(
                "min_blocks_to_keep must be greater than 0 for safety"
            ));
        }

        // Validate auto_prune_interval
        if self.auto_prune && self.auto_prune_interval == 0 {
            return Err(anyhow::anyhow!(
                "auto_prune_interval must be greater than 0 when auto_prune is enabled"
            ));
        }

        // Validate mode-specific settings
        match &self.mode {
            PruningMode::Normal {
                min_recent_blocks, ..
            } => {
                if *min_recent_blocks == 0 {
                    return Err(anyhow::anyhow!(
                        "min_recent_blocks must be greater than 0 in Normal pruning mode"
                    ));
                }
            }
            PruningMode::Aggressive { min_blocks, .. } => {
                if *min_blocks == 0 {
                    return Err(anyhow::anyhow!(
                        "min_blocks must be greater than 0 in Aggressive pruning mode"
                    ));
                }
            }
            PruningMode::Custom { keep_headers, .. } => {
                if !keep_headers {
                    return Err(anyhow::anyhow!(
                        "keep_headers must be true in Custom pruning mode (required for PoW verification)"
                    ));
                }
            }
            PruningMode::Disabled => {
                // No validation needed
            }
        }

        Ok(())
    }
}

/// DoS protection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DosProtectionConfig {
    /// Maximum connections per IP per time window
    #[serde(default = "default_dos_max_connections_per_window")]
    pub max_connections_per_window: usize,

    /// Time window in seconds for connection rate limiting
    #[serde(default = "default_dos_window_seconds")]
    pub window_seconds: u64,

    /// Maximum message queue size
    #[serde(default = "default_dos_max_message_queue_size")]
    pub max_message_queue_size: usize,

    /// Maximum active connections
    #[serde(default = "default_dos_max_active_connections")]
    pub max_active_connections: usize,

    /// Auto-ban threshold (number of violations before auto-ban)
    #[serde(default = "default_dos_auto_ban_threshold")]
    pub auto_ban_threshold: usize,

    /// Default ban duration in seconds
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
    3600 // 1 hour
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

/// Network relay configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    /// Maximum age for relayed items (seconds)
    #[serde(default = "default_relay_max_age")]
    pub max_relay_age: u64,

    /// Maximum number of items to track
    #[serde(default = "default_relay_max_tracked_items")]
    pub max_tracked_items: usize,

    /// Enable block relay
    #[serde(default = "default_true")]
    pub enable_block_relay: bool,

    /// Enable transaction relay
    #[serde(default = "default_true")]
    pub enable_tx_relay: bool,

    /// Enable Dandelion++ privacy relay
    #[serde(default = "default_false")]
    pub enable_dandelion: bool,
}

fn default_relay_max_age() -> u64 {
    3600 // 1 hour
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
    /// Maximum number of addresses to store
    #[serde(default = "default_address_db_max_addresses")]
    pub max_addresses: usize,

    /// Address expiration time in seconds
    #[serde(default = "default_address_db_expiration")]
    pub expiration_seconds: u64,
}

fn default_address_db_max_addresses() -> usize {
    10000
}

fn default_address_db_expiration() -> u64 {
    24 * 60 * 60 // 24 hours
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
    /// Stem phase timeout in seconds
    #[serde(default = "default_dandelion_stem_timeout")]
    pub stem_timeout_seconds: u64,

    /// Probability of fluffing at each hop (0.0 to 1.0)
    #[serde(default = "default_dandelion_fluff_probability")]
    pub fluff_probability: f64,

    /// Maximum stem hops before forced fluff
    #[serde(default = "default_dandelion_max_stem_hops")]
    pub max_stem_hops: u8,
}

#[cfg(feature = "dandelion")]
fn default_dandelion_stem_timeout() -> u64 {
    10
}

#[cfg(feature = "dandelion")]
fn default_dandelion_fluff_probability() -> f64 {
    0.1 // 10%
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
    /// Default burst size (token bucket)
    #[serde(default = "default_peer_rate_burst")]
    pub default_burst: u32,

    /// Default rate (messages per second)
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

/// ZMQ notification configuration
#[cfg(feature = "zmq")]
pub use crate::zmq::ZmqConfig;

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct LoggingConfig {
    /// Log level filter (e.g., "info", "debug", "bllvm_node=debug,network=trace")
    /// If not set, uses RUST_LOG environment variable or defaults to "info"
    #[serde(default)]
    pub filter: Option<String>,

    /// Enable JSON logging format (for log aggregation systems)
    #[serde(default)]
    pub json_format: bool,
}


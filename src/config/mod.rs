//! Configuration management for reference-node
//!
//! Handles configuration loading, validation, and transport selection.

use crate::network::transport::TransportPreference;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

// Note: TOML parsing is optional - can use JSON or manual config instead

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
            TransportPreferenceConfig::Hybrid => TransportPreference::HYBRID,
            #[cfg(all(feature = "quinn", feature = "iroh"))]
            TransportPreferenceConfig::All => TransportPreference::ALL,
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
        }
    }
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
    /// Load configuration from JSON file
    pub fn from_json_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: NodeConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Save configuration to JSON file
    pub fn to_json_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
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

fn default_false() -> bool {
    false
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
    
    /// UTXO commitments integration
    #[cfg(feature = "utxo-commitments")]
    pub utxo_commitments: Option<UtxoCommitmentsPruningConfig>,
    
    /// BIP158 filter integration
    #[cfg(feature = "bip158")]
    pub bip158_filters: Option<Bip158PruningConfig>,
}

fn default_pruning_mode() -> PruningMode {
    PruningMode::Disabled
}

fn default_auto_prune_interval() -> u64 {
    144 // Prune every ~1 day at 10 min/block
}

fn default_min_blocks_to_keep() -> u64 {
    144 // ~1 day at 10 min/block
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            mode: PruningMode::Disabled,
            auto_prune: false,
            auto_prune_interval: 144,
            min_blocks_to_keep: 144,
            prune_on_startup: false,
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
            PruningMode::Normal { min_recent_blocks, .. } => {
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

//! Configuration management for reference-node
//!
//! Handles configuration loading, validation, and transport selection.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use crate::network::transport::TransportPreference;

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
    pub module_configs: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
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


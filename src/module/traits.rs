//! Module system traits and interfaces
//!
//! Defines the core traits that modules and the node use to communicate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

use crate::module::ipc::protocol::ModuleMessage;
use crate::{Block, BlockHeader, Hash, Transaction};

/// Module lifecycle state
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModuleState {
    /// Module is stopped/not running
    Stopped,
    /// Module is initializing
    Initializing,
    /// Module is running normally
    Running,
    /// Module is stopping
    Stopping,
    /// Module has crashed or errored
    Error(String),
}

/// Module metadata describing module identity and capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleMetadata {
    /// Module name (unique identifier)
    pub name: String,
    /// Module version (semantic versioning)
    pub version: String,
    /// Human-readable description
    pub description: String,
    /// Module author
    pub author: String,
    /// Capabilities this module declares it can use
    pub capabilities: Vec<String>,
    /// Required dependencies (module names with versions)
    pub dependencies: HashMap<String, String>,
    /// Module entry point (binary name or path)
    pub entry_point: String,
}

/// Module trait that all modules must implement
///
/// This trait is implemented by module binaries (separate processes),
/// not directly by Rust code in the node. The IPC layer translates
/// between this trait interface and the actual module process.
#[async_trait]
pub trait Module: Send + Sync {
    /// Initialize the module with given context
    ///
    /// Called when module is first loaded. Module should validate
    /// configuration and prepare for operation.
    async fn init(&mut self, context: ModuleContext) -> Result<(), ModuleError>;

    /// Start the module
    ///
    /// Module should begin its main processing loop here.
    async fn start(&mut self) -> Result<(), ModuleError>;

    /// Stop the module (graceful shutdown)
    ///
    /// Module should clean up resources and stop processing.
    async fn stop(&mut self) -> Result<(), ModuleError>;

    /// Shutdown the module (forced shutdown)
    ///
    /// Called when node is shutting down or module is being removed.
    /// Module must terminate immediately.
    async fn shutdown(&mut self) -> Result<(), ModuleError>;

    /// Get module metadata
    fn metadata(&self) -> &ModuleMetadata;

    /// Get current module state
    fn state(&self) -> ModuleState;
}

/// Context provided to modules for communication with node
///
/// This is the interface modules use to communicate with the base node.
/// All communication goes through IPC, so this is essentially a handle
/// to the IPC connection.
#[derive(Debug, Clone)]
pub struct ModuleContext {
    /// Module ID (unique identifier for this module instance)
    pub module_id: String,
    /// IPC socket path (Unix domain socket path for communication)
    pub socket_path: String,
    /// Module data directory (where module can store its state)
    pub data_dir: String,
    /// Module configuration (key-value pairs from config file)
    pub config: HashMap<String, String>,
}

impl ModuleContext {
    /// Create a new module context
    pub fn new(
        module_id: String,
        socket_path: String,
        data_dir: String,
        config: HashMap<String, String>,
    ) -> Self {
        Self {
            module_id,
            socket_path,
            data_dir,
            config,
        }
    }

    /// Get a configuration value
    pub fn get_config(&self, key: &str) -> Option<&String> {
        self.config.get(key)
    }

    /// Get a configuration value with default
    pub fn get_config_or(&self, key: &str, default: &str) -> String {
        self.config
            .get(key)
            .map(|s| s.as_str())
            .unwrap_or(default)
            .to_string()
    }
}

/// Node API trait - interface for modules to query node state
///
/// This trait defines what APIs modules can call on the node.
/// Implemented by the node side, used by modules through IPC.
#[async_trait]
pub trait NodeAPI: Send + Sync {
    /// Get a block by hash
    async fn get_block(&self, hash: &Hash) -> Result<Option<Block>, ModuleError>;

    /// Get a block header by hash
    async fn get_block_header(&self, hash: &Hash) -> Result<Option<BlockHeader>, ModuleError>;

    /// Get a transaction by hash
    async fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>, ModuleError>;

    /// Check if a transaction exists
    async fn has_transaction(&self, hash: &Hash) -> Result<bool, ModuleError>;

    /// Get current chain tip (highest block hash)
    async fn get_chain_tip(&self) -> Result<Hash, ModuleError>;

    /// Get current block height
    async fn get_block_height(&self) -> Result<u64, ModuleError>;

    /// Get UTXO by outpoint (read-only, cannot modify)
    async fn get_utxo(
        &self,
        outpoint: &crate::OutPoint,
    ) -> Result<Option<crate::UTXO>, ModuleError>;

    /// Subscribe to node events
    ///
    /// Returns a receiver that will receive event messages
    async fn subscribe_events(
        &self,
        event_types: Vec<EventType>,
    ) -> Result<tokio::sync::mpsc::Receiver<ModuleMessage>, ModuleError>;
}

/// Event types that modules can subscribe to
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    /// New block connected to chain
    NewBlock,
    /// New transaction in mempool
    NewTransaction,
    /// Block disconnected (chain reorg)
    BlockDisconnected,
    /// Chain reorganization occurred
    ChainReorg,
}

/// Module system errors
#[derive(Debug, Error)]
pub enum ModuleError {
    #[error("IPC communication error: {0}")]
    IpcError(String),

    #[error("Module initialization failed: {0}")]
    InitializationError(String),

    #[error("Module operation failed: {0}")]
    OperationError(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Module not found: {0}")]
    ModuleNotFound(String),

    #[error("Module dependency missing: {0}")]
    DependencyMissing(String),

    #[error("Invalid module manifest: {0}")]
    InvalidManifest(String),

    #[error("Module version incompatible: {0}")]
    VersionIncompatible(String),

    #[error("Module crashed: {0}")]
    ModuleCrashed(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Timeout waiting for module response")]
    Timeout,

    #[error("Resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),
}

impl From<serde_json::Error> for ModuleError {
    fn from(e: serde_json::Error) -> Self {
        ModuleError::SerializationError(e.to_string())
    }
}

impl From<bincode::Error> for ModuleError {
    fn from(e: bincode::Error) -> Self {
        ModuleError::SerializationError(e.to_string())
    }
}

impl From<anyhow::Error> for ModuleError {
    fn from(e: anyhow::Error) -> Self {
        ModuleError::OperationError(e.to_string())
    }
}

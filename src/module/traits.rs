//! Module system traits and interfaces
//!
//! Defines the core traits that modules and the node use to communicate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

use crate::module::ipc::protocol::{EventPayload, ModuleMessage};
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
    /// Hard dependencies - module cannot load without them
    pub dependencies: HashMap<String, String>,
    /// Optional dependencies (module names with versions)
    /// Soft dependencies - module can work without them
    pub optional_dependencies: HashMap<String, String>,
    /// Module entry point (binary name or path)
    pub entry_point: String,
}

/// Module information for discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleInfo {
    /// Module ID (unique identifier, format: {module_name}_{uuid})
    pub module_id: String,
    /// Module name (from metadata)
    pub module_name: String,
    /// Module version
    pub version: String,
    /// Module capabilities
    pub capabilities: Vec<String>,
    /// Module status
    pub status: ModuleState,
    /// API version (for compatibility checking)
    pub api_version: u32,
}

/// Module hooks for providing cached data (advisory only)
///
/// Modules can implement this trait to provide cached values for expensive operations.
/// All hooks are advisory - the node can ignore them if needed.
/// All hooks have timeouts to prevent hanging on unresponsive modules.
#[async_trait]
pub trait ModuleHooks: Send + Sync {
    /// Hook called when getting fee estimate
    /// Return Some(u64) to use cached value, None to calculate normally
    /// Timeout: 100ms (fail fast if module unresponsive)
    async fn get_fee_estimate_cached(
        &self,
        target_blocks: u32,
    ) -> Result<Option<u64>, ModuleError>;
    
    /// Hook called when getting mempool stats
    /// Return Some(MempoolStats) to use cached value, None to calculate normally
    /// Timeout: 50ms (fail fast if module unresponsive)
    async fn get_mempool_stats_cached(
        &self,
    ) -> Result<Option<MempoolSize>, ModuleError>;
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
    
    // === Mempool API Methods ===
    /// Get all transaction hashes in mempool
    async fn get_mempool_transactions(&self) -> Result<Vec<Hash>, ModuleError>;
    
    /// Get a transaction from mempool by hash
    async fn get_mempool_transaction(&self, tx_hash: &Hash) -> Result<Option<Transaction>, ModuleError>;
    
    /// Get mempool size information
    async fn get_mempool_size(&self) -> Result<MempoolSize, ModuleError>;
    
    // === Network API Methods ===
    /// Get network statistics
    async fn get_network_stats(&self) -> Result<NetworkStats, ModuleError>;
    
    /// Get list of connected peers
    async fn get_network_peers(&self) -> Result<Vec<PeerInfo>, ModuleError>;
    
    // === Chain API Methods ===
    /// Get chain information (tip, height, difficulty, etc.)
    async fn get_chain_info(&self) -> Result<ChainInfo, ModuleError>;
    
    /// Get block by height
    async fn get_block_by_height(&self, height: u64) -> Result<Option<Block>, ModuleError>;
    
    // === Lightning API Methods (for Lightning module) ===
    /// Get Lightning node connection info
    async fn get_lightning_node_url(&self) -> Result<Option<String>, ModuleError>;
    
    /// Get Lightning node information
    async fn get_lightning_info(&self) -> Result<Option<LightningInfo>, ModuleError>;
    
    // === Payment API Methods ===
    /// Get payment state by payment ID
    async fn get_payment_state(&self, payment_id: &str) -> Result<Option<PaymentState>, ModuleError>;
    
    // === Additional Mempool API Methods ===
    /// Check if a transaction is in the mempool
    async fn check_transaction_in_mempool(&self, tx_hash: &Hash) -> Result<bool, ModuleError>;
    
    /// Get fee estimate for target confirmation blocks
    /// Returns fee rate in satoshis per vbyte
    async fn get_fee_estimate(&self, target_blocks: u32) -> Result<u64, ModuleError>;
    
    // === Module RPC Endpoint Registration ===
    /// Register a JSON-RPC endpoint
    /// Method name must have module prefix (e.g., "lightning_*", "stratum_*")
    /// Cannot override core endpoints (whitelist enforced)
    async fn register_rpc_endpoint(
        &self,
        method: String,
        description: String,
    ) -> Result<(), ModuleError>;
    
    /// Unregister an RPC endpoint (on module shutdown)
    async fn unregister_rpc_endpoint(&self, method: &str) -> Result<(), ModuleError>;
    
    // === Timers and Scheduled Tasks ===
    /// Register a periodic timer
    /// Returns a timer ID that can be used to cancel
    /// Callback is async and can call NodeAPI methods
    async fn register_timer(
        &self,
        interval_seconds: u64,
        callback: Arc<dyn crate::module::timers::manager::TimerCallback>,
    ) -> Result<crate::module::timers::manager::TimerId, ModuleError>;
    
    /// Cancel a registered timer
    async fn cancel_timer(
        &self,
        timer_id: crate::module::timers::manager::TimerId,
    ) -> Result<(), ModuleError>;
    
    /// Schedule a one-time task
    /// Callback is async and can call NodeAPI methods
    async fn schedule_task(
        &self,
        delay_seconds: u64,
        callback: Arc<dyn crate::module::timers::manager::TaskCallback>,
    ) -> Result<crate::module::timers::manager::TaskId, ModuleError>;
    
    // === Metrics and Telemetry ===
    /// Report a metric to the node
    async fn report_metric(&self, metric: crate::module::metrics::manager::Metric) -> Result<(), ModuleError>;
    
    /// Get module metrics (for RPC/metrics endpoint)
    async fn get_module_metrics(&self, module_id: &str) -> Result<Vec<crate::module::metrics::manager::Metric>, ModuleError>;
    
    /// Get aggregated metrics from all modules
    async fn get_all_metrics(&self) -> Result<std::collections::HashMap<String, Vec<crate::module::metrics::manager::Metric>>, ModuleError>;
    
    // === Filesystem API Methods ===
    /// Read a file from the module's data directory
    /// Path must be within the module's sandboxed data directory
    async fn read_file(&self, path: String) -> Result<Vec<u8>, ModuleError>;
    
    /// Write data to a file in the module's data directory
    /// Path must be within the module's sandboxed data directory
    async fn write_file(&self, path: String, data: Vec<u8>) -> Result<(), ModuleError>;
    
    /// Delete a file from the module's data directory
    async fn delete_file(&self, path: String) -> Result<(), ModuleError>;
    
    /// List directory contents
    async fn list_directory(&self, path: String) -> Result<Vec<String>, ModuleError>;
    
    /// Create a directory in the module's data directory
    async fn create_directory(&self, path: String) -> Result<(), ModuleError>;
    
    /// Get file metadata (size, type, timestamps)
    async fn get_file_metadata(&self, path: String) -> Result<crate::module::ipc::protocol::FileMetadata, ModuleError>;
    
    // === Storage API Methods ===
    /// Open a storage tree (isolated per module)
    /// Returns a tree ID that can be used for subsequent operations
    async fn storage_open_tree(&self, name: String) -> Result<String, ModuleError>;
    
    /// Insert a key-value pair into storage
    async fn storage_insert(&self, tree_id: String, key: Vec<u8>, value: Vec<u8>) -> Result<(), ModuleError>;
    
    /// Get a value from storage by key
    async fn storage_get(&self, tree_id: String, key: Vec<u8>) -> Result<Option<Vec<u8>>, ModuleError>;
    
    /// Remove a key-value pair from storage
    async fn storage_remove(&self, tree_id: String, key: Vec<u8>) -> Result<(), ModuleError>;
    
    /// Check if a key exists in storage
    async fn storage_contains_key(&self, tree_id: String, key: Vec<u8>) -> Result<bool, ModuleError>;
    
    /// Iterate over all key-value pairs in storage
    async fn storage_iter(&self, tree_id: String) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ModuleError>;
    
    /// Execute a transaction (atomic batch of operations)
    async fn storage_transaction(&self, tree_id: String, operations: Vec<crate::module::ipc::protocol::StorageOperation>) -> Result<(), ModuleError>;
    
    /// Initialize filesystem and storage access for a module
    /// Called when a module connects to set up per-module sandbox and storage
    async fn initialize_module(
        &self,
        module_id: String,
        module_data_dir: std::path::PathBuf,
        base_data_dir: std::path::PathBuf,
    ) -> Result<(), ModuleError>;
    
    // === Module Discovery API Methods ===
    /// Discover all available modules
    /// Returns list of module information for all loaded modules
    async fn discover_modules(&self) -> Result<Vec<ModuleInfo>, ModuleError>;
    
    /// Get information about a specific module
    /// Returns None if module is not loaded
    async fn get_module_info(&self, module_id: &str) -> Result<Option<ModuleInfo>, ModuleError>;
    
    /// Check if a module is available (loaded and running)
    async fn is_module_available(&self, module_id: &str) -> Result<bool, ModuleError>;
    
    // === Module Event Publishing ===
    /// Publish an event (modules can publish to other modules)
    /// Event will be delivered to all modules subscribed to the event type
    async fn publish_event(
        &self,
        event_type: EventType,
        payload: EventPayload,
    ) -> Result<(), ModuleError>;
    
    // === Module-to-Module Communication ===
    /// Call an API method on another module
    ///
    /// # Arguments
    /// * `target_module_id` - ID of the target module (or None to use method routing)
    /// * `method` - API method name
    /// * `params` - Serialized parameters (typically JSON)
    ///
    /// # Returns
    /// Serialized response data
    async fn call_module(
        &self,
        target_module_id: Option<&str>,
        method: &str,
        params: Vec<u8>,
    ) -> Result<Vec<u8>, ModuleError>;
    
    /// Register a module API for other modules to call
    ///
    /// # Arguments
    /// * `api` - The API implementation
    async fn register_module_api(
        &self,
        api: Arc<dyn crate::module::inter_module::api::ModuleAPI>,
    ) -> Result<(), ModuleError>;
    
    /// Unregister a module API
    async fn unregister_module_api(&self) -> Result<(), ModuleError>;
    
    // === Network Integration ===
    /// Send mesh packet to a module (for network layer integration)
    /// This allows the network layer to route mesh packets to the mesh module
    async fn send_mesh_packet_to_module(
        &self,
        module_id: &str,
        packet_data: Vec<u8>,
        peer_addr: String,
    ) -> Result<(), ModuleError>;
    
    /// Send mesh packet to a network peer
    /// This allows modules to send mesh packets to network peers
    async fn send_mesh_packet_to_peer(
        &self,
        peer_addr: String,
        packet_data: Vec<u8>,
    ) -> Result<(), ModuleError>;
    
    /// Send Stratum V2 message to peer (miner)
    /// This allows the Stratum V2 module to send messages to miners
    async fn send_stratum_v2_message_to_peer(
        &self,
        peer_addr: String,
        message_data: Vec<u8>,
    ) -> Result<(), ModuleError>;
    
    // === Module Health & Monitoring ===
    /// Get health status of a module
    async fn get_module_health(&self, module_id: &str) -> Result<Option<crate::module::process::monitor::ModuleHealth>, ModuleError>;
    
    /// Get health status of all modules
    async fn get_all_module_health(&self) -> Result<Vec<(String, crate::module::process::monitor::ModuleHealth)>, ModuleError>;
    
    /// Report module health (for modules to self-report)
    async fn report_module_health(
        &self,
        health: crate::module::process::monitor::ModuleHealth,
    ) -> Result<(), ModuleError>;
}

/// Event types that modules can subscribe to
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    // === Core Blockchain Events ===
    /// New block connected to chain
    NewBlock,
    /// New transaction in mempool
    NewTransaction,
    /// Block disconnected (chain reorg)
    BlockDisconnected,
    /// Chain reorganization occurred
    ChainReorg,
    
    // === Payment Events (for Lightning module) ===
    /// Payment request created
    PaymentRequestCreated,
    /// Payment settled (confirmed on-chain)
    PaymentSettled,
    /// Payment failed
    PaymentFailed,
    /// Lightning payment verified
    PaymentVerified,
    /// Payment route discovered
    PaymentRouteFound,
    /// Payment routing failed
    PaymentRouteFailed,
    /// Lightning channel opened
    ChannelOpened,
    /// Lightning channel closed
    ChannelClosed,
    
    // === Mining Events (for Stratum V2 module) ===
    /// Block mined successfully
    BlockMined,
    /// Block template updated
    BlockTemplateUpdated,
    /// Mining difficulty changed
    MiningDifficultyChanged,
    /// Mining job created
    MiningJobCreated,
    
    // === Mesh Networking Events ===
    /// Mesh packet received from network
    MeshPacketReceived,
    // === Stratum V2 Events ===
    /// Stratum V2 message received from network
    StratumV2MessageReceived,
    /// Mining share submitted
    ShareSubmitted,
    /// Merge mining reward received
    MergeMiningReward,
    /// Mining pool connected
    MiningPoolConnected,
    /// Mining pool disconnected
    MiningPoolDisconnected,
    
    // === Governance Events (for Governance module) ===
    /// Governance proposal created
    GovernanceProposalCreated,
    /// Vote cast on proposal
    GovernanceProposalVoted,
    /// Proposal merged
    GovernanceProposalMerged,
    /// Economic node registered
    EconomicNodeRegistered,
    /// Economic node veto signal
    EconomicNodeVeto,
    /// Webhook sent
    WebhookSent,
    /// Webhook delivery failed
    WebhookFailed,
    /// Veto threshold reached
    VetoThresholdReached,
    /// Governance fork detected
    GovernanceForkDetected,
    
    // === Network Events (for Mesh module) ===
    /// Peer connected
    PeerConnected,
    /// Peer disconnected
    PeerDisconnected,
    /// Peer banned
    PeerBanned,
    /// Peer unbanned
    PeerUnbanned,
    /// Network message received
    MessageReceived,
    /// Network message sent
    MessageSent,
    /// Broadcast operation started
    BroadcastStarted,
    /// Broadcast operation completed
    BroadcastCompleted,
    /// Route discovered
    RouteDiscovered,
    /// Route failed
    RouteFailed,
    /// Connection attempt (success/failure)
    ConnectionAttempt,
    /// New peer address discovered
    AddressDiscovered,
    /// Peer address expired
    AddressExpired,
    /// Network partition detected
    NetworkPartition,
    /// Network partition reconnected
    NetworkReconnected,
    /// DoS attack detected
    DoSAttackDetected,
    /// Rate limit exceeded
    RateLimitExceeded,
    
    // === Consensus Events ===
    /// Block validation started
    BlockValidationStarted,
    /// Block validation completed (success/failure)
    BlockValidationCompleted,
    /// Script verification started
    ScriptVerificationStarted,
    /// Script verification completed
    ScriptVerificationCompleted,
    /// UTXO validation started
    UTXOValidationStarted,
    /// UTXO validation completed
    UTXOValidationCompleted,
    /// Network difficulty adjusted
    DifficultyAdjusted,
    /// Soft fork activated (SegWit, Taproot, CTV, etc.)
    SoftForkActivated,
    /// Soft fork locked in (BIP9)
    SoftForkLockedIn,
    /// Consensus rule violation detected
    ConsensusRuleViolation,
    
    // === Sync Events ===
    /// Headers sync started
    HeadersSyncStarted,
    /// Headers sync progress update
    HeadersSyncProgress,
    /// Headers sync completed
    HeadersSyncCompleted,
    /// Block sync started (IBD)
    BlockSyncStarted,
    /// Block sync progress update
    BlockSyncProgress,
    /// Block sync completed
    BlockSyncCompleted,
    /// Sync state changed (Initial → Headers → Blocks → Synced)
    SyncStateChanged,
    
    // === Mempool Events ===
    /// Transaction added to mempool
    MempoolTransactionAdded,
    /// Transaction removed from mempool
    MempoolTransactionRemoved,
    /// Mempool size threshold exceeded
    MempoolThresholdExceeded,
    /// Fee rate changed (derived from mempool)
    FeeRateChanged,
    /// Mempool cleared
    MempoolCleared,
    
    // === Storage Events ===
    /// Storage read operation
    StorageRead,
    /// Storage write operation
    StorageWrite,
    /// Storage query operation
    StorageQuery,
    /// Database backup started
    DatabaseBackupStarted,
    /// Database backup completed
    DatabaseBackupCompleted,
    
    // === Module Lifecycle Events ===
    /// Module loaded successfully
    ModuleLoaded,
    /// Module unloaded
    ModuleUnloaded,
    /// Module reloaded
    ModuleReloaded,
    /// Module started
    ModuleStarted,
    /// Module stopped
    ModuleStopped,
    /// Module crashed
    ModuleCrashed,
    /// Module health status changed
    ModuleHealthChanged,
    /// Module state changed
    ModuleStateChanged,
    
    // === Dandelion++ Events ===
    /// Transaction entered stem phase
    DandelionStemStarted,
    /// Transaction advanced in stem phase
    DandelionStemAdvanced,
    /// Transaction transitioned to fluff phase
    DandelionFluffed,
    /// Stem path expired
    DandelionStemPathExpired,
    
    // === Compact Blocks Events ===
    /// Compact block received
    CompactBlockReceived,
    /// Block reconstruction started
    BlockReconstructionStarted,
    /// Block reconstruction completed
    BlockReconstructionCompleted,
    
    // === FIBRE Events ===
    /// Block encoded for FIBRE
    FibreBlockEncoded,
    /// Block sent via FIBRE
    FibreBlockSent,
    /// FIBRE peer registered
    FibrePeerRegistered,
    
    // === Package Relay Events ===
    /// Transaction package received
    PackageReceived,
    /// Transaction package rejected
    PackageRejected,
    
    // === UTXO Commitments Events ===
    /// UTXO commitment received
    UtxoCommitmentReceived,
    /// UTXO commitment verified
    UtxoCommitmentVerified,
    
    // === Ban List Sharing Events ===
    /// Ban list shared with peer
    BanListShared,
    /// Ban list received from peer
    BanListReceived,
    
    // === Module Registry Events ===
    /// Module discovered
    ModuleDiscovered,
    /// Module installed
    ModuleInstalled,
    /// Module updated
    ModuleUpdated,
    /// Module removed
    ModuleRemoved,
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

    #[error("Rate limit exceeded: {0}")]
    RateLimitExceeded(String),

    #[error("Timeout waiting for module response")]
    Timeout,

    #[error("Resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    #[error("Cryptographic error: {0}")]
    CryptoError(String),
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

// === API Response Types ===

/// Mempool size information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolSize {
    /// Number of transactions in mempool
    pub transaction_count: usize,
    /// Total size in bytes
    pub size_bytes: usize,
    /// Total fee in sats
    pub total_fee_sats: u64,
}

/// Network statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStats {
    /// Number of connected peers
    pub peer_count: usize,
    /// Network hash rate (hashes per second)
    pub hash_rate: f64,
    /// Bytes sent
    pub bytes_sent: u64,
    /// Bytes received
    pub bytes_received: u64,
}

/// Peer information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Peer address
    pub addr: String,
    /// Transport type
    pub transport_type: String,
    /// Services flags
    pub services: u64,
    /// Protocol version
    pub version: u32,
    /// Connected since (timestamp)
    pub connected_since: u64,
}

/// Chain information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    /// Current chain tip hash
    pub tip_hash: Hash,
    /// Current block height
    pub height: u64,
    /// Current difficulty
    pub difficulty: u32,
    /// Chain work
    pub chain_work: u64,
    /// Is synced
    pub is_synced: bool,
}

/// Lightning node information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightningInfo {
    /// Lightning node URL
    pub node_url: String,
    /// Node public key
    pub node_pubkey: Vec<u8>,
    /// Number of channels
    pub channel_count: usize,
    /// Total channel capacity (sats)
    pub total_capacity_sats: u64,
}

/// Payment state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentState {
    /// Payment ID
    pub payment_id: String,
    /// Payment status
    pub status: String, // "pending", "confirmed", "failed"
    /// Amount in sats
    pub amount_sats: u64,
    /// Transaction hash (if confirmed)
    pub tx_hash: Option<Hash>,
    /// Confirmations (if confirmed)
    pub confirmations: Option<u32>,
}

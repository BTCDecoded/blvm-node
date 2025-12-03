//! Node API implementation for modules
//!
//! Provides a NodeAPI implementation that modules can use to query the node state.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Thread-local storage for current module ID during API calls
thread_local! {
    static CURRENT_MODULE_ID: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

use crate::module::api::events::EventManager;
use crate::module::hooks::HookManager;
use crate::module::ipc::protocol::ModuleMessage;
use crate::module::metrics::manager::{Metric, MetricsManager};
use crate::module::ipc::protocol::EventPayload;
use crate::module::traits::{
    ChainInfo, EventType, LightningInfo, MempoolSize, ModuleError, ModuleInfo, ModuleState, NetworkStats, NodeAPI,
    PaymentState, PeerInfo,
};
use crate::node::mempool::MempoolManager;
use crate::network::{NetworkManager, transport::TransportAddr};
use crate::storage::Storage;
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

/// Node API implementation for modules
pub struct NodeApiImpl {
    /// Storage reference for querying blockchain data
    storage: Arc<Storage>,
    /// Event manager for event subscriptions
    event_manager: Option<Arc<EventManager>>,
    /// Module ID for this API instance (used for event subscriptions)
    module_id: Option<String>,
    /// Mempool manager (optional, for mempool queries)
    mempool_manager: Option<Arc<MempoolManager>>,
    /// Network manager (optional, for network queries)
    network_manager: Option<Arc<NetworkManager>>,
    /// RPC server (optional, for RPC endpoint registration)
    rpc_server: Option<Arc<crate::rpc::server::RpcServer>>,
    /// Hook manager (optional, for module hooks)
    hook_manager: Option<Arc<tokio::sync::RwLock<HookManager>>>,
    /// Timer manager (optional, for timers and scheduled tasks)
    timer_manager: Option<Arc<crate::module::timers::manager::TimerManager>>,
    /// Module ID (for timer/task registration)
    module_id_for_timers: Option<String>,
    /// Metrics manager (optional, for metrics reporting)
    metrics_manager: Option<Arc<MetricsManager>>,
    /// Module ID (for metrics reporting)
    module_id_for_metrics: Option<String>,
    /// Filesystem sandbox for path validation
    filesystem_sandbox: Option<Arc<crate::module::sandbox::filesystem::FileSystemSandbox>>,
    /// Module data directory path
    module_data_dir: Option<std::path::PathBuf>,
    /// Storage database for module storage API (isolated per module)
    module_storage: Option<Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn crate::storage::database::Tree>>>>>,
    /// Per-module filesystem sandboxes (module_id -> sandbox)
    module_filesystem_sandboxes: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<crate::module::sandbox::filesystem::FileSystemSandbox>>>>,
    /// Per-module data directories (module_id -> path)
    module_data_dirs: Arc<tokio::sync::RwLock<std::collections::HashMap<String, std::path::PathBuf>>>,
    /// Per-module storage trees (module_id -> tree_id -> tree)
    module_storage_trees: Arc<tokio::sync::RwLock<std::collections::HashMap<String, std::collections::HashMap<String, Arc<dyn crate::storage::database::Tree>>>>>,
    /// Node storage for opening module storage trees
    node_storage: Option<Arc<Storage>>,
    /// IPC server reference (for RPC endpoint registration)
    ipc_server: Option<Arc<tokio::sync::Mutex<crate::module::ipc::server::ModuleIpcServer>>>,
    /// Sync coordinator (optional, for sync status checking)
    sync_coordinator: Option<Arc<tokio::sync::Mutex<crate::node::sync::SyncCoordinator>>>,
    /// Payment state machine (optional, for payment state queries)
    payment_state_machine: Option<Arc<crate::payment::state_machine::PaymentStateMachine>>,
    /// Module manager (optional, for module discovery)
    module_manager: Option<Arc<tokio::sync::Mutex<crate::module::manager::ModuleManager>>>,
    /// Module API registry (for module-to-module communication)
    module_api_registry: Option<Arc<crate::module::inter_module::registry::ModuleApiRegistry>>,
    /// Module router (for routing module-to-module calls)
    module_router: Option<Arc<crate::module::inter_module::router::ModuleRouter>>,
    /// Current module ID (for API registration)
    current_module_id_for_api: Option<String>,
}

impl NodeApiImpl {
    /// Create a new Node API implementation
    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            event_manager: None,
            module_id: None,
            mempool_manager: None,
            network_manager: None,
            rpc_server: None,
            hook_manager: None,
            timer_manager: None,
            module_id_for_timers: None,
            metrics_manager: None,
            module_id_for_metrics: None,
            filesystem_sandbox: None,
            module_data_dir: None,
            module_storage: None,
            module_filesystem_sandboxes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            module_data_dirs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            module_storage_trees: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            node_storage: Some(Arc::clone(&storage)),
            ipc_server: None,
            sync_coordinator: None,
            payment_state_machine: None,
            module_manager: None,
            module_api_registry: None,
            module_router: None,
            current_module_id_for_api: None,
        }
    }

    /// Create a new Node API implementation with event manager
    pub fn with_event_manager(
        storage: Arc<Storage>,
        event_manager: Arc<EventManager>,
        module_id: String,
    ) -> Self {
        Self {
            storage,
            event_manager: Some(event_manager),
            module_id: Some(module_id),
            mempool_manager: None,
            network_manager: None,
            rpc_server: None,
            hook_manager: None,
            timer_manager: None,
            module_id_for_timers: None,
            metrics_manager: None,
            module_id_for_metrics: None,
            filesystem_sandbox: None,
            module_data_dir: None,
            module_storage: None,
            module_filesystem_sandboxes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            module_data_dirs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            module_storage_trees: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            node_storage: None,
            ipc_server: None,
            sync_coordinator: None,
            payment_state_machine: None,
            module_manager: None,
            module_api_registry: None,
            module_router: None,
            current_module_id_for_api: None,
        }
    }
    
    /// Create a new Node API implementation with all dependencies
    pub fn with_dependencies(
        storage: Arc<Storage>,
        event_manager: Option<Arc<EventManager>>,
        module_id: Option<String>,
        mempool_manager: Option<Arc<MempoolManager>>,
        network_manager: Option<Arc<NetworkManager>>,
    ) -> Self {
        Self {
            storage,
            event_manager,
            module_id,
            mempool_manager,
            network_manager,
            rpc_server: None,
            hook_manager: None,
            timer_manager: None,
            module_id_for_timers: None,
            metrics_manager: None,
            module_id_for_metrics: None,
            filesystem_sandbox: None,
            module_data_dir: None,
            module_storage: None,
            module_filesystem_sandboxes: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            module_data_dirs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            module_storage_trees: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            node_storage: Some(Arc::clone(&storage)),
            sync_coordinator: None,
            payment_state_machine: None,
            module_manager: None,
            module_api_registry: None,
            module_router: None,
            current_module_id_for_api: None,
        }
    }
    
    /// Set module manager (for module discovery)
    pub fn set_module_manager(&mut self, module_manager: Arc<tokio::sync::Mutex<crate::module::manager::ModuleManager>>) {
        self.module_manager = Some(module_manager);
    }
    
    /// Set module API registry and router (for module-to-module communication)
    pub fn set_module_api_registry(
        &mut self,
        registry: Arc<crate::module::inter_module::registry::ModuleApiRegistry>,
        router: Arc<crate::module::inter_module::router::ModuleRouter>,
    ) {
        self.module_api_registry = Some(registry);
        self.module_router = Some(router);
    }
    
    /// Set current module ID (for API registration)
    pub fn set_current_module_id_for_api(&mut self, module_id: String) {
        self.current_module_id_for_api = Some(module_id);
    }
    
    /// Initialize filesystem and storage access for a module
    pub async fn initialize_module(
        &self,
        module_id: String,
        module_data_dir: std::path::PathBuf,
        base_data_dir: std::path::PathBuf,
    ) -> Result<(), ModuleError> {
        // Create filesystem sandbox for this module
        let sandbox = Arc::new(crate::module::sandbox::filesystem::FileSystemSandbox::new(&base_data_dir));
        
        // Store per-module state
        {
            let mut sandboxes = self.module_filesystem_sandboxes.write().await;
            sandboxes.insert(module_id.clone(), sandbox);
        }
        
        {
            let mut dirs = self.module_data_dirs.write().await;
            dirs.insert(module_id.clone(), module_data_dir);
        }
        
        // Initialize storage trees map for this module
        {
            let mut trees = self.module_storage_trees.write().await;
            trees.insert(module_id, std::collections::HashMap::new());
        }
        
        Ok(())
    }
    
    /// Get module ID from context (for filesystem/storage operations)
    /// First tries thread-local (set by API hub), then falls back to instance module_id
    fn get_module_id(&self) -> Option<String> {
        CURRENT_MODULE_ID.with(|id| {
            id.borrow().clone()
        }).or_else(|| self.module_id.clone())
    }
    
    /// Set current module ID in thread-local (for API hub to use)
    pub fn set_current_module_id(module_id: String) {
        CURRENT_MODULE_ID.with(|id| {
            *id.borrow_mut() = Some(module_id);
        });
    }
    
    /// Clear current module ID from thread-local
    pub fn clear_current_module_id() {
        CURRENT_MODULE_ID.with(|id| {
            *id.borrow_mut() = None;
        });
    }
    
    /// Set filesystem sandbox and module data directory (for late initialization - deprecated, use initialize_module)
    pub fn set_filesystem_access(&mut self, sandbox: Arc<crate::module::sandbox::filesystem::FileSystemSandbox>, data_dir: std::path::PathBuf) {
        self.filesystem_sandbox = Some(sandbox);
        self.module_data_dir = Some(data_dir);
    }
    
    /// Set module storage (for late initialization)
    pub fn set_module_storage(&mut self, storage: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn crate::storage::database::Tree>>>>) {
        self.module_storage = Some(storage);
    }
    
    /// Set hook manager (for late initialization)
    pub fn set_hook_manager(&mut self, hook_manager: Arc<tokio::sync::RwLock<HookManager>>) {
        self.hook_manager = Some(hook_manager);
    }
    
    /// Set timer manager (for late initialization)
    pub fn set_timer_manager(&mut self, timer_manager: Arc<crate::module::timers::manager::TimerManager>, module_id: String) {
        self.timer_manager = Some(timer_manager);
        self.module_id_for_timers = Some(module_id);
    }
    
    /// Set RPC server (for late initialization)
    pub fn set_rpc_server(&mut self, rpc_server: Arc<crate::rpc::server::RpcServer>) {
        self.rpc_server = Some(rpc_server);
    }

    /// Set event manager (for late initialization)
    pub fn set_event_manager(&mut self, event_manager: Arc<EventManager>, module_id: String) {
        self.event_manager = Some(event_manager);
        self.module_id = Some(module_id);
    }

    /// Set mempool manager (for late initialization)
    pub fn set_mempool_manager(&mut self, mempool_manager: Arc<MempoolManager>) {
        self.mempool_manager = Some(mempool_manager);
    }

    /// Set network manager (for late initialization)
    pub fn set_network_manager(&mut self, network_manager: Arc<NetworkManager>) {
        self.network_manager = Some(network_manager);
    }

    /// Set sync coordinator (for late initialization)
    pub fn set_sync_coordinator(&mut self, sync_coordinator: Arc<tokio::sync::Mutex<crate::node::sync::SyncCoordinator>>) {
        self.sync_coordinator = Some(sync_coordinator);
    }

    /// Set payment state machine (for late initialization)
    pub fn set_payment_state_machine(&mut self, payment_state_machine: Arc<crate::payment::state_machine::PaymentStateMachine>) {
        self.payment_state_machine = Some(payment_state_machine);
    }

    /// Set node storage (for module storage API)
    pub fn set_node_storage(&mut self, storage: Arc<Storage>) {
        self.node_storage = Some(storage);
    }
}

#[async_trait]
impl NodeAPI for NodeApiImpl {
    async fn get_block(&self, hash: &Hash) -> Result<Option<Block>, ModuleError> {
        // Query block store (synchronous operation, but we're in async context)
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage
                    .blocks()
                    .get_block(&hash)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get block: {e}")))
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn get_block_header(&self, hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
        // Query block store for header
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.blocks().get_header(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get block header: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        // Query transaction index
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.transactions().get_transaction(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get transaction: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn has_transaction(&self, hash: &Hash) -> Result<bool, ModuleError> {
        // Check if transaction exists in index
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.transactions().has_transaction(&hash).map_err(|e| {
                    ModuleError::OperationError(format!(
                        "Failed to check transaction existence: {e}"
                    ))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn get_block_height(&self) -> Result<u64, ModuleError> {
        // Get block height from chain state
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .chain()
                    .get_height()
                    .map_err(|e| {
                        ModuleError::OperationError(format!("Failed to get block height: {e}"))
                    })?
                    .ok_or_else(|| {
                        ModuleError::OperationError("Chain not yet initialized".to_string())
                    })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        // Get chain tip from chain state
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .chain()
                    .get_tip_hash()
                    .map_err(|e| {
                        ModuleError::OperationError(format!("Failed to get chain tip: {e}"))
                    })?
                    .ok_or_else(|| {
                        ModuleError::OperationError("Chain not yet initialized".to_string())
                    })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        // Query UTXO store (read-only)
        // Note: This is read-only, modules cannot modify UTXO set
        let outpoint_clone = outpoint.clone();
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .utxos()
                    .get_utxo(&outpoint_clone)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get UTXO: {e}")))
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn subscribe_events(
        &self,
        event_types: Vec<EventType>,
    ) -> Result<mpsc::Receiver<ModuleMessage>, ModuleError> {
        // Create event subscription channel
        let (tx, rx) = mpsc::channel(100);

        // Integrate with event manager if available
        if let (Some(event_manager), Some(module_id)) = (&self.event_manager, &self.module_id) {
            // Register module with event manager
            event_manager
                .subscribe_module(module_id.clone(), event_types, tx)
                .await?;
        } else {
            // Event manager not available - return empty receiver
            // This can happen if NodeAPI is used without event manager setup
            // (e.g., in tests or direct API usage)
            tracing::debug!(
                "Event manager not available for subscribe_events - returning empty receiver"
            );
        }

        Ok(rx)
    }
    
    // === Mempool API Methods ===
    async fn get_mempool_transactions(&self) -> Result<Vec<Hash>, ModuleError> {
        let mempool = self.mempool_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("MempoolManager not available".to_string())
        })?;
        
        // Get all transaction hashes from mempool
        Ok(mempool.transaction_hashes())
    }
    
    async fn get_mempool_transaction(&self, tx_hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        let mempool = self.mempool_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("MempoolManager not available".to_string())
        })?;
        
        Ok(mempool.get_transaction(tx_hash))
    }
    
    async fn get_mempool_size(&self) -> Result<MempoolSize, ModuleError> {
        // Check hooks for cached value first
        if let Some(hook_mgr) = &self.hook_manager {
            let hooks = hook_mgr.read().await;
            if let Some(cached_stats) = hooks.get_mempool_stats_cached().await {
                return Ok(cached_stats);
            }
        }
        
        // Fall back to normal calculation
        let mempool = self.mempool_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("MempoolManager not available".to_string())
        })?;
        
        let transaction_count = mempool.size();
        let transactions = mempool.get_transactions();
        
        // Calculate total size and fees
        let size_bytes: usize = transactions.iter().map(|tx| {
            // Approximate size: serialize to get actual size
            bincode::serialize(tx).map(|bytes| bytes.len()).unwrap_or(0)
        }).sum();
        
        // Calculate total fee from transactions
        let total_fee_sats = tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let transactions_clone = transactions.clone();
            move || {
                let mut total_fee = 0u64;
                for tx in transactions_clone {
                    // Skip coinbase transactions (no fee)
                    if tx.inputs.is_empty() || tx.inputs[0].prevout.txid == [0u8; 32] {
                        continue;
                    }
                    
                    // Calculate fee: sum(inputs) - sum(outputs)
                    let mut input_total = 0u64;
                    for input in &tx.inputs {
                        if let Ok(Some(utxo)) = storage.utxos().get_utxo(&input.prevout) {
                            input_total = input_total.saturating_add(utxo.value as u64);
                        }
                    }
                    
                    let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();
                    let fee = input_total.saturating_sub(output_total);
                    total_fee = total_fee.saturating_add(fee);
                }
                total_fee
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?;
        
        Ok(MempoolSize {
            transaction_count,
            size_bytes,
            total_fee_sats,
        })
    }
    
    // === Network API Methods ===
    async fn get_network_stats(&self) -> Result<NetworkStats, ModuleError> {
        let network = self.network_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("NetworkManager not available".to_string())
        })?;
        
        let peer_count = network.peer_count();
        
        // Get network hash rate from storage (if available)
        let hash_rate = tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                // Try to get network hashrate from chain state cache
                if let Ok(Some(chain_info)) = storage.chain().load_chain_info() {
                    // Calculate approximate hash rate from difficulty
                    // Hash rate = difficulty * 2^32 / 600 (seconds per block)
                    let difficulty = Self::calculate_difficulty_from_bits(chain_info.tip_header.bits);
                    (difficulty * 4294967296.0 / 600.0) as f64
                } else {
                    0.0
                }
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
        .unwrap_or(0.0);
        
        // Network stats don't track bytes sent/received at this level
        // These would need to be tracked by NetworkManager
        Ok(NetworkStats {
            peer_count,
            hash_rate,
            bytes_sent: 0,
            bytes_received: 0,
        })
    }
    
    /// Helper to calculate difficulty from bits
    fn calculate_difficulty_from_bits(bits: u64) -> f64 {
        const MAX_TARGET: u64 = 0x00000000FFFF0000u64;
        let exponent = (bits >> 24) as u8;
        let mantissa = bits & 0x00ffffff;
        
        if mantissa == 0 {
            return 1.0;
        }
        
        let target = if exponent <= 3 {
            mantissa >> (8 * (3 - exponent))
        } else {
            mantissa << (8 * (exponent - 3))
        };
        
        if target == 0 {
            return 1.0;
        }
        
        MAX_TARGET as f64 / target as f64
    }
    
    async fn get_network_peers(&self) -> Result<Vec<PeerInfo>, ModuleError> {
        let network = self.network_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("NetworkManager not available".to_string())
        })?;
        
        let peer_manager_guard = network.peer_manager().await;
        
        let mut peers = Vec::new();
        // Access peers via peer_addresses and get_peer
        for transport_addr in peer_manager_guard.peer_addresses() {
            if let Some(peer) = peer_manager_guard.get_peer(&transport_addr) {
                let addr_str = match transport_addr {
                    TransportAddr::Tcp(addr) => addr.to_string(),
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(addr) => addr.to_string(),
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(node_id) => format!("iroh:{}", hex::encode(node_id)),
                };
                
                let transport_type = match transport_addr {
                    TransportAddr::Tcp(_) => "tcp".to_string(),
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(_) => "quinn".to_string(),
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(_) => "iroh".to_string(),
                };
                
                // Get peer version (stored when version message is received)
                let version = peer.version();
                
                peers.push(PeerInfo {
                    addr: addr_str,
                    transport_type,
                    services: peer.services(),
                    version,
                    connected_since: peer.conntime(),
                });
            }
        }
        
        Ok(peers)
    }
    
    // === Chain API Methods ===
    async fn get_chain_info(&self) -> Result<ChainInfo, ModuleError> {
        // Get chain tip and height
        let tip = self.get_chain_tip().await?;
        let height = self.get_block_height().await?;
        
        // Get difficulty and chain work from storage
        let (difficulty, chain_work, is_synced) = tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let tip_clone = tip;
            move || {
                // Get tip header to calculate difficulty
                let difficulty = if let Ok(Some(tip_header)) = storage.chain().get_tip_header() {
                    Self::calculate_difficulty_from_bits(tip_header.bits) as u32
                } else {
                    0
                };
                
                // Get chain work for tip
                let chain_work = storage.chain()
                    .get_chainwork(&tip_clone)
                    .ok()
                    .flatten()
                    .unwrap_or(0) as u64;
                
                (difficulty, chain_work, true) // Sync status will be checked below
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))??;
        
        // Check sync status from sync coordinator
        let is_synced = if let Some(ref sync_coord) = self.sync_coordinator {
            let sync_guard = sync_coord.lock().await;
            sync_guard.is_synced()
        } else {
            // If sync coordinator not available, assume synced if we have blocks
            height > 0
        };
        
        Ok(ChainInfo {
            tip_hash: tip,
            height,
            difficulty,
            chain_work,
            is_synced,
        })
    }
    
    async fn get_block_by_height(&self, height: u64) -> Result<Option<Block>, ModuleError> {
        // Get block hash by height, then get block
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .blocks()
                    .get_hash_by_height(height)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get hash by height: {e}")))?
                    .and_then(|hash| {
                        storage
                            .blocks()
                            .get_block(&hash)
                            .map_err(|e| ModuleError::OperationError(format!("Failed to get block: {e}")))
                            .transpose()
                    })
                    .transpose()
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }
    
    // === Lightning API Methods ===
    async fn get_lightning_node_url(&self) -> Result<Option<String>, ModuleError> {
        // Query Lightning module storage for node URL
        // Lightning module stores its URL in module storage tree "lightning_config" with key "node_url"
        let lightning_module_id = "bllvm-lightning";
        
        // Check if Lightning module storage tree exists
        let trees = self.module_storage_trees.read().await;
        if let Some(module_trees) = trees.get(lightning_module_id) {
            // Look for lightning_config tree
            let tree_name = format!("module_{}_lightning_config", lightning_module_id);
            if let Some(tree) = module_trees.get(&tree_name) {
                // Get node_url from storage
                if let Ok(Some(url_bytes)) = tree.get(b"node_url") {
                    if let Ok(url) = String::from_utf8(url_bytes) {
                        return Ok(Some(url));
                    }
                }
            }
        }
        
        Ok(None)
    }
    
    async fn get_lightning_info(&self) -> Result<Option<LightningInfo>, ModuleError> {
        // Query Lightning module storage for info
        // Lightning module stores its info in module storage tree "lightning_config"
        let lightning_module_id = "bllvm-lightning";
        
        // Check if Lightning module storage tree exists
        let trees = self.module_storage_trees.read().await;
        if let Some(module_trees) = trees.get(lightning_module_id) {
            // Look for lightning_config tree
            let tree_name = format!("module_{}_lightning_config", lightning_module_id);
            if let Some(tree) = module_trees.get(&tree_name) {
                // Get node_url (required field)
                let node_url = match tree.get(b"node_url") {
                    Ok(Some(bytes)) => {
                        String::from_utf8(bytes).unwrap_or_default()
                    }
                    _ => String::new(),
                };
                
                // Return None if node_url is not available
                if node_url.is_empty() {
                    return Ok(None);
                }
                
                // Get node_pubkey
                let node_pubkey = tree.get(b"node_pubkey")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                
                // Get channel_count
                let channel_count = tree.get(b"channel_count")
                    .ok()
                    .flatten()
                    .and_then(|bytes| {
                        if bytes.len() >= 8 {
                            Some(u64::from_be_bytes([
                                bytes[0], bytes[1], bytes[2], bytes[3],
                                bytes[4], bytes[5], bytes[6], bytes[7],
                            ]) as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                
                // Get total_capacity_sats
                let total_capacity_sats = tree.get(b"total_capacity_sats")
                    .ok()
                    .flatten()
                    .and_then(|bytes| {
                        if bytes.len() >= 8 {
                            Some(u64::from_be_bytes([
                                bytes[0], bytes[1], bytes[2], bytes[3],
                                bytes[4], bytes[5], bytes[6], bytes[7],
                            ]))
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                
                Ok(Some(LightningInfo {
                    node_url,
                    node_pubkey,
                    channel_count,
                    total_capacity_sats,
                }))
            }
        }
        
        Ok(None)
    }
    
    // === Payment API Methods ===
    async fn get_payment_state(&self, payment_id: &str) -> Result<Option<PaymentState>, ModuleError> {
        let payment_state_machine = self.payment_state_machine.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Payment state machine not available".to_string())
        })?;
        
        match payment_state_machine.get_payment_state(payment_id).await {
            Ok(state) => {
                // Convert internal PaymentState to module API PaymentState
                match state {
                    crate::payment::state_machine::PaymentState::RequestCreated { request_id } => {
                        Ok(Some(PaymentState {
                            payment_id: request_id,
                            status: "pending".to_string(),
                            amount_sats: 0, // Amount not available in this state
                            tx_hash: None,
                            confirmations: None,
                        }))
                    }
                    crate::payment::state_machine::PaymentState::ProofCreated { request_id, .. } |
                    crate::payment::state_machine::PaymentState::ProofBroadcast { request_id, .. } => {
                        Ok(Some(PaymentState {
                            payment_id: request_id,
                            status: "pending".to_string(),
                            amount_sats: 0,
                            tx_hash: None,
                            confirmations: None,
                        }))
                    }
                    crate::payment::state_machine::PaymentState::InMempool { request_id, tx_hash } => {
                        Ok(Some(PaymentState {
                            payment_id: request_id,
                            status: "pending".to_string(),
                            amount_sats: 0,
                            tx_hash: Some(tx_hash),
                            confirmations: Some(0),
                        }))
                    }
                    crate::payment::state_machine::PaymentState::Settled { request_id, tx_hash, confirmation_count, .. } => {
                        Ok(Some(PaymentState {
                            payment_id: request_id,
                            status: "confirmed".to_string(),
                            amount_sats: 0,
                            tx_hash: Some(tx_hash),
                            confirmations: Some(confirmation_count),
                        }))
                    }
                    crate::payment::state_machine::PaymentState::Failed { request_id, reason: _ } => {
                        Ok(Some(PaymentState {
                            payment_id: request_id,
                            status: "failed".to_string(),
                            amount_sats: 0,
                            tx_hash: None,
                            confirmations: None,
                        }))
                    }
                }
            }
            Err(_) => Ok(None), // Payment not found
        }
    }
    
    // === Additional Mempool API Methods ===
    async fn check_transaction_in_mempool(&self, tx_hash: &Hash) -> Result<bool, ModuleError> {
        let mempool = self.mempool_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("MempoolManager not available".to_string())
        })?;
        
        Ok(mempool.get_transaction(tx_hash).is_some())
    }
    
    async fn get_fee_estimate(&self, target_blocks: u32) -> Result<u64, ModuleError> {
        // Check hooks for cached value first
        if let Some(hook_mgr) = &self.hook_manager {
            let hooks = hook_mgr.read().await;
            if let Some(cached_estimate) = hooks.get_fee_estimate_cached(target_blocks).await {
                return Ok(cached_estimate);
            }
        }
        
        // Fall back to normal calculation
        let mempool = self.mempool_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("MempoolManager not available".to_string())
        })?;
        
        // Implement fee estimation algorithm
        // Uses a simple approach: calculate fee rate histogram from mempool
        // and return the fee rate needed for target_blocks confirmation
        
        let transactions = mempool.get_transactions();
        if transactions.is_empty() {
            // No transactions in mempool, return minimum fee
            return Ok(1);
        }
        
        // Calculate fee rates for all transactions
        let fee_rates = tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let transactions_clone = transactions.clone();
            move || {
                let mut fee_rates = Vec::new();
                for tx in transactions_clone {
                    // Skip coinbase
                    if tx.inputs.is_empty() {
                        continue;
                    }
                    
                    // Calculate fee
                    let mut input_total = 0u64;
                    for input in &tx.inputs {
                        if let Ok(Some(utxo)) = storage.utxos().get_utxo(&input.prevout) {
                            input_total = input_total.saturating_add(utxo.value as u64);
                        }
                    }
                    let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();
                    let fee = input_total.saturating_sub(output_total);
                    
                    // Estimate transaction size (simplified)
                    let mut size = 8; // version + locktime
                    for input in &tx.inputs {
                        size += 36 + input.script_sig.len() + 4; // prevout + script + sequence
                    }
                    for output in &tx.outputs {
                        size += 8 + output.script_pubkey.len(); // value + script
                    }
                    
                    // Calculate fee rate (sat/vbyte)
                    if size > 0 {
                        let fee_rate = fee / size as u64;
                        fee_rates.push(fee_rate);
                    }
                }
                fee_rates
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))??;
        
        if fee_rates.is_empty() {
            return Ok(1);
        }
        
        // Sort fee rates and find the rate needed for target_blocks confirmation
        // Simple approach: use median fee rate, adjusted for target blocks
        fee_rates.sort();
        let median_idx = fee_rates.len() / 2;
        let median_fee_rate = fee_rates[median_idx];
        
        // Adjust for target blocks (more blocks = lower fee needed)
        // This is a simplified model - real fee estimation uses more sophisticated algorithms
        let adjusted_fee_rate = if target_blocks > 6 {
            median_fee_rate / 2 // Lower fee for longer confirmation time
        } else if target_blocks > 1 {
            median_fee_rate // Standard fee
        } else {
            median_fee_rate * 2 // Higher fee for immediate confirmation
        };
        
        Ok(adjusted_fee_rate.max(1)) // Minimum 1 sat/vbyte
    }
    
    async fn register_rpc_endpoint(
        &self,
        method: String,
        description: String,
    ) -> Result<(), ModuleError> {
        let rpc_server = self.rpc_server.as_ref().ok_or_else(|| {
            ModuleError::OperationError("RPC server not available".to_string())
        })?;
        
        let ipc_server = self.ipc_server.as_ref().ok_or_else(|| {
            ModuleError::OperationError("IPC server not available".to_string())
        })?;
        
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        // Get RPC request channel for this module
        let ipc_server_guard = ipc_server.lock().await;
        let rpc_channel = ipc_server_guard.get_rpc_channel(&module_id).await.ok_or_else(|| {
            ModuleError::OperationError(format!("RPC channel not found for module {}", module_id))
        })?;
        drop(ipc_server_guard);
        
        // Create IPC-based RPC handler
        let handler = Arc::new(crate::module::rpc::ipc_handler::IpcRpcHandler::new(
            module_id.clone(),
            method.clone(),
            rpc_channel,
        ));
        
        // Register with RPC server
        rpc_server.register_module_endpoint(method.clone(), handler).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to register RPC endpoint: {}", e)))?;
        
        Ok(())
    }
    
    async fn unregister_rpc_endpoint(&self, method: &str) -> Result<(), ModuleError> {
        let rpc_server = self.rpc_server.as_ref().ok_or_else(|| {
            ModuleError::OperationError("RPC server not available".to_string())
        })?;
        
        rpc_server.unregister_module_endpoint(method).await
            .map_err(|e| ModuleError::OperationError(e))
    }
    
    async fn register_timer(
        &self,
        interval_seconds: u64,
        callback: Arc<dyn crate::module::timers::manager::TimerCallback>,
    ) -> Result<crate::module::timers::manager::TimerId, ModuleError> {
        let timer_manager = self.timer_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Timer manager not available".to_string())
        })?;
        
        let module_id = self.module_id_for_timers.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set for timer registration".to_string())
        })?;
        
        timer_manager.register_timer(module_id.clone(), interval_seconds, callback).await
            .map_err(|e| ModuleError::OperationError(e))
    }
    
    async fn cancel_timer(
        &self,
        timer_id: crate::module::timers::manager::TimerId,
    ) -> Result<(), ModuleError> {
        let timer_manager = self.timer_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Timer manager not available".to_string())
        })?;
        
        timer_manager.cancel_timer(timer_id).await
            .map_err(|e| ModuleError::OperationError(e))
    }
    
    async fn schedule_task(
        &self,
        delay_seconds: u64,
        callback: Arc<dyn crate::module::timers::manager::TaskCallback>,
    ) -> Result<crate::module::timers::manager::TaskId, ModuleError> {
        let timer_manager = self.timer_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Timer manager not available".to_string())
        })?;
        
        let module_id = self.module_id_for_timers.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set for task scheduling".to_string())
        })?;
        
        timer_manager.schedule_task(module_id.clone(), delay_seconds, callback).await
            .map_err(|e| ModuleError::OperationError(e))
    }
    
    async fn report_metric(&self, metric: Metric) -> Result<(), ModuleError> {
        let metrics_manager = self.metrics_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Metrics manager not available".to_string())
        })?;
        
        let module_id = self.module_id_for_metrics.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set for metrics reporting".to_string())
        })?;
        
        metrics_manager.report_metric(module_id.clone(), metric).await;
        Ok(())
    }
    
    async fn get_module_metrics(&self, module_id: &str) -> Result<Vec<Metric>, ModuleError> {
        let metrics_manager = self.metrics_manager.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Metrics manager not available".to_string())
        })?;
        
        Ok(metrics_manager.get_module_metrics(module_id).await)
    }
    
    async fn get_all_metrics(&self) -> Result<std::collections::HashMap<String, Vec<Metric>>, ModuleError> {
        let metrics_manager = self.metrics_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Metrics manager not available".to_string()))?;
        
        Ok(metrics_manager.get_all_metrics().await)
    }
    
    // === Filesystem API Methods ===
    async fn read_file(&self, path: String) -> Result<Vec<u8>, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let sandbox = {
            let sandboxes = self.module_filesystem_sandboxes.read().await;
            sandboxes.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Filesystem sandbox not initialized for module {}", module_id))
            })?.clone()
        };
        
        let data_dir = {
            let dirs = self.module_data_dirs.read().await;
            dirs.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module data directory not set for module {}", module_id))
            })?.clone()
        };
        
        // Validate and resolve path
        let full_path = if path.starts_with('/') {
            // Absolute path - validate against sandbox
            sandbox.validate_path(&path)?
        } else {
            // Relative path - join with module data directory
            let joined = data_dir.join(&path);
            sandbox.validate_path(&joined)?
        };
        
        tokio::fs::read(&full_path).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to read file: {}", e)))
    }
    
    async fn write_file(&self, path: String, data: Vec<u8>) -> Result<(), ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let sandbox = {
            let sandboxes = self.module_filesystem_sandboxes.read().await;
            sandboxes.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Filesystem sandbox not initialized for module {}", module_id))
            })?.clone()
        };
        
        let data_dir = {
            let dirs = self.module_data_dirs.read().await;
            dirs.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module data directory not set for module {}", module_id))
            })?.clone()
        };
        
        // Validate and resolve path
        let full_path = if path.starts_with('/') {
            sandbox.validate_path(&path)?
        } else {
            let joined = data_dir.join(&path);
            sandbox.validate_path(&joined)?
        };
        
        // Create parent directory if needed
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await
                .map_err(|e| ModuleError::OperationError(format!("Failed to create directory: {}", e)))?;
        }
        
        tokio::fs::write(&full_path, data).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to write file: {}", e)))
    }
    
    async fn delete_file(&self, path: String) -> Result<(), ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let sandbox = {
            let sandboxes = self.module_filesystem_sandboxes.read().await;
            sandboxes.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Filesystem sandbox not initialized for module {}", module_id))
            })?.clone()
        };
        
        let data_dir = {
            let dirs = self.module_data_dirs.read().await;
            dirs.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module data directory not set for module {}", module_id))
            })?.clone()
        };
        
        // Validate and resolve path
        let full_path = if path.starts_with('/') {
            sandbox.validate_path(&path)?
        } else {
            let joined = data_dir.join(&path);
            sandbox.validate_path(&joined)?
        };
        
        tokio::fs::remove_file(&full_path).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to delete file: {}", e)))
    }
    
    async fn list_directory(&self, path: String) -> Result<Vec<String>, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let sandbox = {
            let sandboxes = self.module_filesystem_sandboxes.read().await;
            sandboxes.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Filesystem sandbox not initialized for module {}", module_id))
            })?.clone()
        };
        
        let data_dir = {
            let dirs = self.module_data_dirs.read().await;
            dirs.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module data directory not set for module {}", module_id))
            })?.clone()
        };
        
        // Validate and resolve path
        let full_path = if path.starts_with('/') {
            sandbox.validate_path(&path)?
        } else {
            let joined = data_dir.join(&path);
            sandbox.validate_path(&joined)?
        };
        
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&full_path).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to read directory: {}", e)))?;
        
        while let Some(entry) = dir.next_entry().await
            .map_err(|e| ModuleError::OperationError(format!("Failed to read directory entry: {}", e)))? {
            if let Some(name) = entry.file_name().to_str() {
                entries.push(name.to_string());
            }
        }
        
        Ok(entries)
    }
    
    async fn create_directory(&self, path: String) -> Result<(), ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let sandbox = {
            let sandboxes = self.module_filesystem_sandboxes.read().await;
            sandboxes.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Filesystem sandbox not initialized for module {}", module_id))
            })?.clone()
        };
        
        let data_dir = {
            let dirs = self.module_data_dirs.read().await;
            dirs.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module data directory not set for module {}", module_id))
            })?.clone()
        };
        
        // Validate and resolve path
        let full_path = if path.starts_with('/') {
            sandbox.validate_path(&path)?
        } else {
            let joined = data_dir.join(&path);
            sandbox.validate_path(&joined)?
        };
        
        tokio::fs::create_dir_all(&full_path).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to create directory: {}", e)))
    }
    
    async fn get_file_metadata(&self, path: String) -> Result<crate::module::ipc::protocol::FileMetadata, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let sandbox = {
            let sandboxes = self.module_filesystem_sandboxes.read().await;
            sandboxes.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Filesystem sandbox not initialized for module {}", module_id))
            })?.clone()
        };
        
        let data_dir = {
            let dirs = self.module_data_dirs.read().await;
            dirs.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module data directory not set for module {}", module_id))
            })?.clone()
        };
        
        // Validate and resolve path
        let full_path = if path.starts_with('/') {
            sandbox.validate_path(&path)?
        } else {
            let joined = data_dir.join(&path);
            sandbox.validate_path(&joined)?
        };
        
        let metadata = tokio::fs::metadata(&full_path).await
            .map_err(|e| ModuleError::OperationError(format!("Failed to get file metadata: {}", e)))?;
        
        let modified = metadata.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        
        let created = metadata.created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        
        Ok(crate::module::ipc::protocol::FileMetadata {
            path: full_path.to_string_lossy().to_string(),
            size: metadata.len(),
            is_file: metadata.is_file(),
            is_directory: metadata.is_dir(),
            modified,
            created,
        })
    }
    
    // === Storage API Methods ===
    async fn storage_open_tree(&self, name: String) -> Result<String, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        // Use node's storage to create an isolated tree for this module
        // Tree name format: module_{module_id}_{name}
        let tree_name = format!("module_{}_{}", module_id, name);
        
        // Check if tree already exists
        {
            let trees = self.module_storage_trees.read().await;
            if let Some(module_trees) = trees.get(&module_id) {
                if module_trees.contains_key(&tree_name) {
                    return Ok(tree_name);
                }
            }
        }
        
        // Open the tree in the node's database
        let node_storage = self.node_storage.as_ref().ok_or_else(|| {
            ModuleError::OperationError("Node storage not available".to_string())
        })?;
        
        // Access the database and open the tree
        let tree = tokio::task::spawn_blocking({
            let storage = Arc::clone(node_storage);
            let tree_name_clone = tree_name.clone();
            move || {
                storage.open_tree(&tree_name_clone)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to open storage tree: {}", e)))
            }
        }).await.map_err(|e| ModuleError::OperationError(format!("Failed to spawn blocking task: {}", e)))??;
        
        // Store the tree
        {
            let mut trees = self.module_storage_trees.write().await;
            let module_trees = trees.entry(module_id).or_insert_with(std::collections::HashMap::new);
            module_trees.insert(tree_name.clone(), tree);
        }
        
        Ok(tree_name)
    }
    
    async fn storage_insert(&self, tree_id: String, key: Vec<u8>, value: Vec<u8>) -> Result<(), ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let tree = {
            let trees = self.module_storage_trees.read().await;
            let module_trees = trees.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module storage not initialized for module {}", module_id))
            })?;
            module_trees.get(&tree_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Storage tree not found: {}", tree_id))
            })?.clone()
        };
        
        tree.insert(&key, &value)
            .map_err(|e| ModuleError::OperationError(format!("Failed to insert into storage: {}", e)))
    }
    
    async fn storage_get(&self, tree_id: String, key: Vec<u8>) -> Result<Option<Vec<u8>>, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let tree = {
            let trees = self.module_storage_trees.read().await;
            let module_trees = trees.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module storage not initialized for module {}", module_id))
            })?;
            module_trees.get(&tree_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Storage tree not found: {}", tree_id))
            })?.clone()
        };
        
        tree.get(&key)
            .map_err(|e| ModuleError::OperationError(format!("Failed to get from storage: {}", e)))
    }
    
    async fn storage_remove(&self, tree_id: String, key: Vec<u8>) -> Result<(), ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let tree = {
            let trees = self.module_storage_trees.read().await;
            let module_trees = trees.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module storage not initialized for module {}", module_id))
            })?;
            module_trees.get(&tree_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Storage tree not found: {}", tree_id))
            })?.clone()
        };
        
        tree.remove(&key)
            .map_err(|e| ModuleError::OperationError(format!("Failed to remove from storage: {}", e)))
    }
    
    async fn storage_contains_key(&self, tree_id: String, key: Vec<u8>) -> Result<bool, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let tree = {
            let trees = self.module_storage_trees.read().await;
            let module_trees = trees.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module storage not initialized for module {}", module_id))
            })?;
            module_trees.get(&tree_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Storage tree not found: {}", tree_id))
            })?.clone()
        };
        
        tree.contains_key(&key)
            .map_err(|e| ModuleError::OperationError(format!("Failed to check key in storage: {}", e)))
    }
    
    async fn storage_iter(&self, tree_id: String) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let tree = {
            let trees = self.module_storage_trees.read().await;
            let module_trees = trees.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module storage not initialized for module {}", module_id))
            })?;
            module_trees.get(&tree_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Storage tree not found: {}", tree_id))
            })?.clone()
        };
        
        let mut result = Vec::new();
        for item in tree.iter() {
            match item {
                Ok((key, value)) => result.push((key, value)),
                Err(e) => return Err(ModuleError::OperationError(format!("Failed to iterate storage: {}", e))),
            }
        }
        
        Ok(result)
    }
    
    async fn storage_transaction(&self, tree_id: String, operations: Vec<crate::module::ipc::protocol::StorageOperation>) -> Result<(), ModuleError> {
        let module_id = self.get_module_id().ok_or_else(|| {
            ModuleError::OperationError("Module ID not set".to_string())
        })?;
        
        let tree = {
            let trees = self.module_storage_trees.read().await;
            let module_trees = trees.get(&module_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Module storage not initialized for module {}", module_id))
            })?;
            module_trees.get(&tree_id).ok_or_else(|| {
                ModuleError::OperationError(format!("Storage tree not found: {}", tree_id))
            })?.clone()
        };
        
        // Execute operations sequentially (atomicity depends on database backend)
        for op in operations {
            match op {
                crate::module::ipc::protocol::StorageOperation::Insert { key, value } => {
                    tree.insert(&key, &value)
                        .map_err(|e| ModuleError::OperationError(format!("Failed to insert in transaction: {}", e)))?;
                }
                crate::module::ipc::protocol::StorageOperation::Remove { key } => {
                    tree.remove(&key)
                        .map_err(|e| ModuleError::OperationError(format!("Failed to remove in transaction: {}", e)))?;
                }
            }
        }
        
        Ok(())
    }
    
    async fn initialize_module(
        &self,
        module_id: String,
        module_data_dir: std::path::PathBuf,
        base_data_dir: std::path::PathBuf,
    ) -> Result<(), ModuleError> {
        // Delegate to the public method
        NodeApiImpl::initialize_module(self, module_id, module_data_dir, base_data_dir).await
    }
    
    async fn discover_modules(&self) -> Result<Vec<crate::module::traits::ModuleInfo>, ModuleError> {
        let module_manager = self.module_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module manager not available".to_string()))?;
        
        let manager = module_manager.lock().await;
        let module_info_list = manager.get_all_module_info().await;
        
        let mut result = Vec::new();
        for (module_id, metadata, state) in module_info_list {
            // Extract module name from module_id (format: {module_name}_{uuid})
            let module_name = module_id.split('_').next().unwrap_or(&module_id).to_string();
            
            result.push(crate::module::traits::ModuleInfo {
                module_id: module_id.clone(),
                module_name: metadata.name.clone(),
                version: metadata.version.clone(),
                capabilities: metadata.capabilities.clone(),
                status: state,
                api_version: 1, // Current API version
            });
        }
        
        Ok(result)
    }
    
    async fn get_module_info(&self, module_id: &str) -> Result<Option<crate::module::traits::ModuleInfo>, ModuleError> {
        let module_manager = self.module_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module manager not available".to_string()))?;
        
        let manager = module_manager.lock().await;
        
        // Try to find by full module_id first
        if let Some(metadata) = manager.get_module_metadata(module_id).await {
            let state = manager.get_module_state(module_id).await
                .unwrap_or(crate::module::traits::ModuleState::Stopped);
            
            // Extract module name from module_id
            let module_name = module_id.split('_').next().unwrap_or(module_id).to_string();
            
            return Ok(Some(crate::module::traits::ModuleInfo {
                module_id: module_id.to_string(),
                module_name: metadata.name.clone(),
                version: metadata.version.clone(),
                capabilities: metadata.capabilities.clone(),
                status: state,
                api_version: 1,
            }));
        }
        
        // If not found by full ID, try by module name (first part before _)
        let module_name = module_id.split('_').next().unwrap_or(module_id);
        if let Some(metadata) = manager.get_module_metadata(module_name).await {
            let state = manager.get_module_state(module_name).await
                .unwrap_or(crate::module::traits::ModuleState::Stopped);
            
            // Find the actual module_id (with UUID)
            let modules = manager.list_modules().await;
            let actual_module_id = modules.iter()
                .find(|id| id.starts_with(&format!("{}_", module_name)))
                .cloned()
                .unwrap_or_else(|| module_id.to_string());
            
            return Ok(Some(crate::module::traits::ModuleInfo {
                module_id: actual_module_id,
                module_name: metadata.name.clone(),
                version: metadata.version.clone(),
                capabilities: metadata.capabilities.clone(),
                status: state,
                api_version: 1,
            }));
        }
        
        Ok(None)
    }
    
    async fn is_module_available(&self, module_id: &str) -> Result<bool, ModuleError> {
        let module_manager = self.module_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module manager not available".to_string()))?;
        
        let manager = module_manager.lock().await;
        
        // Check by full module_id
        if manager.get_module_state(module_id).await.is_some() {
            return Ok(true);
        }
        
        // Check by module name
        let module_name = module_id.split('_').next().unwrap_or(module_id);
        Ok(manager.get_module_state(module_name).await.is_some())
    }
    
    async fn publish_event(
        &self,
        event_type: EventType,
        payload: EventPayload,
    ) -> Result<(), ModuleError> {
        let event_manager = self.event_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Event manager not available".to_string()))?;
        
        event_manager.publish_event(event_type, payload).await
    }
    
    async fn call_module(
        &self,
        target_module_id: Option<&str>,
        method: &str,
        params: Vec<u8>,
    ) -> Result<Vec<u8>, ModuleError> {
        let router = self.module_router.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module router not available".to_string()))?;
        
        // Get caller module ID from thread-local storage
        let caller_module_id = NodeApiImpl::get_current_module_id()
            .unwrap_or_else(|| "unknown".to_string());
        
        router.route_call(&caller_module_id, target_module_id, method, &params).await
    }
    
    async fn register_module_api(
        &self,
        api: Arc<dyn crate::module::inter_module::api::ModuleAPI>,
    ) -> Result<(), ModuleError> {
        let registry = self.module_api_registry.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module API registry not available".to_string()))?;
        
        let module_id = self.current_module_id_for_api.as_ref()
            .or_else(|| NodeApiImpl::get_current_module_id())
            .ok_or_else(|| ModuleError::OperationError("Module ID not available for API registration".to_string()))?;
        
        registry.register_api(module_id.clone(), api).await
    }
    
    async fn unregister_module_api(&self) -> Result<(), ModuleError> {
        let registry = self.module_api_registry.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module API registry not available".to_string()))?;
        
        let module_id = self.current_module_id_for_api.as_ref()
            .or_else(|| NodeApiImpl::get_current_module_id())
            .ok_or_else(|| ModuleError::OperationError("Module ID not available for API unregistration".to_string()))?;
        
        registry.unregister_api(module_id).await
    }
    
    async fn send_mesh_packet_to_module(
        &self,
        module_id: &str,
        packet_data: Vec<u8>,
        peer_addr: String,
    ) -> Result<(), ModuleError> {
        // Use call_module to send mesh packet to the mesh module
        // The mesh module should have a "handle_mesh_packet" method registered
        let params = bincode::serialize(&(packet_data, peer_addr))
            .map_err(|e| ModuleError::SerializationError(e.to_string()))?;
        
        self.call_module(Some(module_id), "handle_mesh_packet", params).await?;
        Ok(())
    }
    
    async fn send_mesh_packet_to_peer(
        &self,
        peer_addr: String,
        packet_data: Vec<u8>,
    ) -> Result<(), ModuleError> {
        let network_manager = self.network_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Network manager not available".to_string()))?;
        
        // Parse peer address (can be SocketAddr string or TransportAddr)
        // Try parsing as SocketAddr first
        if let Ok(socket_addr) = peer_addr.parse::<std::net::SocketAddr>() {
            // Send via SocketAddr
            network_manager.send_to_peer(socket_addr, packet_data)
                .await
                .map_err(|e| ModuleError::OperationError(format!("Failed to send mesh packet: {}", e)))?;
        } else {
            // Try parsing as TransportAddr (format: "tcp:127.0.0.1:8333" or "iroh:...")
            use crate::network::transport::TransportAddr;
            let transport_addr = if peer_addr.starts_with("tcp:") {
                let addr_str = &peer_addr[4..];
                addr_str.parse::<std::net::SocketAddr>()
                    .map(TransportAddr::Tcp)
                    .map_err(|e| ModuleError::OperationError(format!("Invalid TCP address: {}", e)))?
            } else if peer_addr.starts_with("quinn:") {
                #[cfg(feature = "quinn")]
                {
                    let addr_str = &peer_addr[6..];
                    addr_str.parse::<std::net::SocketAddr>()
                        .map(TransportAddr::Quinn)
                        .map_err(|e| ModuleError::OperationError(format!("Invalid Quinn address: {}", e)))?
                }
                #[cfg(not(feature = "quinn"))]
                return Err(ModuleError::OperationError("Quinn transport not enabled".to_string()));
            } else if peer_addr.starts_with("iroh:") {
                #[cfg(feature = "iroh")]
                {
                    use iroh::net::NodeId;
                    let node_id_str = &peer_addr[5..];
                    let node_id_bytes = hex::decode(node_id_str)
                        .map_err(|e| ModuleError::OperationError(format!("Invalid Iroh node ID hex: {}", e)))?;
                    if node_id_bytes.len() != 32 {
                        return Err(ModuleError::OperationError("Iroh node ID must be 32 bytes".to_string()));
                    }
                    let mut node_id = [0u8; 32];
                    node_id.copy_from_slice(&node_id_bytes);
                    TransportAddr::Iroh(node_id)
                }
                #[cfg(not(feature = "iroh"))]
                return Err(ModuleError::OperationError("Iroh transport not enabled".to_string()));
            } else {
                return Err(ModuleError::OperationError(format!("Invalid peer address format: {}", peer_addr)));
            };
            
            // Send via TransportAddr
            network_manager.send_to_peer_by_transport(transport_addr, packet_data)
                .await
                .map_err(|e| ModuleError::OperationError(format!("Failed to send mesh packet: {}", e)))?;
        }
        
        Ok(())
    }
    
    async fn send_stratum_v2_message_to_peer(
        &self,
        peer_addr: String,
        message_data: Vec<u8>,
    ) -> Result<(), ModuleError> {
        let network_manager = self.network_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Network manager not available".to_string()))?;
        
        // Parse peer address (can be SocketAddr string or TransportAddr)
        // Try parsing as SocketAddr first
        if let Ok(socket_addr) = peer_addr.parse::<std::net::SocketAddr>() {
            // Send via SocketAddr
            network_manager.send_to_peer(socket_addr, message_data)
                .await
                .map_err(|e| ModuleError::OperationError(format!("Failed to send Stratum V2 message: {}", e)))?;
        } else {
            // Try parsing as TransportAddr (format: "tcp:127.0.0.1:8333" or "iroh:...")
            use crate::network::transport::TransportAddr;
            let transport_addr = if peer_addr.starts_with("tcp:") {
                let addr_str = &peer_addr[4..];
                addr_str.parse::<std::net::SocketAddr>()
                    .map(TransportAddr::Tcp)
                    .map_err(|e| ModuleError::OperationError(format!("Invalid TCP address: {}", e)))?
            } else if peer_addr.starts_with("quinn:") {
                #[cfg(feature = "quinn")]
                {
                    let addr_str = &peer_addr[6..];
                    addr_str.parse::<std::net::SocketAddr>()
                        .map(TransportAddr::Quinn)
                        .map_err(|e| ModuleError::OperationError(format!("Invalid Quinn address: {}", e)))?
                }
                #[cfg(not(feature = "quinn"))]
                return Err(ModuleError::OperationError("Quinn transport not enabled".to_string()));
            } else if peer_addr.starts_with("iroh:") {
                #[cfg(feature = "iroh")]
                {
                    use iroh::net::NodeId;
                    let node_id_str = &peer_addr[5..];
                    let node_id_bytes = hex::decode(node_id_str)
                        .map_err(|e| ModuleError::OperationError(format!("Invalid Iroh node ID hex: {}", e)))?;
                    if node_id_bytes.len() != 32 {
                        return Err(ModuleError::OperationError("Iroh node ID must be 32 bytes".to_string()));
                    }
                    let mut node_id = [0u8; 32];
                    node_id.copy_from_slice(&node_id_bytes);
                    TransportAddr::Iroh(node_id)
                }
                #[cfg(not(feature = "iroh"))]
                return Err(ModuleError::OperationError("Iroh transport not enabled".to_string()));
            } else {
                return Err(ModuleError::OperationError(format!("Invalid peer address format: {}", peer_addr)));
            };
            
            // Send via TransportAddr
            network_manager.send_to_peer_by_transport(transport_addr, message_data)
                .await
                .map_err(|e| ModuleError::OperationError(format!("Failed to send Stratum V2 message: {}", e)))?;
        }
        
        Ok(())
    }
    
    async fn get_module_health(&self, module_id: &str) -> Result<Option<crate::module::process::monitor::ModuleHealth>, ModuleError> {
        let module_manager = self.module_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module manager not available".to_string()))?;
        
        let manager = module_manager.lock().await;
        // Extract module name from module_id (format: {module_name}_{uuid})
        let module_name = module_id.split('_').next().unwrap_or(module_id);
        
        // Get module state
        let state = manager.get_module_state(module_name).await;
        
        // Convert ModuleState to ModuleHealth
        match state {
            Some(ModuleState::Running) => Ok(Some(crate::module::process::monitor::ModuleHealth::Healthy)),
            Some(ModuleState::Initialized) => Ok(Some(crate::module::process::monitor::ModuleHealth::Healthy)),
            Some(ModuleState::Stopped) => Ok(Some(crate::module::process::monitor::ModuleHealth::Unresponsive)),
            Some(ModuleState::Error(err)) => Ok(Some(crate::module::process::monitor::ModuleHealth::Crashed(err))),
            None => Ok(None),
        }
    }
    
    async fn get_all_module_health(&self) -> Result<Vec<(String, crate::module::process::monitor::ModuleHealth)>, ModuleError> {
        let module_manager = self.module_manager.as_ref()
            .ok_or_else(|| ModuleError::OperationError("Module manager not available".to_string()))?;
        
        let manager = module_manager.lock().await;
        let modules = manager.list_modules().await;
        
        let mut result = Vec::new();
        for module_id in modules {
            let module_name = module_id.split('_').next().unwrap_or(&module_id);
            if let Some(state) = manager.get_module_state(module_name).await {
                let health = match state {
                    ModuleState::Running => crate::module::process::monitor::ModuleHealth::Healthy,
                    ModuleState::Initialized => crate::module::process::monitor::ModuleHealth::Healthy,
                    ModuleState::Stopped => crate::module::process::monitor::ModuleHealth::Unresponsive,
                    ModuleState::Error(err) => crate::module::process::monitor::ModuleHealth::Crashed(err),
                };
                result.push((module_id, health));
            }
        }
        
        Ok(result)
    }
    
    async fn report_module_health(
        &self,
        health: crate::module::process::monitor::ModuleHealth,
    ) -> Result<(), ModuleError> {
        // Get current module ID
        let module_id = NodeApiImpl::get_current_module_id()
            .ok_or_else(|| ModuleError::OperationError("Module ID not available for health reporting".to_string()))?;
        
        // Health reporting is handled by ModuleProcessMonitor automatically
        // This method allows modules to self-report additional health information
        debug!("Module {} reported health: {:?}", module_id, health);
        Ok(())
    }
}

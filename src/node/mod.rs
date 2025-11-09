//! Node orchestration for reference-node
//!
//! This module provides sync coordination, mempool management,
//! mining coordination, and overall node state management.

pub mod block_processor;
pub mod event_publisher;
pub mod mempool;
pub mod miner;
pub mod sync;
pub mod metrics;
pub mod health;
pub mod performance;

use anyhow::Result;
use std::net::SocketAddr;
use tracing::{debug, info, warn};

use crate::config::NodeConfig;
use crate::module::api::NodeApiImpl;
use crate::module::ModuleManager;
use crate::network::NetworkManager;
use crate::node::event_publisher::EventPublisher;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::PerformanceProfiler;
use crate::rpc::RpcManager;
use crate::storage::Storage;
use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Main node orchestrator
pub struct Node {
    protocol: Arc<BitcoinProtocolEngine>,
    storage: Storage,
    network: NetworkManager,
    rpc: RpcManager,
    #[allow(dead_code)]
    sync_coordinator: sync::SyncCoordinator,
    mempool_manager: Arc<mempool::MempoolManager>,
    #[allow(dead_code)]
    mining_coordinator: miner::MiningCoordinator,
    /// Module manager for process-isolated modules
    #[allow(dead_code)]
    module_manager: Option<ModuleManager>,
    /// Event publisher for module notifications
    #[allow(dead_code)]
    event_publisher: Option<EventPublisher>,
    /// Metrics collector for monitoring
    metrics: Arc<MetricsCollector>,
    /// Performance profiler for critical path timing
    profiler: Arc<PerformanceProfiler>,
}

impl Node {
    /// Create a new node
    pub fn new(
        data_dir: &str,
        network_addr: SocketAddr,
        rpc_addr: SocketAddr,
        protocol_version: Option<ProtocolVersion>,
    ) -> Result<Self> {
        info!("Initializing reference-node");

        // Initialize components
        let protocol =
            BitcoinProtocolEngine::new(protocol_version.unwrap_or(ProtocolVersion::Regtest))?;
        let protocol_arc = Arc::new(protocol);
        let storage = Storage::new(data_dir)?;
        let storage_arc = Arc::new(storage);
        let mempool_manager_arc = Arc::new(mempool::MempoolManager::new());
        let network = NetworkManager::new(network_addr)
            .with_dependencies(Arc::clone(&protocol_arc), Arc::clone(&storage_arc), Arc::clone(&mempool_manager_arc));
        let network_arc = Arc::new(network);
        let metrics_arc = Arc::new(MetricsCollector::new());
        let profiler_arc = Arc::new(PerformanceProfiler::new(1000));
        let rpc = RpcManager::new(rpc_addr)
            .with_metrics(Arc::clone(&metrics_arc))
            .with_profiler(Arc::clone(&profiler_arc))
            .with_dependencies(Arc::clone(&storage_arc), Arc::clone(&mempool_manager_arc))
            .with_network_manager(Arc::clone(&network_arc));
        let sync_coordinator = sync::SyncCoordinator::default();
        let mining_coordinator = miner::MiningCoordinator::new(
            Arc::clone(&mempool_manager_arc),
            Some(Arc::clone(&storage_arc)),
        );
        let metrics = metrics_arc;
        let profiler = profiler_arc;

        Ok(Self {
            protocol: protocol_arc,
            storage: Arc::try_unwrap(storage_arc)
                .unwrap_or_else(|_| Storage::new(data_dir).unwrap()),
            network: Arc::try_unwrap(network_arc)
                .unwrap_or_else(|_| NetworkManager::new(network_addr)),
            rpc,
            sync_coordinator,
            mempool_manager: mempool_manager_arc,
            mining_coordinator,
            module_manager: None,
            event_publisher: None,
            metrics,
            profiler,
        })
    }

    /// Enable module system from configuration
    pub fn with_modules_from_config(mut self, config: &NodeConfig) -> anyhow::Result<Self> {
        if let Some(module_config) = &config.modules {
            if !module_config.enabled {
                info!("Module system disabled in configuration");
                return Ok(self);
            }

            let module_manager = ModuleManager::new(
                &module_config.modules_dir,
                &module_config.data_dir,
                &module_config.socket_dir,
            );
            self.module_manager = Some(module_manager);
            info!(
                "Module system enabled: modules_dir={}, data_dir={}, socket_dir={}",
                module_config.modules_dir, module_config.data_dir, module_config.socket_dir
            );
        }
        Ok(self)
    }

    /// Enable module system with explicit paths (for backward compatibility)
    pub fn with_modules<P: AsRef<Path>>(
        mut self,
        modules_dir: P,
        socket_dir: P,
    ) -> anyhow::Result<Self> {
        let data_dir =
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string()));
        let modules_data_dir = data_dir.join("modules");

        let module_manager = ModuleManager::new(
            modules_dir.as_ref(),
            modules_data_dir.as_ref(),
            socket_dir.as_ref(),
        );
        self.module_manager = Some(module_manager);
        Ok(self)
    }

    /// Start the node
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting reference-node");

        // Start all components
        self.start_components().await?;

        // Main node loop
        self.run().await?;

        Ok(())
    }

    /// Start all node components
    async fn start_components(&mut self) -> Result<()> {
        info!("Starting node components");

        // Simplified component startup
        // In a real implementation, each component would be started in separate tasks
        // For now, we'll just initialize them

        info!("RPC server initialized");
        info!("Network manager initialized");
        info!("Sync coordinator initialized");
        info!("Mempool manager initialized");
        info!("Mining coordinator initialized");
        
        // Note: NetworkManager::start() and initialize_peer_connections() should be called
        // separately with appropriate configuration. This allows the caller to:
        // 1. Start the network manager with listen address
        // 2. Initialize peer connections with config, network type, port, and target peer count

        // Prune on startup if configured
        if let Some(pruning_manager) = self.storage.pruning() {
            let config = &pruning_manager.config;
            if config.prune_on_startup {
                let current_height = self.storage.chain().get_height()?.unwrap_or(0);
                let is_ibd = current_height == 0;
                
                if !is_ibd && pruning_manager.is_enabled() {
                    info!("Prune on startup enabled, checking if pruning is needed...");
                    
                    // Calculate prune height based on configuration
                    let prune_height = match &config.mode {
                        crate::config::PruningMode::Disabled => {
                            // Skip if disabled
                            None
                        }
                        crate::config::PruningMode::Normal { keep_from_height, .. } => {
                            // Prune up to keep_from_height
                            Some(*keep_from_height)
                        }
                        #[cfg(feature = "utxo-commitments")]
                        crate::config::PruningMode::Aggressive { keep_from_height, .. } => {
                            // Prune up to keep_from_height
                            Some(*keep_from_height)
                        }
                        #[cfg(not(feature = "utxo-commitments"))]
                        crate::config::PruningMode::Aggressive { .. } => {
                            // Aggressive pruning requires utxo-commitments feature
                            // Fall back to no pruning if feature is disabled
                            None
                        }
                        crate::config::PruningMode::Custom { keep_bodies_from_height, .. } => {
                            // Prune up to keep_bodies_from_height
                            Some(*keep_bodies_from_height)
                        }
                    };
                    
                    if let Some(prune_to_height) = prune_height {
                        if prune_to_height < current_height {
                            match pruning_manager.prune_to_height(prune_to_height, current_height, is_ibd) {
                                Ok(stats) => {
                                    info!("Startup pruning completed: {} blocks pruned, {} blocks kept", 
                                          stats.blocks_pruned, stats.blocks_kept);
                                    // Flush storage to persist pruning changes
                                    if let Err(e) = self.storage.flush() {
                                        warn!("Failed to flush storage after startup pruning: {}", e);
                                    }
                                }
                                Err(e) => {
                                    warn!("Startup pruning failed: {}", e);
                                }
                            }
                        }
                    }
                } else if is_ibd {
                    info!("Skipping startup pruning: initial block download in progress");
                }
            }
        }

        // Start module manager if enabled
        if let Some(ref mut module_manager) = self.module_manager {
            // Create new Storage instance for modules (Storage doesn't implement Clone)
            // Both instances use the same data directory, so they access the same data
            let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string());
            let storage_arc = Arc::new(
                Storage::new(&data_dir)
                    .map_err(|e| anyhow::anyhow!("Failed to create storage for modules: {}", e))?,
            );
            let node_api = Arc::new(NodeApiImpl::new(storage_arc));
            let socket_path = std::env::var("MODULE_SOCKET_DIR")
                .unwrap_or_else(|_| "data/modules/socket".to_string());

            module_manager
                .start(&socket_path, node_api)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start module manager: {}", e))?;

            info!("Module manager started");

            // Auto-discover and load modules
            if let Err(e) = module_manager.auto_load_modules().await {
                warn!("Failed to auto-load modules: {}", e);
            }

            // Create event publisher for this node
            let event_manager = module_manager.event_manager();
            self.event_publisher = Some(EventPublisher::new(Arc::clone(event_manager)));
            info!("Event publisher initialized");
        }

        Ok(())
    }

    /// Main node run loop
    async fn run(&mut self) -> Result<()> {
        info!("Node running - main loop started");

        // Get initial state for block processing
        let mut current_height = self.storage.chain().get_height()?.unwrap_or(0);
        let mut utxo_set = bllvm_protocol::UtxoSet::new();

        // Main node loop - in a real implementation this would coordinate
        // between all components and handle shutdown signals
        loop {
            // Process any received blocks (non-blocking)
            while let Some(block_data) = self.network.try_recv_block() {
                info!("Processing block from network");
                let blocks_arc = self.storage.blocks();
                match self.sync_coordinator.process_block(
                    &*blocks_arc,
                    &block_data,
                    current_height,
                    &mut utxo_set,
                    Some(Arc::clone(&self.metrics)),
                    Some(Arc::clone(&self.profiler)),
                ) {
                    Ok(true) => {
                        info!("Block accepted at height {}", current_height);
                        
                        // Persist UTXO set to storage after block validation
                        // This is critical for commitment generation and incremental pruning
                        if let Err(e) = self.storage.utxos().store_utxo_set(&utxo_set) {
                            warn!("Failed to persist UTXO set after block {}: {}", current_height, e);
                        }
                        
                        // Generate UTXO commitment from current state (if enabled)
                        // Use current_height (the block that was just validated) before incrementing
                        #[cfg(feature = "utxo-commitments")]
                        {
                            if let Some(pruning_manager) = self.storage.pruning() {
                                if let (Some(commitment_store), Some(_utxostore)) = 
                                    (pruning_manager.commitment_store(), pruning_manager.utxostore()) 
                                {
                                    // Get block hash from storage (block was just stored at current_height)
                                    let blocks_arc = self.storage.blocks();
                                    if let Ok(Some(block_hash)) = blocks_arc.get_hash_by_height(current_height) {
                                        // Generate commitment from current UTXO set state
                                        if let Err(e) = pruning_manager.generate_commitment_from_current_state(
                                            &block_hash,
                                            current_height,
                                            &utxo_set,
                                            &commitment_store,
                                        ) {
                                            warn!("Failed to generate commitment for block {}: {}", current_height, e);
                                        } else {
                                            debug!("Generated UTXO commitment for block {}", current_height);
                                        }
                                    } else {
                                        warn!("Could not find block hash for height {} to generate commitment", current_height);
                                    }
                                }
                            }
                        }
                        
                        // Increment height after processing
                        current_height += 1;
                        
                        // Check for incremental pruning during IBD
                        // Consider IBD if we're still syncing (height < tip or no recent blocks)
                        let is_ibd = current_height < 1000; // Simple heuristic: consider IBD if < 1000 blocks
                        if let Some(pruning_manager) = self.storage.pruning() {
                            if let Ok(Some(prune_stats)) = pruning_manager.incremental_prune_during_ibd(current_height, is_ibd) {
                                info!("Incremental pruning during IBD: {} blocks pruned, {} bytes freed", 
                                      prune_stats.blocks_pruned, prune_stats.storage_freed);
                                // Flush storage to persist pruning changes
                                if let Err(e) = self.storage.flush() {
                                    warn!("Failed to flush storage after incremental pruning: {}", e);
                                }
                            }
                        }
                        
                        // Check for automatic pruning after block acceptance
                        if let Some(pruning_manager) = self.storage.pruning() {
                            let stats = pruning_manager.get_stats();
                            let should_prune = pruning_manager.should_auto_prune(
                                current_height,
                                stats.last_prune_height,
                            );
                            
                            if should_prune {
                                info!("Automatic pruning triggered at height {}", current_height);
                                
                                // Calculate prune height based on configuration
                                let prune_height = match &pruning_manager.config.mode {
                                    crate::config::PruningMode::Disabled => None,
                                    crate::config::PruningMode::Normal { keep_from_height, .. } => {
                                        // Prune to keep_from_height, but ensure we keep min_blocks
                                        let min_keep = pruning_manager.config.min_blocks_to_keep;
                                        let effective_keep = (*keep_from_height).max(current_height.saturating_sub(min_keep));
                                        Some(effective_keep)
                                    }
                                    #[cfg(feature = "utxo-commitments")]
                                    crate::config::PruningMode::Aggressive { keep_from_height, min_blocks, .. } => {
                                        // Prune to keep_from_height, respecting min_blocks
                                        let effective_keep = (*keep_from_height).max(current_height.saturating_sub(*min_blocks));
                                        Some(effective_keep)
                                    }
                                    #[cfg(not(feature = "utxo-commitments"))]
                                    crate::config::PruningMode::Aggressive { .. } => {
                                        // Aggressive pruning requires utxo-commitments feature
                                        // Fall back to no pruning if feature is disabled
                                        None
                                    }
                                    crate::config::PruningMode::Custom { keep_bodies_from_height, .. } => {
                                        // Prune to keep_bodies_from_height, respecting min_blocks
                                        let min_keep = pruning_manager.config.min_blocks_to_keep;
                                        let effective_keep = (*keep_bodies_from_height).max(current_height.saturating_sub(min_keep));
                                        Some(effective_keep)
                                    }
                                };
                                
                                if let Some(prune_to_height) = prune_height {
                                    if prune_to_height < current_height {
                                        match pruning_manager.prune_to_height(prune_to_height, current_height, false) {
                                            Ok(prune_stats) => {
                                                info!("Automatic pruning completed: {} blocks pruned, {} blocks kept", 
                                                      prune_stats.blocks_pruned, prune_stats.blocks_kept);
                                                // Flush storage to persist pruning changes
                                                if let Err(e) = self.storage.flush() {
                                                    warn!("Failed to flush storage after automatic pruning: {}", e);
                                                }
                                            }
                                            Err(e) => {
                                                warn!("Automatic pruning failed: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(false) => {
                        warn!("Block rejected at height {}", current_height);
                    }
                    Err(e) => {
                        warn!("Error processing block: {}", e);
                    }
                }
            }

            // Process other network messages (non-blocking, processes one message if available)
            // Note: This is a simplified approach - in production, network processing
            // would run in a separate task
            if let Err(e) = self.network.process_messages().await {
                warn!("Error processing network messages: {}", e);
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            // Check node health periodically
            self.check_health().await?;
        }
    }

    /// Run node processing once (for testing)
    pub async fn run_once(&mut self) -> Result<()> {
        info!("Running node processing once");

        // Check node health
        self.check_health().await?;

        Ok(())
    }

    /// Check node health
    async fn check_health(&self) -> Result<()> {
        // Simplified health check
        let peer_count = self.network.peer_count();
        let storage_blocks = self.storage.blocks().block_count()?;

        if peer_count == 0 {
            warn!("No peers connected");
        }

        if storage_blocks == 0 {
            warn!("No blocks in storage");
        }

        Ok(())
    }

    /// Stop the node
    pub async fn stop(&mut self) -> Result<()> {
        info!("Stopping reference-node");

        // Stop module manager
        if let Some(ref mut module_manager) = self.module_manager {
            module_manager
                .shutdown()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to shutdown module manager: {}", e))?;
        }

        // Stop all components
        self.rpc.stop()?;

        // Flush storage
        self.storage.flush()?;

        info!("Node stopped");
        Ok(())
    }

    /// Get module manager (mutable)
    pub fn module_manager_mut(&mut self) -> Option<&mut ModuleManager> {
        self.module_manager.as_mut()
    }

    /// Get module manager (immutable)
    pub fn module_manager(&self) -> Option<&ModuleManager> {
        self.module_manager.as_ref()
    }

    /// Get event publisher (immutable)
    pub fn event_publisher(&self) -> Option<&EventPublisher> {
        self.event_publisher.as_ref()
    }

    /// Get event publisher (mutable)
    pub fn event_publisher_mut(&mut self) -> Option<&mut EventPublisher> {
        self.event_publisher.as_mut()
    }

    /// Get protocol engine
    pub fn protocol(&self) -> &BitcoinProtocolEngine {
        &*self.protocol
    }

    /// Get storage
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Get network manager
    pub fn network(&self) -> &NetworkManager {
        &self.network
    }

    /// Get RPC manager
    pub fn rpc(&self) -> &RpcManager {
        &self.rpc
    }

    /// Get health report
    pub fn health_check(&self) -> health::HealthReport {
        use crate::node::health::HealthChecker;
        
        let checker = HealthChecker::new();
        let network_healthy = self.network.is_network_active();
        let storage_healthy = self.storage.check_storage_bounds().unwrap_or(false);
        let rpc_healthy = true; // RPC is always healthy if node is running
        
        // Get metrics if available (simplified for now)
        checker.check_health(
            network_healthy,
            storage_healthy,
            rpc_healthy,
            None, // Network metrics - would need to be passed from NetworkManager
            None, // Storage metrics - would need to be collected
        )
    }
}

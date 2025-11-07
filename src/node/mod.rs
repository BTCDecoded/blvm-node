//! Node orchestration for reference-node
//!
//! This module provides sync coordination, mempool management,
//! mining coordination, and overall node state management.

pub mod block_processor;
pub mod event_publisher;
pub mod mempool;
pub mod miner;
pub mod sync;

use anyhow::Result;
use std::net::SocketAddr;
use tracing::{info, warn};

use crate::config::NodeConfig;
use crate::module::api::NodeApiImpl;
use crate::module::ModuleManager;
use crate::network::NetworkManager;
use crate::node::event_publisher::EventPublisher;
use crate::rpc::RpcManager;
use crate::storage::Storage;
use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Main node orchestrator
pub struct Node {
    protocol: BitcoinProtocolEngine,
    storage: Storage,
    network: NetworkManager,
    rpc: RpcManager,
    #[allow(dead_code)]
    sync_coordinator: sync::SyncCoordinator,
    mempool_manager: mempool::MempoolManager,
    #[allow(dead_code)]
    mining_coordinator: miner::MiningCoordinator,
    /// Module manager for process-isolated modules
    #[allow(dead_code)]
    module_manager: Option<ModuleManager>,
    /// Event publisher for module notifications
    #[allow(dead_code)]
    event_publisher: Option<EventPublisher>,
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
        let storage = Storage::new(data_dir)?;
        let storage_arc = Arc::new(storage);
        let network = NetworkManager::new(network_addr);
        let mempool_manager_arc = Arc::new(mempool::MempoolManager::new());
        let rpc = RpcManager::new(rpc_addr)
            .with_dependencies(Arc::clone(&storage_arc), Arc::clone(&mempool_manager_arc));
        let sync_coordinator = sync::SyncCoordinator::default();
        let mempool_manager =
            Arc::try_unwrap(mempool_manager_arc).unwrap_or_else(|_| mempool::MempoolManager::new());
        let mining_coordinator = miner::MiningCoordinator::default();

        Ok(Self {
            protocol,
            storage: Arc::try_unwrap(storage_arc)
                .unwrap_or_else(|_| Storage::new(data_dir).unwrap()),
            network,
            rpc,
            sync_coordinator,
            mempool_manager,
            mining_coordinator,
            module_manager: None,
            event_publisher: None,
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
                match self.sync_coordinator.process_block(
                    self.storage.blocks(),
                    &block_data,
                    current_height,
                    &mut utxo_set,
                ) {
                    Ok(true) => {
                        info!("Block accepted at height {}", current_height);
                        current_height += 1;
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
        &self.protocol
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
}

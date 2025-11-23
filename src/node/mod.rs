//! Node orchestration for reference-node
//!
//! This module provides sync coordination, mempool management,
//! mining coordination, and overall node state management.

pub mod block_processor;
pub mod event_publisher;
pub mod health;
pub mod mempool;
#[cfg(kani)]
pub mod mempool_proofs;
pub mod metrics;
pub mod miner;
pub mod performance;
pub mod sync;

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
    storage: Arc<Storage>,
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
    /// Protocol version (for determining network type)
    protocol_version: ProtocolVersion,
    /// Network address (for determining port)
    network_addr: SocketAddr,
    /// Node configuration (optional)
    config: Option<NodeConfig>,
    /// Data directory path (stored for recreating storage if needed)
    data_dir: PathBuf,
    /// Disk check counter (for periodic monitoring)
    disk_check_counter: std::sync::atomic::AtomicU64,
    /// Governance webhook client (for fee forwarding integration)
    #[cfg(feature = "governance")]
    governance_webhook: Option<crate::governance::GovernanceWebhookClient>,
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
        let protocol_version = protocol_version.unwrap_or(ProtocolVersion::Regtest);
        let protocol = BitcoinProtocolEngine::new(protocol_version)?;
        let protocol_arc = Arc::new(protocol);
        let storage = Storage::new(data_dir)?;
        let storage_arc = Arc::new(storage);
        let mempool_manager_arc = Arc::new(mempool::MempoolManager::new());

        // Create network manager (config will be applied later if available)
        let network = NetworkManager::new(network_addr).with_dependencies(
            Arc::clone(&protocol_arc),
            Arc::clone(&storage_arc),
            Arc::clone(&mempool_manager_arc),
        );
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
            storage: storage_arc,
            network: Arc::try_unwrap(network_arc)
                .unwrap_or_else(|_| NetworkManager::new(network_addr)),
            rpc,
            data_dir: PathBuf::from(data_dir),
            sync_coordinator,
            mempool_manager: mempool_manager_arc,
            mining_coordinator,
            module_manager: None,
            event_publisher: None,
            metrics,
            profiler,
            protocol_version,
            network_addr,
            config: None,
            disk_check_counter: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "governance")]
            governance_webhook: None,
        })
    }

    /// Set node configuration
    pub fn with_config(mut self, config: NodeConfig) -> Result<Self> {
        // Apply network configuration if available
        let max_peers = config.max_peers.unwrap_or(100);
        let transport_preference = config.get_transport_preference();

        // Recreate network manager with config
        let network_addr = self.network_addr;
        let protocol_arc = self.protocol.clone();
        // Use existing storage Arc instead of creating a new one
        let storage_arc = Arc::clone(&self.storage);
        let mempool_manager_arc = Arc::clone(&self.mempool_manager);

        let network = NetworkManager::with_config(
            network_addr,
            max_peers,
            transport_preference,
            Some(&config),
        )
        .with_dependencies(protocol_arc, storage_arc, mempool_manager_arc);

        // Initialize governance webhook client if configured (from environment variables)
        #[cfg(feature = "governance")]
        let governance_webhook = std::env::var("GOVERNANCE_WEBHOOK_URL").ok().map(|url| {
            crate::governance::GovernanceWebhookClient::new(
                Some(url),
                std::env::var("GOVERNANCE_NODE_ID").ok(),
            )
        });

        self.network = network;
        self.config = Some(config);
        #[cfg(feature = "governance")]
        {
            self.governance_webhook = governance_webhook;
        }
        Ok(self)
    }

    /// Enable module system from configuration
    pub fn with_modules_from_config(mut self, config: &NodeConfig) -> anyhow::Result<Self> {
        if let Some(module_config) = &config.modules {
            if !module_config.enabled {
                info!("Module system disabled in configuration");
                return Ok(self);
            }

            // Get module resource limits config
            let module_resource_limits = self
                .config
                .as_ref()
                .and_then(|c| c.module_resource_limits.as_ref());
            let module_manager = ModuleManager::with_config(
                &module_config.modules_dir,
                &module_config.data_dir,
                &module_config.socket_dir,
                module_resource_limits,
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
        use crate::utils::env_or_default;
        let data_dir = PathBuf::from(env_or_default("DATA_DIR", "data"));
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

        // Start network manager
        if let Err(e) = self.network.start(self.network_addr).await {
            warn!("Failed to start network manager: {}", e);
            // Continue anyway - network might be optional
        }

        // Initialize peer connections automatically
        self.initialize_peer_connections().await?;

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
                        crate::config::PruningMode::Normal {
                            keep_from_height, ..
                        } => {
                            // Prune up to keep_from_height
                            Some(*keep_from_height)
                        }
                        #[cfg(feature = "utxo-commitments")]
                        crate::config::PruningMode::Aggressive {
                            keep_from_height, ..
                        } => {
                            // Prune up to keep_from_height
                            Some(*keep_from_height)
                        }
                        #[cfg(not(feature = "utxo-commitments"))]
                        crate::config::PruningMode::Aggressive { .. } => {
                            // Aggressive pruning requires utxo-commitments feature
                            // Fall back to no pruning if feature is disabled
                            None
                        }
                        crate::config::PruningMode::Custom {
                            keep_bodies_from_height,
                            ..
                        } => {
                            // Prune up to keep_bodies_from_height
                            Some(*keep_bodies_from_height)
                        }
                    };

                    if let Some(prune_to_height) = prune_height {
                        if prune_to_height < current_height {
                            match pruning_manager.prune_to_height(
                                prune_to_height,
                                current_height,
                                is_ibd,
                            ) {
                                Ok(stats) => {
                                    info!("Startup pruning completed: {} blocks pruned, {} blocks kept", 
                                          stats.blocks_pruned, stats.blocks_kept);
                                    // Flush storage to persist pruning changes
                                    use crate::utils::log_error;
                                    log_error(
                                        || self.storage.flush(),
                                        "Failed to flush storage after startup pruning",
                                    );
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
            use crate::utils::env_or_default;
            let data_dir = env_or_default("DATA_DIR", "data");
            let storage_arc = Arc::new(
                Storage::new(&data_dir)
                    .map_err(|e| anyhow::anyhow!("Failed to create storage for modules: {}", e))?,
            );
            let node_api = Arc::new(NodeApiImpl::new(storage_arc));
            let socket_path = env_or_default("MODULE_SOCKET_DIR", "data/modules/socket");

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

            // Initialize ZMQ publisher if configured
            // Note: ZMQ is enabled by default, but only initializes if endpoints are configured.
            // To disable: either don't configure endpoints, or build without --features zmq
            #[cfg(feature = "zmq")]
            let zmq_publisher =
                if let Some(zmq_config) = self.config.as_ref().and_then(|c| c.zmq.as_ref()) {
                    if zmq_config.is_enabled() {
                        match crate::zmq::ZmqPublisher::new(zmq_config) {
                            Ok(publisher) => {
                                info!("ZMQ publisher initialized");
                                Some(Arc::new(publisher))
                            }
                            Err(e) => {
                                warn!("Failed to initialize ZMQ publisher: {}", e);
                                None
                            }
                        }
                    } else {
                        debug!(
                        "ZMQ configured but no endpoints enabled - ZMQ publisher not initialized"
                    );
                        None
                    }
                } else {
                    debug!("No ZMQ configuration provided - ZMQ publisher not initialized");
                    None
                };

            #[cfg(not(feature = "zmq"))]
            let zmq_publisher = None;

            self.event_publisher = {
                #[cfg(feature = "zmq")]
                {
                    Some(EventPublisher::with_zmq(
                        Arc::clone(event_manager),
                        zmq_publisher,
                    ))
                }
                #[cfg(not(feature = "zmq"))]
                {
                    Some(EventPublisher::new(Arc::clone(event_manager)))
                }
            };
            info!("Event publisher initialized");
        }

        Ok(())
    }

    /// Initialize peer connections automatically
    ///
    /// Determines network type from protocol version and uses config if available.
    async fn initialize_peer_connections(&self) -> Result<()> {
        // Determine network type from protocol version
        let network = match self.protocol_version {
            ProtocolVersion::BitcoinV1 => "mainnet",
            ProtocolVersion::Testnet3 => "testnet",
            ProtocolVersion::Regtest => {
                // Regtest doesn't use DNS seeds
                info!("Regtest network: skipping DNS seed discovery");
                // Still connect to persistent peers if configured
                if let Some(ref config) = self.config {
                    if !config.persistent_peers.is_empty() {
                        let persistent_peers = &config.persistent_peers;
                        if let Err(e) = self
                            .network
                            .connect_persistent_peers(persistent_peers)
                            .await
                        {
                            warn!("Failed to connect to some persistent peers: {}", e);
                        }
                    }
                }
                return Ok(());
            }
        };

        // Get port from network address
        let port = self.network_addr.port();

        // Use config if available, otherwise use defaults
        let default_config = NodeConfig {
            listen_addr: Some(self.network_addr),
            ..Default::default()
        };
        let config = self.config.as_ref().unwrap_or(&default_config);

        // Get target peer count from config
        let default_timing = crate::config::NetworkTimingConfig::default();
        let timing_config = config.network_timing.as_ref().unwrap_or(&default_timing);
        let target_peer_count = timing_config.target_peer_count;

        // Initialize peer connections
        if let Err(e) = self
            .network
            .initialize_peer_connections(config, network, port, target_peer_count)
            .await
        {
            warn!("Failed to initialize peer connections: {}", e);
            // Don't fail startup - peer connections are best-effort
        }

        Ok(())
    }

    /// Main node run loop
    async fn run(&mut self) -> Result<()> {
        info!("Node running - main loop started");

        // Set up graceful shutdown signal handling
        let shutdown_rx = crate::utils::create_shutdown_receiver();

        // Get initial state for block processing
        let mut current_height = self.storage.chain().get_height()?.unwrap_or(0);
        let mut utxo_set = bllvm_protocol::UtxoSet::new();

        // Main node loop - coordinates between all components and handles shutdown signals
        loop {
            // Check for shutdown signal (non-blocking)
            if *shutdown_rx.borrow() {
                info!("Shutdown signal received, stopping node gracefully...");
                break;
            }
            // Process any received blocks (non-blocking)
            while let Some(block_data) = self.network.try_recv_block() {
                info!("Processing block from network");
                let blocks_arc = self.storage.blocks();
                match self.sync_coordinator.process_block(
                    &blocks_arc,
                    &block_data,
                    current_height,
                    &mut utxo_set,
                    self.protocol_version,
                    Some(Arc::clone(&self.metrics)),
                    Some(Arc::clone(&self.profiler)),
                ) {
                    Ok(true) => {
                        info!("Block accepted at height {}", current_height);

                        // Parse block for governance webhook (need block object, not just block_data)
                        // We'll get it from storage after it's stored
                        let blocks_arc = self.storage.blocks();
                        let block_hash =
                            if let Ok(Some(hash)) = blocks_arc.get_hash_by_height(current_height) {
                                hash
                            } else {
                                warn!("Failed to get block hash for height {}", current_height);
                                [0u8; 32]
                            };

                        // Update chain tip (for chainwork, etc.)
                        if let Ok(Some(block)) = blocks_arc.get_block(&block_hash) {
                            if let Err(e) = self.storage.chain().update_tip(
                                &block_hash,
                                &block.header,
                                current_height,
                            ) {
                                warn!("Failed to update chain tip: {}", e);
                            }

                            // Update UTXO stats cache (for fast gettxoutsetinfo RPC)
                            let transaction_count =
                                self.storage.transaction_count().unwrap_or(0) as u64;
                            if let Err(e) = self.storage.chain().update_utxo_stats_cache(
                                &block_hash,
                                current_height,
                                &utxo_set,
                                transaction_count,
                            ) {
                                warn!("Failed to update UTXO stats cache: {}", e);
                            }

                            // Update network hashrate cache (for fast getmininginfo RPC)
                            if let Err(e) = self
                                .storage
                                .chain()
                                .calculate_and_cache_network_hashrate(current_height, &blocks_arc)
                            {
                                warn!("Failed to update network hashrate cache: {}", e);
                            }

                            // Notify governance app about new block (for fee forwarding tracking)
                            #[cfg(feature = "governance")]
                            if let Some(ref webhook) = self.governance_webhook {
                                if let Err(e) = webhook.notify_block(&block, current_height).await {
                                    warn!("Failed to notify governance app about block at height {}: {}", current_height, e);
                                }
                            }
                        }

                        // Persist UTXO set to storage after block validation
                        // This is critical for commitment generation and incremental pruning
                        if let Err(e) = self.storage.utxos().store_utxo_set(&utxo_set) {
                            warn!(
                                "Failed to persist UTXO set after block {}: {}",
                                current_height, e
                            );
                        }

                        // Generate UTXO commitment from current state (if enabled)
                        // Use current_height (the block that was just validated) before incrementing
                        #[cfg(feature = "utxo-commitments")]
                        {
                            if let Some(pruning_manager) = self.storage.pruning() {
                                if let (Some(commitment_store), Some(_utxostore)) = (
                                    pruning_manager.commitment_store(),
                                    pruning_manager.utxostore(),
                                ) {
                                    // Get block hash from storage (block was just stored at current_height)
                                    let blocks_arc = self.storage.blocks();
                                    if let Ok(Some(block_hash)) =
                                        blocks_arc.get_hash_by_height(current_height)
                                    {
                                        // Generate commitment from current UTXO set state
                                        if let Err(e) = pruning_manager
                                            .generate_commitment_from_current_state(
                                                &block_hash,
                                                current_height,
                                                &utxo_set,
                                                &commitment_store,
                                            )
                                        {
                                            warn!(
                                                "Failed to generate commitment for block {}: {}",
                                                current_height, e
                                            );
                                        } else {
                                            debug!(
                                                "Generated UTXO commitment for block {}",
                                                current_height
                                            );
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
                            if let Ok(Some(prune_stats)) =
                                pruning_manager.incremental_prune_during_ibd(current_height, is_ibd)
                            {
                                info!("Incremental pruning during IBD: {} blocks pruned, {} bytes freed", 
                                      prune_stats.blocks_pruned, prune_stats.storage_freed);
                                // Flush storage to persist pruning changes
                                if let Err(e) = self.storage.flush() {
                                    warn!(
                                        "Failed to flush storage after incremental pruning: {}",
                                        e
                                    );
                                }
                            }
                        }

                        // Check for automatic pruning after block acceptance
                        if let Some(pruning_manager) = self.storage.pruning() {
                            let stats = pruning_manager.get_stats();
                            let should_prune = pruning_manager
                                .should_auto_prune(current_height, stats.last_prune_height);

                            if should_prune {
                                info!("Automatic pruning triggered at height {}", current_height);

                                // Calculate prune height based on configuration
                                let prune_height = match &pruning_manager.config.mode {
                                    crate::config::PruningMode::Disabled => None,
                                    crate::config::PruningMode::Normal {
                                        keep_from_height, ..
                                    } => {
                                        // Prune to keep_from_height, but ensure we keep min_blocks
                                        let min_keep = pruning_manager.config.min_blocks_to_keep;
                                        let effective_keep = (*keep_from_height)
                                            .max(current_height.saturating_sub(min_keep));
                                        Some(effective_keep)
                                    }
                                    #[cfg(feature = "utxo-commitments")]
                                    crate::config::PruningMode::Aggressive {
                                        keep_from_height,
                                        min_blocks,
                                        ..
                                    } => {
                                        // Prune to keep_from_height, respecting min_blocks
                                        let effective_keep = (*keep_from_height)
                                            .max(current_height.saturating_sub(*min_blocks));
                                        Some(effective_keep)
                                    }
                                    #[cfg(not(feature = "utxo-commitments"))]
                                    crate::config::PruningMode::Aggressive { .. } => {
                                        // Aggressive pruning requires utxo-commitments feature
                                        // Fall back to no pruning if feature is disabled
                                        None
                                    }
                                    crate::config::PruningMode::Custom {
                                        keep_bodies_from_height,
                                        ..
                                    } => {
                                        // Prune to keep_bodies_from_height, respecting min_blocks
                                        let min_keep = pruning_manager.config.min_blocks_to_keep;
                                        let effective_keep = (*keep_bodies_from_height)
                                            .max(current_height.saturating_sub(min_keep));
                                        Some(effective_keep)
                                    }
                                };

                                if let Some(prune_to_height) = prune_height {
                                    if prune_to_height < current_height {
                                        match pruning_manager.prune_to_height(
                                            prune_to_height,
                                            current_height,
                                            false,
                                        ) {
                                            Ok(prune_stats) => {
                                                info!("Automatic pruning completed: {} blocks pruned, {} blocks kept", 
                                                      prune_stats.blocks_pruned, prune_stats.blocks_kept);
                                                // Flush storage to persist pruning changes
                                                use crate::utils::log_error;
                                                log_error(|| self.storage.flush(), "Failed to flush storage after automatic pruning");
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

            // Check disk space periodically (every 10 iterations = ~1 second)
            // Use timeout to prevent hanging on slow disk operations
            let counter = self
                .disk_check_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if counter % 10 == 0 {
                use crate::utils::with_storage_timeout;
                match with_storage_timeout(async { self.check_disk_space().await }).await {
                    Ok(Ok(())) => {
                        // Disk check succeeded
                    }
                    Ok(Err(e)) => {
                        warn!("Disk space check failed: {}", e);
                        // Continue - disk errors don't stop the node
                    }
                    Err(_) => {
                        warn!("Disk space check timed out");
                        // Continue - timeout doesn't stop the node
                    }
                }
            }
        }

        // Graceful shutdown - stop all components
        info!("Initiating graceful shutdown...");
        self.stop().await?;
        Ok(())
    }

    /// Run node processing once (for testing)
    pub async fn run_once(&mut self) -> Result<()> {
        info!("Running node processing once");

        // Check node health
        self.check_health().await?;

        Ok(())
    }

    /// Check node health with graceful error handling
    async fn check_health(&self) -> Result<()> {
        // Check peer count (non-blocking, always succeeds)
        let peer_count = self.network.peer_count();
        if peer_count == 0 {
            warn!("No peers connected");
        }

        // Check storage with timeout and graceful degradation
        use crate::utils::with_storage_timeout;
        match with_storage_timeout(async { self.storage.blocks().block_count() }).await {
            Ok(Ok(blocks)) => {
                if blocks == 0 {
                    warn!("No blocks in storage");
                }
            }
            Ok(Err(e)) => {
                warn!("Storage error during health check: {}", e);
                // Continue - storage errors don't stop the node
            }
            Err(_) => {
                warn!("Storage health check timed out");
                // Continue - timeout doesn't stop the node
            }
        }

        Ok(())
    }

    /// Check disk space and trigger pruning if needed
    async fn check_disk_space(&self) -> Result<()> {
        // Check storage bounds (80% threshold)
        let within_bounds = self.storage.check_storage_bounds()?;

        if !within_bounds {
            warn!("Storage bounds exceeded - disk space may be low");

            // Check if pruning is enabled
            if self.storage.is_pruning_enabled() {
                if let Some(pruning_manager) = self.storage.pruning() {
                    // Get current chain height from chain info
                    let chain_info = self.storage.chain().load_chain_info()?;
                    if let Some(info) = chain_info {
                        let current_height = info.height;

                        // Get last prune height
                        let last_prune_height = pruning_manager.get_stats().last_prune_height;

                        // Check if we should auto-prune
                        if pruning_manager.should_auto_prune(current_height, last_prune_height) {
                            info!("Triggering automatic pruning due to storage bounds");

                            // Perform pruning (prune_to_height is not async, but we're in async context)
                            // Calculate prune height: keep only the last 1000 blocks (configurable)
                            let prune_window = 1000; // Keep last 1000 blocks
                            let prune_to_height = current_height.saturating_sub(prune_window);

                            if prune_to_height > 0 && prune_to_height < current_height {
                                match tokio::task::spawn_blocking({
                                    let pruning_manager = Arc::clone(&pruning_manager);
                                    move || {
                                        pruning_manager.prune_to_height(
                                            prune_to_height,
                                            current_height,
                                            false,
                                        )
                                    }
                                })
                                .await
                                {
                                    Ok(Ok(stats)) => {
                                        info!(
                                        "Automatic pruning completed: {} blocks pruned, {} blocks kept, {} bytes freed",
                                        stats.blocks_pruned,
                                        stats.blocks_kept,
                                        stats.storage_freed
                                    );

                                        // Flush storage to persist pruning changes
                                        use crate::utils::log_error;
                                        log_error(
                                            || self.storage.flush(),
                                            "Failed to flush storage after automatic pruning",
                                        );
                                    }
                                    Ok(Err(e)) => {
                                        warn!("Automatic pruning failed: {}", e);
                                    }
                                    Err(e) => {
                                        warn!("Pruning task failed: {}", e);
                                    }
                                }
                            }
                        } else {
                            warn!("Storage bounds exceeded but auto-pruning not yet due (last prune: {:?}, current: {})", 
                                  last_prune_height, current_height);
                        }
                    } else {
                        warn!("Storage bounds exceeded but chain info not available");
                    }
                } else {
                    warn!("Storage bounds exceeded but pruning manager not available");
                }
            } else {
                warn!("Storage bounds exceeded but pruning is disabled (archival node mode)");
            }
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

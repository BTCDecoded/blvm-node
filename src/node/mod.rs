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
#[cfg(feature = "production")]
pub mod parallel_ibd;

use anyhow::Result;
use hex;
use secp256k1;
use std::net::SocketAddr;
use tracing::{debug, info, warn};

use crate::config::NodeConfig;
use crate::module::api::events::EventManager;
use crate::module::ModuleManager;
use crate::network::NetworkManager;
use crate::node::event_publisher::EventPublisher;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::PerformanceProfiler;
use crate::rpc::RpcManager;
use crate::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Main node orchestrator
pub struct Node {
    protocol: Arc<BitcoinProtocolEngine>,
    storage: Arc<Storage>,
    network: Arc<NetworkManager>,
    /// Module registry (shared between network and module manager)
    module_registry: Option<Arc<crate::module::registry::client::ModuleRegistry>>,
    /// Payment processor for BIP70 payments (HTTP and P2P)
    payment_processor: Option<Arc<crate::payment::processor::PaymentProcessor>>,
    /// Payment state machine for unified payment coordination
    payment_state_machine: Option<Arc<crate::payment::state_machine::PaymentStateMachine>>,
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
    event_publisher: Option<Arc<EventPublisher>>,
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
    // Governance is handled via the module system (blvm-governance module)
    // The module subscribes to governance events and handles webhook delivery
}

impl Node {
    /// Create a new node
    pub fn new(
        data_dir: &str,
        network_addr: SocketAddr,
        rpc_addr: SocketAddr,
        protocol_version: Option<ProtocolVersion>,
    ) -> Result<Self> {
        Self::with_storage_config(
            data_dir,
            network_addr,
            rpc_addr,
            protocol_version,
            None,
            None,
        )
    }

    /// Create a new node with storage configuration
    pub fn with_storage_config(
        data_dir: &str,
        network_addr: SocketAddr,
        rpc_addr: SocketAddr,
        protocol_version: Option<ProtocolVersion>,
        pruning_config: Option<crate::config::PruningConfig>,
        indexing_config: Option<crate::config::IndexingConfig>,
    ) -> Result<Self> {
        info!("Initializing reference-node");

        // Initialize components
        let protocol_version = protocol_version.unwrap_or(ProtocolVersion::Regtest);
        let protocol = BitcoinProtocolEngine::new(protocol_version)?;
        let protocol_arc = Arc::new(protocol);

        // Create storage with configuration
        use crate::storage::database::default_backend;
        let backend = default_backend();
        let storage = Storage::with_backend_pruning_and_indexing(
            data_dir,
            backend,
            pruning_config,
            indexing_config,
        )?;
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
        // Note: EventPublisher will be set later in start_components() after it's created
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
            network: network_arc,
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
            module_registry: None,
            payment_processor: None,
            payment_state_machine: None,
            // Governance handled via module system
        })
    }

    /// Set node configuration
    pub fn with_config(mut self, config: NodeConfig) -> Result<Self> {
        // Auto-detect governance server if configured (best-effort, non-blocking)
        #[cfg(feature = "governance")]
        {
            // Spawn async task to check governance server health (if in async runtime)
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                if let Some(ref gov_config) = config.governance {
                    if !gov_config.enabled {
                        if let Some(ref commons_url) = gov_config.commons_url {
                            let url = commons_url.clone();
                            handle.spawn(async move {
                                // Check if governance server is reachable
                                let client = reqwest::Client::builder()
                                    .timeout(std::time::Duration::from_secs(5))
                                    .build();

                                if let Ok(client) = client {
                                    let health_url = format!("{}/internal/health", url);
                                    if let Ok(response) = client.get(&health_url).send().await {
                                        if response.status().is_success() {
                                            info!("Governance server detected at {}, consider setting governance.enabled = true", url);
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
            }
        }

        // Apply network configuration if available
        let max_peers = config.max_peers.unwrap_or(100);
        let transport_preference = config.get_transport_preference();

        // Recreate network manager with config
        let network_addr = self.network_addr;
        let protocol_arc = self.protocol.clone();
        // Use existing storage Arc instead of creating a new one
        let storage_arc = Arc::clone(&self.storage);
        let mempool_manager_arc = Arc::clone(&self.mempool_manager);

        let network = Arc::new(
            NetworkManager::with_config(
                network_addr,
                max_peers,
                transport_preference,
                Some(&config),
            )
            .with_dependencies(protocol_arc, storage_arc, mempool_manager_arc)
        );

        // Governance is handled via the blvm-governance module
        // The module subscribes to governance events (EconomicNodeRegistered, EconomicNodeVeto, etc.)
        // and handles webhook delivery and economic node tracking

        self.network = network;
        self.config = Some(config.clone());

        // Apply RBF and mempool policy configurations to mempool manager
        // Uses interior mutability so we can set configs even when mempool is in an Arc
        if let Some(ref rbf_config) = config.rbf {
            self.mempool_manager
                .set_rbf_config(Some(rbf_config.clone()));
            info!("RBF configuration applied: mode={:?}", rbf_config.mode);
        }
        if let Some(ref mempool_policy) = config.mempool {
            self.mempool_manager
                .set_policy_config(Some(mempool_policy.clone()));
            info!(
                "Mempool policy configuration applied: max_mempool_mb={}, eviction_strategy={:?}",
                mempool_policy.max_mempool_mb, mempool_policy.eviction_strategy
            );
        }

        // Governance handled via module system - no direct webhook client needed
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

        // Validate security configuration and emit warnings
        if let Some(ref config) = self.config {
            let rpc_addr = self.rpc.rpc_addr();
            #[cfg(feature = "rest-api")]
            let rest_api_addr: Option<SocketAddr> = self.rpc.rest_api_addr();
            #[cfg(not(feature = "rest-api"))]
            let rest_api_addr: Option<SocketAddr> = None;

            let warnings: Vec<String> = config.validate_security(rpc_addr, rest_api_addr);
            for warning in warnings {
                warn!("{}", warning);
            }
        }

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

        // Wait for peer handshakes to complete (Version/VerAck exchange)
        // Process network messages during this time to handle Version/VerAck exchange
        info!("Waiting for peer handshakes to complete...");
        let handshake_timeout = tokio::time::Duration::from_secs(10);
        let handshake_start = std::time::Instant::now();
        let mut total_processed = 0usize;
        while handshake_start.elapsed() < handshake_timeout {
            // Process pending network messages without blocking (handles Version/VerAck)
            match self.network.process_pending_messages().await {
                Ok(count) => {
                    if count > 0 {
                        info!("Processed {} network messages during handshake", count);
                        total_processed += count;
                    }
                }
                Err(e) => {
                    warn!("Error processing network messages during handshake: {}", e);
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        info!("Handshake period complete, processed {} total messages", total_processed);

        // Spawn background message processing task BEFORE starting IBD
        // This ensures Headers responses and other messages are processed during IBD
        let network_for_processing = Arc::clone(&self.network);
        let _message_processor = tokio::spawn(async move {
            loop {
                if let Err(e) = network_for_processing.process_messages().await {
                    tracing::warn!("Error in background message processing: {}", e);
                }
                // Small sleep to prevent busy-looping if no messages
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });
        info!("Background message processor spawned");

        // Check if we need to do IBD and attempt parallel IBD if available
        let current_height = self.storage.chain().get_height()?.unwrap_or(0);
        let is_ibd = current_height == 0;
        
        if is_ibd {
            info!("Detected IBD (height: {}), checking for parallel IBD support...", current_height);
            
            // Get connected peers for parallel IBD
            let peer_addresses: Vec<String> = self.network
                .peer_addresses()
                .iter()
                .map(|addr| addr.to_string())
                .collect();
            
            if peer_addresses.len() >= 2 {
                info!("Attempting parallel IBD with {} peers", peer_addresses.len());
                
                // Get target height from peer's start_height (best block height from Version messages)
                // Use the highest reported height from all connected peers
                let target_height = match self.network.get_highest_peer_start_height() {
                    Some(peer_height) => {
                        info!("Using peer-reported chain tip height: {} (current: {})", peer_height, current_height);
                        peer_height.max(current_height) // Ensure target is at least current height
                    }
                    None => {
                        // Fallback: if no peers have reported start_height yet, use a large number
                        // The iterative header sync will stop when it reaches the actual chain tip
                        warn!("No peer start_height available yet, using fallback target height");
                        current_height + 1_000_000
                    }
                };
                
                // Attempt parallel IBD
                let blockstore = Arc::clone(&self.storage.blocks());
                let storage_arc = Arc::clone(&self.storage);
                let protocol_arc = Arc::clone(&self.protocol);
                let mut utxo_set = blvm_protocol::UtxoSet::new();
                
                match self.sync_coordinator.start_parallel_ibd(
                    current_height,
                    target_height,
                    blockstore,
                    Some(storage_arc),
                    protocol_arc,
                    &mut utxo_set,
                    Some(Arc::clone(&self.network)),
                    peer_addresses,
                ).await {
                    Ok(true) => {
                        info!("Parallel IBD completed successfully");
                        // Update current height after parallel IBD
                        let new_height = self.storage.chain().get_height()?.unwrap_or(0);
                        info!("IBD completed, current height: {}", new_height);
                    }
                    Ok(false) => {
                        info!("Parallel IBD not available or failed, will use sequential sync");
                    }
                    Err(e) => {
                        warn!("Parallel IBD error: {}, will use sequential sync", e);
                    }
                }
            } else {
                info!("Not enough peers for parallel IBD (have {}, need 2), will use sequential sync", peer_addresses.len());
            }
        }

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
                            Some(keep_from_height)
                        }
                        #[cfg(feature = "utxo-commitments")]
                        crate::config::PruningMode::Aggressive {
                            keep_from_height, ..
                        } => {
                            // Prune up to keep_from_height
                            Some(keep_from_height)
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
                            Some(keep_bodies_from_height)
                        }
                    };

                    if let Some(prune_to_height) = prune_height {
                        if *prune_to_height < current_height {
                            match pruning_manager.prune_to_height(
                                *prune_to_height,
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
            use crate::utils::env_or_default;
            
            // Reuse the existing storage instance (Redb only allows one connection)
            let storage_arc = Arc::clone(&self.storage);

            // Get event manager from module manager
            let event_manager = module_manager.event_manager();

            // Create NodeApiImpl with all dependencies
            let mut node_api_impl = crate::module::api::node_api::NodeApiImpl::with_dependencies(
                Arc::clone(&storage_arc),
                Some(Arc::clone(event_manager)),
                None, // module_id will be set per-module
                Some(Arc::clone(&self.mempool_manager)),
                Some(Arc::clone(&self.network)),  // Now we can set it directly
            );

            // Set node storage for module storage API (needed for opening storage trees)
            node_api_impl.set_node_storage(Arc::clone(&storage_arc));

            // Set sync coordinator for sync status checking
            let sync_coord_arc = Arc::new(tokio::sync::Mutex::new(self.sync_coordinator.clone()));
            node_api_impl.set_sync_coordinator(sync_coord_arc);

            // Set module manager for module discovery
            // Note: We can't clone ModuleManager, so we'll set it later after it's been moved into an Arc
            // For now, we skip this - the module manager can be accessed via other means if needed

            // Set up module API registry and router for module-to-module communication
            let module_api_registry =
                Arc::new(crate::module::inter_module::registry::ModuleApiRegistry::new());
            // Note: ModuleManager doesn't implement Clone, so we can't pass it to the router here
            // The router can access the module manager through other means if needed
            let module_router = Arc::new(crate::module::inter_module::router::ModuleRouter::new(
                Arc::clone(&module_api_registry),
            ));
            node_api_impl.set_module_api_registry(
                Arc::clone(&module_api_registry),
                Arc::clone(&module_router),
            );

            // Note: Payment state machine will be set after payment processor initialization
            // We'll update it via Arc::get_mut if possible, or store it separately

            let mut node_api = Arc::new(node_api_impl);
            let socket_path = env_or_default("MODULE_SOCKET_DIR", "data/modules/socket");

            // Initialize module registry if network is available
            let registry_cache_dir = self.data_dir.join("modules").join("registry_cache");
            let registry_cas_dir = self.data_dir.join("modules").join("registry_cas");
            let registry_mirrors = Vec::new(); // Empty by default - can be configured later

            if let Ok(mut module_registry) = crate::module::registry::client::ModuleRegistry::new(
                &registry_cache_dir,
                &registry_cas_dir,
                registry_mirrors,
            ) {
                // Set network manager on registry (now simple since network is already Arc)
                module_registry.set_network_manager(Arc::clone(&self.network));

                let module_registry_arc = Arc::new(module_registry);

                // Store registry in node for later use
                self.module_registry = Some(Arc::clone(&module_registry_arc));

                // Connect network manager to registry (using mutable reference)
                // This allows the network to serve module requests
                self.network
                    .set_module_registry(Arc::clone(&module_registry_arc))
                    .await;

                // Connect module manager to registry (using mutable reference)
                module_manager.set_module_registry(Arc::clone(&module_registry_arc));

                info!("Module registry initialized and connected to network and module manager");

                // Initialize payment processor if payment is enabled
                let payment_config = self
                    .config
                    .as_ref()
                    .and_then(|c| c.payment.as_ref())
                    .cloned()
                    .unwrap_or_default();

                if payment_config.p2p_enabled || payment_config.http_enabled {
                    match crate::payment::processor::PaymentProcessor::new(payment_config.clone()) {
                        Ok(mut processor) => {
                            // Connect module registry to payment processor
                            processor =
                                processor.with_module_registry(Arc::clone(&module_registry_arc));

                            // Add module encryption support
                            let module_encryption =
                                Arc::new(crate::module::encryption::ModuleEncryption::new());
                            processor =
                                processor.with_module_encryption(Arc::clone(&module_encryption));

                            // Add modules directory for storing encrypted/decrypted modules
                            if let Some(ref module_config) =
                                self.config.as_ref().and_then(|c| c.modules.as_ref())
                            {
                                let modules_dir = PathBuf::from(&module_config.modules_dir);
                                processor = processor.with_modules_dir(modules_dir);
                            }

                            let processor_arc = Arc::new(processor);

                            // Store processor in node
                            self.payment_processor = Some(Arc::clone(&processor_arc));

                            // Create payment state machine for unified payment coordination
                            #[cfg(feature = "ctv")]
                            let state_machine =
                                crate::payment::state_machine::PaymentStateMachine::with_storage(
                                    Arc::clone(&processor_arc),
                                    Some(Arc::clone(&self.storage)),
                                )
                                .with_congestion_manager(
                                    Some(Arc::clone(&self.mempool_manager)),
                                    Some(Arc::clone(&self.storage)),
                                    crate::payment::congestion::BatchConfig::default(),
                                );
                            #[cfg(not(feature = "ctv"))]
                            let state_machine =
                                crate::payment::state_machine::PaymentStateMachine::new(
                                    Arc::clone(&processor_arc),
                                );

                            let mut state_machine_arc = Arc::new(state_machine);

                            // Set network sender for payment proof broadcasting
                            // We need to get mutable access to set the sender
                            #[cfg(feature = "ctv")]
                            {
                                use std::sync::Arc as StdArc;
                                if let Some(sm) = StdArc::get_mut(&mut state_machine_arc) {
                                    // Create a clone with the network sender set
                                    let sm_with_sender = sm.clone().with_network_sender(
                                        // We need to get peer_tx from network, but it's private
                                        // For now, we'll set it via NetworkManager's set_payment_state_machine
                                        // which will handle setting the sender
                                        // Actually, let's pass it here if we can access it
                                        // Since we can't easily get peer_tx, we'll set it in set_payment_state_machine
                                    );
                                    // This won't work because we can't replace the Arc contents
                                    // Let's use a different approach - set it in set_payment_state_machine
                                }
                            }

                            self.payment_state_machine = Some(Arc::clone(&state_machine_arc));

                            // Connect to network manager (for P2P payments)
                            self.network
                                .set_payment_processor(Arc::clone(&processor_arc))
                                .await;
                            self.network
                                .set_payment_state_machine(Arc::clone(&state_machine_arc))
                                .await;

                            // Set merchant key from config if available
                            let merchant_key =
                                payment_config.merchant_key.as_ref().and_then(|hex_key| {
                                    hex::decode(hex_key).ok().and_then(|bytes| {
                                        if bytes.len() == 32 {
                                            secp256k1::SecretKey::from_slice(&bytes).ok()
                                        } else {
                                            None
                                        }
                                    })
                                });
                            self.network.set_merchant_key(merchant_key).await;

                            // Set node payment address script from config if available
                            let node_script = payment_config
                                .node_payment_address
                                .as_ref()
                                .and_then(|addr_str| {
                                    // Decode Bitcoin address to script pubkey
                                    use blvm_protocol::address::BitcoinAddress;
                                    BitcoinAddress::decode(addr_str).ok().and_then(|addr| {
                                        // Convert address to script pubkey
                                        match (addr.witness_version, addr.witness_program.len()) {
                                            // SegWit v0: P2WPKH (20 bytes) or P2WSH (32 bytes)
                                            (0, 20) | (0, 32) => {
                                                let mut script = vec![0x00]; // OP_0
                                                script.extend_from_slice(&addr.witness_program);
                                                Some(script)
                                            }
                                            // Taproot v1: P2TR (32 bytes)
                                            (1, 32) => {
                                                let mut script = vec![0x51]; // OP_1
                                                script.extend_from_slice(&addr.witness_program);
                                                Some(script)
                                            }
                                            _ => None,
                                        }
                                    })
                                });
                            self.network.set_node_payment_script(node_script).await;

                            // Set module encryption and modules directory in network manager
                            self.network
                                .set_module_encryption(Arc::clone(&module_encryption))
                                .await;
                            if let Some(ref module_config) =
                                self.config.as_ref().and_then(|c| c.modules.as_ref())
                            {
                                self.network
                                    .set_modules_dir(PathBuf::from(&module_config.modules_dir))
                                    .await;
                            }

                            // Connect to RPC manager (for HTTP payments)
                            #[cfg(feature = "bip70-http")]
                            {
                                let rpc = std::mem::replace(
                                    &mut self.rpc,
                                    crate::rpc::RpcManager::new("127.0.0.1:0".parse().unwrap()),
                                );
                                self.rpc = rpc.with_payment_processor(Arc::clone(&processor_arc));
                                #[cfg(feature = "ctv")]
                                {
                                    let rpc = std::mem::replace(
                                        &mut self.rpc,
                                        crate::rpc::RpcManager::new("127.0.0.1:0".parse().unwrap()),
                                    );
                                    self.rpc = rpc
                                        .with_payment_state_machine(Arc::clone(&state_machine_arc));
                                }
                            }

                            // Update node API with payment state machine
                            // Try to get mutable access to update the Arc
                            if let Some(node_api_mut) = Arc::get_mut(&mut node_api) {
                                node_api_mut
                                    .set_payment_state_machine(Arc::clone(&state_machine_arc));
                                info!("Payment state machine set on NodeApiImpl");
                            } else {
                                // If we can't get mutable access (multiple references exist),
                                // the payment state machine will be None until node_api is recreated
                                // This is acceptable as payment features will work once modules reconnect
                                warn!("Could not set payment state machine on NodeApiImpl (multiple references exist)");
                            }

                            info!(
                                "Payment processor initialized: P2P={}, HTTP={}",
                                payment_config.p2p_enabled, payment_config.http_enabled
                            );
                        }
                        Err(e) => {
                            warn!("Failed to initialize payment processor: {}", e);
                        }
                    }
                }
            } else {
                warn!("Failed to initialize module registry - modules will only load from local directory");
            }

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
            self.event_publisher = {
                #[cfg(feature = "zmq")]
                {
                    let zmq_publisher = if let Some(zmq_config) =
                        self.config.as_ref().and_then(|c| c.zmq.as_ref())
                    {
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
                    Some(Arc::new(EventPublisher::with_zmq(
                        Arc::clone(event_manager),
                        zmq_publisher,
                    )))
                }
                #[cfg(not(feature = "zmq"))]
                {
                    Some(Arc::new(EventPublisher::new(Arc::clone(event_manager))))
                }
            };
            info!("Event publisher initialized");

            // Set EventPublisher on MempoolManager for mempool event publishing
            if let Some(ref event_publisher) = self.event_publisher {
                self.mempool_manager
                    .set_event_publisher(Some(Arc::clone(event_publisher)));
                info!("Event publisher set on mempool manager");

                // Set EventPublisher on NetworkManager for network event publishing
                // Note: NetworkManager is stored as a value, not Arc, so we need to use a mutable reference
                // Since we're in start_components which is &mut self, we can do this
                self.network
                    .set_event_publisher(Some(Arc::clone(event_publisher)))
                    .await;
                info!("Event publisher set on network manager");

                // Publish ConfigLoaded event for modules to react to node configuration
                // This allows modules like blvm-governance to configure themselves based on node config
                if let Some(ref config) = self.config {
                    // Determine which config sections are relevant
                    let mut changed_sections = Vec::new();
                    if config.network_timing.is_some() {
                        changed_sections.push("network_timing".to_string());
                    }
                    if config.governance.is_some() {
                        changed_sections.push("governance".to_string());
                    }
                    if config.modules.is_some() {
                        changed_sections.push("modules".to_string());
                    }
                    if config.mempool.is_some() {
                        changed_sections.push("mempool".to_string());
                    }
                    if config.rbf.is_some() {
                        changed_sections.push("rbf".to_string());
                    }
                    if config.payment.is_some() {
                        changed_sections.push("payment".to_string());
                    }

                    // Serialize config to JSON for modules that need full config
                    let config_json = serde_json::to_string(config)
                        .ok()
                        .map(|s| format!("{{\"config\":{}}}", s));

                    // Publish event (non-blocking, modules will receive it when they subscribe)
                    // Modules can also request config via NodeAPI if needed
                    let sections_count = changed_sections.len();
                    event_publisher
                        .publish_config_loaded(changed_sections, config_json)
                        .await;
                    info!(
                        "ConfigLoaded event published for {} sections",
                        sections_count
                    );

                    // Publish NodeStartupCompleted event
                    use crate::module::ipc::protocol::EventPayload;
                    use crate::module::traits::EventType;
                    let startup_components = vec![
                        "network".to_string(),
                        "storage".to_string(),
                        "rpc".to_string(),
                        "modules".to_string(),
                    ];
                    let payload = EventPayload::NodeStartupCompleted {
                        duration_ms: 0, // Could track actual startup duration
                        components: startup_components,
                    };
                    if let Err(e) = event_publisher
                        .publish_event(EventType::NodeStartupCompleted, payload)
                        .await
                    {
                        warn!("Failed to publish NodeStartupCompleted event: {}", e);
                    } else {
                        info!("NodeStartupCompleted event published");
                    }
                }
            }
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
            payment: None,
            rest_api: None,
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
        let mut utxo_set = blvm_protocol::UtxoSet::new();

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

                // Parse block to get hash for event publishing
                use crate::node::block_processor::parse_block_from_wire;
                let block_hash_for_validation =
                    if let Ok((block, _)) = parse_block_from_wire(&block_data) {
                        use crate::storage::blockstore::BlockStore;
                        blocks_arc.get_block_hash(&block)
                    } else {
                        [0u8; 32]
                    };

                // Publish block validation started event
                if let Some(ref event_publisher) = self.event_publisher {
                    event_publisher
                        .publish_block_validation_started(
                            &block_hash_for_validation,
                            current_height,
                        )
                        .await;
                }

                let validation_start_time = std::time::Instant::now();
                match self.sync_coordinator.process_block(
                    &blocks_arc,
                    &self.protocol,
                    Some(&self.storage),
                    &block_data,
                    current_height,
                    &mut utxo_set,
                    Some(Arc::clone(&self.metrics)),
                    Some(Arc::clone(&self.profiler)),
                ) {
                    Ok(true) => {
                        info!("Block accepted at height {}", current_height);

                        let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;

                        // Publish block validation completed event (success)
                        if let Some(ref event_publisher) = self.event_publisher {
                            event_publisher
                                .publish_block_validation_completed(
                                    &block_hash_for_validation,
                                    current_height,
                                    true,
                                    validation_time_ms,
                                    None,
                                )
                                .await;
                        }

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

                            // Publish NewBlock event to modules
                            if let Some(ref event_publisher) = self.event_publisher {
                                event_publisher
                                    .publish_new_block(&block, &block_hash, current_height)
                                    .await;
                            }

                            // Governance module subscribes to NewBlock events and handles notifications
                            // No direct webhook call needed - handled via event system
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
                                        let effective_keep = keep_from_height
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
                        let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;

                        // Publish block validation completed event (failure)
                        if let Some(ref event_publisher) = self.event_publisher {
                            event_publisher
                                .publish_block_validation_completed(
                                    &block_hash_for_validation,
                                    current_height,
                                    false,
                                    validation_time_ms,
                                    Some("Block validation failed"),
                                )
                                .await;
                        }
                    }
                    Err(e) => {
                        warn!("Error processing block: {}", e);
                        let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;

                        // Publish block validation completed event (error)
                        if let Some(ref event_publisher) = self.event_publisher {
                            event_publisher
                                .publish_block_validation_completed(
                                    &block_hash_for_validation,
                                    current_height,
                                    false,
                                    validation_time_ms,
                                    Some(&format!("Block processing error: {}", e)),
                                )
                                .await;
                        }
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

            // Publish DiskSpaceLow event to modules
            if let Some(ref event_publisher) = self.event_publisher {
                // Get disk space info (simplified - could get actual disk stats)
                let total_bytes = 1_000_000_000_000; // 1TB placeholder
                let available_bytes = total_bytes / 5; // 20% free (low threshold)
                let percent_free = 20.0;
                let disk_path = self.data_dir.to_string_lossy().to_string();

                event_publisher
                    .publish_disk_space_low(available_bytes, total_bytes, percent_free, disk_path)
                    .await;
            }

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

        // Publish NodeShutdown event to modules (give them time to clean up)
        if let Some(ref event_publisher) = self.event_publisher {
            event_publisher
                .publish_node_shutdown("graceful".to_string(), 30)
                .await;
            // Give modules a moment to process shutdown event
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        // Publish DataMaintenance event to modules (high urgency flush for shutdown)
        if let Some(ref event_publisher) = self.event_publisher {
            event_publisher
                .publish_data_maintenance(
                    "flush".to_string(),    // Flush pending writes
                    "high".to_string(),     // High urgency (shutdown)
                    "shutdown".to_string(), // Reason
                    None,                   // No cleanup needed
                    Some(5),                // 5 second timeout
                )
                .await;
            // Give modules a moment to flush
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        // Stop module manager
        if let Some(ref mut module_manager) = self.module_manager {
            module_manager
                .shutdown()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to shutdown module manager: {}", e))?;
        }

        // Network manager doesn't need explicit stopping - it's managed by dropping
        // The peer manager and connections will be cleaned up when the manager is dropped

        // Stop all components
        self.rpc.stop()?;

        // Flush storage
        self.storage.flush()?;

        // Publish shutdown completed event
        if let Some(ref event_publisher) = self.event_publisher {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let payload = EventPayload::NodeShutdownCompleted {
                duration_ms: 0, // Could track actual duration if needed
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::NodeShutdownCompleted, payload)
                .await
            {
                warn!("Failed to publish NodeShutdownCompleted event: {}", e);
            }
        }

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
        self.event_publisher.as_ref().map(|arc| arc.as_ref())
    }

    /// Get event publisher (mutable)
    /// Note: This returns a reference to the Arc, not the inner EventPublisher
    /// since Arc doesn't allow mutable access to the inner value
    pub fn event_publisher_arc(&self) -> Option<&Arc<EventPublisher>> {
        self.event_publisher.as_ref()
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
        &*self.network  // Deref Arc to maintain API compatibility
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

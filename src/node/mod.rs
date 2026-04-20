//! Node orchestration for blvm-node
//!
//! This module provides sync coordination, mempool management,
//! mining coordination, and overall node state management.

#[cfg(feature = "production")]
pub mod background_validation;
pub mod block_processor;
pub mod event_publisher;
pub mod health;
pub mod mempool;
pub mod metrics;
pub mod miner;
#[cfg(feature = "production")]
pub mod parallel_ibd;
pub mod performance;
pub mod run_loop;
pub mod subsystems;
pub mod sync;

use anyhow::Result;
use hex;
use secp256k1;
use std::net::SocketAddr;
use tracing::{debug, error, info, warn};

use crate::config::{NodeConfig, RpcAuthConfig};
use crate::module::api::events::EventManager;
use crate::module::ModuleManager;
use crate::network::NetworkManager;
use crate::node::event_publisher::EventPublisher;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::PerformanceProfiler;
use crate::rpc::RpcManager;
use crate::storage::Storage;
use crate::utils::{log_error_async, HANDSHAKE_POLL_SLEEP, MESSAGE_PROCESSOR_POLL_SLEEP};
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Main node orchestrator
pub struct Node {
    protocol: Arc<BitcoinProtocolEngine>,
    storage: Arc<Storage>,
    network: Arc<NetworkManager>,
    /// Module subsystem: registry, manager, event publisher
    module_subsystem: Option<subsystems::ModuleSubsystem>,
    /// Payment subsystem: processor, state machine, reorg handler
    payment_subsystem: Option<subsystems::PaymentSubsystem>,
    rpc: RpcManager,
    #[allow(dead_code)]
    sync_coordinator: sync::SyncCoordinator,
    mempool_manager: Arc<mempool::MempoolManager>,
    #[allow(dead_code)]
    mining_coordinator: miner::MiningCoordinator,
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
    /// WASM loader (injected by binary; e.g. from blvm-sdk)
    #[cfg(feature = "wasm-modules")]
    wasm_loader: Option<std::sync::Arc<dyn crate::module::wasm::WasmModuleLoader>>,
    // Governance is handled via the module system (blvm-governance module)
    // The module subscribes to governance events and handles webhook delivery
}

impl Node {
    /// Return a config subfield if config is present; avoids repeating `config.as_ref().and_then(|c| c.field.as_ref())`.
    fn config_sub<T>(&self, f: impl FnOnce(&NodeConfig) -> Option<&T>) -> Option<&T> {
        self.config.as_ref().and_then(f)
    }

    /// Create a new node
    pub fn new(
        data_dir: &str,
        network_addr: SocketAddr,
        rpc_addr: SocketAddr,
        protocol_version: Option<ProtocolVersion>,
    ) -> Result<Self> {
        Self::with_storage_config(data_dir, network_addr, rpc_addr, protocol_version, None)
    }

    /// Create a new node with storage configuration
    ///
    /// When `storage_config` is `Some`, uses its `database_backend`, `pruning`, and `indexing`.
    /// When `None`, uses default backend and no pruning/indexing.
    pub fn with_storage_config(
        data_dir: &str,
        network_addr: SocketAddr,
        rpc_addr: SocketAddr,
        protocol_version: Option<ProtocolVersion>,
        storage_config: Option<&crate::config::StorageConfig>,
    ) -> Result<Self> {
        info!("Initializing node");

        // Initialize components
        let protocol_version = protocol_version.unwrap_or(ProtocolVersion::Regtest);
        let protocol = BitcoinProtocolEngine::new(protocol_version)?;
        let protocol_arc = Arc::new(protocol);

        // Create storage with configuration
        info!("[NODE_INIT] Creating storage...");
        let (backend, pruning_config, indexing_config) = if let Some(sc) = storage_config {
            let backend = crate::storage::database::backend_from_config(sc.database_backend)?;
            (backend, sc.pruning.clone(), sc.indexing.clone())
        } else {
            use crate::storage::database::default_backend;
            (default_backend(), None, None)
        };
        info!("[NODE_INIT] Using backend: {:?}", backend);
        info!("[NODE_INIT] Calling Storage::with_backend_pruning_and_indexing()...");
        let storage = Storage::with_backend_pruning_and_indexing(
            data_dir,
            backend,
            pruning_config,
            indexing_config,
            storage_config,
        )?;
        info!("[NODE_INIT] Storage created successfully");
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
            .with_network_manager(Arc::clone(&network_arc))
            .with_protocol_engine(Arc::clone(&protocol_arc));
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
            module_subsystem: None,
            payment_subsystem: None,
            metrics,
            profiler,
            protocol_version,
            network_addr,
            config: None,
            disk_check_counter: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "wasm-modules")]
            wasm_loader: None,
            // Governance handled via module system
        })
    }

    /// Set node configuration
    pub fn with_config(mut self, config: NodeConfig) -> Result<Self> {
        // Wire block validation (assume-valid) to consensus config FIRST.
        // Must call init_consensus_config before init_rayon (which reads the global config).
        // Precedence: ENV (BLVM_ASSUME_VALID_HEIGHT) > config > defaults.
        // Always init (not only when block_validation is Some): otherwise OnceLock fell back to
        // from_env() without merge, and assume_valid_height could stay 0 (no skip in connect_block).
        #[cfg(feature = "production")]
        {
            let mut consensus_config = blvm_consensus::config::ConsensusConfig::from_env();
            let assume_valid_from_env = std::env::var("BLVM_ASSUME_VALID_HEIGHT").is_ok();
            let file_assume_valid_height = config
                .block_validation
                .as_ref()
                .map(|bv| bv.assume_valid_height)
                .unwrap_or(0);
            if let Some(ref bv) = config.block_validation {
                if std::env::var("BLVM_ASSUME_VALID_HEIGHT").is_err() {
                    consensus_config.block_validation.assume_valid_height = bv.assume_valid_height;
                    consensus_config.block_validation.assume_valid_hash = bv.assume_valid_hash;
                }
            }
            let height = consensus_config.block_validation.assume_valid_height;
            let hash = consensus_config.block_validation.assume_valid_hash;
            blvm_consensus::config::init_consensus_config(consensus_config);
            let source = if assume_valid_from_env {
                "env"
            } else if file_assume_valid_height > 0 {
                "config"
            } else if height > 0 {
                "default"
            } else {
                "off"
            };
            if height > 0 {
                let hash_s = hash
                    .map(|h| hex::encode(h))
                    .unwrap_or_else(|| "none".to_string());
                info!(
                    "Consensus assume-valid: height={} hash={} source={} (blocks before this height may skip script checks per network policy)",
                    height, hash_s, source
                );
            } else {
                info!(
                    "Consensus assume-valid: disabled height=0 source={}",
                    source
                );
            }
        }

        // Initialize Rayon pool for script verification (uses BLVM_SCRIPT_THREADS from consensus config)
        #[cfg(feature = "production")]
        blvm_consensus::config::init_rayon_for_script_verification();

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
                                    let health_url = format!("{url}/internal/health");
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
        let max_peers = config.max_outbound_peers.unwrap_or(100);
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
            .with_dependencies(protocol_arc, storage_arc, mempool_manager_arc),
        );

        // Governance is handled via the blvm-governance module (webhooks, proposal store, etc.)

        self.network = network;
        self.config = Some(config.clone());

        // Apply RPC config (max request size, request timeouts, connection limits).
        // `self.network` was just replaced above; re-wire RPC so MiningRpc/NetworkRpc use the same
        // NetworkManager instance that receives `network.start()` and peer connections (otherwise
        // e.g. `generatetoaddress` P2P broadcast targets a stale manager with no peers).
        self.rpc = self
            .rpc
            .with_max_request_size(config.max_request_size_bytes())
            .with_max_connections_per_ip(config.max_connections_per_ip_per_minute())
            .with_batch_rate_multiplier_cap(config.rpc_batch_rate_multiplier_cap())
            .with_connection_rate_limit_window(config.rpc_connection_rate_limit_window_seconds())
            .with_request_timeouts(config.request_timeouts.clone())
            .with_network_manager(Arc::clone(&self.network));

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
                "Mempool policy configuration applied: max_mempool_mb={}, eviction_strategy={:?}, reject_spam_in_mempool={}",
                mempool_policy.max_mempool_mb,
                mempool_policy.eviction_strategy,
                mempool_policy.reject_spam_in_mempool
            );
        }

        // Governance handled via module system - no direct webhook client needed
        Ok(self)
    }

    /// Set WASM module loader (injected by binary; e.g. from blvm-sdk).
    /// Call before with_modules_from_config when WASM support is desired.
    #[cfg(feature = "wasm-modules")]
    pub fn with_wasm_loader(
        mut self,
        loader: std::sync::Arc<dyn crate::module::wasm::WasmModuleLoader>,
    ) -> Self {
        self.wasm_loader = Some(loader);
        self
    }

    /// Enable module system from configuration
    pub fn with_modules_from_config(mut self, config: &NodeConfig) -> anyhow::Result<Self> {
        if let Some(module_config) = &config.modules {
            if !module_config.enabled {
                info!("Module system disabled in configuration");
                return Ok(self);
            }

            // Get module resource limits config
            let module_resource_limits = self.config_sub(|c| c.module_resource_limits.as_ref());
            let mut module_manager = ModuleManager::with_config(
                &module_config.modules_dir,
                &module_config.data_dir,
                &module_config.socket_dir,
                module_resource_limits,
            );
            #[cfg(feature = "wasm-modules")]
            if let Some(ref loader) = self.wasm_loader {
                module_manager.set_wasm_loader(std::sync::Arc::clone(loader));
            }
            self.module_subsystem
                .get_or_insert_with(Default::default)
                .module_manager = Some(Arc::new(tokio::sync::Mutex::new(module_manager)));
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

        let mut module_manager = ModuleManager::new(
            modules_dir.as_ref(),
            modules_data_dir.as_ref(),
            socket_dir.as_ref(),
        );
        #[cfg(feature = "wasm-modules")]
        if let Some(ref loader) = self.wasm_loader {
            module_manager.set_wasm_loader(std::sync::Arc::clone(loader));
        }
        self.module_subsystem
            .get_or_insert_with(Default::default)
            .module_manager = Some(Arc::new(tokio::sync::Mutex::new(module_manager)));
        Ok(self)
    }

    /// Start the node
    pub async fn start(&mut self) -> Result<()> {
        info!("[NODE] Starting node");

        // Start all components
        info!("[NODE] Calling start_components()...");
        self.start_components().await?;
        info!("[NODE] start_components() completed");

        // Main node loop
        info!("[NODE] Starting main run loop...");
        self.run().await?;
        info!("[NODE] Main run loop exited");

        Ok(())
    }

    /// Start all node components
    async fn start_components(&mut self) -> Result<()> {
        info!("[START_COMPONENTS] Starting node components");

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

        // Create early event publisher for RPC (BlockchainRpc needs it for invalidateblock → BlockDisconnected)
        // Must be done before rpc.start() so BlockchainRpc gets event_publisher
        if let Some(ref module_manager) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.module_manager.as_ref())
        {
            let event_manager = Arc::clone(module_manager.lock().await.event_manager());
            let early_publisher = Arc::new(EventPublisher::new(event_manager));
            self.rpc.set_event_publisher(Some(early_publisher));
            self.rpc.set_module_manager(Arc::clone(module_manager));
            info!("[START_COMPONENTS] Event publisher and module manager set on RPC");
        }

        // Apply RPC auth config before starting RPC server
        if let Some(ref config) = self.config {
            if let Some(ref rpc_auth) = config.rpc_auth {
                self.rpc.with_auth_config(rpc_auth.clone()).await;
                info!("[START_COMPONENTS] RPC auth config applied (tokens/certs from rpc_auth)");
            } else if config.rpc_rate_limit_when_auth_disabled() {
                let burst = config.rpc_ip_rate_limit_burst();
                let rate = config.rpc_ip_rate_limit_rate();
                self.rpc
                    .with_auth_config(RpcAuthConfig::rate_limit_only(burst, rate))
                    .await;
                info!(
                    "[START_COMPONENTS] RPC rate-limit-only mode applied (burst={}, rate={})",
                    burst, rate
                );
            }
        }

        // Start RPC server
        info!("[START_COMPONENTS] Starting RPC server...");
        if log_error_async(
            || self.rpc.start(),
            "[START_COMPONENTS] Failed to start RPC server",
        )
        .await
        .is_some()
        {
            info!(
                "[START_COMPONENTS] RPC server started on {}",
                self.rpc.rpc_addr()
            );
        }

        info!("Network manager initialized");
        info!("Sync coordinator initialized");
        info!("Mempool manager initialized");
        info!("Mining coordinator initialized");

        // Start network manager
        info!(
            "[START_COMPONENTS] About to call network.start() on {:?}",
            self.network_addr
        );
        if let Err(e) = self.network.start(self.network_addr).await {
            warn!("[START_COMPONENTS] Failed to start network manager: {}", e);
            // Continue anyway - network might be optional
        } else {
            info!("[START_COMPONENTS] network.start() completed successfully");
        }

        // Start Stratum V2 listener on dedicated port when configured
        #[cfg(feature = "stratum-v2")]
        if let Some(ref stratum_config) = self.config_sub(|c| c.stratum_v2.as_ref()) {
            if stratum_config.enabled {
                if let Some(addr) = stratum_config.listen_addr {
                    if let Err(e) = crate::network::stratum_v2_listener::start_stratum_v2_listener(
                        &self.network,
                        addr,
                    )
                    .await
                    {
                        warn!(
                            "[START_COMPONENTS] Failed to start Stratum V2 listener on {}: {}",
                            addr, e
                        );
                    } else {
                        info!("[START_COMPONENTS] Stratum V2 listener started on {}", addr);
                    }
                }
            }
        }

        // Initialize peer connections automatically
        info!("[START_COMPONENTS] Initializing peer connections...");
        self.initialize_peer_connections().await?;
        info!("[START_COMPONENTS] Peer connections initialized");

        // Wait for peer handshakes to complete (Version/VerAck exchange)
        // Process network messages during this time to handle Version/VerAck exchange
        info!("[START_COMPONENTS] Waiting for peer handshakes to complete...");
        let handshake_secs = self
            .config_sub(|c| c.request_timeouts.as_ref())
            .map(|t| t.handshake_timeout_secs)
            .unwrap_or(10);
        let handshake_timeout = tokio::time::Duration::from_secs(handshake_secs);
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
            tokio::time::sleep(HANDSHAKE_POLL_SLEEP).await;
        }
        info!(
            "[START_COMPONENTS] Handshake period complete, processed {} total messages",
            total_processed
        );

        // Spawn background message processing task BEFORE starting IBD
        // This ensures Headers responses and other messages are processed during IBD
        let network_for_processing = Arc::clone(&self.network);
        let _message_processor = tokio::spawn(async move {
            loop {
                if let Err(e) = network_for_processing.process_messages().await {
                    tracing::warn!("Error in background message processing: {}", e);
                }
                // Small sleep to prevent busy-looping if no messages
                tokio::time::sleep(MESSAGE_PROCESSOR_POLL_SLEEP).await;
            }
        });
        info!("[START_COMPONENTS] Background message processor spawned");

        // Rebuild `chain_info` from block index if missing (crash mid-flush, legacy IBD path).
        if let Err(e) = self.storage.recover_chain_tip_from_blockstore() {
            warn!(
                "[START_COMPONENTS] recover_chain_tip_from_blockstore failed: {}",
                e
            );
        }

        // Fresh datadir: no `chain_info` yet — anchor with this network's genesis so RPC mining
        // (`generatetoaddress`, templates) and tip queries work without a prior sync.
        if self.storage.chain().load_chain_info()?.is_none() {
            let np = self.protocol.get_network_params();
            let header = np.genesis_block.header.clone();
            match self.storage.chain().initialize_from_network_metadata(
                &header,
                &np.network_name,
                np.max_target as u64,
                np.halving_interval,
            ) {
                Ok(()) => info!(
                    "[START_COMPONENTS] Initialized chainstate from network genesis (fresh datadir)"
                ),
                Err(e) => warn!(
                    "[START_COMPONENTS] Failed to initialize chain with genesis: {}",
                    e
                ),
            }
        }

        // Header sync starts at height 1 and anchors GetHeaders on `get_hash_by_height(0)`.
        // Fresh `initialize_from_network_metadata` only wrote chain_info — index genesis here.
        if let Ok(Some(info)) = self.storage.chain().load_chain_info() {
            if info.height == 0 {
                let bs = self.storage.blocks();
                if matches!(bs.get_hash_by_height(0), Ok(None)) {
                    match bs
                        .store_header(&info.tip_hash, &info.tip_header)
                        .and_then(|_| bs.store_height(0, &info.tip_hash))
                    {
                        Ok(()) => info!(
                            "[START_COMPONENTS] Indexed genesis header in blockstore (height 0)"
                        ),
                        Err(e) => warn!(
                            "[START_COMPONENTS] Failed to index genesis in blockstore: {}",
                            e
                        ),
                    }
                }
            }
        }

        // AssumeUTXO: if -assumeutxo=<blockhash> and empty chain, try to load snapshot
        let mut current_height = match self.storage.chain().get_height() {
            Ok(Some(h)) => {
                info!("[START_COMPONENTS] Storage returned height: {}", h);
                h
            }
            Ok(None) => {
                info!("[START_COMPONENTS] Storage returned None for height, using 0");
                0
            }
            Err(e) => {
                warn!(
                    "[START_COMPONENTS] Error getting height from storage: {}, using 0",
                    e
                );
                0
            }
        };
        let mut initial_utxo_set = blvm_protocol::UtxoSet::default();
        if current_height == 0 {
            if let Some(block_hash) = self.config_sub(|c| c.assumeutxo_blockhash.as_ref()) {
                use crate::storage::assumeutxo::{
                    height_for_blockhash, write_base_blockhash_marker, AssumeUtxoManager,
                };
                let network = self
                    .config
                    .as_ref()
                    .and_then(|c| c.protocol_version.as_deref())
                    .unwrap_or("regtest");
                if let Some(snapshot_height) = height_for_blockhash(network, block_hash) {
                    let data_dir = std::path::Path::new(&self.data_dir);
                    let mut manager = AssumeUtxoManager::new(data_dir);
                    if let Ok((utxo_set, metadata)) = manager.load_snapshot(snapshot_height) {
                        if let Err(e) = self
                            .storage
                            .load_assumeutxo_snapshot(&utxo_set, &metadata, None)
                        {
                            warn!("AssumeUTXO: failed to load snapshot into storage: {}. Falling back to full IBD.", e);
                        } else {
                            current_height = metadata.block_height;
                            initial_utxo_set = utxo_set;
                            if let Err(e) =
                                write_base_blockhash_marker(data_dir, &metadata.block_hash)
                            {
                                warn!(
                                    "AssumeUTXO: failed to write chainstate_snapshot marker: {}",
                                    e
                                );
                            }
                            info!("AssumeUTXO: loaded snapshot at height {}, continuing sync from tip", current_height);
                        }
                    } else {
                        warn!("AssumeUTXO: snapshot file not found for height {}. Falling back to full IBD.", snapshot_height);
                    }
                } else {
                    warn!("AssumeUTXO: block hash not in chainparams. Add to MAINNET_ASSUMEUTXO_DATA or REGTEST_ASSUMEUTXO_DATA. Falling back to full IBD.");
                }
            }
        }
        // Get target height and peer addresses for IBD decision
        let peer_addresses: Vec<String> = self
            .network
            .peer_addresses()
            .iter()
            .map(|addr| addr.to_string())
            .collect();

        // AssumeUTXO: spawn background validation when snapshot is active (marker present)
        #[cfg(feature = "production")]
        {
            use crate::storage::assumeutxo::{
                height_for_blockhash, is_background_validated, read_base_blockhash_marker,
            };
            let data_dir = std::path::Path::new(&self.data_dir);
            if let Ok(Some(base_blockhash)) = read_base_blockhash_marker(data_dir) {
                if !is_background_validated(data_dir, &base_blockhash) {
                    let network = self
                        .config
                        .as_ref()
                        .and_then(|c| c.protocol_version.as_deref())
                        .unwrap_or("regtest");
                    if let Some(base_height) = height_for_blockhash(network, &base_blockhash) {
                        crate::node::background_validation::spawn_assumeutxo_background_validation(
                            data_dir,
                            base_blockhash,
                            base_height,
                            network,
                            Arc::clone(&self.protocol),
                            Arc::clone(&self.network),
                            peer_addresses.clone(),
                        );
                    }
                }
            }
        }

        #[cfg(feature = "production")]
        {
            let data_dir = self.data_dir.as_path();
            if let Err(e) = crate::storage::ibd_autorepair::apply_ibd_utxo_autorepair_if_needed(
                self.storage.as_ref(),
                data_dir,
            ) {
                warn!(
                    "IBD UTXO autorepair (marker-based) failed: {} — continuing with existing state",
                    e
                );
            }
        }

        // Determine the effective resume point. Two heights matter:
        //   chain_tip      — the last block index height written (chain_info.height)
        //   utxo_watermark — the last height at which ALL UTXOs were guaranteed flushed to disk
        //
        // After a clean shutdown they are equal. After an unclean shutdown the watermark may lag
        // the chain tip. We restart from watermark so the UTXO store matches the height told to
        // validation. Blocks watermark+1..chain_tip are already on disk and re-fetched from a
        // local peer quickly.
        //
        // Fresh DB: get_height() returns None → start from genesis (synced_tip=0, first_block=0).
        // Existing DB: get_height() returns Some(H) → effective_tip = min(H, watermark).
        let (synced_tip, ibd_first_block_height) = match self.storage.chain().get_height() {
            Ok(None) => (0u64, 0u64), // fresh DB — start from genesis
            Ok(Some(chain_tip)) => {
                let watermark = self
                    .storage
                    .chain()
                    .get_utxo_watermark()
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                let effective_tip = chain_tip.min(watermark);
                if effective_tip < chain_tip {
                    warn!(
                        "[START_COMPONENTS] UTXO watermark ({}) < chain tip ({}); \
                         IBD will re-validate {} block(s) from height {} to restore UTXO consistency",
                        watermark,
                        chain_tip,
                        chain_tip - effective_tip,
                        effective_tip + 1,
                    );
                }
                (effective_tip, effective_tip.saturating_add(1))
            }
            Err(e) => {
                warn!(
                    "[START_COMPONENTS] get_height failed: {}, treating as fresh sync",
                    e
                );
                (0u64, 0u64)
            }
        };

        let target_height = match self.network.get_highest_peer_start_height() {
            Some(peer_height) => peer_height.max(synced_tip),
            None => synced_tip.saturating_add(1_000_000),
        };
        let is_ibd = synced_tip < target_height;

        info!(
            "[START_COMPONENTS] IBD check: synced_tip={}, ibd_first_block_height={}, target_height={}, is_ibd={}",
            synced_tip, ibd_first_block_height, target_height, is_ibd
        );

        if is_ibd {
            info!("[START_COMPONENTS] Need to sync (synced_tip {} < target {}), checking for parallel IBD support...", synced_tip, target_height);

            info!(
                "[START_COMPONENTS] IBD: Found {} peer addresses: {:?}",
                peer_addresses.len(),
                peer_addresses
            );

            // Allow 1 peer when preferred_peers is set (e.g. LAN-only IBD)
            let ibd_config_owned = self.config_sub(|c| c.ibd.as_ref()).cloned();
            let min_peers = if ibd_config_owned
                .as_ref()
                .map(|c| !c.preferred_peers.is_empty())
                .unwrap_or(false)
                || std::env::var("BLVM_IBD_PEERS")
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
            {
                1
            } else {
                2
            };
            if peer_addresses.len() >= min_peers {
                info!(
                    "[START_COMPONENTS] Attempting parallel IBD with {} peers",
                    peer_addresses.len()
                );

                info!(
                    "[START_COMPONENTS] Starting parallel IBD: synced_tip={}, first_block={}, target_height={}",
                    synced_tip, ibd_first_block_height, target_height
                );

                // Attempt parallel IBD (initial_utxo_set is snapshot when AssumeUTXO loaded, else empty)
                let blockstore = Arc::clone(&self.storage.blocks());
                let storage_arc = Arc::clone(&self.storage);
                let protocol_arc = Arc::clone(&self.protocol);
                let mut utxo_set = initial_utxo_set;

                info!("[START_COMPONENTS] Calling sync_coordinator.start_parallel_ibd()...");
                match self
                    .sync_coordinator
                    .start_parallel_ibd(
                        synced_tip,
                        ibd_first_block_height,
                        target_height,
                        blockstore,
                        Some(storage_arc),
                        protocol_arc,
                        &mut utxo_set,
                        Some(Arc::clone(&self.network)),
                        peer_addresses,
                        ibd_config_owned.as_ref(),
                        self.rpc.event_publisher(),
                        Some(self.data_dir.as_path()),
                    )
                    .await
                {
                    Ok(true) => {
                        info!("[START_COMPONENTS] Parallel IBD completed successfully");
                        if let Err(e) = crate::storage::ibd_autorepair::clear_ibd_utxo_repair_flag(
                            self.data_dir.as_path(),
                        ) {
                            warn!(
                                "Failed to clear IBD UTXO autorepair marker: {} (safe to delete {} manually)",
                                e,
                                crate::storage::ibd_autorepair::repair_marker_path(self.data_dir.as_path())
                                    .display()
                            );
                        }
                        // Update current height after parallel IBD
                        let new_height = self.storage.chain().get_height()?.unwrap_or(0);
                        info!(
                            "[START_COMPONENTS] IBD completed, current height: {}",
                            new_height
                        );
                    }
                    Ok(false) => {
                        warn!("[START_COMPONENTS] Parallel IBD not available (not enough peers or already synced)");
                    }
                    Err(e) => {
                        error!("[START_COMPONENTS] Parallel IBD failed: {}. Sequential sync is not supported.", e);
                        return Err(e);
                    }
                }
            } else {
                info!("[START_COMPONENTS] Not enough peers for parallel IBD (have {}, need {}), waiting for more peers...", peer_addresses.len(), min_peers);
            }
        } else {
            info!(
                "[START_COMPONENTS] Not in IBD (synced_tip={}), skipping IBD",
                synced_tip
            );
        }

        info!("[START_COMPONENTS] Component startup complete");

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

        // Initialize module registry before borrowing module_manager (avoids overlapping mutable borrows)
        use crate::utils::env_or_default;
        let registry_cache_dir = self.data_dir.join("modules").join("registry_cache");
        let registry_cas_dir = self.data_dir.join("modules").join("registry_cas");
        let registry_mirrors = Vec::new();
        let module_registry_arc = if let Ok(mut module_registry) =
            crate::module::registry::client::ModuleRegistry::new(
                &registry_cache_dir,
                &registry_cas_dir,
                registry_mirrors.clone(),
            ) {
            module_registry.set_network_manager(Arc::clone(&self.network));
            let arc = Arc::new(module_registry);
            self.module_subsystem
                .get_or_insert_with(Default::default)
                .module_registry = Some(Arc::clone(&arc));
            self.network.set_module_registry(Arc::clone(&arc)).await;
            Some(arc)
        } else {
            None
        };

        // Start module manager if enabled
        if let Some(ref module_manager) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.module_manager.as_ref())
        {
            // Reuse the existing storage instance (Redb only allows one connection)
            let storage_arc = Arc::clone(&self.storage);

            // Get event manager from module manager
            let event_manager = Arc::clone(module_manager.lock().await.event_manager());

            // Create NodeApiImpl with all dependencies
            let mut node_api_impl = crate::module::api::node_api::NodeApiImpl::with_dependencies(
                Arc::clone(&storage_arc),
                Some(Arc::clone(&event_manager)),
                None, // module_id will be set per-module
                Some(Arc::clone(&self.mempool_manager)),
                Some(Arc::clone(&self.network)), // Now we can set it directly
            );

            // Set sync coordinator for sync status checking
            let sync_coord_arc = Arc::new(tokio::sync::Mutex::new(self.sync_coordinator.clone()));
            node_api_impl.set_sync_coordinator(sync_coord_arc);

            // Set module manager for module discovery and RPC
            node_api_impl.set_module_manager(Arc::clone(module_manager));

            // Set up module API registry and router for module-to-module communication
            let module_api_registry =
                Arc::new(crate::module::inter_module::registry::ModuleApiRegistry::new());
            let module_router = Arc::new(
                crate::module::inter_module::router::ModuleRouter::new(Arc::clone(
                    &module_api_registry,
                ))
                .with_module_manager(Arc::clone(module_manager)),
            );
            node_api_impl.set_module_api_registry(
                Arc::clone(&module_api_registry),
                Arc::clone(&module_router),
            );

            // Note: Payment state machine will be set after payment processor initialization
            // We'll update it via Arc::get_mut if possible, or store it separately

            let mut node_api = Arc::new(node_api_impl);
            let socket_path = self
                .config_sub(|c| c.modules.as_ref())
                .map(|mc| PathBuf::from(&mc.socket_dir).join("node.sock"))
                .unwrap_or_else(|| {
                    PathBuf::from(env_or_default("MODULE_SOCKET_DIR", "data/modules/socket"))
                });

            if let Some(ref module_registry_arc) = module_registry_arc {
                module_manager
                    .lock()
                    .await
                    .set_module_registry(Arc::clone(module_registry_arc));
                info!("Module registry initialized and connected to network and module manager");

                // Initialize payment processor if payment is enabled
                let payment_config = self
                    .config_sub(|c| c.payment.as_ref())
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
                            if let Some(module_config) = self.config_sub(|c| c.modules.as_ref()) {
                                let modules_dir = PathBuf::from(&module_config.modules_dir);
                                processor = processor.with_modules_dir(modules_dir);
                            }

                            let processor_arc = Arc::new(processor);

                            // Store processor in node
                            self.payment_subsystem
                                .get_or_insert_with(Default::default)
                                .payment_processor = Some(Arc::clone(&processor_arc));

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

                            self.payment_subsystem
                                .get_or_insert_with(Default::default)
                                .payment_state_machine = Some(Arc::clone(&state_machine_arc));

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
                                        let arr: [u8; 32] = bytes.try_into().ok()?;
                                        secp256k1::SecretKey::from_slice(&arr).ok()
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
                                                let mut script =
                                                    vec![blvm_consensus::opcodes::OP_0];
                                                script.extend_from_slice(&addr.witness_program);
                                                Some(script)
                                            }
                                            // Taproot v1: P2TR (32 bytes)
                                            (1, 32) => {
                                                let mut script =
                                                    vec![blvm_consensus::opcodes::OP_1];
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
                            if let Some(module_config) = self.config_sub(|c| c.modules.as_ref()) {
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

                            // Create SettlementMonitor and PaymentTxCache for reorg resilience
                            #[cfg(feature = "ctv")]
                            {
                                let tx_cache = Arc::new(crate::payment::PaymentTxCache::new());
                                let settlement_monitor = Arc::new(
                                    crate::payment::SettlementMonitor::new(Arc::clone(
                                        &state_machine_arc,
                                    ))
                                    .with_mempool_manager(Arc::clone(&self.mempool_manager))
                                    .with_storage(Arc::clone(&self.storage))
                                    .with_tx_cache(Arc::clone(&tx_cache)),
                                );
                                self.payment_subsystem
                                    .get_or_insert_with(Default::default)
                                    .payment_reorg_handler =
                                    Some(crate::payment::PaymentReorgHandler::new(
                                        Arc::clone(&state_machine_arc),
                                        Some(Arc::clone(&self.storage)),
                                        Some(Arc::clone(&self.mempool_manager)),
                                        Some(tx_cache),
                                        Some(settlement_monitor),
                                        Arc::clone(event_manager),
                                    ));
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
            }
            if module_registry_arc.is_none() {
                warn!("Failed to initialize module registry - modules will only load from local directory");
            }

            module_manager
                .lock()
                .await
                .start(&socket_path, node_api)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to start module manager: {}", e))?;

            info!("Module manager started");

            // Set node config overrides ([modules.<name>] from config.toml) before loading
            if let Some(module_config) = self.config_sub(|c| c.modules.as_ref()) {
                module_manager
                    .lock()
                    .await
                    .set_module_config_overrides(module_config.module_configs.clone());
            }

            // Set default database backend for modules (same format as node)
            let backend = self
                .config_sub(|c| c.storage.as_ref())
                .and_then(|sc| {
                    crate::storage::database::backend_from_config(sc.database_backend).ok()
                })
                .unwrap_or_else(crate::storage::database::default_backend);
            let backend_str = match backend {
                crate::storage::database::DatabaseBackend::Redb => "redb",
                crate::storage::database::DatabaseBackend::RocksDB => "rocksdb",
                crate::storage::database::DatabaseBackend::Sled => "sled",
                crate::storage::database::DatabaseBackend::TidesDB => "tidesdb",
            };
            module_manager
                .lock()
                .await
                .set_default_database_backend(backend_str.to_string());

            // Auto-discover and load modules
            if let Err(e) = module_manager.lock().await.auto_load_modules().await {
                warn!("Failed to auto-load modules: {}", e);
            }

            // Start module file watcher for hot-reload (when feature enabled and watch_enabled)
            #[cfg(feature = "module-watcher")]
            {
                let watch_enabled = self
                    .config_sub(|c| c.modules.as_ref())
                    .map_or(true, |m| m.watch_enabled);
                if watch_enabled {
                    let modules_dir = module_manager.lock().await.modules_dir().to_path_buf();
                    let watcher_config = self
                        .config_sub(|c| c.modules.as_ref())
                        .map(|m| crate::module::watcher::WatcherConfig {
                            auto_load: m.watch_auto_load,
                            auto_unload: m.watch_auto_unload,
                        })
                        .unwrap_or_default();
                    let watcher = Arc::new(crate::module::watcher::ModuleWatcher::with_config(
                        modules_dir,
                        Arc::clone(module_manager),
                        watcher_config,
                    ));
                    if let Err(e) = watcher.clone().start() {
                        warn!("Module watcher failed to start: {}", e);
                    } else {
                        info!("Module file watcher started (hot-reload on change)");
                    }
                }
            }

            // Create event publisher for this node
            // Clone event_manager Arc while lock is held so we can use it after
            let event_manager = Arc::clone(module_manager.lock().await.event_manager());

            // Initialize ZMQ publisher if configured
            // Note: ZMQ is enabled by default, but only initializes if endpoints are configured.
            // To disable: either don't configure endpoints, or build without --features zmq
            self.module_subsystem
                .get_or_insert_with(Default::default)
                .event_publisher = {
                #[cfg(feature = "zmq")]
                {
                    let zmq_publisher = if let Some(zmq_config) =
                        self.config_sub(|c| c.zmq.as_ref())
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
                        event_manager,
                        zmq_publisher,
                    )))
                }
                #[cfg(not(feature = "zmq"))]
                {
                    Some(Arc::new(EventPublisher::new(event_manager)))
                }
            };
            info!("Event publisher initialized");

            // Set EventPublisher on MempoolManager for mempool event publishing
            if let Some(ref event_publisher) = self
                .module_subsystem
                .as_ref()
                .and_then(|s| s.event_publisher.as_ref())
            {
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
                    #[cfg(feature = "governance")]
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
                        .map(|s| format!("{{\"config\":{s}}}"));

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
        let target_peer_count = timing_config.target_outbound_peers;

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
        run_loop::run(self).await
    }

    /// Run node processing once (for testing)
    pub async fn run_once(&mut self) -> Result<()> {
        info!("Running node processing once");

        // Check node health
        self.check_health().await?;

        Ok(())
    }

    /// Storage timeout from config or default
    fn storage_timeout(&self) -> std::time::Duration {
        crate::utils::storage_timeout_from_config(self.config_sub(|c| c.request_timeouts.as_ref()))
    }

    /// Check node health with graceful error handling
    async fn check_health(&self) -> Result<()> {
        // Check peer count (non-blocking, always succeeds)
        let peer_count = self.network.peer_count();
        if peer_count == 0 {
            warn!("No peers connected");
        }

        // Check storage with timeout and graceful degradation
        let timeout_dur = self.storage_timeout();
        use crate::utils::with_custom_timeout;
        match with_custom_timeout(async { self.storage.blocks().block_count() }, timeout_dur).await
        {
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

    /// Get disk space (total, available, percent_free) for the path's mount.
    /// Uses sysinfo when available; fallback to placeholder otherwise.
    fn get_disk_space_for_path(path: &Path) -> (u64, u64, f64) {
        #[cfg(feature = "sysinfo")]
        {
            use sysinfo::Disks;
            let canonical = match std::fs::canonicalize(path) {
                Ok(p) => p,
                Err(_) => path.to_path_buf(),
            };
            let disks = Disks::new_with_refreshed_list();
            // Find disk whose mount point is the longest prefix of canonical path
            let mut best: Option<&sysinfo::Disk> = None;
            let mut best_len = 0usize;
            for disk in disks.list() {
                let mount = disk.mount_point();
                if canonical.starts_with(mount) {
                    let mount_len = mount.as_os_str().len();
                    if mount_len > best_len {
                        best_len = mount_len;
                        best = Some(disk);
                    }
                }
            }
            if let Some(disk) = best {
                let total = disk.total_space();
                let available = disk.available_space();
                let percent_free = if total > 0 {
                    100.0 * (available as f64) / (total as f64)
                } else {
                    0.0
                };
                return (total, available, percent_free);
            }
        }
        // Fallback when sysinfo unavailable or path not found on any disk
        let _ = path;
        let total_bytes = 1_000_000_000_000u64; // 1TB placeholder
        let available_bytes = total_bytes / 5; // 20% free
        let percent_free = 20.0;
        (total_bytes, available_bytes, percent_free)
    }

    /// Check disk space and trigger pruning if needed
    async fn check_disk_space(&self) -> Result<()> {
        // Check storage bounds (80% threshold)
        let within_bounds = self.storage.check_storage_bounds()?;

        if !within_bounds {
            warn!("Storage bounds exceeded - disk space may be low");

            // Publish DiskSpaceLow event to modules
            if let Some(ref event_publisher) = self
                .module_subsystem
                .as_ref()
                .and_then(|s| s.event_publisher.as_ref())
            {
                let (total_bytes, available_bytes, percent_free) =
                    Self::get_disk_space_for_path(&self.data_dir);
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
        info!("Stopping node");

        // Publish NodeShutdown event to modules (give them time to clean up)
        if let Some(ref event_publisher) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.event_publisher.as_ref())
        {
            event_publisher
                .publish_node_shutdown("graceful".to_string(), 30)
                .await;
            // Give modules a moment to process shutdown event
            tokio::time::sleep(HANDSHAKE_POLL_SLEEP).await;
        }

        // Publish DataMaintenance event to modules (high urgency flush for shutdown)
        if let Some(ref event_publisher) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.event_publisher.as_ref())
        {
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
            tokio::time::sleep(HANDSHAKE_POLL_SLEEP).await;
        }

        // Stop module manager
        if let Some(ref module_manager) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.module_manager.as_ref())
        {
            module_manager
                .lock()
                .await
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
        if let Some(ref event_publisher) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.event_publisher.as_ref())
        {
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

    /// Get module manager (shared via Arc<Mutex<>> for RPC and other subsystems)
    pub fn module_manager(&self) -> Option<Arc<tokio::sync::Mutex<ModuleManager>>> {
        self.module_subsystem
            .as_ref()
            .and_then(|s| s.module_manager.as_ref())
            .cloned()
    }

    /// Get event publisher (immutable)
    pub fn event_publisher(&self) -> Option<&EventPublisher> {
        self.module_subsystem
            .as_ref()
            .and_then(|s| s.event_publisher.as_ref())
            .map(|arc| arc.as_ref())
    }

    /// Get event publisher (mutable)
    /// Note: This returns a reference to the Arc, not the inner EventPublisher
    /// since Arc doesn't allow mutable access to the inner value
    pub fn event_publisher_arc(&self) -> Option<&Arc<EventPublisher>> {
        self.module_subsystem
            .as_ref()
            .and_then(|s| s.event_publisher.as_ref())
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
        &self.network // Deref Arc to maintain API compatibility
    }

    /// Get RPC manager
    pub fn rpc(&self) -> &RpcManager {
        &self.rpc
    }

    /// Get health report
    pub fn health_check(&self) -> health::HealthReport {
        use crate::node::health::{HealthChecker, HealthStatus};

        let checker = HealthChecker::new();
        let network_healthy = self.network.is_network_active();
        let storage_healthy = self.storage.check_storage_bounds().unwrap_or(false);
        let rpc_healthy = true; // RPC is always healthy if node is running

        // Get metrics if available (simplified for now)
        let report = checker.check_health(
            network_healthy,
            storage_healthy,
            rpc_healthy,
            None, // Network metrics - would need to be passed from NetworkManager
            None, // Storage metrics - would need to be collected
        );

        // Publish HealthCheck event for module subscribers
        if let Some(ref ep) = self
            .module_subsystem
            .as_ref()
            .and_then(|s| s.event_publisher.as_ref())
        {
            let check_type = "on_demand".to_string();
            let node_healthy = report.overall_status == HealthStatus::Healthy;
            let health_report = serde_json::to_string(&report).ok();
            let ep_clone = Arc::clone(ep);
            let _ = tokio::spawn(async move {
                ep_clone
                    .publish_health_check(check_type, node_healthy, health_report)
                    .await;
            });
        }

        report
    }
}

//! RPC interface for blvm-node
//!
//! This module provides JSON-RPC server, blockchain query methods,
//! network info methods, transaction submission, and mining methods.

pub mod auth;
pub mod blockchain;
pub mod cache;
pub mod control;
pub mod errors;
pub mod mempool;
pub mod methods;
pub mod params;
pub mod mining;
#[cfg(feature = "miniscript")]
pub mod miniscript;
pub mod network;
#[cfg(feature = "bip70-http")]
pub mod payment;
pub mod rawtx;
#[cfg(feature = "rest-api")]
pub mod rest;
pub mod server;
pub mod types;
pub mod validation;

#[cfg(feature = "quinn")]
pub mod quinn_server;

use crate::config::{RequestTimeoutConfig, RpcAuthConfig};
use crate::network::dos_protection::ConnectionRateLimiter;
use crate::utils::AUTH_RATE_LIMITER_CLEANUP_INTERVAL;
use crate::module::manager::ModuleManager;
use crate::node::mempool::MempoolManager;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::PerformanceProfiler;
use crate::storage::Storage;
use anyhow::Result;
use blvm_protocol::BitcoinProtocolEngine;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

#[cfg(feature = "rest-api")]
use crate::rpc::rest::RestApiServer;

/// RPC manager that coordinates all RPC operations
///
/// Supports both TCP (default, standard compatible) and optional QUIC transport.
pub struct RpcManager {
    server_addr: SocketAddr,
    quinn_addr: Option<SocketAddr>,
    #[cfg(feature = "rest-api")]
    rest_api_addr: Option<SocketAddr>,
    blockchain_rpc: blockchain::BlockchainRpc,
    network_rpc: network::NetworkRpc,
    mining_rpc: mining::MiningRpc,
    control_rpc: control::ControlRpc,
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
    network_manager: Option<Arc<crate::network::NetworkManager>>,
    shutdown_tx: Option<mpsc::UnboundedSender<()>>,
    #[cfg(feature = "quinn")]
    quinn_shutdown_tx: Option<mpsc::UnboundedSender<()>>,
    #[cfg(feature = "rest-api")]
    rest_api_shutdown_tx: Option<mpsc::UnboundedSender<()>>,
    /// RPC authentication manager (optional)
    auth_manager: Option<Arc<auth::RpcAuthManager>>,
    /// Node shutdown callback (optional)
    node_shutdown: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
    /// Metrics collector (optional)
    metrics: Option<Arc<MetricsCollector>>,
    /// Performance profiler (optional)
    profiler: Option<Arc<PerformanceProfiler>>,
    /// Payment processor for BIP70 HTTP endpoints
    #[cfg(feature = "bip70-http")]
    payment_processor: Option<Arc<crate::payment::processor::PaymentProcessor>>,
    /// Payment state machine for CTV payment endpoints
    #[cfg(all(feature = "bip70-http", feature = "ctv"))]
    payment_state_machine: Option<Arc<crate::payment::state_machine::PaymentStateMachine>>,
    /// Event publisher for module event notifications (optional)
    event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    /// Maximum request body size in bytes (default: 1MB)
    max_request_size_bytes: usize,
    /// Max connections per IP per minute (default: 10)
    max_connections_per_ip_per_minute: u32,
    /// Batch rate multiplier cap: min(batch_len, this) (default: 10)
    batch_rate_multiplier_cap: u32,
    /// Connection rate limit window in seconds (default: 60)
    connection_rate_limit_window_seconds: u64,
    /// Request timeout config (storage/network/rpc timeouts)
    request_timeouts: Option<RequestTimeoutConfig>,
    /// Module manager for load/unload/reload RPC (optional)
    module_manager: Option<Arc<tokio::sync::Mutex<ModuleManager>>>,
    /// Protocol engine for mining RPC (`generatetoaddress` on regtest).
    protocol_engine: Option<Arc<BitcoinProtocolEngine>>,
}

impl RpcManager {
    /// Create a new RPC manager with TCP only (standard compatible)
    pub fn new(server_addr: SocketAddr) -> Self {
        Self {
            server_addr,
            quinn_addr: None,
            #[cfg(feature = "rest-api")]
            rest_api_addr: None,
            blockchain_rpc: blockchain::BlockchainRpc::new(),
            network_rpc: network::NetworkRpc::new(),
            mining_rpc: mining::MiningRpc::new(),
            control_rpc: control::ControlRpc::new(),
            storage: None,
            metrics: None,
            profiler: None,
            mempool: None,
            network_manager: None,
            shutdown_tx: None,
            #[cfg(feature = "quinn")]
            quinn_shutdown_tx: None,
            #[cfg(feature = "rest-api")]
            rest_api_shutdown_tx: None,
            auth_manager: None,
            node_shutdown: None,
            #[cfg(feature = "bip70-http")]
            payment_processor: None,
            #[cfg(all(feature = "bip70-http", feature = "ctv"))]
            payment_state_machine: None,
            event_publisher: None,
            max_request_size_bytes: 1_048_576,
            max_connections_per_ip_per_minute: 10,
            batch_rate_multiplier_cap: 10,
            connection_rate_limit_window_seconds: 60,
            request_timeouts: None,
            module_manager: None,
            protocol_engine: None,
        }
    }

    /// Set batch rate multiplier cap (from config or BLVM_RPC_BATCH_RATE_MULTIPLIER_CAP)
    pub fn with_batch_rate_multiplier_cap(mut self, cap: u32) -> Self {
        self.batch_rate_multiplier_cap = cap;
        self
    }

    /// Set connection rate limit window in seconds (from config or BLVM_RPC_CONNECTION_RATE_LIMIT_WINDOW_SECS)
    pub fn with_connection_rate_limit_window(mut self, secs: u64) -> Self {
        self.connection_rate_limit_window_seconds = secs;
        self
    }

    /// Set max connections per IP per minute (from config rpc.max_connections_per_ip_per_minute)
    pub fn with_max_connections_per_ip(mut self, n: u32) -> Self {
        self.max_connections_per_ip_per_minute = n;
        self
    }

    /// Set module manager for load/unload/reload RPC methods
    pub fn set_module_manager(
        &mut self,
        module_manager: Arc<tokio::sync::Mutex<ModuleManager>>,
    ) {
        self.module_manager = Some(module_manager);
    }

    /// Set request timeout config (storage/network/rpc timeouts from config)
    pub fn with_request_timeouts(mut self, config: Option<RequestTimeoutConfig>) -> Self {
        self.request_timeouts = config;
        self
    }

    /// Set maximum request body size (from config rpc.max_request_size_bytes)
    pub fn with_max_request_size(mut self, bytes: usize) -> Self {
        self.max_request_size_bytes = bytes;
        self
    }

    /// Set node shutdown callback
    pub fn with_node_shutdown(
        mut self,
        shutdown_fn: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Self {
        self.node_shutdown = Some(shutdown_fn);
        self
    }

    /// Set RPC authentication configuration
    pub async fn with_auth_config(&mut self, auth_config: RpcAuthConfig) {
        let auth_manager = Arc::new(auth::RpcAuthManager::with_rate_limits(
            auth_config.required,
            auth_config.rate_limit_burst,
            auth_config.rate_limit_rate,
        ));

        // Load tokens from environment variable, token file, or config file
        match auth_config.load_tokens() {
            Ok(tokens) => {
                if !tokens.is_empty() {
                    info!(
                        "Loaded {} RPC auth token(s) from secure source",
                        tokens.len()
                    );
                }
                // Add tokens to auth manager
                for token in tokens {
                    if let Err(e) = auth_manager.add_token(token.clone()).await {
                        // Redact token from error message
                        let redacted_error =
                            auth::redact_tokens_from_log(&format!("{e}"), &[token]);
                        error!("Failed to add RPC auth token: {}", redacted_error);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to load RPC auth tokens: {}. Using tokens from config file if available.", e);
                // Fall back to config file tokens
                for token in auth_config.tokens {
                    if let Err(e) = auth_manager.add_token(token.clone()).await {
                        // Redact token from error message
                        let redacted_error =
                            auth::redact_tokens_from_log(&format!("{e}"), &[token]);
                        error!("Failed to add RPC auth token: {}", redacted_error);
                    }
                }
            }
        }

        // Add certificates to auth manager
        for cert in auth_config.certificates {
            if let Err(e) = auth_manager.add_certificate(cert).await {
                error!("Failed to add RPC auth certificate: {}", e);
            }
        }

        self.auth_manager = Some(auth_manager);
    }

    /// Set storage and mempool dependencies for RPC handlers
    pub fn with_dependencies(
        mut self,
        storage: Arc<Storage>,
        mempool: Arc<MempoolManager>,
    ) -> Self {
        // Update all RPC handlers with dependencies
        self.mining_rpc =
            mining::MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool));
        self.blockchain_rpc = blockchain::BlockchainRpc::with_dependencies(Arc::clone(&storage));
        // Note: mempool_rpc is created later in with_dependencies_auth_and_metrics if needed
        // This early creation was unused - removed to avoid warning
        let _rawtx_rpc = rawtx::RawTxRpc::with_dependencies(
            Arc::clone(&storage),
            Arc::clone(&mempool),
            self.metrics.clone(),
            self.profiler.clone(),
        );

        self.mempool = Some(Arc::clone(&mempool));

        self.storage = Some(storage);
        self.mempool = Some(mempool);
        self
    }

    /// Set metrics collector
    pub fn with_metrics(mut self, metrics: Arc<MetricsCollector>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Set performance profiler
    pub fn with_profiler(mut self, profiler: Arc<PerformanceProfiler>) -> Self {
        self.profiler = Some(profiler);
        self
    }

    /// Set event publisher for module event notifications
    pub fn with_event_publisher(
        mut self,
        event_publisher: Arc<crate::node::event_publisher::EventPublisher>,
    ) -> Self {
        self.event_publisher = Some(event_publisher);
        self
    }

    /// Set event publisher after construction (e.g. when created during startup)
    pub fn set_event_publisher(
        &mut self,
        event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    ) {
        self.event_publisher = event_publisher;
    }

    /// Get event publisher for IBD/sync event emission
    pub fn event_publisher(&self) -> Option<Arc<crate::node::event_publisher::EventPublisher>> {
        self.event_publisher.clone()
    }

    /// Set network manager dependency
    pub fn with_network_manager(
        mut self,
        network_manager: Arc<crate::network::NetworkManager>,
    ) -> Self {
        self.network_rpc = network::NetworkRpc::with_dependencies(Arc::clone(&network_manager));
        self.network_manager = Some(network_manager);
        self
    }

    /// Set protocol engine (enables `generatetoaddress` on regtest).
    pub fn with_protocol_engine(mut self, engine: Arc<BitcoinProtocolEngine>) -> Self {
        self.protocol_engine = Some(engine);
        self
    }

    /// Set payment processor for BIP70 HTTP endpoints
    #[cfg(feature = "bip70-http")]
    pub fn with_payment_processor(
        mut self,
        processor: Arc<crate::payment::processor::PaymentProcessor>,
    ) -> Self {
        self.payment_processor = Some(processor);
        self
    }

    /// Set payment state machine for CTV payment endpoints
    #[cfg(all(feature = "bip70-http", feature = "ctv"))]
    pub fn with_payment_state_machine(
        mut self,
        state_machine: Arc<crate::payment::state_machine::PaymentStateMachine>,
    ) -> Self {
        self.payment_state_machine = Some(state_machine);
        self
    }

    /// Create a new RPC manager with both TCP and QUIC transports
    #[cfg(feature = "quinn")]
    pub fn with_quinn(tcp_addr: SocketAddr, quinn_addr: SocketAddr) -> Self {
        Self {
            server_addr: tcp_addr,
            quinn_addr: Some(quinn_addr),
            #[cfg(feature = "rest-api")]
            rest_api_addr: None,
            #[cfg(feature = "rest-api")]
            rest_api_shutdown_tx: None,
            blockchain_rpc: blockchain::BlockchainRpc::new(),
            network_rpc: network::NetworkRpc::new(),
            mining_rpc: mining::MiningRpc::new(),
            metrics: None,
            profiler: None,
            control_rpc: control::ControlRpc::new(),
            storage: None,
            mempool: None,
            network_manager: None,
            shutdown_tx: None,
            quinn_shutdown_tx: None,
            auth_manager: None,
            node_shutdown: None,
            #[cfg(feature = "bip70-http")]
            payment_processor: None,
            #[cfg(all(feature = "bip70-http", feature = "ctv"))]
            payment_state_machine: None,
            event_publisher: None,
            max_request_size_bytes: 1_048_576,
            max_connections_per_ip_per_minute: 10,
            batch_rate_multiplier_cap: 10,
            connection_rate_limit_window_seconds: 60,
            request_timeouts: None,
            module_manager: None,
            protocol_engine: None,
        }
    }

    /// Enable QUIC RPC server on specified address
    #[cfg(feature = "quinn")]
    pub fn enable_quinn(&mut self, quinn_addr: SocketAddr) {
        self.quinn_addr = Some(quinn_addr);
    }

    /// Enable REST API server on specified address
    #[cfg(feature = "rest-api")]
    pub fn enable_rest_api(&mut self, rest_api_addr: SocketAddr) {
        self.rest_api_addr = Some(rest_api_addr);
    }

    /// Get the RPC server address
    pub fn rpc_addr(&self) -> SocketAddr {
        self.server_addr
    }

    /// Get the REST API server address (if enabled)
    #[cfg(feature = "rest-api")]
    pub fn rest_api_addr(&self) -> Option<SocketAddr> {
        self.rest_api_addr
    }

    /// Start the RPC server(s)
    ///
    /// Starts TCP server (always) and optionally QUIC server if enabled
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting TCP RPC server on {}", self.server_addr);

        let connection_limiter = Arc::new(tokio::sync::Mutex::new(ConnectionRateLimiter::new(
            self.max_connections_per_ip_per_minute as usize,
            self.connection_rate_limit_window_seconds,
        )));

        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        self.shutdown_tx = Some(shutdown_tx.clone());

        // Create control RPC with shutdown capability and optional module manager
        let mut control_rpc = control::ControlRpc::with_shutdown(
            shutdown_tx.clone(),
            self.node_shutdown.clone(),
        );
        if let Some(ref mgr) = self.module_manager {
            control_rpc = control_rpc.with_module_manager(Arc::clone(mgr));
        }
        let control_rpc = Arc::new(control_rpc);

        // Create server with or without authentication
        let server = if let (Some(storage), Some(mempool)) =
            (self.storage.as_ref(), self.mempool.as_ref())
        {
            let blockchain = Arc::new(
                blockchain::BlockchainRpc::with_dependencies(Arc::clone(storage))
                    .with_event_publisher(self.event_publisher.clone())
                    .with_request_timeouts(self.request_timeouts.clone()),
            );
            let mempool_rpc = Arc::new(mempool::MempoolRpc::with_dependencies(
                Arc::clone(mempool),
                Arc::clone(storage),
            ));
            let rawtx_rpc = Arc::new(
                rawtx::RawTxRpc::with_dependencies(
                    Arc::clone(storage),
                    Arc::clone(mempool),
                    self.metrics.clone(),
                    self.profiler.clone(),
                )
                .with_request_timeouts(self.request_timeouts.clone()),
            );
            let mining = Arc::new({
                let mut m = mining::MiningRpc::with_dependencies(
                    Arc::clone(storage),
                    Arc::clone(mempool),
                )
                .with_event_publisher(self.event_publisher.clone());
                if let Some(ref pe) = self.protocol_engine {
                    m = m.with_protocol_engine(Arc::clone(pe));
                }
                match &self.network_manager {
                    Some(nm) => m.with_network_manager(Some(Arc::clone(nm))),
                    None => m,
                }
            });
            let network = if let Some(ref network_manager) = self.network_manager {
                Arc::new(network::NetworkRpc::with_dependencies(Arc::clone(
                    network_manager,
                )))
            } else {
                Arc::new(network::NetworkRpc::new())
            };

            // Use auth manager and/or metrics if configured
            let max_request_size = self.max_request_size_bytes;
            let add_limiter = |s: server::RpcServer| {
                s.with_connection_limiter(Arc::clone(&connection_limiter))
                    .with_batch_rate_multiplier_cap(self.batch_rate_multiplier_cap)
            };
            match (self.auth_manager.as_ref(), self.metrics.as_ref()) {
                (Some(auth_manager), Some(metrics)) => add_limiter(
                    server::RpcServer::with_dependencies_auth_and_metrics(
                        self.server_addr,
                        blockchain,
                        network,
                        mempool_rpc,
                        mining,
                        rawtx_rpc,
                        Arc::clone(&control_rpc),
                        Arc::clone(auth_manager),
                        Arc::clone(metrics),
                        max_request_size,
                    ),
                ),
                (Some(auth_manager), None) => add_limiter(server::RpcServer::with_dependencies_and_auth(
                    self.server_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                    Arc::clone(&control_rpc),
                    Arc::clone(auth_manager),
                    max_request_size,
                )),
                (None, Some(metrics)) => add_limiter(server::RpcServer::with_dependencies_and_metrics(
                    self.server_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                    Arc::clone(&control_rpc),
                    Arc::clone(metrics),
                    max_request_size,
                )),
                (None, None) => add_limiter(server::RpcServer::with_dependencies(
                    self.server_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                    Arc::clone(&control_rpc),
                    max_request_size,
                )),
            }
        } else {
            // No dependencies - use auth and/or metrics if configured
            let connection_limiter = Arc::new(tokio::sync::Mutex::new(ConnectionRateLimiter::new(
                self.max_connections_per_ip_per_minute as usize,
                self.connection_rate_limit_window_seconds,
            )));
            if let Some(ref auth_manager) = self.auth_manager {
                server::RpcServer::with_auth(self.server_addr, Arc::clone(auth_manager))
                    .with_connection_limiter(connection_limiter)
                    .with_batch_rate_multiplier_cap(self.batch_rate_multiplier_cap)
            } else {
                server::RpcServer::new(self.server_addr)
                    .with_connection_limiter(connection_limiter)
                    .with_batch_rate_multiplier_cap(self.batch_rate_multiplier_cap)
            }
        };

        // Start TCP server in a background task
        let tcp_handle = tokio::spawn(async move {
            if let Err(e) = server.start().await {
                error!("TCP RPC server error: {}", e);
            }
        });

        // Start periodic rate limiter cleanup when auth is enabled
        if let Some(ref auth_manager) = self.auth_manager {
            let auth_manager = Arc::clone(auth_manager);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(AUTH_RATE_LIMITER_CLEANUP_INTERVAL);
                interval.tick().await; // skip immediate first tick
                loop {
                    interval.tick().await;
                    auth_manager.cleanup_stale_limiters().await;
                }
            });
        }

        // Start QUIC server if enabled
        #[cfg(feature = "quinn")]
        let quinn_handle = if let Some(quinn_addr) = self.quinn_addr {
            info!("Starting QUIC RPC server on {}", quinn_addr);

            let (quinn_shutdown_tx, mut quinn_shutdown_rx) = mpsc::unbounded_channel();
            self.quinn_shutdown_tx = Some(quinn_shutdown_tx);

            let mut quinn_server = quinn_server::QuinnRpcServer::new(quinn_addr)
                .with_max_request_size(self.max_request_size_bytes)
                .with_batch_rate_multiplier_cap(self.batch_rate_multiplier_cap);
            if let Some(ref auth_manager) = self.auth_manager {
                quinn_server = quinn_server.with_auth_manager(Arc::clone(auth_manager));
            }

            Some(tokio::spawn(async move {
                tokio::select! {
                    result = quinn_server.start() => {
                        if let Err(e) = result {
                            error!("QUIC RPC server error: {}", e);
                        }
                    }
                    _ = quinn_shutdown_rx.recv() => {
                        info!("QUIC RPC server shutdown requested");
                    }
                }
            }))
        } else {
            None
        };

        // Start REST API server if enabled
        #[cfg(feature = "rest-api")]
        let rest_api_handle = if let Some(rest_api_addr) = self.rest_api_addr {
            if let (Some(storage), Some(mempool)) = (self.storage.as_ref(), self.mempool.as_ref()) {
                info!("Starting REST API server on {}", rest_api_addr);

                let (rest_api_shutdown_tx, mut rest_api_shutdown_rx) = mpsc::unbounded_channel();
                self.rest_api_shutdown_tx = Some(rest_api_shutdown_tx);

                let blockchain = Arc::new(
                    blockchain::BlockchainRpc::with_dependencies(Arc::clone(storage))
                        .with_event_publisher(self.event_publisher.clone()),
                );
                let mempool_rpc = Arc::new(mempool::MempoolRpc::with_dependencies(
                    Arc::clone(mempool),
                    Arc::clone(storage),
                ));
                let rawtx_rpc = Arc::new(rawtx::RawTxRpc::with_dependencies(
                    Arc::clone(storage),
                    Arc::clone(mempool),
                    None,
                    None,
                ));
                let mining = Arc::new({
                    let mut m = mining::MiningRpc::with_dependencies(
                        Arc::clone(storage),
                        Arc::clone(mempool),
                    )
                    .with_event_publisher(self.event_publisher.clone());
                    if let Some(ref pe) = self.protocol_engine {
                        m = m.with_protocol_engine(Arc::clone(pe));
                    }
                    match &self.network_manager {
                        Some(nm) => m.with_network_manager(Some(Arc::clone(nm))),
                        None => m,
                    }
                });
                let network = if let Some(ref network_manager) = self.network_manager {
                    Arc::new(network::NetworkRpc::with_dependencies(Arc::clone(
                        network_manager,
                    )))
                } else {
                    Arc::new(network::NetworkRpc::new())
                };

                // Create REST API server with authentication if available
                let mut rest_server = if let Some(ref auth_manager) = self.auth_manager {
                    RestApiServer::with_auth(
                        rest_api_addr,
                        blockchain,
                        network,
                        mempool_rpc,
                        mining,
                        rawtx_rpc,
                        Arc::clone(auth_manager),
                    )
                } else {
                    RestApiServer::new(
                        rest_api_addr,
                        blockchain,
                        network,
                        mempool_rpc,
                        mining,
                        rawtx_rpc,
                    )
                };
                rest_server = rest_server.with_connection_limiter(Arc::clone(&connection_limiter));

                // Set payment processor if available
                #[cfg(feature = "bip70-http")]
                if let Some(ref processor) = self.payment_processor {
                    rest_server = rest_server.with_payment_processor(Arc::clone(processor));
                }
                #[cfg(all(feature = "bip70-http", feature = "ctv"))]
                if let Some(ref state_machine) = self.payment_state_machine {
                    rest_server = rest_server.with_payment_state_machine(Arc::clone(state_machine));
                }

                Some(tokio::spawn(async move {
                    tokio::select! {
                        result = rest_server.start() => {
                            if let Err(e) = result {
                                error!("REST API server error: {}", e);
                            }
                        }
                        _ = rest_api_shutdown_rx.recv() => {
                            info!("REST API server shutdown requested");
                        }
                    }
                }))
            } else {
                warn!("REST API requested but storage/mempool not available");
                None
            }
        } else {
            None
        };

        // Don't wait for shutdown - return immediately after spawning servers
        // Shutdown is handled via the shutdown channels stored in self.shutdown_tx
        // The spawned tasks will continue running in the background
        // The stop() method sends shutdown signals via those channels
        Ok(())
    }

    /// Stop the RPC server(s)
    pub fn stop(&self) -> Result<()> {
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(());
        }

        #[cfg(feature = "quinn")]
        if let Some(tx) = &self.quinn_shutdown_tx {
            let _ = tx.send(());
        }

        #[cfg(feature = "rest-api")]
        if let Some(tx) = &self.rest_api_shutdown_tx {
            let _ = tx.send(());
        }

        Ok(())
    }

    /// Get blockchain RPC methods
    pub fn blockchain(&self) -> &blockchain::BlockchainRpc {
        &self.blockchain_rpc
    }

    /// Get network RPC methods
    pub fn network(&self) -> &network::NetworkRpc {
        &self.network_rpc
    }

    /// Get mining RPC methods
    pub fn mining(&self) -> &mining::MiningRpc {
        &self.mining_rpc
    }
}

//! RPC interface for reference-node
//!
//! This module provides JSON-RPC server, blockchain query methods,
//! network info methods, transaction submission, and mining methods.

pub mod auth;
pub mod blockchain;
pub mod control;
pub mod errors;
pub mod mempool;
pub mod mining;
pub mod network;
pub mod rawtx;
#[cfg(feature = "rest-api")]
pub mod rest;
#[cfg(kani)]
pub mod rpc_proofs;
pub mod server;
pub mod types;
pub mod validation;

#[cfg(feature = "quinn")]
pub mod quinn_server;

use crate::config::RpcAuthConfig;
use crate::node::mempool::MempoolManager;
use crate::node::metrics::MetricsCollector;
use crate::node::performance::PerformanceProfiler;
use crate::storage::Storage;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
#[cfg(feature = "rest-api")]
use tracing::warn;
use tracing::{error, info};

#[cfg(feature = "rest-api")]
use crate::rpc::rest::RestApiServer;

/// RPC manager that coordinates all RPC operations
///
/// Supports both TCP (default, Bitcoin Core compatible) and optional QUIC transport.
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
}

impl RpcManager {
    /// Create a new RPC manager with TCP only (Bitcoin Core compatible)
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
        }
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
    pub async fn with_auth_config(mut self, auth_config: RpcAuthConfig) -> Self {
        let auth_manager = Arc::new(auth::RpcAuthManager::with_rate_limits(
            auth_config.required,
            auth_config.rate_limit_burst,
            auth_config.rate_limit_rate,
        ));

        // Add tokens and certificates to auth manager (synchronously)
        for token in auth_config.tokens {
            if let Err(e) = auth_manager.add_token(token).await {
                error!("Failed to add RPC auth token: {}", e);
            }
        }
        for cert in auth_config.certificates {
            if let Err(e) = auth_manager.add_certificate(cert).await {
                error!("Failed to add RPC auth certificate: {}", e);
            }
        }

        self.auth_manager = Some(auth_manager);
        self
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

    /// Set network manager dependency
    pub fn with_network_manager(
        mut self,
        network_manager: Arc<crate::network::NetworkManager>,
    ) -> Self {
        self.network_rpc = network::NetworkRpc::with_dependencies(Arc::clone(&network_manager));
        self.network_manager = Some(network_manager);
        self
    }

    /// Create a new RPC manager with both TCP and QUIC transports
    #[cfg(feature = "quinn")]
    pub fn with_quinn(tcp_addr: SocketAddr, quinn_addr: SocketAddr) -> Self {
        Self {
            server_addr: tcp_addr,
            quinn_addr: Some(quinn_addr),
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

    /// Start the RPC server(s)
    ///
    /// Starts TCP server (always) and optionally QUIC server if enabled
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting TCP RPC server on {}", self.server_addr);

        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        self.shutdown_tx = Some(shutdown_tx.clone());

        // Create control RPC with shutdown capability
        let control_rpc = Arc::new(control::ControlRpc::with_shutdown(
            shutdown_tx.clone(),
            self.node_shutdown.clone(),
        ));

        // Create server with or without authentication
        let server = if let (Some(storage), Some(mempool)) =
            (self.storage.as_ref(), self.mempool.as_ref())
        {
            let blockchain = Arc::new(blockchain::BlockchainRpc::with_dependencies(Arc::clone(
                storage,
            )));
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
            let mining = Arc::new(mining::MiningRpc::with_dependencies(
                Arc::clone(storage),
                Arc::clone(mempool),
            ));
            let network = if let Some(ref network_manager) = self.network_manager {
                Arc::new(network::NetworkRpc::with_dependencies(Arc::clone(
                    network_manager,
                )))
            } else {
                Arc::new(network::NetworkRpc::new())
            };

            // Use auth manager and/or metrics if configured
            match (self.auth_manager.as_ref(), self.metrics.as_ref()) {
                (Some(auth_manager), Some(metrics)) => {
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
                    )
                }
                (Some(auth_manager), None) => server::RpcServer::with_dependencies_and_auth(
                    self.server_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                    Arc::clone(&control_rpc),
                    Arc::clone(auth_manager),
                ),
                (None, Some(metrics)) => server::RpcServer::with_dependencies_and_metrics(
                    self.server_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                    Arc::clone(&control_rpc),
                    Arc::clone(metrics),
                ),
                (None, None) => server::RpcServer::with_dependencies(
                    self.server_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                    Arc::clone(&control_rpc),
                ),
            }
        } else {
            // No dependencies - use auth and/or metrics if configured
            // Note: Without dependencies, metrics won't be very useful, but we support it
            if let Some(ref auth_manager) = self.auth_manager {
                server::RpcServer::with_auth(self.server_addr, Arc::clone(auth_manager))
            } else {
                server::RpcServer::new(self.server_addr)
            }
        };

        // Start TCP server in a background task
        let tcp_handle = tokio::spawn(async move {
            if let Err(e) = server.start().await {
                error!("TCP RPC server error: {}", e);
            }
        });

        // Start QUIC server if enabled
        #[cfg(feature = "quinn")]
        let quinn_handle = if let Some(quinn_addr) = self.quinn_addr {
            info!("Starting QUIC RPC server on {}", quinn_addr);

            let (quinn_shutdown_tx, mut quinn_shutdown_rx) = mpsc::unbounded_channel();
            self.quinn_shutdown_tx = Some(quinn_shutdown_tx);

            let quinn_server = quinn_server::QuinnRpcServer::new(quinn_addr);

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

                let blockchain = Arc::new(blockchain::BlockchainRpc::with_dependencies(Arc::clone(
                    storage,
                )));
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
                let mining = Arc::new(mining::MiningRpc::with_dependencies(
                    Arc::clone(storage),
                    Arc::clone(mempool),
                ));
                let network = if let Some(ref network_manager) = self.network_manager {
                    Arc::new(network::NetworkRpc::with_dependencies(Arc::clone(
                        network_manager,
                    )))
                } else {
                    Arc::new(network::NetworkRpc::new())
                };

                let rest_server = RestApiServer::new(
                    rest_api_addr,
                    blockchain,
                    network,
                    mempool_rpc,
                    mining,
                    rawtx_rpc,
                );

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

        // Wait for shutdown signal
        shutdown_rx.recv().await;

        // Shutdown TCP server
        tcp_handle.abort();
        info!("TCP RPC server stopped");

        // Shutdown QUIC server if it was started
        #[cfg(feature = "quinn")]
        if let Some(handle) = quinn_handle {
            handle.abort();
            info!("QUIC RPC server stopped");
        }

        // Shutdown REST API server if it was started
        #[cfg(feature = "rest-api")]
        if let Some(handle) = rest_api_handle {
            handle.abort();
            info!("REST API server stopped");
        }

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

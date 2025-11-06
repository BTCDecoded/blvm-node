//! RPC interface for reference-node
//!
//! This module provides JSON-RPC server, blockchain query methods,
//! network info methods, transaction submission, and mining methods.

pub mod blockchain;
pub mod errors;
pub mod mempool;
pub mod mining;
pub mod network;
pub mod rawtx;
pub mod server;
pub mod types;

#[cfg(feature = "quinn")]
pub mod quinn_server;

use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

/// RPC manager that coordinates all RPC operations
///
/// Supports both TCP (default, Bitcoin Core compatible) and optional QUIC transport.
pub struct RpcManager {
    server_addr: SocketAddr,
    quinn_addr: Option<SocketAddr>,
    blockchain_rpc: blockchain::BlockchainRpc,
    network_rpc: network::NetworkRpc,
    mining_rpc: mining::MiningRpc,
    storage: Option<Arc<Storage>>,
    mempool: Option<Arc<MempoolManager>>,
    shutdown_tx: Option<mpsc::UnboundedSender<()>>,
    #[cfg(feature = "quinn")]
    quinn_shutdown_tx: Option<mpsc::UnboundedSender<()>>,
}

impl RpcManager {
    /// Create a new RPC manager with TCP only (Bitcoin Core compatible)
    pub fn new(server_addr: SocketAddr) -> Self {
        Self {
            server_addr,
            quinn_addr: None,
            blockchain_rpc: blockchain::BlockchainRpc::new(),
            network_rpc: network::NetworkRpc::new(),
            mining_rpc: mining::MiningRpc::new(),
            storage: None,
            mempool: None,
            shutdown_tx: None,
            #[cfg(feature = "quinn")]
            quinn_shutdown_tx: None,
        }
    }

    /// Set storage and mempool dependencies for RPC handlers
    pub fn with_dependencies(
        mut self,
        storage: Arc<Storage>,
        mempool: Arc<MempoolManager>,
    ) -> Self {
        // Update mining RPC with dependencies
        self.mining_rpc =
            mining::MiningRpc::with_dependencies(Arc::clone(&storage), Arc::clone(&mempool));

        self.storage = Some(storage);
        self.mempool = Some(mempool);
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
            shutdown_tx: None,
            quinn_shutdown_tx: None,
        }
    }

    /// Enable QUIC RPC server on specified address
    #[cfg(feature = "quinn")]
    pub fn enable_quinn(&mut self, quinn_addr: SocketAddr) {
        self.quinn_addr = Some(quinn_addr);
    }

    /// Start the RPC server(s)
    ///
    /// Starts TCP server (always) and optionally QUIC server if enabled
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting TCP RPC server on {}", self.server_addr);

        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel();
        self.shutdown_tx = Some(shutdown_tx.clone());

        let server = server::RpcServer::new(self.server_addr);

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

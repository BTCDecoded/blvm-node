//! Connection manager for handling network connections.
//!
//! Note: This module is deprecated - use Transport abstraction instead.
//! Extracted from network/mod.rs for Phase 1 of complexity remediation.

use anyhow::Result;
use std::net::SocketAddr;
use tracing::info;

/// Network I/O operations for testing
/// Note: This is deprecated - use TcpTransport instead
pub struct NetworkIO;

impl NetworkIO {
    pub async fn bind(&self, addr: SocketAddr) -> Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// Connection manager for handling network connections
/// Note: This is deprecated - use Transport abstraction instead
pub struct ConnectionManager {
    listen_addr: SocketAddr,
    network_io: NetworkIO,
}

impl ConnectionManager {
    pub fn new(listen_addr: SocketAddr) -> Self {
        Self {
            listen_addr,
            network_io: NetworkIO,
        }
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub async fn start_listening(&self) -> Result<tokio::net::TcpListener> {
        info!("Starting network listener on {}", self.listen_addr);
        self.network_io.bind(self.listen_addr).await
    }

    pub async fn connect_to_peer(&self, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
        info!("Connecting to peer at {}", addr);
        self.network_io.connect(addr).await
    }
}

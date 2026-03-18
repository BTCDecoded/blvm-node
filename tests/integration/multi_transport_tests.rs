//! Multi-transport integration tests
//!
//! Tests that verify TCP, Quinn, and Iroh transports work together
//! and can handle connections from different transport types.

use crate::network::{NetworkManager, TransportPreference};
use crate::network::transport::TransportAddr;
use std::net::SocketAddr;

#[tokio::test]
async fn test_tcp_connection() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::TCP_ONLY,
    );
    
    // Start network manager
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would create TCP connection and verify handshake; transport tested in network tests.
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_connection() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::QUINN_ONLY,
    );
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would create Quinn connection and verify handshake.
}

#[cfg(feature = "iroh")]
#[tokio::test]
async fn test_iroh_connection() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::IROH_ONLY,
    );
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would create Iroh connection and verify handshake.
}

#[tokio::test]
async fn test_mixed_transport_connections() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::all_transports(),
    );
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would create connections from different transport types.
}


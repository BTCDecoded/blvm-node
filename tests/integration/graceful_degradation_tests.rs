//! Graceful degradation integration tests
//!
//! Tests that verify the system gracefully falls back between transports
//! when preferred transport fails.

use crate::network::{NetworkManager, TransportPreference};
use std::net::SocketAddr;

#[tokio::test]
async fn test_tcp_fallback() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::all_transports(),
    );
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Test would simulate Quinn failure and verify TCP fallback works
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_to_tcp_fallback() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::QUINN_PREFERRED,
    );
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Test would simulate Quinn connection failure
    // Verify that system falls back to TCP
}

#[tokio::test]
async fn test_transport_preference_ordering() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_transport_preference(
        listen_addr,
        10,
        TransportPreference::QUINN_PREFERRED,
    );
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Test would verify that transports are tried in preference order
    // Quinn first, then TCP if Quinn fails
}


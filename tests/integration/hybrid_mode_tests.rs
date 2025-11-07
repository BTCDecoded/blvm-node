//! Integration tests for hybrid transport mode

use anyhow::Result;
use bllvm_node::network::{
    transport::TransportPreference,
    NetworkManager,
};
use std::net::SocketAddr;

#[tokio::test]
async fn test_network_manager_tcp_only_mode() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);
    
    assert_eq!(manager.transport_preference(), TransportPreference::TcpOnly);
    assert!(manager.transport_preference().allows_tcp());
    
    #[cfg(feature = "iroh")]
    {
        assert!(!manager.transport_preference().allows_iroh());
    }
}

#[cfg(feature = "iroh")]
#[tokio::test]
async fn test_network_manager_iroh_only_mode() {
    use bllvm_node::network::transport::TransportPreference;
    
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::with_transport_preference(
        addr,
        100,
        TransportPreference::IrohOnly,
    );
    
    assert_eq!(manager.transport_preference(), TransportPreference::IrohOnly);
    assert!(!manager.transport_preference().allows_tcp());
    assert!(manager.transport_preference().allows_iroh());
}

#[cfg(feature = "iroh")]
#[tokio::test]
async fn test_network_manager_hybrid_mode() {
    use bllvm_node::network::transport::TransportPreference;
    
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::with_transport_preference(
        addr,
        100,
        TransportPreference::Hybrid,
    );
    
    assert_eq!(manager.transport_preference(), TransportPreference::Hybrid);
    assert!(manager.transport_preference().allows_tcp());
    assert!(manager.transport_preference().allows_iroh());
    assert!(manager.transport_preference().prefers_iroh());
}

#[tokio::test]
async fn test_network_manager_backward_compatibility() {
    // Test that default mode is TCP-only (backward compatible)
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);
    
    // Should default to TCP-only
    assert_eq!(manager.transport_preference(), TransportPreference::TcpOnly);
    
    // Should have zero peers initially
    assert_eq!(manager.peer_count(), 0);
}

#[tokio::test]
async fn test_transport_preference_allows_methods() {
    let tcp_only = TransportPreference::TcpOnly;
    assert!(tcp_only.allows_tcp());
    
    #[cfg(feature = "iroh")]
    {
        assert!(!tcp_only.allows_iroh());
        
        let iroh_only = TransportPreference::IrohOnly;
        assert!(!iroh_only.allows_tcp());
        assert!(iroh_only.allows_iroh());
        
        let hybrid = TransportPreference::Hybrid;
        assert!(hybrid.allows_tcp());
        assert!(hybrid.allows_iroh());
        assert!(hybrid.prefers_iroh());
    }
}


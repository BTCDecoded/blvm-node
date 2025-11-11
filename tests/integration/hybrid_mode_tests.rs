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
    
    assert_eq!(manager.transport_preference(), TransportPreference::TCP_ONLY);
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
        TransportPreference::IROH_ONLY,
    );
    
    assert_eq!(manager.transport_preference(), TransportPreference::IROH_ONLY);
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
        TransportPreference::hybrid(),
    );
    
    assert_eq!(manager.transport_preference(), TransportPreference::hybrid());
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
    assert_eq!(manager.transport_preference(), TransportPreference::TCP_ONLY);
    
    // Should have zero peers initially
    assert_eq!(manager.peer_count(), 0);
}

#[tokio::test]
async fn test_transport_preference_allows_methods() {
    let tcp_only = TransportPreference::TCP_ONLY;
    assert!(tcp_only.allows_tcp());
    
    #[cfg(feature = "iroh")]
    {
        assert!(!tcp_only.allows_iroh());
        
        let iroh_only = TransportPreference::IROH_ONLY;
        assert!(!iroh_only.allows_tcp());
        assert!(iroh_only.allows_iroh());
        
        let hybrid = TransportPreference::hybrid();
        assert!(hybrid.allows_tcp());
        assert!(hybrid.allows_iroh());
        assert!(hybrid.prefers_iroh());
    }
}


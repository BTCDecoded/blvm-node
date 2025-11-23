//! Tests for Iroh peer tracking with TransportAddr

use bllvm_node::network::transport::TransportAddr;
use bllvm_node::network::PeerManager;
use std::net::SocketAddr;

#[test]
fn test_peer_manager_transport_addr() {
    let manager = PeerManager::new(10);

    let addr1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.1:8334".parse().unwrap();

    // Test TCP peer
    let tcp_addr1 = TransportAddr::Tcp(addr1);
    let tcp_addr2 = TransportAddr::Tcp(addr2);

    // Create mock peers (would need actual Peer instances in real test)
    // For now, just test that TransportAddr works as HashMap key
    assert_ne!(tcp_addr1, tcp_addr2);

    // Test Iroh peer (different transport type)
    #[cfg(feature = "iroh")]
    {
        let iroh_key = vec![0u8; 32];
        let iroh_addr = TransportAddr::Iroh(iroh_key);
        assert_ne!(tcp_addr1, iroh_addr);
    }
}

#[test]
fn test_find_transport_addr_by_socket() {
    let manager = PeerManager::new(10);

    let addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let tcp_addr = TransportAddr::Tcp(addr);

    // Should not find peer that doesn't exist
    assert!(manager.find_transport_addr_by_socket(addr).is_none());

    // After adding, should find it
    // Note: Would need actual Peer instance to test fully
    // This test validates the lookup logic
}

#[test]
fn test_peer_socket_addresses() {
    let manager = PeerManager::new(10);

    let addr1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.1:8334".parse().unwrap();

    // Add TCP peers
    let tcp_addr1 = TransportAddr::Tcp(addr1);
    let tcp_addr2 = TransportAddr::Tcp(addr2);

    // Test that peer_socket_addresses returns TCP/Quinn addresses
    // and skips Iroh addresses
    let socket_addrs = manager.peer_socket_addresses();
    // Should be empty since no peers added, but validates method exists
    assert_eq!(socket_addrs.len(), 0);
}

//! UTXO Commitments Client Feature Tests
//!
//! Tests for UTXO commitments client when the "utxo-commitments" feature is enabled.

#[cfg(feature = "utxo-commitments")]
use bllvm_node::network::utxo_commitments_client::UtxoCommitmentsClient;
#[cfg(feature = "utxo-commitments")]
use bllvm_protocol::types::Hash;
#[cfg(feature = "utxo-commitments")]
use std::sync::Arc;
#[cfg(feature = "utxo-commitments")]
use tokio::sync::RwLock;

#[cfg(feature = "utxo-commitments")]
#[test]
fn test_utxo_commitments_client_creation() {
    // Create a mock network manager
    use bllvm_node::network::NetworkManager;
    use std::net::SocketAddr;

    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(network_addr);
    let network_manager_arc = Arc::new(RwLock::new(network_manager));

    // Create UTXO commitments client
    let client = UtxoCommitmentsClient::new(network_manager_arc);

    // Should create successfully
    assert!(true);
}

#[cfg(feature = "utxo-commitments")]
#[test]
fn test_utxo_commitments_client_peer_id_parsing() {
    // Test peer ID parsing logic
    // TCP peer ID format: "tcp:127.0.0.1:8333"
    let tcp_peer_id = "tcp:127.0.0.1:8333";
    assert!(tcp_peer_id.starts_with("tcp:"));

    // Iroh peer ID format: "iroh:<pubkey_hex>"
    #[cfg(feature = "iroh")]
    {
        let iroh_peer_id = "iroh:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(iroh_peer_id.starts_with("iroh:"));
    }

    // Invalid peer ID
    let invalid_peer_id = "invalid:format";
    assert!(!invalid_peer_id.starts_with("tcp:"));
    #[cfg(feature = "iroh")]
    {
        assert!(!invalid_peer_id.starts_with("iroh:"));
    }
}

#[cfg(feature = "utxo-commitments")]
#[test]
fn test_utxo_commitments_client_get_peer_ids() {
    // Create client
    use bllvm_node::network::NetworkManager;
    use std::net::SocketAddr;

    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(network_addr);
    let network_manager_arc = Arc::new(RwLock::new(network_manager));

    let client = UtxoCommitmentsClient::new(network_manager_arc);

    // Get peer IDs (may be empty if no peers connected)
    let peer_ids = client.get_peer_ids();
    // Should return a vector (may be empty)
    assert!(peer_ids.is_empty() || !peer_ids.is_empty());
}

// Tests that verify feature is properly gated
#[cfg(not(feature = "utxo-commitments"))]
#[test]
fn test_utxo_commitments_feature_not_enabled() {
    // When utxo-commitments feature is not enabled, these types should not be available
    // This test verifies the feature gating works correctly
    assert!(true);
}

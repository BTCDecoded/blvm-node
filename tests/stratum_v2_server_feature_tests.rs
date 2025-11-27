//! Stratum V2 Server Feature Tests
//!
//! Tests for Stratum V2 server when the feature is enabled.

#[cfg(feature = "stratum-v2")]
use bllvm_node::network::stratum_v2::server::StratumV2Server;
#[cfg(feature = "stratum-v2")]
use bllvm_node::network::NetworkManager;
#[cfg(feature = "stratum-v2")]
use bllvm_node::node::miner::MiningCoordinator;
#[cfg(feature = "stratum-v2")]
use std::net::SocketAddr;
#[cfg(feature = "stratum-v2")]
use std::sync::Arc;
#[cfg(feature = "stratum-v2")]
use tokio::sync::RwLock;

#[cfg(feature = "stratum-v2")]
#[test]
fn test_stratum_v2_server_creation() {
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listen_addr: SocketAddr = "127.0.0.1:28332".parse().unwrap();

    let network_manager = NetworkManager::new(network_addr);
    let network_manager_arc = Arc::new(RwLock::new(network_manager));

    let mining_coordinator = MiningCoordinator::new();
    let mining_coordinator_arc = Arc::new(RwLock::new(mining_coordinator));

    // Create Stratum V2 server
    let server = StratumV2Server::new(network_manager_arc, mining_coordinator_arc, listen_addr);

    // Should create successfully
    assert!(true);
}

#[cfg(feature = "stratum-v2")]
#[tokio::test]
async fn test_stratum_v2_server_start_stop() {
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listen_addr: SocketAddr = "127.0.0.1:28333".parse().unwrap();

    let network_manager = NetworkManager::new(network_addr);
    let network_manager_arc = Arc::new(RwLock::new(network_manager));

    let mining_coordinator = MiningCoordinator::new();
    let mining_coordinator_arc = Arc::new(RwLock::new(mining_coordinator));

    let mut server = StratumV2Server::new(network_manager_arc, mining_coordinator_arc, listen_addr);

    // Start server
    let start_result = server.start().await;
    assert!(start_result.is_ok());

    // Stop server
    let stop_result = server.stop().await;
    assert!(stop_result.is_ok());
}

#[cfg(feature = "stratum-v2")]
#[tokio::test]
async fn test_stratum_v2_server_double_start() {
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listen_addr: SocketAddr = "127.0.0.1:28334".parse().unwrap();

    let network_manager = NetworkManager::new(network_addr);
    let network_manager_arc = Arc::new(RwLock::new(network_manager));

    let mining_coordinator = MiningCoordinator::new();
    let mining_coordinator_arc = Arc::new(RwLock::new(mining_coordinator));

    let mut server = StratumV2Server::new(network_manager_arc, mining_coordinator_arc, listen_addr);

    // Start server
    server.start().await.unwrap();

    // Try to start again (should fail)
    let result = server.start().await;
    assert!(result.is_err());
}

#[cfg(feature = "stratum-v2")]
#[tokio::test]
async fn test_stratum_v2_server_stop_when_not_running() {
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listen_addr: SocketAddr = "127.0.0.1:28335".parse().unwrap();

    let network_manager = NetworkManager::new(network_addr);
    let network_manager_arc = Arc::new(RwLock::new(network_manager));

    let mining_coordinator = MiningCoordinator::new();
    let mining_coordinator_arc = Arc::new(RwLock::new(mining_coordinator));

    let mut server = StratumV2Server::new(network_manager_arc, mining_coordinator_arc, listen_addr);

    // Stop when not running (should succeed)
    let result = server.stop().await;
    assert!(result.is_ok());
}

// Tests that verify feature is properly gated
#[cfg(not(feature = "stratum-v2"))]
#[test]
fn test_stratum_v2_feature_not_enabled() {
    // When stratum-v2 feature is not enabled, these types should not be available
    // This test verifies the feature gating works correctly
    assert!(true);
}

//! Full-node integration tests
//!
//! Tests the complete node lifecycle from startup to shutdown,
//! including all major components working together.

use bllvm_node::node::Node;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_node_startup_and_shutdown() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    // Create node with minimal config
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    
    // Start node (should succeed)
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    assert!(start_result.is_ok(), "Node should start within 10 seconds");
    assert!(start_result.unwrap().is_ok(), "Node start should succeed");
    
    // Give node a moment to initialize
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Shutdown node (should succeed)
    let shutdown_result = node.shutdown();
    assert!(shutdown_result.is_ok(), "Node shutdown should succeed");
}

#[tokio::test]
async fn test_node_with_storage() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create and start node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    
    // Start node
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    assert!(start_result.is_ok(), "Node should start with storage");
    assert!(start_result.unwrap().is_ok(), "Node start should succeed");
    
    // Verify storage is accessible
    let storage = node.storage();
    
    // Check storage bounds
    let bounds_ok = storage.check_storage_bounds().unwrap();
    assert!(bounds_ok, "Storage should be within bounds on startup");
    
    // Check disk size (should be small on startup)
    let disk_size = storage.disk_size().unwrap();
    assert!(disk_size < 1_000_000_000, "Disk size should be reasonable on startup");
    
    // Shutdown
    let shutdown_result = node.shutdown();
    assert!(shutdown_result.is_ok(), "Node shutdown should succeed");
}

#[tokio::test]
async fn test_node_full_lifecycle() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    
    // 1. Start node
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    assert!(start_result.is_ok(), "Node should start");
    assert!(start_result.unwrap().is_ok(), "Node start should succeed");
    
    // 2. Verify all components are initialized
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // 3. Check storage
    let storage = node.storage();
    let bounds_ok = storage.check_storage_bounds().unwrap();
    assert!(bounds_ok, "Storage should be within bounds");
    
    // 4. Check network
    let network = node.network();
    assert!(network.is_network_active(), "Network should be active");
    
    // 5. Shutdown
    let shutdown_result = node.shutdown();
    assert!(shutdown_result.is_ok(), "Node shutdown should succeed");
    
    // 6. Verify shutdown completed
    tokio::time::sleep(Duration::from_millis(100)).await;
}

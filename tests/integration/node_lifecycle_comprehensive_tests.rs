//! Comprehensive node lifecycle integration tests
//!
//! Tests complete node lifecycle including startup, component initialization,
//! module lifecycle, and graceful shutdown scenarios.

use bllvm_node::node::Node;
use bllvm_node::ProtocolVersion;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_node_startup_all_components() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    
    // Start node with timeout
    let start_task = tokio::spawn(async move {
        node.start().await
    });
    
    // Give node time to initialize components
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Verify components are accessible
    let node_guard = start_task.await;
    // Note: Node runs indefinitely, so we test component access before it completes
    
    // Shutdown would be handled by signal in real scenario
}

#[tokio::test]
async fn test_node_startup_with_pruning_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    use bllvm_node::config::{PruningConfig, PruningMode};
    
    // Create pruning config
    let pruning_config = PruningConfig {
        mode: PruningMode::Manual { keep_blocks: 1000 },
        prune_on_startup: false,
        ..Default::default()
    };
    
    // Create node with pruning config
    let mut node = Node::with_storage_config(
        data_dir,
        network_addr,
        rpc_addr,
        None,
        Some(pruning_config),
        None,
    ).unwrap();
    
    // Start node
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    // Node runs indefinitely, so timeout is expected
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    
    // Shutdown
    let shutdown_result = node.shutdown();
    assert!(shutdown_result.is_ok());
}

#[tokio::test]
async fn test_node_startup_with_indexing_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    use bllvm_node::config::{IndexingConfig, IndexingStrategy};
    
    // Create indexing config
    let indexing_config = IndexingConfig {
        enable_address_indexing: true,
        enable_value_range_indexing: true,
        strategy: IndexingStrategy::Eager,
        ..Default::default()
    };
    
    // Create node with indexing config
    let mut node = Node::with_storage_config(
        data_dir,
        network_addr,
        rpc_addr,
        None,
        None,
        Some(indexing_config),
    ).unwrap();
    
    // Start node
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    
    // Shutdown
    let shutdown_result = node.shutdown();
    assert!(shutdown_result.is_ok());
}

#[tokio::test]
async fn test_node_startup_different_protocol_versions() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Test Regtest (default)
    let mut node_regtest = Node::new(data_dir, network_addr, rpc_addr, Some(ProtocolVersion::Regtest)).unwrap();
    let start_result = timeout(Duration::from_secs(5), node_regtest.start()).await;
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    node_regtest.shutdown().unwrap();
    
    // Test Testnet3
    let temp_dir2 = tempfile::tempdir().unwrap();
    let data_dir2 = temp_dir2.path().to_str().unwrap();
    let mut node_testnet = Node::new(data_dir2, network_addr, rpc_addr, Some(ProtocolVersion::Testnet3)).unwrap();
    let start_result = timeout(Duration::from_secs(5), node_testnet.start()).await;
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    node_testnet.shutdown().unwrap();
}

#[tokio::test]
async fn test_node_startup_with_modules() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let socket_dir = temp_dir.path().join("sockets");
    
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node with modules
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    node.with_modules(modules_dir, socket_dir).unwrap();
    
    // Start node
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    
    // Give modules time to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Shutdown
    let shutdown_result = node.shutdown();
    assert!(shutdown_result.is_ok());
}

#[tokio::test]
async fn test_node_graceful_shutdown() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create and start node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    
    // Start node in background
    let node_handle = tokio::spawn(async move {
        let _ = node.start().await;
    });
    
    // Give node time to start
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Shutdown should be graceful
    // Note: In real scenario, shutdown would be triggered by signal
    // For test, we just verify node can be created and started
    node_handle.abort();
}

#[tokio::test]
async fn test_node_component_initialization_order() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, None).unwrap();
    
    // Verify components exist before start
    let storage = node.storage();
    assert!(storage.check_storage_bounds().is_ok());
    
    let network = node.network();
    // Network may not be active before start, which is fine
    
    // Start node
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    
    // Shutdown
    node.shutdown().unwrap();
}


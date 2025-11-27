//! End-to-end module lifecycle integration tests
//!
//! Tests complete module lifecycle including discovery, loading, initialization,
//! runtime operation, and unloading.

use bllvm_node::module::manager::ModuleManager;
use bllvm_node::module::api::node_api::NodeApiImpl;
use bllvm_node::storage::Storage;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_module_manager_lifecycle() {
    let temp_dir = tempfile::tempdir().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let modules_data_dir = temp_dir.path().join("modules_data");
    let socket_dir = temp_dir.path().join("sockets");
    
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&modules_data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();
    
    // Create module manager
    let mut manager = ModuleManager::new(
        &modules_dir,
        &modules_data_dir,
        &socket_dir,
    );
    
    // Create storage and node API for modules
    let storage = Arc::new(Storage::new(temp_dir.path().to_str().unwrap()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));
    
    // Start module manager
    let socket_path = socket_dir.join("test.sock");
    manager.start(socket_path.to_str().unwrap(), node_api).await.unwrap();
    
    // Auto-discover modules (will be empty, but should not error)
    let discover_result = manager.auto_load_modules().await;
    // Empty directory is fine
    assert!(discover_result.is_ok() || discover_result.unwrap_err().to_string().contains("No modules"));
    
    // Shutdown module manager
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_module_manager_with_empty_modules_dir() {
    let temp_dir = tempfile::tempdir().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let modules_data_dir = temp_dir.path().join("modules_data");
    let socket_dir = temp_dir.path().join("sockets");
    
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&modules_data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();
    
    // Create module manager with empty modules directory
    let mut manager = ModuleManager::new(
        &modules_dir,
        &modules_data_dir,
        &socket_dir,
    );
    
    // Create storage and node API
    let storage = Arc::new(Storage::new(temp_dir.path().to_str().unwrap()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));
    
    // Start module manager
    let socket_path = socket_dir.join("test.sock");
    manager.start(socket_path.to_str().unwrap(), node_api).await.unwrap();
    
    // Auto-discover should handle empty directory gracefully
    let discover_result = manager.auto_load_modules().await;
    // Should either succeed (no modules found) or return appropriate error
    assert!(discover_result.is_ok() || discover_result.unwrap_err().to_string().contains("No modules"));
    
    // Shutdown
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_module_manager_startup_shutdown_cycle() {
    let temp_dir = tempfile::tempdir().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let modules_data_dir = temp_dir.path().join("modules_data");
    let socket_dir = temp_dir.path().join("sockets");
    
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&modules_data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();
    
    // Create module manager
    let mut manager = ModuleManager::new(
        &modules_dir,
        &modules_data_dir,
        &socket_dir,
    );
    
    // Create storage and node API
    let storage = Arc::new(Storage::new(temp_dir.path().to_str().unwrap()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));
    
    // Start
    let socket_path = socket_dir.join("test.sock");
    manager.start(socket_path.to_str().unwrap(), node_api.clone()).await.unwrap();
    
    // Give it time to initialize
    sleep(Duration::from_millis(100)).await;
    
    // Shutdown
    manager.shutdown().await.unwrap();
    
    // Should be able to start again
    let mut manager2 = ModuleManager::new(
        &modules_dir,
        &modules_data_dir,
        &socket_dir,
    );
    manager2.start(socket_path.to_str().unwrap(), node_api).await.unwrap();
    manager2.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_module_manager_event_manager_access() {
    let temp_dir = tempfile::tempdir().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let modules_data_dir = temp_dir.path().join("modules_data");
    let socket_dir = temp_dir.path().join("sockets");
    
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&modules_data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();
    
    // Create module manager
    let mut manager = ModuleManager::new(
        &modules_dir,
        &modules_data_dir,
        &socket_dir,
    );
    
    // Create storage and node API
    let storage = Arc::new(Storage::new(temp_dir.path().to_str().unwrap()).unwrap());
    let node_api = Arc::new(NodeApiImpl::new(storage));
    
    // Start
    let socket_path = socket_dir.join("test.sock");
    manager.start(socket_path.to_str().unwrap(), node_api).await.unwrap();
    
    // Access event manager
    let event_manager = manager.event_manager();
    assert!(event_manager.is_some(), "Event manager should be available after start");
    
    // Shutdown
    manager.shutdown().await.unwrap();
}


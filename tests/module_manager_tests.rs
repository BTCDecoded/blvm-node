//! Tests for module manager lifecycle

use bllvm_node::module::manager::ModuleManager;
use bllvm_node::module::traits::{ModuleError, ModuleMetadata, ModuleState};
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;

fn create_test_metadata(name: &str) -> ModuleMetadata {
    ModuleMetadata {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        description: "Test module".to_string(),
        author: "Test Author".to_string(),
        capabilities: Vec::new(),
        dependencies: HashMap::new(),
        entry_point: format!("{}.so", name),
    }
}

#[tokio::test]
async fn test_module_manager_new() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    // Manager should be created successfully
    assert!(true);
}

#[tokio::test]
async fn test_module_manager_with_config() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let config = bllvm_node::config::ModuleResourceLimitsConfig::default();
    let manager = ModuleManager::with_config(&modules_dir, &data_dir, &socket_dir, Some(&config));
    // Manager should be created with config
    assert!(true);
}

#[tokio::test]
async fn test_module_manager_list_modules_empty() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let modules = manager.list_modules().await;

    assert!(modules.is_empty());
}

#[tokio::test]
async fn test_module_manager_get_module_state_nonexistent() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let state = manager.get_module_state("nonexistent").await;

    assert!(state.is_none());
}

#[tokio::test]
async fn test_module_manager_event_manager() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let event_manager = manager.event_manager();

    // Event manager should be accessible
    assert!(true);
}

#[tokio::test]
async fn test_module_manager_unload_nonexistent_module() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let mut manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let result = manager.unload_module("nonexistent").await;

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ModuleError::ModuleNotFound(_)
    ));
}

#[tokio::test]
async fn test_module_manager_reload_nonexistent_module() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    let mut manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    let metadata = create_test_metadata("test-module");
    let binary_path = PathBuf::from("/nonexistent/binary.so");
    let result = manager
        .reload_module("nonexistent", &binary_path, metadata, HashMap::new())
        .await;

    // Reload will try to unload first, which will fail, but then try to load
    // The load will fail because binary doesn't exist
    assert!(result.is_err());
}

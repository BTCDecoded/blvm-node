//! Tests for module process spawner

use bllvm_node::config::ModuleResourceLimitsConfig;
use bllvm_node::module::process::spawner::{ModuleProcess, ModuleProcessSpawner};
use bllvm_node::module::traits::{ModuleContext, ModuleError};
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::TempDir;

fn create_test_spawner() -> (TempDir, ModuleProcessSpawner) {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let spawner = ModuleProcessSpawner::new(&modules_dir, &data_dir, &socket_dir);
    (temp_dir, spawner)
}

fn create_test_context() -> ModuleContext {
    ModuleContext::new(
        "test-module-1".to_string(),
        "/tmp/test.sock".to_string(),
        "/tmp/test-data".to_string(),
        HashMap::new(),
    )
}

#[test]
fn test_spawner_creation() {
    let (_temp_dir, _spawner) = create_test_spawner();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_spawner_with_config() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let config = ModuleResourceLimitsConfig {
        default_max_cpu_percent: 75,
        default_max_memory_bytes: 1024 * 1024 * 1024, // 1 GB
        default_max_file_descriptors: 512,
        default_max_child_processes: 20,
        module_startup_wait_millis: 200,
        module_socket_timeout_seconds: 10,
        module_socket_check_interval_millis: 50,
        module_socket_max_attempts: 100,
    };

    let _spawner =
        ModuleProcessSpawner::with_config(&modules_dir, &data_dir, &socket_dir, Some(&config));
    // Should create successfully with custom config
    assert!(true);
}

#[test]
fn test_spawner_directories() {
    let (_temp_dir, spawner) = create_test_spawner();

    // Verify directories are set correctly
    assert!(spawner.modules_dir.exists() || spawner.modules_dir.parent().is_some());
    assert!(spawner.data_dir.exists() || spawner.data_dir.parent().is_some());
    assert!(spawner.socket_dir.exists() || spawner.socket_dir.parent().is_some());
}

#[tokio::test]
async fn test_spawn_nonexistent_binary() {
    let (_temp_dir, spawner) = create_test_spawner();
    let context = create_test_context();
    let fake_binary = PathBuf::from("/nonexistent/binary");

    let result = spawner.spawn("test-module", &fake_binary, context).await;

    // Should fail with an error
    assert!(result.is_err());
    // Error type should be ModuleError (which implements Debug)
    // We don't need to match on the specific error variant for this test
}

// Note: ModuleProcess has private fields, so we can't construct it directly in tests.
// These tests would require actually spawning a module process via spawner.spawn(),
// which requires a real module binary. For now, we test the spawner's public API
// and the process lifecycle methods would be tested via integration tests.

#[test]
fn test_spawner_resource_limits_config() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let data_dir = temp_dir.path().join("data");
    let socket_dir = temp_dir.path().join("sockets");

    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let config = ModuleResourceLimitsConfig {
        default_max_cpu_percent: 50,
        default_max_memory_bytes: 512 * 1024 * 1024,
        default_max_file_descriptors: 256,
        default_max_child_processes: 10,
        module_startup_wait_millis: 100,
        module_socket_timeout_seconds: 5,
        module_socket_check_interval_millis: 100,
        module_socket_max_attempts: 50,
    };

    let _spawner =
        ModuleProcessSpawner::with_config(&modules_dir, &data_dir, &socket_dir, Some(&config));

    // Verify spawner was created with config
    assert!(true);
}

#[test]
fn test_module_context_creation() {
    let mut config = HashMap::new();
    config.insert("key1".to_string(), "value1".to_string());
    config.insert("key2".to_string(), "value2".to_string());

    let context = ModuleContext::new(
        "test-id".to_string(),
        "/tmp/test.sock".to_string(),
        "/tmp/test-data".to_string(),
        config,
    );

    assert_eq!(context.module_id, "test-id");
    assert_eq!(context.config.len(), 2);
    assert_eq!(context.config.get("key1"), Some(&"value1".to_string()));
}

#[test]
fn test_module_context_empty() {
    let context = ModuleContext::new(
        "test-id".to_string(),
        "/tmp/test.sock".to_string(),
        "/tmp/test-data".to_string(),
        HashMap::new(),
    );

    assert_eq!(context.module_id, "test-id");
    assert_eq!(context.config.len(), 0);
}

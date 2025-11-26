//! Tests for process sandbox

use bllvm_node::module::sandbox::process::{
    ProcessSandbox, ResourceLimits, ResourceUsage, SandboxConfig,
};
use bllvm_node::module::traits::ModuleError;
use std::path::PathBuf;
use tempfile::TempDir;

fn create_test_sandbox() -> (TempDir, ProcessSandbox) {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    
    let config = SandboxConfig::new(&data_dir);
    let sandbox = ProcessSandbox::new(config);
    (temp_dir, sandbox)
}

#[test]
fn test_process_sandbox_creation() {
    let (_temp_dir, _sandbox) = create_test_sandbox();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_resource_limits_default() {
    let limits = ResourceLimits::default();
    
    assert_eq!(limits.max_cpu_percent, Some(50));
    assert_eq!(limits.max_memory_bytes, Some(512 * 1024 * 1024));
    assert_eq!(limits.max_file_descriptors, Some(256));
    assert_eq!(limits.max_child_processes, Some(10));
}

#[test]
fn test_resource_limits_custom() {
    let limits = ResourceLimits {
        max_cpu_percent: Some(75),
        max_memory_bytes: Some(1024 * 1024 * 1024),
        max_file_descriptors: Some(512),
        max_child_processes: Some(20),
    };
    
    assert_eq!(limits.max_cpu_percent, Some(75));
    assert_eq!(limits.max_memory_bytes, Some(1024 * 1024 * 1024));
    assert_eq!(limits.max_file_descriptors, Some(512));
    assert_eq!(limits.max_child_processes, Some(20));
}

#[test]
fn test_sandbox_config_creation() {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    
    let config = SandboxConfig::new(&data_dir);
    
    assert_eq!(config.allowed_data_dir, data_dir);
    assert!(!config.strict_mode);
    assert_eq!(config.resource_limits.max_cpu_percent, Some(50));
}

#[test]
fn test_sandbox_config_strict() {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    
    let config = SandboxConfig::strict(&data_dir);
    
    assert_eq!(config.allowed_data_dir, data_dir);
    assert!(config.strict_mode);
}

#[test]
fn test_sandbox_config_with_resource_limits() {
    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    
    let resource_limits_config = bllvm_node::config::ModuleResourceLimitsConfig {
        default_max_cpu_percent: 75,
        default_max_memory_bytes: 1024 * 1024 * 1024,
        default_max_file_descriptors: 512,
        default_max_child_processes: 20,
        module_startup_wait_millis: 200,
        module_socket_timeout_seconds: 10,
        module_socket_check_interval_millis: 50,
        module_socket_max_attempts: 100,
    };
    
    let config = SandboxConfig::with_resource_limits(&data_dir, &resource_limits_config);
    
    assert_eq!(config.resource_limits.max_cpu_percent, Some(75));
    assert_eq!(config.resource_limits.max_memory_bytes, Some(1024 * 1024 * 1024));
    assert_eq!(config.resource_limits.max_file_descriptors, Some(512));
    assert_eq!(config.resource_limits.max_child_processes, Some(20));
}

#[test]
fn test_resource_usage_exceeds_limits() {
    let limits = ResourceLimits {
        max_cpu_percent: Some(50),
        max_memory_bytes: Some(512 * 1024 * 1024),
        max_file_descriptors: Some(256),
        max_child_processes: Some(10),
    };
    
    // Usage within limits
    let usage = ResourceUsage {
        cpu_percent: 25.0,
        memory_bytes: 256 * 1024 * 1024,
        file_descriptors: 128,
        child_processes: 5,
    };
    
    assert!(!usage.exceeds_limits(&limits));
    
    // Usage exceeds CPU limit
    let usage = ResourceUsage {
        cpu_percent: 75.0,
        memory_bytes: 256 * 1024 * 1024,
        file_descriptors: 128,
        child_processes: 5,
    };
    
    assert!(usage.exceeds_limits(&limits));
    
    // Usage exceeds memory limit
    let usage = ResourceUsage {
        cpu_percent: 25.0,
        memory_bytes: 1024 * 1024 * 1024,
        file_descriptors: 128,
        child_processes: 5,
    };
    
    assert!(usage.exceeds_limits(&limits));
    
    // Usage exceeds file descriptor limit
    let usage = ResourceUsage {
        cpu_percent: 25.0,
        memory_bytes: 256 * 1024 * 1024,
        file_descriptors: 512,
        child_processes: 5,
    };
    
    assert!(usage.exceeds_limits(&limits));
    
    // Usage exceeds child process limit
    let usage = ResourceUsage {
        cpu_percent: 25.0,
        memory_bytes: 256 * 1024 * 1024,
        file_descriptors: 128,
        child_processes: 20,
    };
    
    assert!(usage.exceeds_limits(&limits));
}

#[test]
fn test_resource_usage_with_none_limits() {
    let limits = ResourceLimits {
        max_cpu_percent: None,
        max_memory_bytes: None,
        max_file_descriptors: None,
        max_child_processes: None,
    };
    
    // Any usage should not exceed limits if limits are None
    let usage = ResourceUsage {
        cpu_percent: 100.0,
        memory_bytes: u64::MAX,
        file_descriptors: u32::MAX,
        child_processes: u32::MAX,
    };
    
    assert!(!usage.exceeds_limits(&limits));
}

#[test]
fn test_process_sandbox_config() {
    let (_temp_dir, sandbox) = create_test_sandbox();
    
    // Should be able to get config
    let config = sandbox.config();
    assert_eq!(config.allowed_data_dir, _temp_dir.path().join("data"));
}

#[tokio::test]
async fn test_apply_limits_no_pid() {
    let (_temp_dir, sandbox) = create_test_sandbox();
    
    // Applying limits with no PID should succeed (no-op)
    let result = sandbox.apply_limits(None);
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_monitor_resources_no_pid() {
    let (_temp_dir, sandbox) = create_test_sandbox();
    
    // Monitoring resources with no PID should return zeros
    let result = sandbox.monitor_resources(None).await;
    assert!(result.is_ok());
    
    let usage = result.unwrap();
    assert_eq!(usage.cpu_percent, 0.0);
    assert_eq!(usage.memory_bytes, 0);
    assert_eq!(usage.file_descriptors, 0);
    assert_eq!(usage.child_processes, 0);
}

#[test]
fn test_resource_usage_structure() {
    let usage = ResourceUsage {
        cpu_percent: 50.0,
        memory_bytes: 1024 * 1024,
        file_descriptors: 100,
        child_processes: 5,
    };
    
    assert_eq!(usage.cpu_percent, 50.0);
    assert_eq!(usage.memory_bytes, 1024 * 1024);
    assert_eq!(usage.file_descriptors, 100);
    assert_eq!(usage.child_processes, 5);
}


//! Tests for module process monitor

use bllvm_node::module::process::monitor::{ModuleHealth, ModuleProcessMonitor};
use bllvm_node::module::process::spawner::ModuleProcess;
use bllvm_node::module::traits::ModuleError;
use tokio::sync::mpsc;
use tokio::time::Duration;

#[test]
fn test_monitor_creation() {
    let (crash_tx, _crash_rx) = mpsc::unbounded_channel();
    let _monitor = ModuleProcessMonitor::new(crash_tx);
    // Should create successfully
    assert!(true);
}

#[test]
fn test_monitor_with_interval() {
    let (crash_tx, _crash_rx) = mpsc::unbounded_channel();
    let monitor = ModuleProcessMonitor::new(crash_tx)
        .with_interval(Duration::from_secs(10));
    // Should create successfully with custom interval
    assert!(true);
}

#[test]
fn test_module_health_variants() {
    // Test all health status variants
    let health_variants = vec![
        ModuleHealth::Healthy,
        ModuleHealth::Unresponsive,
        ModuleHealth::Crashed("Test crash".to_string()),
    ];

    for health in health_variants {
        match health {
            ModuleHealth::Healthy => assert!(true),
            ModuleHealth::Unresponsive => assert!(true),
            ModuleHealth::Crashed(msg) => assert!(!msg.is_empty()),
        }
    }
}

#[test]
fn test_module_health_clone() {
    let health = ModuleHealth::Healthy;
    let cloned = health.clone();
    
    match (health, cloned) {
        (ModuleHealth::Healthy, ModuleHealth::Healthy) => assert!(true),
        _ => panic!("Health should clone correctly"),
    }
}

#[test]
fn test_module_health_crashed() {
    let health = ModuleHealth::Crashed("Test error".to_string());
    
    match health {
        ModuleHealth::Crashed(msg) => {
            assert_eq!(msg, "Test error");
        }
        _ => panic!("Expected Crashed variant"),
    }
}

// Note: Full monitoring tests would require:
// - An actual spawned module process
// - IPC client connection
// - Integration test setup
// These are better suited for integration tests.
// Here we test the monitor structure and health enum.


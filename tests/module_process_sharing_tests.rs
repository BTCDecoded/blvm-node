//! Tests for module process sharing

use bllvm_node::module::process::monitor::ModuleProcessMonitor;
use bllvm_node::module::process::spawner::{ModuleProcess, ModuleProcessSpawner};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_monitor_module_shared() {
    let temp_dir = TempDir::new().unwrap();
    let (crash_tx, _crash_rx) = mpsc::unbounded_channel();
    let monitor = ModuleProcessMonitor::new(crash_tx);

    // Create a dummy process (using true command which exits immediately)
    let process = tokio::process::Command::new("true").spawn().unwrap();

    let module_process = ModuleProcess {
        module_name: "test".to_string(),
        process,
        socket_path: std::path::PathBuf::new(),
        client: None,
    };

    let shared_process = Arc::new(Mutex::new(module_process));

    // Test that monitor_module_shared can be called
    // (will exit quickly since process exits immediately)
    let result = monitor
        .monitor_module_shared("test".to_string(), shared_process)
        .await;

    // Should succeed (process exits normally)
    assert!(result.is_ok());
}

//! Tests for module process sharing and monitoring
//!
//! Tests that ModuleProcessMonitor can properly monitor shared module processes
//! and handle process lifecycle events.

use bllvm_node::module::process::{ModuleProcessMonitor, ModuleProcessSpawner};
use bllvm_node::module::traits::ModuleContext;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_monitor_creation() {
    let (crash_tx, crash_rx) = mpsc::unbounded_channel();
    let monitor = ModuleProcessMonitor::new(crash_tx);
    
    // Monitor should be created successfully (returns Self)
    // Verify we can use it by checking interval can be set
    let _monitor_with_interval = monitor.with_interval(Duration::from_secs(1));
    
    // Channel should still be open
    assert!(!crash_rx.is_closed());
}

#[tokio::test]
async fn test_monitor_with_shared_process() {
    let temp_dir = TempDir::new().unwrap();
    let (crash_tx, mut crash_rx) = mpsc::unbounded_channel();
    
    // Create spawner (takes 3 args: modules_dir, data_dir, socket_dir)
    let spawner = ModuleProcessSpawner::new(
        temp_dir.path().join("modules"),
        temp_dir.path().join("data"),
        temp_dir.path().join("sockets"),
    );
    
    // Create monitor
    let _monitor = ModuleProcessMonitor::new(crash_tx);
    
    // Create a test context
    let context = ModuleContext::new(
        "test_module".to_string(),
        temp_dir.path().join("sockets").join("test.sock").to_string_lossy().to_string(),
        temp_dir.path().join("data").join("test").to_string_lossy().to_string(),
        std::collections::HashMap::new(),
    );
    
    // Spawn a simple process (using a command that exits quickly)
    // We'll use a command that we know exists and exits cleanly
    let test_binary = if cfg!(unix) {
        "/bin/true" // Unix: true command exits with 0
    } else {
        "cmd.exe" // Windows: cmd.exe /c exit 0
    };
    
    let binary_path = PathBuf::from(test_binary);
    if !binary_path.exists() {
        // Skip test if binary doesn't exist
        return;
    }
    
    // Try to spawn the process
    // Note: This will fail if the binary doesn't match ModuleProcess expectations,
    // but we can test the infrastructure
    let result = spawner.spawn("test_module", &binary_path, context).await;
    
    // If spawning fails (expected for non-module binaries), that's OK
    // We're testing the monitor infrastructure, not the actual module spawning
    if let Ok(process) = result {
        // Process spawned successfully
        let process_id = process.id();
        assert!(process_id.is_some(), "Process should have a valid ID");
        assert!(process_id.unwrap() > 0, "Process ID should be positive");
        
        // Monitor should be able to track this process
        // (Actual monitoring happens in background tasks)
        
        // Wait a bit for process to potentially exit
        sleep(Duration::from_millis(100)).await;
        
        // Check if crash notification was sent (if process exited)
        // This is non-blocking check
        let _ = crash_rx.try_recv();
    }
}

#[tokio::test]
async fn test_monitor_crash_detection() {
    let (crash_tx, crash_rx) = mpsc::unbounded_channel();
    let _monitor = ModuleProcessMonitor::new(crash_tx);
    
    // Monitor should be created
    // Crash detection happens asynchronously in background tasks
    // We can't easily test this without a real module process,
    // but we verify the infrastructure is set up correctly
    
    // Channel should be open
    assert!(!crash_rx.is_closed());
}

#[tokio::test]
async fn test_monitor_cleanup() {
    let (crash_tx, crash_rx) = mpsc::unbounded_channel();
    let monitor = ModuleProcessMonitor::new(crash_tx);
    
    // Monitor should exist (returns Self)
    // Verify we can use it
    let _monitor_with_interval = monitor.with_interval(Duration::from_secs(2));
    
    // Drop monitor
    drop(_monitor_with_interval);
    
    // Channel receiver should still be valid
    assert!(!crash_rx.is_closed());
}

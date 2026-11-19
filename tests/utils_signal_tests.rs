//! Signal handling utility tests
//!
//! Tests for signal handling functions.

use std::sync::{Mutex, MutexGuard};

use blvm_node::rpc::control::ControlRpc;
use blvm_node::utils::signal::{create_shutdown_receiver, request_shutdown, shutdown_requested};
use serde_json::json;
use tokio::sync::mpsc;

fn shutdown_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tokio::test]
async fn test_create_shutdown_receiver() {
    let _guard = shutdown_test_lock();
    let receiver = create_shutdown_receiver();
    if !shutdown_requested() {
        assert!(!*receiver.borrow());
    }
}

#[tokio::test]
async fn test_shutdown_receiver_initial_state() {
    let _guard = shutdown_test_lock();
    let receiver = create_shutdown_receiver();
    if !shutdown_requested() {
        assert!(!*receiver.borrow());
    }
}

#[tokio::test]
async fn rpc_stop_sets_the_shutdown_watch() {
    let _guard = shutdown_test_lock();
    let (tx, mut listener_shutdown) = mpsc::unbounded_channel();
    let rpc = ControlRpc::with_shutdown(
        tx,
        Some(std::sync::Arc::new(|| {
            request_shutdown();
            Ok(())
        })),
    );

    let result = rpc.stop(&json!([])).await.expect("stop");
    assert_eq!(result, json!("Bitcoin node stopping"));
    assert!(shutdown_requested());
    assert!(listener_shutdown.try_recv().is_ok());
}

// Note: Testing actual signal delivery (SIGTERM, SIGINT) is difficult in unit tests
// as it requires process-level signal handling. These tests verify the receiver
// can be created and has the correct initial state.

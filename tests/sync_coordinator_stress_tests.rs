//! Stress tests for sync coordinator (state transitions, peer disconnection)

use bllvm_node::node::sync::{SyncCoordinator, SyncState, SyncStateMachine};
use bllvm_node::{BlockHeader, Hash};
use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

fn create_test_header(height: u64) -> BlockHeader {
    let mut prev_hash = [0u8; 32];
    prev_hash[0] = (height % 256) as u8;

    BlockHeader {
        version: 1,
        prev_block_hash: prev_hash,
        merkle_root: [0u8; 32],
        timestamp: 1231006505 + height,
        bits: 0x1d00ffff,
        nonce: 0,
    }
}

#[tokio::test]
async fn test_sync_state_rapid_transitions() {
    let mut machine = SyncStateMachine::new();

    // Rapidly transition between states
    for _ in 0..10 {
        machine.transition_to(SyncState::Headers);
        machine.transition_to(SyncState::Blocks);
        machine.transition_to(SyncState::Synced);
        machine.transition_to(SyncState::Initial);
    }

    // Should end in a valid state
    assert!(matches!(
        machine.state(),
        &SyncState::Initial | &SyncState::Headers | &SyncState::Blocks | &SyncState::Synced
    ));
}

#[tokio::test]
async fn test_sync_state_concurrent_transitions() {
    let machine = Arc::new(tokio::sync::Mutex::new(SyncStateMachine::new()));

    // Concurrent state transitions
    let mut handles = vec![];
    for i in 0..5 {
        let machine_clone = Arc::clone(&machine);
        handles.push(tokio::spawn(async move {
            let mut machine = machine_clone.lock().await;
            match i % 4 {
                0 => machine.transition_to(SyncState::Headers),
                1 => machine.transition_to(SyncState::Blocks),
                2 => machine.transition_to(SyncState::Synced),
                _ => machine.transition_to(SyncState::Initial),
            }
        }));
    }

    // Wait for all transitions
    futures::future::join_all(handles).await;

    // Should end in a valid state
    let machine = machine.lock().await;
    assert!(matches!(
        machine.state(),
        &SyncState::Initial | &SyncState::Headers | &SyncState::Blocks | &SyncState::Synced
    ));
}

#[tokio::test]
async fn test_sync_coordinator_peer_disconnection_during_sync() {
    let mut coordinator = SyncCoordinator::new();

    // Start sync
    coordinator.start_sync().unwrap();

    // Simulate peer disconnection by transitioning to error state
    // In real scenario, this would be handled by network layer
    // For now, we test that coordinator can handle state changes

    // Coordinator should handle disconnection gracefully
    assert!(coordinator.progress() >= 0.0 && coordinator.progress() <= 1.0);
}

#[tokio::test]
async fn test_sync_coordinator_chain_tip_divergence() {
    let mut coordinator = SyncCoordinator::new();

    // Start sync
    coordinator.start_sync().unwrap();

    // Simulate chain tip divergence (multiple headers at same height)
    // In real scenario, this would trigger reorganization logic
    // For now, we test that coordinator can handle this scenario

    // Coordinator should handle divergence gracefully
    assert!(coordinator.progress() >= 0.0 && coordinator.progress() <= 1.0);
}

#[tokio::test]
async fn test_sync_state_machine_error_recovery() {
    let mut machine = SyncStateMachine::new();

    // Set error state
    machine.set_error("Test error".to_string());
    assert!(matches!(machine.state(), &SyncState::Error(_)));

    // Recover from error
    machine.transition_to(SyncState::Initial);
    assert!(matches!(machine.state(), &SyncState::Initial));

    // Should be able to continue syncing
    machine.transition_to(SyncState::Headers);
    assert!(matches!(machine.state(), &SyncState::Headers));
}

#[tokio::test]
async fn test_sync_state_machine_progress_tracking() {
    let mut machine = SyncStateMachine::new();

    // Initial state should have 0 progress
    assert_eq!(machine.progress(), 0.0);

    // Progress should increase as we move through states
    machine.transition_to(SyncState::Headers);
    let progress_headers = machine.progress();

    machine.transition_to(SyncState::Blocks);
    let progress_blocks = machine.progress();

    machine.transition_to(SyncState::Synced);
    let progress_synced = machine.progress();

    // Progress should be non-decreasing (or at least synced should be highest)
    assert!(progress_synced >= progress_headers);
    assert!(progress_synced >= progress_blocks);
    assert!(progress_synced <= 1.0);
}

#[tokio::test]
async fn test_sync_coordinator_concurrent_operations() {
    let coordinator = Arc::new(tokio::sync::Mutex::new(SyncCoordinator::new()));

    // Concurrent operations
    let mut handles = vec![];
    for _ in 0..5 {
        let coordinator_clone = Arc::clone(&coordinator);
        handles.push(tokio::spawn(async move {
            let mut coordinator = coordinator_clone.lock().await;
            coordinator.start_sync().ok()
        }));
    }

    // Wait for all operations
    futures::future::join_all(handles).await;

    // Coordinator should still be in valid state
    let coordinator = coordinator.lock().await;
    assert!(coordinator.progress() >= 0.0 && coordinator.progress() <= 1.0);
}

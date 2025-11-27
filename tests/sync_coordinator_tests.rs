//! Tests for sync coordinator and state machine

use bllvm_node::node::sync::{SyncState, SyncStateMachine};
use bllvm_node::{BlockHeader, Hash};
use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

fn create_test_header() -> BlockHeader {
    BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1231006505,
        bits: 0x1d00ffff,
        nonce: 0,
    }
}

#[test]
fn test_sync_state_machine_initial_state() {
    let machine = SyncStateMachine::new();
    assert!(matches!(machine.state(), &SyncState::Initial));
    assert_eq!(machine.progress(), 0.0);
    assert!(!machine.is_synced());
}

#[test]
fn test_sync_state_machine_transitions() {
    let mut machine = SyncStateMachine::new();

    // Test state transitions
    machine.transition_to(SyncState::Headers);
    assert!(matches!(machine.state(), &SyncState::Headers));

    machine.transition_to(SyncState::Blocks);
    assert!(matches!(machine.state(), &SyncState::Blocks));

    machine.transition_to(SyncState::Synced);
    assert!(matches!(machine.state(), &SyncState::Synced));
    assert!(machine.is_synced());
}

#[test]
fn test_sync_state_machine_error_state() {
    let mut machine = SyncStateMachine::new();

    machine.set_error("Test error".to_string());
    assert!(matches!(machine.state(), &SyncState::Error(_)));
    assert_eq!(machine.progress(), 0.0);
    assert!(!machine.is_synced());
}

#[test]
fn test_sync_state_machine_update_best_header() {
    let mut machine = SyncStateMachine::new();
    let header = create_test_header();

    machine.update_best_header(header.clone());
    assert!(machine.best_header().is_some());
    assert_eq!(machine.best_header().unwrap().version, header.version);
}

#[test]
fn test_sync_state_machine_update_chain_tip() {
    let mut machine = SyncStateMachine::new();
    let header = create_test_header();

    machine.update_chain_tip(header.clone());
    assert!(machine.chain_tip().is_some());
    assert_eq!(machine.chain_tip().unwrap().version, header.version);
}

#[test]
fn test_sync_state_machine_progress_updates() {
    let mut machine = SyncStateMachine::new();

    // Initial progress should be 0.0
    assert_eq!(machine.progress(), 0.0);

    // Progress should update on state transitions
    machine.transition_to(SyncState::Headers);
    let progress1 = machine.progress();

    machine.transition_to(SyncState::Blocks);
    let progress2 = machine.progress();

    // Progress should increase (or at least not decrease)
    assert!(progress2 >= progress1);

    machine.transition_to(SyncState::Synced);
    // Synced state should have high progress
    assert!(machine.progress() > 0.0);
}

#[test]
fn test_sync_state_all_variants() {
    // Test that all SyncState variants can be created
    let states = vec![
        SyncState::Initial,
        SyncState::Headers,
        SyncState::Blocks,
        SyncState::Synced,
        SyncState::Error("test".to_string()),
    ];

    for state in states {
        let mut machine = SyncStateMachine::new();
        machine.transition_to(state.clone());
        // Verify state was set correctly
        match (&state, machine.state()) {
            (SyncState::Initial, &SyncState::Initial) => {}
            (SyncState::Headers, &SyncState::Headers) => {}
            (SyncState::Blocks, &SyncState::Blocks) => {}
            (SyncState::Synced, &SyncState::Synced) => {}
            (SyncState::Error(_), &SyncState::Error(_)) => {}
            _ => panic!("State mismatch"),
        }
    }
}

#[test]
fn test_sync_state_machine_default() {
    let machine = SyncStateMachine::default();
    assert!(matches!(machine.state(), &SyncState::Initial));
    assert_eq!(machine.progress(), 0.0);
}

#[test]
fn test_sync_state_machine_error_message() {
    let mut machine = SyncStateMachine::new();
    let error_msg = "Connection failed".to_string();

    machine.set_error(error_msg.clone());

    match machine.state() {
        SyncState::Error(msg) => assert_eq!(msg, &error_msg),
        _ => panic!("Expected Error state"),
    }

    // Verify error state has progress reset
    assert_eq!(machine.progress(), 0.0);
}

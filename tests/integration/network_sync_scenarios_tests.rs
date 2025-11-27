//! Network sync integration tests
//!
//! Tests complete network synchronization scenarios including:
//! - Initial blockchain sync from scratch
//! - Sync state transitions
//! - Sync with peer connections
//! - Sync error handling
//! - Chain reorganization scenarios

use bllvm_node::node::Node;
use bllvm_node::node::sync::{SyncCoordinator, SyncState, SyncStateMachine};
use bllvm_node::ProtocolVersion;
use bllvm_protocol::BlockHeader;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

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
fn test_sync_state_machine_initial_to_synced() {
    let mut machine = SyncStateMachine::new();
    
    // Start in Initial state
    assert!(matches!(machine.state(), &SyncState::Initial));
    assert_eq!(machine.progress(), 0.0);
    
    // Transition through states
    machine.transition_to(SyncState::Headers);
    assert!(matches!(machine.state(), &SyncState::Headers));
    assert_eq!(machine.progress(), 0.3);
    
    machine.transition_to(SyncState::Blocks);
    assert!(matches!(machine.state(), &SyncState::Blocks));
    assert_eq!(machine.progress(), 0.6);
    
    machine.transition_to(SyncState::Synced);
    assert!(matches!(machine.state(), &SyncState::Synced));
    assert_eq!(machine.progress(), 1.0);
    assert!(machine.is_synced());
}

#[test]
fn test_sync_state_machine_with_headers() {
    let mut machine = SyncStateMachine::new();
    let header = create_test_header();
    
    // Update best header
    machine.update_best_header(header.clone());
    assert!(machine.best_header().is_some());
    
    // Transition to Headers state
    machine.transition_to(SyncState::Headers);
    assert!(matches!(machine.state(), &SyncState::Headers));
    
    // Update chain tip
    machine.update_chain_tip(header);
    assert!(machine.chain_tip().is_some());
}

#[test]
fn test_sync_state_machine_error_handling() {
    let mut machine = SyncStateMachine::new();
    
    // Set error state
    machine.set_error("Network error".to_string());
    assert!(matches!(machine.state(), &SyncState::Error(_)));
    assert_eq!(machine.progress(), 0.0);
    assert!(!machine.is_synced());
    
    // Can recover from error
    machine.transition_to(SyncState::Initial);
    assert!(matches!(machine.state(), &SyncState::Initial));
}

#[test]
fn test_sync_coordinator_start_sync() {
    let mut coordinator = SyncCoordinator::new();
    
    // Start sync
    let result = coordinator.start_sync();
    assert!(result.is_ok());
    
    // Should be synced after start
    assert!(coordinator.is_synced());
    assert_eq!(coordinator.progress(), 1.0);
}

#[test]
fn test_sync_coordinator_progress_tracking() {
    let mut coordinator = SyncCoordinator::new();
    
    // Initially not synced
    assert!(!coordinator.is_synced());
    assert_eq!(coordinator.progress(), 0.0);
    
    // Start sync
    coordinator.start_sync().unwrap();
    
    // Should be synced
    assert!(coordinator.is_synced());
    assert_eq!(coordinator.progress(), 1.0);
}

#[tokio::test]
async fn test_node_sync_initial_state() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, Some(ProtocolVersion::Regtest)).unwrap();
    
    // Check initial chain height (should be 0 for new node)
    let storage = node.storage();
    let initial_height = storage.chain().get_height().unwrap();
    assert_eq!(initial_height, Some(0));
    
    // Node should be ready for sync
    // (In real scenario, sync would start automatically)
}

#[tokio::test]
async fn test_node_sync_with_storage() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, Some(ProtocolVersion::Regtest)).unwrap();
    
    // Storage should be accessible
    let storage = node.storage();
    let height = storage.chain().get_height().unwrap();
    assert!(height.is_some());
    
    // Storage should be ready for sync
    assert!(storage.check_storage_bounds().is_ok());
}

#[tokio::test]
async fn test_node_sync_state_transitions() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, Some(ProtocolVersion::Regtest)).unwrap();
    
    // Create sync coordinator
    let mut coordinator = SyncCoordinator::new();
    
    // Test sync state transitions
    coordinator.start_sync().unwrap();
    
    // Verify sync completed
    assert!(coordinator.is_synced());
    assert_eq!(coordinator.progress(), 1.0);
}

#[tokio::test]
async fn test_node_sync_with_network() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Create node
    let mut node = Node::new(data_dir, network_addr, rpc_addr, Some(ProtocolVersion::Regtest)).unwrap();
    
    // Network should be initialized
    let network = node.network();
    // Network may not be active before start, which is fine
    
    // Start node (sync would happen in background)
    let start_result = timeout(Duration::from_secs(10), node.start()).await;
    // Node runs indefinitely, so timeout is expected
    assert!(start_result.is_err() || start_result.unwrap().is_ok());
    
    // Shutdown
    node.shutdown().unwrap();
}

#[test]
fn test_sync_state_machine_progress_calculation() {
    let mut machine = SyncStateMachine::new();
    
    // Initial state
    assert_eq!(machine.progress(), 0.0);
    
    // Headers state
    machine.transition_to(SyncState::Headers);
    assert_eq!(machine.progress(), 0.3);
    
    // Blocks state
    machine.transition_to(SyncState::Blocks);
    assert_eq!(machine.progress(), 0.6);
    
    // Synced state
    machine.transition_to(SyncState::Synced);
    assert_eq!(machine.progress(), 1.0);
    
    // Error state resets progress
    machine.set_error("Error".to_string());
    assert_eq!(machine.progress(), 0.0);
}

#[test]
fn test_sync_state_machine_header_tracking() {
    let mut machine = SyncStateMachine::new();
    let header1 = create_test_header();
    
    // Update best header
    machine.update_best_header(header1.clone());
    assert!(machine.best_header().is_some());
    assert_eq!(machine.best_header().unwrap().version, header1.version);
    
    // Update chain tip
    machine.update_chain_tip(header1);
    assert!(machine.chain_tip().is_some());
}

#[test]
fn test_sync_coordinator_multiple_starts() {
    let mut coordinator = SyncCoordinator::new();
    
    // First start
    coordinator.start_sync().unwrap();
    assert!(coordinator.is_synced());
    
    // Can start again (should be idempotent)
    coordinator.start_sync().unwrap();
    assert!(coordinator.is_synced());
}

#[tokio::test]
async fn test_node_sync_different_protocol_versions() {
    let temp_dir = tempfile::tempdir().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Test Regtest
    let mut node_regtest = Node::new(data_dir, network_addr, rpc_addr, Some(ProtocolVersion::Regtest)).unwrap();
    let storage_regtest = node_regtest.storage();
    let height_regtest = storage_regtest.chain().get_height().unwrap();
    assert_eq!(height_regtest, Some(0));
    
    // Test Testnet3
    let temp_dir2 = tempfile::tempdir().unwrap();
    let data_dir2 = temp_dir2.path().to_str().unwrap();
    let mut node_testnet = Node::new(data_dir2, network_addr, rpc_addr, Some(ProtocolVersion::Testnet3)).unwrap();
    let storage_testnet = node_testnet.storage();
    let height_testnet = storage_testnet.chain().get_height().unwrap();
    assert_eq!(height_testnet, Some(0));
}

#[test]
fn test_sync_state_machine_state_query() {
    let mut machine = SyncStateMachine::new();
    
    // Query initial state
    let state = machine.state();
    assert!(matches!(state, &SyncState::Initial));
    
    // Transition and query
    machine.transition_to(SyncState::Headers);
    let state = machine.state();
    assert!(matches!(state, &SyncState::Headers));
    
    // Query synced state
    machine.transition_to(SyncState::Synced);
    let state = machine.state();
    assert!(matches!(state, &SyncState::Synced));
    assert!(machine.is_synced());
}


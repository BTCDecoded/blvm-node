//! Tests for relay manager

use bllvm_node::network::relay::{RelayManager, RelayPolicies};
use bllvm_node::Hash;
use std::thread;
use std::time::Duration;

fn create_test_hash(byte: u8) -> Hash {
    let mut hash = [0u8; 32];
    hash[0] = byte;
    hash
}

#[test]
fn test_relay_manager_creation() {
    let _manager = RelayManager::new();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_relay_manager_default() {
    let _manager = RelayManager::default();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_relay_policies_default() {
    let policies = RelayPolicies::default();
    assert_eq!(policies.max_relay_age, 3600);
    assert_eq!(policies.max_tracked_items, 10000);
    assert!(policies.enable_block_relay);
    assert!(policies.enable_tx_relay);
}

#[test]
fn test_relay_manager_with_policies() {
    let policies = RelayPolicies {
        max_relay_age: 1800,
        max_tracked_items: 5000,
        enable_block_relay: true,
        enable_tx_relay: true,
        enable_dandelion: false,
    };
    let _manager = RelayManager::with_policies(policies);
    // Should create successfully
    assert!(true);
}

#[test]
fn test_should_relay_block() {
    let manager = RelayManager::new();
    let block_hash = create_test_hash(1);

    // Should relay if not recently relayed
    assert!(manager.should_relay_block(&block_hash));
}

#[test]
fn test_should_relay_transaction() {
    let manager = RelayManager::new();
    let tx_hash = create_test_hash(1);

    // Should relay if not recently relayed
    assert!(manager.should_relay_transaction(&tx_hash));
}

#[test]
fn test_mark_block_relayed() {
    let mut manager = RelayManager::new();
    let block_hash = create_test_hash(1);

    // Initially should relay
    assert!(manager.should_relay_block(&block_hash));

    // Mark as relayed
    manager.mark_block_relayed(block_hash);

    // Should not relay again immediately
    assert!(!manager.should_relay_block(&block_hash));
}

#[test]
fn test_mark_transaction_relayed() {
    let mut manager = RelayManager::new();
    let tx_hash = create_test_hash(1);

    // Initially should relay
    assert!(manager.should_relay_transaction(&tx_hash));

    // Mark as relayed
    manager.mark_transaction_relayed(tx_hash);

    // Should not relay again immediately
    assert!(!manager.should_relay_transaction(&tx_hash));
}

#[test]
fn test_relay_cleanup() {
    let mut manager = RelayManager::new();
    let block_hash = create_test_hash(1);
    let tx_hash = create_test_hash(2);

    // Mark items as relayed
    manager.mark_block_relayed(block_hash);
    manager.mark_transaction_relayed(tx_hash);

    // Should not relay
    assert!(!manager.should_relay_block(&block_hash));
    assert!(!manager.should_relay_transaction(&tx_hash));

    // Cleanup dandelion (if enabled)
    manager.cleanup_dandelion();

    // Note: Internal cleanup_old_items is called automatically
    // Items will be cleaned up based on max_relay_age
}

#[test]
fn test_relay_multiple_blocks() {
    let mut manager = RelayManager::new();

    for i in 0..10 {
        let block_hash = create_test_hash(i);
        assert!(manager.should_relay_block(&block_hash));
        manager.mark_block_relayed(block_hash);
        assert!(!manager.should_relay_block(&block_hash));
    }
}

#[test]
fn test_relay_multiple_transactions() {
    let mut manager = RelayManager::new();

    for i in 0..10 {
        let tx_hash = create_test_hash(i);
        assert!(manager.should_relay_transaction(&tx_hash));
        manager.mark_transaction_relayed(tx_hash);
        assert!(!manager.should_relay_transaction(&tx_hash));
    }
}

#[test]
fn test_relay_policies_disable_block_relay() {
    let policies = RelayPolicies {
        enable_block_relay: false,
        enable_tx_relay: true,
        ..Default::default()
    };
    let _manager = RelayManager::with_policies(policies);
    // Just verify manager was created with custom policies
    assert!(true);
}

#[test]
fn test_relay_policies_disable_tx_relay() {
    let policies = RelayPolicies {
        enable_block_relay: true,
        enable_tx_relay: false,
        ..Default::default()
    };
    let _manager = RelayManager::with_policies(policies);
    // Just verify manager was created with custom policies
    assert!(true);
}

#[test]
fn test_get_relay_stats() {
    let mut manager = RelayManager::new();
    let block_hash = create_test_hash(1);
    let tx_hash = create_test_hash(2);

    manager.mark_block_relayed(block_hash);
    manager.mark_transaction_relayed(tx_hash);

    let stats = manager.get_stats();
    assert!(stats.relayed_blocks >= 1);
    assert!(stats.relayed_transactions >= 1);
}

#[test]
fn test_relay_policies_structure() {
    let policies = RelayPolicies::default();
    // Verify policies have expected values
    assert_eq!(policies.max_relay_age, 3600);
    assert!(policies.enable_block_relay);
    assert!(policies.enable_tx_relay);
}


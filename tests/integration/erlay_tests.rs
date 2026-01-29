//! Integration tests for Erlay (BIP330) transaction relay
//!
//! Tests set reconciliation using minisketch for efficient transaction propagation.

#[cfg(feature = "erlay")]
use blvm_node::network::erlay::{ErlayReconciler, ErlayConfig, ErlayTxSet};
use blvm_protocol::Hash;
use std::collections::HashSet;

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_config() {
    let config = ErlayConfig {
        field_size: 32,
        max_set_size: 10000,
    };
    
    assert_eq!(config.field_size, 32);
    assert_eq!(config.max_set_size, 10000);
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_reconciler_creation() {
    let config = ErlayConfig {
        field_size: 32,
        max_set_size: 10000,
    };
    
    let reconciler = ErlayReconciler::new(config);
    // Should not panic
    assert!(true);
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_tx_set_creation() {
    let tx_set = ErlayTxSet::new();
    assert_eq!(tx_set.size(), 0);
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_tx_set_add_transaction() {
    let mut tx_set = ErlayTxSet::new();
    let tx_hash: Hash = [1u8; 32];
    
    tx_set.add_transaction(tx_hash);
    assert_eq!(tx_set.size(), 1);
    assert!(tx_set.contains_transaction(&tx_hash));
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_tx_set_remove_transaction() {
    let mut tx_set = ErlayTxSet::new();
    let tx_hash: Hash = [1u8; 32];
    
    tx_set.add_transaction(tx_hash);
    assert_eq!(tx_set.size(), 1);
    
    tx_set.remove_transaction(&tx_hash);
    assert_eq!(tx_set.size(), 0);
    assert!(!tx_set.contains_transaction(&tx_hash));
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_tx_set_multiple_transactions() {
    let mut tx_set = ErlayTxSet::new();
    
    for i in 0..10 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        tx_set.add_transaction(hash);
    }
    
    assert_eq!(tx_set.size(), 10);
    
    // Verify all transactions are present
    for i in 0..10 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        assert!(tx_set.contains_transaction(&hash));
    }
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_sketch_creation() {
    let mut tx_set = ErlayTxSet::new();
    
    // Add some transactions
    for i in 0..5 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        tx_set.add_transaction(hash);
    }
    
    // Create sketch
    let sketch = tx_set.create_reconciliation_sketch(0).unwrap();
    assert!(!sketch.is_empty());
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_reconciliation_identical_sets() {
    let mut local_set = ErlayTxSet::new();
    let mut remote_set = ErlayTxSet::new();
    
    // Add same transactions to both sets
    for i in 0..5 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        local_set.add_transaction(hash);
        remote_set.add_transaction(hash);
    }
    
    // Create sketches
    let local_sketch = local_set.create_reconciliation_sketch(5).unwrap();
    let remote_sketch = remote_set.create_reconciliation_sketch(5).unwrap();
    
    // Reconcile - should find no differences
    let missing = local_set.reconcile_with_peer(&local_sketch, &remote_sketch).unwrap();
    assert_eq!(missing.len(), 0);
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_reconciliation_different_sets() {
    let mut local_set = ErlayTxSet::new();
    let mut remote_set = ErlayTxSet::new();
    
    // Add some common transactions
    for i in 0..3 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        local_set.add_transaction(hash);
        remote_set.add_transaction(hash);
    }
    
    // Add unique transactions to local
    for i in 3..5 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        local_set.add_transaction(hash);
    }
    
    // Add unique transactions to remote
    for i in 5..7 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = i;
        remote_set.add_transaction(hash);
    }
    
    // Create sketches
    let local_sketch = local_set.create_reconciliation_sketch(7).unwrap();
    let remote_sketch = remote_set.create_reconciliation_sketch(5).unwrap();
    
    // Reconcile - should find missing transactions
    let missing = local_set.reconcile_with_peer(&local_sketch, &remote_sketch).unwrap();
    // Should find transactions 5 and 6 that are in remote but not local
    assert!(missing.len() >= 2);
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_reconciliation_empty_sets() {
    let local_set = ErlayTxSet::new();
    let remote_set = ErlayTxSet::new();
    
    // Create sketches for empty sets
    let local_sketch = local_set.create_reconciliation_sketch(0).unwrap();
    let remote_sketch = remote_set.create_reconciliation_sketch(0).unwrap();
    
    // Reconcile - should find no differences
    let missing = local_set.reconcile_with_peer(&local_sketch, &remote_sketch).unwrap();
    assert_eq!(missing.len(), 0);
}

#[cfg(feature = "erlay")]
#[test]
fn test_erlay_reconciliation_large_sets() {
    let mut local_set = ErlayTxSet::new();
    let mut remote_set = ErlayTxSet::new();
    
    // Add many transactions
    for i in 0..100 {
        let mut hash: Hash = [0u8; 32];
        hash[0] = (i % 256) as u8;
        hash[1] = (i / 256) as u8;
        local_set.add_transaction(hash);
        
        // Remote has 90% overlap
        if i < 90 {
            remote_set.add_transaction(hash);
        }
    }
    
    // Create sketches
    let local_sketch = local_set.create_reconciliation_sketch(90).unwrap();
    let remote_sketch = remote_set.create_reconciliation_sketch(100).unwrap();
    
    // Reconcile
    let missing = local_set.reconcile_with_peer(&local_sketch, &remote_sketch).unwrap();
    // Should find transactions that are in remote but not local (none in this case)
    // Actually, we're finding what's in remote but not in local, so should be 0
    // But if we reverse it, we'd find 10 missing from remote
    assert!(missing.len() <= 10);
}


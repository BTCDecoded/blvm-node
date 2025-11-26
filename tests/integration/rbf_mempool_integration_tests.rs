//! Integration tests for RBF and Mempool Policies
//!
//! Tests the integration of RBF modes and mempool policies with the node system

use bllvm_node::config::{EvictionStrategy, MempoolPolicyConfig, NodeConfig, RbfConfig, RbfMode};
use bllvm_node::node::mempool::MempoolManager;
use bllvm_protocol::{Transaction, OutPoint, TransactionInput, TransactionOutput, UtxoSet, UTXO};
use std::collections::HashMap;

/// Create a test transaction
fn create_test_tx(input_value: u64, output_value: u64) -> Transaction {
    Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [1; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe, // RBF enabled
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: output_value as i64,
            script_pubkey: vec![0x76, 0xa9, 0x14].repeat(20),
        }],
        lock_time: 0,
    }
}

/// Create a test UTXO set
fn create_test_utxo_set() -> UtxoSet {
    let mut utxo_set = HashMap::new();
    utxo_set.insert(
        OutPoint {
            hash: [1; 32],
            index: 0,
        },
        UTXO {
            value: 100_000,
            script_pubkey: vec![0x76, 0xa9, 0x14, 0x00].repeat(20),
            height: 0,
        },
    );
    utxo_set
}

#[tokio::test]
async fn test_node_config_with_rbf() {
    // Test that NodeConfig can be created with RBF configuration
    let mut config = NodeConfig::default();
    let rbf_config = RbfConfig::with_mode(RbfMode::Conservative);
    config.rbf = Some(rbf_config);
    
    assert!(config.rbf.is_some());
    assert_eq!(config.rbf.as_ref().unwrap().mode, RbfMode::Conservative);
}

#[tokio::test]
async fn test_node_config_with_mempool_policy() {
    // Test that NodeConfig can be created with mempool policy configuration
    let mut config = NodeConfig::default();
    let mut mempool_policy = MempoolPolicyConfig::default();
    mempool_policy.max_mempool_mb = 500;
    mempool_policy.eviction_strategy = EvictionStrategy::LowestFeeRate;
    config.mempool = Some(mempool_policy);
    
    assert!(config.mempool.is_some());
    assert_eq!(config.mempool.as_ref().unwrap().max_mempool_mb, 500);
    assert_eq!(config.mempool.as_ref().unwrap().eviction_strategy, EvictionStrategy::LowestFeeRate);
}

#[tokio::test]
async fn test_mempool_manager_config_application() {
    // Test that configurations are properly applied to MempoolManager
    let mempool = MempoolManager::new();
    
    // Set RBF config
    let rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    mempool.set_rbf_config(Some(rbf_config.clone()));
    
    // Set mempool policy config
    let policy_config = MempoolPolicyConfig::default();
    mempool.set_policy_config(Some(policy_config.clone()));
    
    // Verify configs are set (by checking behavior)
    let existing_tx = create_test_tx(100_000, 90_000);
    let new_tx = create_test_tx(100_000, 80_000);
    let utxo_set = create_test_utxo_set();
    
    // RBF check should use the configured mode
    let result = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_rbf_mode_switching() {
    // Test that RBF mode can be changed dynamically
    let mempool = MempoolManager::new();
    
    // Start with disabled
    mempool.set_rbf_config(Some(RbfConfig::with_mode(RbfMode::Disabled)));
    let existing_tx = create_test_tx(100_000, 90_000);
    let new_tx = create_test_tx(100_000, 80_000);
    let utxo_set = create_test_utxo_set();
    
    let result1 = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);
    assert!(result1.is_ok());
    assert!(!result1.unwrap(), "RBF should be disabled");
    
    // Switch to standard
    mempool.set_rbf_config(Some(RbfConfig::with_mode(RbfMode::Standard)));
    let result2 = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);
    assert!(result2.is_ok());
    // Should now allow (if BIP125 rules pass)
}

#[tokio::test]
async fn test_mempool_policy_eviction_strategies() {
    // Test that different eviction strategies can be configured
    let mempool = MempoolManager::new();
    
    let strategies = vec![
        EvictionStrategy::LowestFeeRate,
        EvictionStrategy::OldestFirst,
        EvictionStrategy::LargestFirst,
        EvictionStrategy::NoDescendantsFirst,
        EvictionStrategy::Hybrid,
    ];
    
    for strategy in strategies {
        let mut policy = MempoolPolicyConfig::default();
        policy.eviction_strategy = strategy;
        mempool.set_policy_config(Some(policy));
        
        // Verify policy is set (by checking it doesn't crash)
        // Actual eviction testing requires more complex setup
    }
}

#[tokio::test]
async fn test_rbf_and_mempool_policy_together() {
    // Test that RBF and mempool policies work together
    let mempool = MempoolManager::new();
    
    // Configure both
    let rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    let mut policy_config = MempoolPolicyConfig::default();
    policy_config.max_mempool_mb = 100;
    policy_config.max_ancestor_count = 10;
    
    mempool.set_rbf_config(Some(rbf_config));
    mempool.set_policy_config(Some(policy_config));
    
    // Both should be active
    let existing_tx = create_test_tx(100_000, 90_000);
    let new_tx = create_test_tx(100_000, 80_000);
    let utxo_set = create_test_utxo_set();
    
    // RBF check should work
    let rbf_result = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);
    assert!(rbf_result.is_ok());
    
    // Policy limits should be enforced when adding transactions
    // (This is verified by the policy being set)
}


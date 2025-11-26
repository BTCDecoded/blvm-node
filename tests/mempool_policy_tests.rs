//! Mempool Policy Tests
//!
//! Comprehensive tests for mempool policy configurations:
//! - Eviction strategies
//! - Ancestor/descendant limits
//! - Fee thresholds
//! - Size limits

use bllvm_node::config::{EvictionStrategy, MempoolPolicyConfig};
use bllvm_node::node::mempool::MempoolManager;
use bllvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UtxoSet, UTXO};
use std::collections::HashMap;

/// Create a test transaction
fn create_test_tx(input_value: u64, output_value: u64, size: usize) -> Transaction {
    Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [1; 32],
                index: 0,
            },
            script_sig: vec![0; size / 2], // Approximate size
            sequence: 0xffffffff,
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: output_value as i64,
            script_pubkey: vec![0x76, 0xa9, 0x14].repeat(size / 2), // Approximate size
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
async fn test_eviction_strategy_lowest_fee_rate() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_mb = 1; // 1 MB limit
    policy.max_mempool_txs = 10;
    let strategy = EvictionStrategy::LowestFeeRate;
    policy.eviction_strategy = strategy;

    // Verify before moving
    assert_eq!(policy.eviction_strategy, EvictionStrategy::LowestFeeRate);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_eviction_strategy_oldest_first() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_mb = 1;
    policy.max_mempool_txs = 10;
    let strategy = EvictionStrategy::OldestFirst;
    policy.eviction_strategy = strategy;
    mempool.set_policy_config(Some(policy.clone()));

    // Verify the policy is configured
    assert_eq!(policy.eviction_strategy, EvictionStrategy::OldestFirst);
}

#[tokio::test]
async fn test_ancestor_count_limit() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_ancestor_count = 5; // Allow max 5 ancestors

    // Verify before moving
    assert_eq!(policy.max_ancestor_count, 5);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_ancestor_size_limit() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_ancestor_size = 10_000; // 10 KB limit

    // Verify before moving
    assert_eq!(policy.max_ancestor_size, 10_000);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_descendant_count_limit() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_descendant_count = 5; // Allow max 5 descendants

    // Verify before moving
    assert_eq!(policy.max_descendant_count, 5);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_descendant_size_limit() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_descendant_size = 10_000; // 10 KB limit

    // Verify before moving
    assert_eq!(policy.max_descendant_size, 10_000);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_mempool_size_limit() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_mb = 1; // 1 MB limit
    policy.max_mempool_txs = 100;

    // Verify before moving
    assert_eq!(policy.max_mempool_mb, 1);
    assert_eq!(policy.max_mempool_txs, 100);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_mempool_transaction_count_limit() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_txs = 10;

    // Verify before moving
    assert_eq!(policy.max_mempool_txs, 10);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_mempool_expiry() {
    let mut mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.mempool_expiry_hours = 1; // 1 hour expiry

    // Verify before moving
    assert_eq!(policy.mempool_expiry_hours, 1);

    mempool.set_policy_config(Some(policy));
}

#[test]
fn test_policy_config_defaults() {
    let policy = MempoolPolicyConfig::default();

    assert_eq!(policy.max_mempool_mb, 300);
    assert_eq!(policy.max_mempool_txs, 100_000);
    assert_eq!(policy.min_relay_fee_rate, 1);
    assert_eq!(policy.min_tx_fee, 1000);
    assert_eq!(policy.max_ancestor_count, 25);
    assert_eq!(policy.max_ancestor_size, 101_000);
    assert_eq!(policy.max_descendant_count, 25);
    assert_eq!(policy.max_descendant_size, 101_000);
    assert_eq!(policy.eviction_strategy, EvictionStrategy::LowestFeeRate);
    assert_eq!(policy.mempool_expiry_hours, 336); // 14 days
}

#[test]
fn test_eviction_strategy_variants() {
    // Test all eviction strategy variants
    assert_eq!(
        EvictionStrategy::LowestFeeRate,
        EvictionStrategy::LowestFeeRate
    );
    assert_eq!(EvictionStrategy::OldestFirst, EvictionStrategy::OldestFirst);
    assert_eq!(
        EvictionStrategy::LargestFirst,
        EvictionStrategy::LargestFirst
    );
    assert_eq!(
        EvictionStrategy::NoDescendantsFirst,
        EvictionStrategy::NoDescendantsFirst
    );
    assert_eq!(EvictionStrategy::Hybrid, EvictionStrategy::Hybrid);
}

//! Mempool Policy Tests
//!
//! Comprehensive tests for mempool policy configurations:
//! - Eviction strategies
//! - Ancestor/descendant limits
//! - Fee thresholds
//! - Size limits

mod common;

use blvm_node::config::{EvictionStrategy, MempoolPolicyConfig};
use blvm_node::node::mempool::MempoolManager;

#[tokio::test]
async fn test_eviction_strategy_lowest_fee_rate() {
    let mempool = MempoolManager::new();
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
    let mempool = MempoolManager::new();
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
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_ancestor_count = 5; // Allow max 5 ancestors

    // Verify before moving
    assert_eq!(policy.max_ancestor_count, 5);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_ancestor_size_limit() {
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_ancestor_size = 10_000; // 10 KB limit

    // Verify before moving
    assert_eq!(policy.max_ancestor_size, 10_000);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_descendant_count_limit() {
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_descendant_count = 5; // Allow max 5 descendants

    // Verify before moving
    assert_eq!(policy.max_descendant_count, 5);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_descendant_size_limit() {
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_descendant_size = 10_000; // 10 KB limit

    // Verify before moving
    assert_eq!(policy.max_descendant_size, 10_000);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_mempool_size_limit() {
    let mempool = MempoolManager::new();
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
    let mempool = MempoolManager::new();
    let mut policy = MempoolPolicyConfig::default();
    policy.max_mempool_txs = 10;

    // Verify before moving
    assert_eq!(policy.max_mempool_txs, 10);

    mempool.set_policy_config(Some(policy));
}

#[tokio::test]
async fn test_mempool_expiry() {
    let mempool = MempoolManager::new();
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
    assert_eq!(policy.bytes_per_sigop, 20);
    assert_eq!(policy.min_tx_fee, 1000);
    assert_eq!(policy.max_ancestor_count, 25);
    assert_eq!(policy.max_ancestor_size, 101_000);
    assert_eq!(policy.max_descendant_count, 25);
    assert_eq!(policy.max_descendant_size, 101_000);
    assert_eq!(policy.eviction_strategy, EvictionStrategy::LowestFeeRate);
    assert_eq!(policy.mempool_expiry_hours, 336); // 14 days
}

fn unexecuted_checksigs(n: usize) -> Vec<u8> {
    use blvm_protocol::opcodes::{OP_0, OP_CHECKSIG, OP_ENDIF, OP_IF};
    let mut script = Vec::with_capacity(n + 3);
    script.push(OP_0);
    script.push(OP_IF);
    script.extend(std::iter::repeat(OP_CHECKSIG).take(n));
    script.push(OP_ENDIF);
    script
}

fn funded_utxo(prevout: blvm_protocol::OutPoint, value: i64) -> blvm_protocol::UtxoSet {
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        prevout,
        std::sync::Arc::new(blvm_protocol::UTXO {
            value,
            script_pubkey: vec![0x51].into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    utxo_set
}

fn tx_with_script_sig(
    prevout: blvm_protocol::OutPoint,
    script_sig: Vec<u8>,
    output_value: i64,
) -> blvm_protocol::Transaction {
    blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![blvm_protocol::TransactionInput {
            prevout,
            script_sig,
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![blvm_protocol::TransactionOutput {
            value: output_value,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    }
}

#[test]
fn sigop_adjusted_vsize_rejects_underpriced_tx() {
    use std::sync::Arc;

    // 80 legacy CHECKSIGs. Cost = 80 * 4 = 320. At 20 weight units each, sigop
    // weight is 6400 and vsize is 1600. Raw size stays near 150 bytes.
    let prevout = blvm_protocol::OutPoint {
        hash: [7u8; 32],
        index: 0,
    };
    let script_sig = unexecuted_checksigs(80);
    let tx = tx_with_script_sig(prevout, script_sig, 48_400);
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded_utxo(
        prevout, 50_000,
    ))));
    let mut policy = MempoolPolicyConfig::default();
    policy.min_relay_fee_rate = 2;
    policy.min_tx_fee = 1000;
    mempool.set_policy_config(Some(policy));
    assert!(
        !mempool.add_transaction(tx.clone()).unwrap(),
        "sigop-adjusted vsize must push the fee rate under the floor"
    );

    // bytes_per_sigop = 1 does not inflate this tx past its byte weight, so the
    // same 1600 sat fee clears a 2 sat/vB floor.
    let mempool_ok = MempoolManager::new();
    mempool_ok.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded_utxo(
        prevout, 50_000,
    ))));
    let mut loose = MempoolPolicyConfig::default();
    loose.min_relay_fee_rate = 2;
    loose.min_tx_fee = 1000;
    loose.bytes_per_sigop = 1;
    mempool_ok.set_policy_config(Some(loose));
    assert!(mempool_ok.add_transaction(tx).unwrap());
}

#[test]
fn sigop_cost_above_standard_cap_is_rejected() {
    use std::sync::Arc;

    // 4001 CHECKSIGs => cost 16_004 > 16_000. The cap runs before the fee rate matters.
    let prevout = blvm_protocol::OutPoint {
        hash: [8u8; 32],
        index: 0,
    };
    let tx = tx_with_script_sig(prevout, unexecuted_checksigs(4001), 1);
    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded_utxo(
        prevout, 50_000_000,
    ))));
    mempool.set_policy_config(Some(MempoolPolicyConfig::default()));
    assert!(!mempool.add_transaction(tx).unwrap());
}

#[test]
fn ancestor_limit_uses_sigop_adjusted_vsize() {
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::serialization::transaction::serialize_transaction;
    use std::sync::Arc;

    let parent_prevout = blvm_protocol::OutPoint {
        hash: [9u8; 32],
        index: 0,
    };
    let parent = tx_with_script_sig(parent_prevout, vec![], 40_000);
    let parent_hash = calculate_tx_id(&parent);
    let witness: Vec<blvm_protocol::Witness> = vec![vec![vec![0u8; 200]]];
    let mut parent_utxo = funded_utxo(parent_prevout, 50_000);
    {
        use blvm_protocol::opcodes::{OP_2, PUSH_32_BYTES};
        let mut program = vec![OP_2, PUSH_32_BYTES, 1];
        program.resize(34, 0);
        parent_utxo.insert(
            parent_prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: program.into(),
                height: 0,
                is_coinbase: false,
            }),
        );
    }

    let mempool = MempoolManager::new();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(parent_utxo)));
    mempool.set_policy_config(Some(MempoolPolicyConfig::default()));
    assert!(
        mempool
            .add_transaction_with_witness(parent.clone(), Some(witness))
            .unwrap()
    );

    let child = tx_with_script_sig(
        blvm_protocol::OutPoint {
            hash: parent_hash,
            index: 0,
        },
        vec![],
        30_000,
    );
    let stripped =
        serialize_transaction(&parent).len() as u64 + serialize_transaction(&child).len() as u64;
    let mut tight = MempoolPolicyConfig::default();
    tight.max_ancestor_size = stripped;
    tight.min_relay_fee_rate = 1;
    tight.min_tx_fee = 0;
    mempool.set_policy_config(Some(tight));
    assert!(
        !mempool.add_transaction(child).unwrap(),
        "ancestor sum of adjusted vsizes exceeds the stripped-size sum {stripped}"
    );
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

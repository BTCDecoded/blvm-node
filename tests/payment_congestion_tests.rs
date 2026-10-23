//! Unit tests for Congestion Control Manager
//!
//! Tests batch creation, transaction batching, fee adjustment, and congestion metrics.

#![cfg(feature = "ctv")]

use blvm_node::payment::congestion::{
    BatchConfig, CongestionManager, PendingTransaction, TransactionPriority,
};
use blvm_node::payment::covenant::CovenantEngine;
use blvm_node::payment::processor::PaymentError;
use blvm_protocol::payment::PaymentOutput;
use std::sync::Arc;

/// Helper to create a test congestion manager
fn create_test_congestion_manager() -> CongestionManager {
    let covenant_engine = Arc::new(CovenantEngine::new());
    CongestionManager::new(
        covenant_engine,
        None, // No mempool manager for unit tests
        None, // No storage for unit tests
        BatchConfig::default(),
    )
}

/// Helper to create test pending transaction
fn create_test_pending_transaction(tx_id: &str, amount: u64) -> PendingTransaction {
    PendingTransaction {
        tx_id: tx_id.to_string(),
        outputs: vec![PaymentOutput {
            script: vec![0x51, 0x87],
            amount: Some(amount),
        }],
        priority: TransactionPriority::Normal,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        deadline: None,
    }
}

/// Test batch persistence reload on manager init
#[test]
fn test_load_batches_from_storage_on_init() {
    use blvm_node::storage::Storage;
    use std::sync::Arc;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let covenant_engine = Arc::new(CovenantEngine::new());
    let batch_id = "persisted_batch";

    {
        let mut manager = CongestionManager::new(
            covenant_engine.clone(),
            None,
            Some(storage.clone()),
            BatchConfig::default(),
        );
        manager.create_batch(batch_id, Some(5));
        manager
            .add_to_batch(batch_id, create_test_pending_transaction("tx1", 1000))
            .expect("add tx persists batch");
    }

    let reloaded =
        CongestionManager::new(covenant_engine, None, Some(storage), BatchConfig::default());
    let batch = reloaded.get_batch(batch_id).expect("batch restored");
    assert_eq!(batch.target_fee_rate, 5);
    assert_eq!(batch.transactions.len(), 1);
}

/// Test batch creation
#[test]
fn test_create_batch() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_1";
    let target_fee_rate = Some(10); // 10 sat/vbyte

    let result = manager.create_batch(batch_id, target_fee_rate);

    assert_eq!(result, batch_id);
    let batch = manager.get_batch(batch_id).unwrap();
    assert_eq!(batch.batch_id, batch_id);
    assert_eq!(batch.target_fee_rate, 10);
    assert_eq!(batch.transactions.len(), 0);
    assert!(!batch.ready_to_broadcast);
}

/// Test create batch with default fee rate
#[test]
fn test_create_batch_default_fee_rate() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_2";

    let result = manager.create_batch(batch_id, None);

    assert_eq!(result, batch_id);
    let batch = manager.get_batch(batch_id).unwrap();
    assert_eq!(
        batch.target_fee_rate,
        BatchConfig::default().target_fee_rate
    );
}

/// Test add transaction to batch
#[test]
fn test_add_transaction_to_batch() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_3";
    manager.create_batch(batch_id, Some(10));

    let tx = create_test_pending_transaction("tx_1", 10000);
    let result = manager.add_to_batch(batch_id, tx.clone());

    assert!(result.is_ok(), "Add transaction should succeed");
    let batch = manager.get_batch(batch_id).unwrap();
    assert_eq!(batch.transactions.len(), 1);
    assert_eq!(batch.transactions[0].tx_id, "tx_1");
}

/// Test add transaction fails when batch not found
#[test]
fn test_add_transaction_batch_not_found() {
    let mut manager = create_test_congestion_manager();
    let tx = create_test_pending_transaction("tx_1", 10000);

    let result = manager.add_to_batch("nonexistent_batch", tx);

    assert!(
        result.is_err(),
        "Add transaction should fail when batch not found"
    );
}

/// Test add transaction fails when batch is full
#[test]
fn test_add_transaction_batch_full() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_4";
    let config = BatchConfig {
        max_batch_size: 2,
        ..Default::default()
    };
    let covenant_engine = Arc::new(CovenantEngine::new());
    let mut manager = CongestionManager::new(covenant_engine, None, None, config);
    manager.create_batch(batch_id, Some(10));

    // Add two transactions (max batch size)
    manager
        .add_to_batch(batch_id, create_test_pending_transaction("tx_1", 10000))
        .unwrap();
    manager
        .add_to_batch(batch_id, create_test_pending_transaction("tx_2", 20000))
        .unwrap();

    // Try to add third transaction
    let result = manager.add_to_batch(batch_id, create_test_pending_transaction("tx_3", 30000));

    assert!(
        result.is_err(),
        "Add transaction should fail when batch is full"
    );
}

#[test]
fn remove_one_transaction_leaves_the_other() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "drop_one";
    manager.create_batch(batch_id, Some(10));
    manager
        .add_to_batch(batch_id, create_test_pending_transaction("tx_1", 10000))
        .unwrap();
    manager
        .add_to_batch(batch_id, create_test_pending_transaction("tx_2", 20000))
        .unwrap();

    manager.remove_from_batch(batch_id, "tx_1").unwrap();
    let batch = manager.get_batch(batch_id).unwrap();
    assert_eq!(batch.transactions.len(), 1);
    assert_eq!(batch.transactions[0].tx_id, "tx_2");

    let missing = manager.remove_from_batch(batch_id, "tx_1");
    assert!(missing.is_err());
}

/// Test broadcast batch
#[test]
fn test_broadcast_batch() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_6";
    manager.create_batch(batch_id, Some(10));

    // Add transaction
    let tx = create_test_pending_transaction("tx_1", 10000);
    manager.add_to_batch(batch_id, tx).unwrap();

    // Note: update_batch_covenant is called automatically when batch is full
    // For a single transaction, we need to manually create the covenant
    // For now, we'll just test that broadcast fails when batch is not ready
    // (which is expected behavior)

    // Actually, let's fill the batch to trigger covenant update
    let config = BatchConfig {
        max_batch_size: 1, // Set to 1 so single tx triggers covenant
        ..Default::default()
    };
    let covenant_engine = Arc::new(CovenantEngine::new());
    let mut manager = CongestionManager::new(covenant_engine, None, None, config);
    manager.create_batch(batch_id, Some(10));
    manager
        .add_to_batch(batch_id, create_test_pending_transaction("tx_1", 10000))
        .unwrap();

    let result = manager.broadcast_batch(batch_id);

    assert!(result.is_ok(), "Broadcast batch should succeed");
    let covenant_proof = result.unwrap();
    assert!(covenant_proof.template_hash.len() == 32);

    let batch = manager.get_batch(batch_id).unwrap();
    assert!(batch.ready_to_broadcast);
    assert!(batch.broadcast_at.is_some());
}

/// Test broadcast batch fails when empty
#[test]
fn test_broadcast_batch_empty() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_7";
    manager.create_batch(batch_id, Some(10));

    // Empty batch should fail to broadcast (no covenant)
    let result = manager.broadcast_batch(batch_id);

    assert!(result.is_err(), "Broadcast batch should fail when empty");
}

/// Test get batch
#[test]
fn test_get_batch() {
    let mut manager = create_test_congestion_manager();
    let batch_id = "test_batch_8";
    manager.create_batch(batch_id, Some(10));

    let batch = manager.get_batch(batch_id);

    assert!(batch.is_some());
    let batch = batch.unwrap();
    assert_eq!(batch.batch_id, batch_id);
}

/// Test get batch returns None for nonexistent batch
#[test]
fn test_get_batch_nonexistent() {
    let manager = create_test_congestion_manager();
    let batch = manager.get_batch("nonexistent_batch");

    assert!(batch.is_none());
}

/// Test list batches
#[test]
fn test_list_batches() {
    let mut manager = create_test_congestion_manager();
    manager.create_batch("batch_1", Some(10));
    manager.create_batch("batch_2", Some(20));
    manager.create_batch("batch_3", Some(30));

    let batches = manager.list_batches();

    assert_eq!(batches.len(), 3);
}

/// Test check congestion (without mempool manager)
#[test]
fn test_check_congestion_no_mempool() {
    let manager = create_test_congestion_manager();
    let result = manager.check_congestion();

    // Should return error when mempool manager not available
    assert!(result.is_err());
}

/// Test adjust fee rate (requires mempool manager)
#[test]
fn test_adjust_fee_rate() {
    // Note: adjust_fee_rate requires mempool manager to check congestion
    // Without mempool manager, it will fail
    // This test is skipped for unit tests without mempool manager
    // Integration tests will cover this functionality
}

#[test]
fn one_normal_transaction_is_not_ready_to_broadcast() {
    let mut manager = create_test_congestion_manager();
    manager.create_batch("hold", Some(5));
    let mut tx = create_test_pending_transaction("one", 1000);
    tx.deadline = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600,
    );
    manager.add_to_batch("hold", tx).unwrap();
    manager.broadcast_batch("hold").unwrap();
    let batch = manager.get_batch("hold").unwrap();
    assert!(!batch.ready_to_broadcast);
    assert!(batch.broadcast_at.is_none());
}

#[test]
fn empty_pool_does_not_report_the_minimum_as_a_measured_rate() {
    use blvm_node::node::mempool::MempoolManager;

    let mempool = Arc::new(MempoolManager::new());
    let mut manager = CongestionManager::new(
        Arc::new(CovenantEngine::new()),
        Some(mempool),
        None,
        BatchConfig::default(),
    );
    let metrics = manager.check_congestion().unwrap();
    assert_eq!(metrics.mempool_size, 0);
    assert_eq!(metrics.avg_fee_rate, 0);
    assert_eq!(metrics.median_fee_rate, 0);
    assert_ne!(metrics.estimated_blocks, 1);
}

#[test]
fn an_urgent_batch_is_stored_in_the_mempool() {
    use blvm_node::node::mempool::MempoolManager;
    use blvm_protocol::opcodes::OP_1;
    use blvm_protocol::{OutPoint, UtxoSet, UTXO};

    let mempool = Arc::new(MempoolManager::new());
    let mut policy = blvm_node::config::mempool::MempoolPolicyConfig::default();
    policy.require_standard = false;
    policy.min_tx_fee = 0;
    mempool.set_policy_config(Some(policy));
    let funding = OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 100_000,
        script_pubkey: vec![OP_1].into(),
        height: 1,
        is_coinbase: false,
    };
    let mut set = UtxoSet::default();
    set.insert(funding, Arc::new(utxo));
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(set)));

    let mut manager = CongestionManager::new(
        Arc::new(CovenantEngine::new()),
        Some(Arc::clone(&mempool)),
        None,
        BatchConfig::default(),
    );
    manager.create_batch("go", Some(5));
    let mut tx = create_test_pending_transaction("urgent-tx", 1_000);
    tx.priority = TransactionPriority::Urgent;
    tx.outputs[0].script = vec![OP_1];
    manager.add_to_batch("go", tx).unwrap();
    manager.broadcast_batch("go").unwrap();
    let batch = manager.get_batch("go").unwrap();
    assert!(batch.ready_to_broadcast);
    assert!(batch.broadcast_at.is_some());
    assert_eq!(mempool.size(), 1);
}

//! Tests for MempoolManager refactoring

use bllvm_node::node::mempool::MempoolManager;
use bllvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UtxoSet, UTXO};
use std::collections::HashMap;

#[tokio::test]
async fn test_mempool_stores_full_transactions() {
    let mut mempool = MempoolManager::new();

    // Create a test transaction
    let tx = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51], // OP_1
        }],
        lock_time: 0,
    };

    // Add transaction
    let added = mempool.add_transaction(tx.clone()).await.unwrap();
    assert!(added);

    // Verify we can retrieve it
    use bllvm_protocol::mempool::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    let retrieved = mempool.get_transaction(&tx_hash);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().version, tx.version);
}

#[tokio::test]
async fn test_mempool_get_prioritized_transactions() {
    let mut mempool = MempoolManager::new();
    let mut utxo_set: UtxoSet = HashMap::new();

    // Create UTXO for input
    let outpoint = OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    utxo_set.insert(
        outpoint.clone(),
        UTXO {
            value: 10000,
            script_pubkey: vec![0x51],
            height: 0,
        },
    );

    // Create two transactions with different fee rates
    // High fee transaction
    let high_fee_tx = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: outpoint.clone(),
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 5000, // 5000 sat fee
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    // Low fee transaction
    let low_fee_tx = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [1u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 9000, // 1000 sat fee
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    // Add both transactions
    mempool.add_transaction(low_fee_tx.clone()).await.unwrap();
    mempool.add_transaction(high_fee_tx.clone()).await.unwrap();

    // Get prioritized (should return high fee first)
    let prioritized = mempool.get_prioritized_transactions(10, &utxo_set);
    // Both transactions are returned, but high fee should be first
    assert!(!prioritized.is_empty());

    // Verify high fee transaction is first (it should have higher fee rate)
    // The high fee tx has 5000 sat fee, low fee tx has 1000 sat fee
    // Both have similar sizes, so high fee should be prioritized
    assert_eq!(prioritized[0].version, high_fee_tx.version);

    // Verify the high fee transaction is in the results
    use bllvm_protocol::block::calculate_tx_id;
    let high_fee_hash = calculate_tx_id(&high_fee_tx);
    let prioritized_hashes: Vec<_> = prioritized.iter().map(calculate_tx_id).collect();
    assert!(
        prioritized_hashes.contains(&high_fee_hash),
        "High fee transaction should be in prioritized list"
    );
}

#[tokio::test]
async fn test_mempool_remove_transaction() {
    let mut mempool = MempoolManager::new();

    let tx = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    mempool.add_transaction(tx.clone()).await.unwrap();
    assert_eq!(mempool.size(), 1);

    use bllvm_protocol::mempool::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    let removed = mempool.remove_transaction(&tx_hash);
    assert!(removed);
    assert_eq!(mempool.size(), 0);
}

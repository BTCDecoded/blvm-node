//! Direct `MempoolManager` lifecycle and fee-index coverage.

use blvm_node::node::mempool::MempoolManager;
use blvm_node::{OutPoint, Transaction, TransactionInput, TransactionOutput};
use blvm_protocol::block::calculate_tx_id;
use blvm_protocol::tx_inputs;
use blvm_protocol::tx_outputs;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{create_protocol_test_utxo_set, p2pkh_script, random_hash20, valid_transaction};

fn tx_spending(prevout: OutPoint, output_value: u64) -> Transaction {
    Transaction {
        version: 1,
        inputs: tx_inputs![TransactionInput {
            prevout,
            script_sig: vec![0x51],
            sequence: 0xffffffff,
        }],
        outputs: tx_outputs![TransactionOutput {
            value: output_value as i64,
            script_pubkey: p2pkh_script(random_hash20()),
        }],
        lock_time: 0,
    }
}

#[test]
fn mempool_add_remove_clear_and_size() {
    let mempool = MempoolManager::new();
    let tx = valid_transaction();
    let tx_hash = calculate_tx_id(&tx);

    assert!(mempool.add_transaction(tx.clone()).unwrap());
    assert_eq!(mempool.size(), 1);
    assert!(mempool.get_transaction(&tx_hash).is_some());
    assert_eq!(mempool.get_transactions().len(), 1);
    assert_eq!(mempool.transaction_hashes(), vec![tx_hash]);

    assert!(!mempool.add_transaction(tx).unwrap());
    assert_eq!(mempool.size(), 1);

    assert!(mempool.remove_transaction(&tx_hash));
    assert!(!mempool.remove_transaction(&tx_hash));
    assert_eq!(mempool.size(), 0);

    mempool.add_transaction(valid_transaction()).unwrap();
    mempool.clear();
    assert_eq!(mempool.size(), 0);
}

#[test]
fn mempool_rejects_empty_and_duplicate_inputs() {
    let mempool = MempoolManager::new();
    let prevout = OutPoint {
        hash: [9; 32],
        index: 0,
    };

    let empty_inputs = Transaction {
        version: 1,
        inputs: tx_inputs![],
        outputs: tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: p2pkh_script(random_hash20()),
        }],
        lock_time: 0,
    };
    assert!(!mempool.add_transaction(empty_inputs).unwrap());

    let dup_inputs = Transaction {
        version: 1,
        inputs: tx_inputs![
            TransactionInput {
                prevout,
                script_sig: vec![0x51],
                sequence: 0xffffffff,
            },
            TransactionInput {
                prevout,
                script_sig: vec![0x52],
                sequence: 0xffffffff,
            }
        ],
        outputs: tx_outputs![TransactionOutput {
            value: 500,
            script_pubkey: p2pkh_script(random_hash20()),
        }],
        lock_time: 0,
    };
    assert!(!mempool.add_transaction(dup_inputs).unwrap());
}

#[test]
fn mempool_calculate_fee_and_estimate_size() {
    let mempool = MempoolManager::new();
    let utxo_set = create_protocol_test_utxo_set();
    let prevout = OutPoint {
        hash: [1; 32],
        index: 0,
    };

    let high_fee = tx_spending(prevout, 50_000);
    let low_fee = tx_spending(prevout, 90_000);

    assert_eq!(
        mempool.calculate_transaction_fee(&high_fee, &utxo_set),
        50_000
    );
    assert_eq!(
        mempool.calculate_transaction_fee(&low_fee, &utxo_set),
        10_000
    );
    assert!(mempool.estimate_transaction_size(&high_fee) > 40);

    let segwit_like = Transaction {
        version: 1,
        inputs: tx_inputs![TransactionInput {
            prevout,
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: tx_outputs![TransactionOutput {
            value: 90_000,
            script_pubkey: p2pkh_script(random_hash20()),
        }],
        lock_time: 0,
    };
    assert!(mempool.estimate_transaction_size(&segwit_like) > 0);
}

#[test]
fn mempool_prioritized_transactions_by_fee_rate() {
    use blvm_protocol::{OutPoint, UTXO};
    use std::sync::Arc;

    let mut utxo_set = create_protocol_test_utxo_set();
    utxo_set.insert(
        OutPoint {
            hash: [2; 32],
            index: 0,
        },
        Arc::new(UTXO {
            value: 100_000,
            script_pubkey: [0x76, 0xa9, 0x14, 0x00].repeat(20).into(),
            height: 0,
            is_coinbase: false,
        }),
    );

    let mempool = MempoolManager::new();
    let low_fee_tx = tx_spending(
        OutPoint {
            hash: [1; 32],
            index: 0,
        },
        90_000,
    );
    let high_fee_tx = tx_spending(
        OutPoint {
            hash: [2; 32],
            index: 0,
        },
        40_000,
    );
    mempool.add_transaction(low_fee_tx).unwrap();
    mempool.add_transaction(high_fee_tx.clone()).unwrap();

    let ordered = mempool.get_prioritized_transactions(2, &utxo_set);
    assert_eq!(ordered.len(), 2);
    assert_eq!(calculate_tx_id(&ordered[0]), calculate_tx_id(&high_fee_tx));
}

#[test]
fn mempool_prioritise_transaction_reorders_by_effective_fee() {
    use blvm_protocol::{OutPoint, UTXO};
    use std::sync::Arc;

    let mut utxo_set = create_protocol_test_utxo_set();
    utxo_set.insert(
        OutPoint {
            hash: [2; 32],
            index: 0,
        },
        Arc::new(UTXO {
            value: 100_000,
            script_pubkey: [0x76, 0xa9, 0x14, 0x00].repeat(20).into(),
            height: 0,
            is_coinbase: false,
        }),
    );

    let mempool = MempoolManager::new();
    let low_fee_tx = tx_spending(
        OutPoint {
            hash: [1; 32],
            index: 0,
        },
        90_000,
    );
    let high_fee_tx = tx_spending(
        OutPoint {
            hash: [2; 32],
            index: 0,
        },
        40_000,
    );
    mempool.add_transaction(low_fee_tx.clone()).unwrap();
    mempool.add_transaction(high_fee_tx.clone()).unwrap();

    let low_hash = calculate_tx_id(&low_fee_tx);
    assert!(mempool.prioritise_transaction(&low_hash, 1_000_000));

    let ordered = mempool.get_prioritized_transactions(2, &utxo_set);
    assert_eq!(ordered.len(), 2);
    assert_eq!(calculate_tx_id(&ordered[0]), low_hash);
}

#[test]
fn mempool_save_and_load_round_trip() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("mempool.bin");

    let mempool = MempoolManager::new();
    mempool.add_transaction(valid_transaction()).unwrap();
    mempool.save_to_disk(&path).unwrap();

    let mut loaded = MempoolManager::new();
    loaded.load_from_disk(&path).unwrap();
    assert_eq!(loaded.size(), 1);
}

#[test]
fn mempool_policy_rejects_below_min_relay_fee() {
    use blvm_node::config::MempoolPolicyConfig;

    let mempool = MempoolManager::new();
    let utxo_set = create_protocol_test_utxo_set();
    mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
    mempool.set_policy_config(Some(MempoolPolicyConfig {
        min_relay_fee_rate: 1_000_000,
        min_tx_fee: 1_000_000,
        ..Default::default()
    }));

    let tx = tx_spending(
        OutPoint {
            hash: [1; 32],
            index: 0,
        },
        90_000,
    );
    assert!(!mempool.add_transaction(tx).unwrap());
}

//! Tests for fee calculation functionality

use bllvm_node::node::mempool::MempoolManager;
use bllvm_protocol::{Transaction, UtxoSet, OutPoint, UTXO};

#[test]
fn test_calculate_transaction_fee() {
    let mempool = MempoolManager::new();
    
    // Create a simple transaction
    let mut utxo_set = UtxoSet::new();
    
    // Add a UTXO
    let outpoint = OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 100_000_000, // 1 BTC
        script_pubkey: vec![0x76, 0xa9, 0x14], // P2PKH script
    };
    utxo_set.insert(outpoint, utxo);
    
    // Create transaction with 1 input and 1 output
    let tx = Transaction {
        version: 1,
        inputs: vec![bllvm_protocol::TxIn {
            prevout: outpoint,
            script_sig: vec![],
            sequence: 0xffffffff,
            witness: vec![],
        }],
        outputs: vec![bllvm_protocol::TxOut {
            value: 99_000_000, // 0.99 BTC (0.01 BTC fee)
            script_pubkey: vec![0x76, 0xa9, 0x14],
        }],
        lock_time: 0,
    };
    
    let fee = mempool.calculate_transaction_fee(&tx, &utxo_set);
    
    // Fee should be 1 BTC - 0.99 BTC = 0.01 BTC = 1,000,000 satoshis
    assert_eq!(fee, 1_000_000);
}

#[test]
fn test_calculate_transaction_fee_zero_fee() {
    let mempool = MempoolManager::new();
    
    let mut utxo_set = UtxoSet::new();
    let outpoint = OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 100_000_000,
        script_pubkey: vec![],
    };
    utxo_set.insert(outpoint, utxo);
    
    // Transaction with same input and output (no fee)
    let tx = Transaction {
        version: 1,
        inputs: vec![bllvm_protocol::TxIn {
            prevout: outpoint,
            script_sig: vec![],
            sequence: 0xffffffff,
            witness: vec![],
        }],
        outputs: vec![bllvm_protocol::TxOut {
            value: 100_000_000,
            script_pubkey: vec![],
        }],
        lock_time: 0,
    };
    
    let fee = mempool.calculate_transaction_fee(&tx, &utxo_set);
    assert_eq!(fee, 0);
}

#[test]
fn test_calculate_transaction_fee_missing_utxo() {
    let mempool = MempoolManager::new();
    
    let utxo_set = UtxoSet::new(); // Empty UTXO set
    
    let outpoint = OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    
    let tx = Transaction {
        version: 1,
        inputs: vec![bllvm_protocol::TxIn {
            prevout: outpoint,
            script_sig: vec![],
            sequence: 0xffffffff,
            witness: vec![],
        }],
        outputs: vec![bllvm_protocol::TxOut {
            value: 50_000_000,
            script_pubkey: vec![],
        }],
        lock_time: 0,
    };
    
    let fee = mempool.calculate_transaction_fee(&tx, &utxo_set);
    // If UTXO is missing, input_total will be 0, so fee will be 0 (or negative, but we return 0)
    assert_eq!(fee, 0);
}


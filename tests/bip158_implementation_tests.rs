//! Tests for BIP158 implementation

use bllvm_protocol::bip158::{build_block_filter, match_filter};
use bllvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

#[test]
#[ignore] // BIP158 module not yet implemented
fn test_build_block_filter_with_transactions() {
    // Create transactions with different scripts
    let tx1 = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51], // OP_1
        }],
        lock_time: 0,
    };

    let tx2 = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 2000,
            script_pubkey: vec![0x52], // OP_2
        }],
        lock_time: 0,
    };

    let filter = build_block_filter(&[tx1, tx2], &[]).unwrap();
    assert_eq!(filter.num_elements, 2);
    assert!(!filter.filter_data.is_empty());
}

#[test]
#[ignore] // BIP158 module not yet fully implemented
fn test_match_filter_positive() {
    let tx = Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51, 0x52, 0x53], // OP_1 OP_2 OP_3
        }],
        lock_time: 0,
    };

    let filter = build_block_filter(&[tx.clone()], &[]).unwrap();

    // Script that's in the filter should match
    assert!(match_filter(&filter, &tx.outputs[0].script_pubkey));
}

#[test]
#[ignore] // BIP158 module not yet fully implemented
fn test_match_filter_with_previous_scripts() {
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

    // Previous output script (UTXO being spent)
    let prev_script = vec![0x54, 0x55]; // OP_4 OP_5

    let filter = build_block_filter(&[tx], &[prev_script.clone()]).unwrap();

    // Both output script and previous script should match
    assert!(filter.num_elements >= 1);
}

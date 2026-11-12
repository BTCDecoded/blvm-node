//! Test-line QA: a transaction with no inputs is rejected.

use blvm_node::node::mempool::MempoolManager;
use blvm_protocol::{Transaction, TransactionOutput, UtxoSet};

#[test]
fn preview_accept_rejects_a_transaction_with_no_inputs() {
    let mempool = MempoolManager::new();
    let tx = Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: 1,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    match mempool.preview_accept(&tx, None, &UtxoSet::default()) {
        Err(reason) => {
            assert!(
                reason == "bad-txns-vin-empty",
                "preview_accept returned {reason}"
            );
        }
        Ok(fee) => panic!("transaction with no inputs was accepted at fee {fee}"),
    }
}

//! Mempool preview rejects that are not the empty-input case.

use blvm_node::node::mempool::MempoolManager;
use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UtxoSet};

fn p2pkh() -> Vec<u8> {
    let mut script = vec![0x76, 0xa9, 0x14];
    script.extend_from_slice(&[0x11; 20]);
    script.extend_from_slice(&[0x88, 0xac]);
    script
}

fn tx(
    version: u64,
    script_sig: Vec<u8>,
    sequence: u64,
    lock_time: u64,
    script_pubkey: Vec<u8>,
) -> Transaction {
    Transaction {
        version,
        inputs: blvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0xab; 32],
                index: 0,
            },
            script_sig,
            sequence,
        }],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: 100_000,
            script_pubkey,
        }],
        lock_time,
    }
}

fn expect_reject(tx: &Transaction, expected: &str) {
    let mempool = MempoolManager::new();
    match mempool.preview_accept(tx, None, &UtxoSet::default()) {
        Err(reason) => assert_eq!(reason, expected, "preview_accept returned {reason}"),
        Ok(fee) => panic!("{expected} was accepted at fee {fee}"),
    }
}

#[test]
fn preview_accept_rejects_a_nonstandard_version() {
    expect_reject(&tx(3, Vec::new(), 0xffff_ffff, 0, p2pkh()), "version");
}

#[test]
fn preview_accept_rejects_a_nonstandard_script_pubkey() {
    expect_reject(
        &tx(1, Vec::new(), 0xffff_ffff, 0, vec![0x51]),
        "scriptpubkey",
    );
}

#[test]
fn preview_accept_rejects_an_oversize_script_sig() {
    expect_reject(
        &tx(1, vec![0; 1_651], 0xffff_ffff, 0, p2pkh()),
        "scriptsig-size",
    );
}

#[test]
fn preview_accept_rejects_a_non_pushonly_script_sig() {
    expect_reject(
        &tx(1, vec![0x61], 0xffff_ffff, 0, p2pkh()),
        "scriptsig-not-pushonly",
    );
}

#[test]
fn preview_accept_rejects_a_missing_input() {
    expect_reject(
        &tx(1, Vec::new(), 0xffff_ffff, 0, p2pkh()),
        "missing-inputs",
    );
}

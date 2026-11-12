//! Test-line QA: a prefilled compact-block index past the block is rejected.

use std::collections::HashMap;

use blvm_node::network::compact_blocks::{CompactBlock, begin_compact_assembly};
use blvm_protocol::tx_inputs;
use blvm_protocol::tx_outputs;
use blvm_protocol::{BlockHeader, Hash, Transaction};

#[test]
fn begin_compact_assembly_rejects_a_prefilled_index_past_the_block() {
    let tx = Transaction {
        version: 1,
        inputs: tx_inputs![],
        outputs: tx_outputs![blvm_protocol::TransactionOutput {
            value: 1,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };
    let compact = CompactBlock {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_231_006_505,
            bits: 0x1d00ffff,
            nonce: 1,
        },
        nonce: 1,
        short_ids: Vec::new(),
        prefilled_txs: vec![(5, tx, None)],
    };
    let pool: HashMap<Hash, (Transaction, Option<Vec<blvm_protocol::segwit::Witness>>)> =
        HashMap::new();

    match begin_compact_assembly(&compact, &pool) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("prefilled compact-block index 5 is past 1"),
                "begin_compact_assembly returned {message}"
            );
        }
        Ok(assembly) => panic!("past-the-end prefilled index was accepted: {assembly:?}"),
    }
}

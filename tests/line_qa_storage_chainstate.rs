//! Test-line QA: a snapshot tip is rejected when the header hash does not match.

use std::sync::Arc;

use blvm_node::storage::chainstate::ChainState;
use blvm_node::storage::database::{create_database, default_backend};
use blvm_protocol::BlockHeader;

#[test]
fn set_snapshot_tip_rejects_a_mismatched_header() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = create_database(dir.path(), default_backend(), None).expect("database");
    let state = ChainState::new(Arc::from(db)).expect("chainstate");
    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0; 32],
        merkle_root: [0; 32],
        timestamp: 0,
        bits: 0x1d00ffff,
        nonce: 0,
    };
    let wrong_hash = [0x11; 32];

    match state.set_snapshot_tip(&wrong_hash, 1, &header) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("snapshot header does not match the snapshot block hash"),
                "set_snapshot_tip returned {message}"
            );
        }
        Ok(()) => panic!("snapshot tip accepted a header that does not hash to the block"),
    }
}

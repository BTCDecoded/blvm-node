//! Test-line QA: witness repair refuses when the block body is gone.

use std::sync::Arc;

use blvm_node::node::parallel_ibd::local_block::try_repair_missing_witness;
use blvm_node::storage::blockstore::BlockStore;
use blvm_node::storage::database::{create_database, default_backend};
use blvm_protocol::ProtocolVersion;

#[test]
fn witness_repair_rejects_a_missing_block() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = create_database(dir.path(), default_backend(), None).expect("database");
    let blockstore = BlockStore::new(Arc::from(db)).expect("blockstore");
    let hash = [0xabu8; 32];
    blockstore
        .store_witness(&hash, &[vec![]])
        .expect("empty witness row");

    match try_repair_missing_witness(
        &blockstore,
        1,
        hash,
        &[vec![vec![vec![0x51u8]]]],
        ProtocolVersion::Regtest,
        None,
    ) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("block disappeared during witness repair"),
                "try_repair_missing_witness returned {message}"
            );
        }
        Ok(repaired) => panic!("missing block was repaired: {repaired}"),
    }
}

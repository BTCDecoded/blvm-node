//! Test-line QA: chain-info sync fails when the target height is not stored.

use blvm_node::storage::Storage;

#[test]
fn sync_chain_info_rejects_a_missing_block() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Storage::new(dir.path()).expect("storage");

    match storage.sync_chain_info_to_height(1) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("block not in blockstore"),
                "sync_chain_info_to_height returned {message}"
            );
        }
        Ok(()) => panic!("chain_info advanced to a height with no stored block"),
    }
}

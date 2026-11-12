//! Test-line QA: a reorg walk refuses a tip that is not in the block index.

use blvm_node::node::reorg_executor::collect_fork_chains;
use blvm_node::storage::Storage;

#[test]
fn collect_fork_chains_rejects_a_missing_tip() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Storage::new(dir.path()).expect("storage");
    let missing = [0xabu8; 32];

    match collect_fork_chains(&storage, storage.blocks().as_ref(), &missing, &missing) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("active tip missing from block index"),
                "collect_fork_chains returned {message}"
            );
        }
        Ok((active, candidate)) => panic!(
            "missing tip produced forks of {} and {} blocks",
            active.len(),
            candidate.len()
        ),
    }
}

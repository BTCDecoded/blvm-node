//! Test-line QA: opening a removed module tree is rejected.

use blvm_node::storage::database::{Database, Heed3Database};

#[test]
fn open_tree_rejects_a_removed_module_tree() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = Heed3Database::new(dir.path(), None).expect("heed3");

    match db.open_tree("modules") {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Module storage has been removed"),
                "open_tree returned {message}"
            );
        }
        Ok(_) => panic!("removed module tree was opened"),
    }
}

//! Test-line QA: a truncated transaction row does not decode.

use std::sync::Arc;

use blvm_node::storage::database::{Database, create_database, default_backend};
use blvm_node::storage::txindex::TxIndex;

#[test]
fn get_transaction_rejects_a_truncated_row() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = create_database(dir.path(), default_backend(), None).expect("database");
    let db: Arc<dyn Database> = Arc::from(db);
    let hash = [0xabu8; 32];
    db.open_tree("tx_by_hash")
        .expect("tree")
        .insert(hash.as_slice(), b"x")
        .expect("insert");
    let index = TxIndex::new(Arc::clone(&db)).expect("index");

    match index.get_transaction(&hash) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("unexpected end of file"),
                "get_transaction returned {message}"
            );
        }
        Ok(found) => panic!("truncated row decoded: {found:?}"),
    }
}

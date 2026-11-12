//! Test-line QA: a short UTXO key is not an outpoint.

use std::sync::Arc;

use blvm_node::storage::database::{Database, create_database, default_backend};
use blvm_node::storage::utxostore::UtxoStore;

#[test]
fn get_all_utxos_rejects_a_short_key() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let db = create_database(dir.path(), default_backend(), None).expect("database");
    let db: Arc<dyn Database> = Arc::from(db);
    db.open_tree("utxos")
        .expect("tree")
        .insert(b"x", b"y")
        .expect("insert");
    let store = UtxoStore::new(Arc::clone(&db)).expect("store");

    match store.get_all_utxos() {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Invalid outpoint key length"),
                "get_all_utxos returned {message}"
            );
        }
        Ok(_) => panic!("short key was loaded as a UTXO"),
    }
}

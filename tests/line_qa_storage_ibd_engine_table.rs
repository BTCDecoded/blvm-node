//! Test-line QA: the UTXO table refuses to open a directory.

use blvm_node::storage::ibd_engine::table::UtxoTable;

#[test]
fn open_rejects_a_directory() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    match UtxoTable::open(dir.path()) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Is a directory"),
                "UtxoTable::open returned {message}"
            );
        }
        Ok(_) => panic!("a directory was opened as a UTXO table"),
    }
}

//! Test-line QA: loading a snapshot that is not on disk is rejected.

use blvm_node::storage::assumeutxo::AssumeUtxoManager;

#[test]
fn load_snapshot_rejects_a_missing_height() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut manager = AssumeUtxoManager::new(dir.path());

    match manager.load_snapshot(1) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("No snapshot found for height 1"),
                "load_snapshot returned {message}"
            );
        }
        Ok((utxos, _)) => panic!("missing snapshot loaded {} outputs", utxos.len()),
    }
}

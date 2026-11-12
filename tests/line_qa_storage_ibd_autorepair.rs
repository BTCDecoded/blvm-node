//! Test-line QA: the repair flag cannot be written when the data dir is a file.

use blvm_node::storage::ibd_autorepair::set_ibd_utxo_repair_flag;
use std::fs;

#[test]
fn set_repair_flag_rejects_a_file_path() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let file = dir.path().join("not_a_dir");
    fs::write(&file, b"x").expect("file");

    match set_ibd_utxo_repair_flag(&file) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("write ibd_utxo_repair_required"),
                "set_ibd_utxo_repair_flag returned {message}"
            );
        }
        Ok(()) => panic!("repair flag was written under a file path"),
    }
}

//! Test-line QA: DiskIndex::new rejects a path that is a file.

use blvm_node::storage::ibd_engine::disk_index::DiskIndex;
use std::fs;

#[test]
fn disk_index_new_rejects_a_file_path() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let file = dir.path().join("not-a-directory");
    fs::write(&file, b"x").expect("seed file");

    match DiskIndex::new(&file) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("File exists") || message.contains("Not a directory"),
                "DiskIndex::new returned {message}"
            );
        }
        Ok(_) => panic!("DiskIndex::new accepted a file path"),
    }
}

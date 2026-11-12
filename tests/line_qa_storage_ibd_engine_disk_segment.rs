//! Test-line QA: peek_max_height rejects a segment with a bad magic.

use blvm_node::storage::ibd_engine::disk_segment::DiskSegment;
use std::fs;

#[test]
fn peek_max_height_rejects_bad_magic() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("seg_bad.bin");
    fs::write(&path, [0u8; 24]).expect("header");

    match DiskSegment::peek_max_height(&path) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("bad segment magic"),
                "peek_max_height returned {message}"
            );
        }
        Ok(height) => panic!("bad segment magic was accepted as height {height}"),
    }
}

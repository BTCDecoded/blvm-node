//! Test-line QA: a directory without CURRENT is not a LevelDB database.

use blvm_node::storage::bitcoin_detection::BitcoinCoreDetection;

#[test]
fn detect_db_format_rejects_a_directory_without_current() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    match BitcoinCoreDetection::detect_db_format(dir.path()) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("CURRENT file not found - not a LevelDB database"),
                "detect_db_format returned {message}"
            );
        }
        Ok(format) => panic!("empty directory was detected as {format:?}"),
    }
}

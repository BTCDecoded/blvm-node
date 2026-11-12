//! Test-line QA: a key that was never appended is a miss.

use blvm_node::storage::ibd_engine::index::UtxoIndex;

#[test]
fn lookup_key_misses_an_absent_key() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let index = UtxoIndex::open(dir.path(), 1024).expect("index");
    let key = [0xab; 36];

    match index.lookup_key(&key) {
        None => {}
        Some(id) => panic!("absent key resolved to output id {id}"),
    }
}

//! Test-line QA: an empty age tier misses a key.

use blvm_node::storage::ibd_engine::memory_age::MemoryAge;

#[test]
fn lookup_key_misses_an_absent_key() {
    let age = MemoryAge::new(true, 4);
    let key = [0xabu8; 36];
    match age.lookup_key(&key, 0, i32::MAX) {
        None => {}
        Some(id) => panic!("empty age tier returned output {id}"),
    }
}

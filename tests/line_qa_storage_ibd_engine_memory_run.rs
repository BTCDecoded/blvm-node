//! Test-line QA: an empty memory run misses a key.

use blvm_node::storage::ibd_engine::memory_run::MemoryRun;

#[test]
fn lookup_key_misses_an_absent_key() {
    let run = MemoryRun::new_mutable();
    let key = [0xabu8; 36];
    match run.lookup_key(&key, 0, i32::MAX) {
        None => {}
        Some(id) => panic!("empty memory run returned output {id}"),
    }
}

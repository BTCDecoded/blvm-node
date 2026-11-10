#![no_main]
//! Pure mempool helpers: RBF signaling and conflict detection (no async node state).
use blvm_protocol::mempool::{has_conflict_with_tx, signals_rbf};
use blvm_protocol::serialization::transaction::deserialize_transaction;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let mid = data.len() / 2;
    let (a, b) = data.split_at(mid);

    if let Ok(t1) = deserialize_transaction(a) {
        let _ = signals_rbf(&t1);
        if let Ok(t2) = deserialize_transaction(b) {
            let _ = signals_rbf(&t2);
            let _ = has_conflict_with_tx(&t1, &t2);
        }
    }
});

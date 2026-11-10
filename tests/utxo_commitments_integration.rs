//! Wires `tests/integration/utxo_commitments_tests.rs` into a Cargo integration test binary.
//! Without this root `tests/*.rs` crate, files under `tests/integration/` are not compiled.

#[path = "integration/utxo_commitments_tests.rs"]
mod utxo_commitments_tests;

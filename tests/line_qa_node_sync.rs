//! Test-line QA: connecting an empty wire payload is rejected.

use std::sync::Arc;

use blvm_node::node::sync::SyncCoordinator;
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion, UtxoSet};

#[test]
fn connect_block_wire_rejects_an_empty_payload() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Arc::new(Storage::new(dir.path()).expect("storage"));
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest).expect("protocol");
    let mut utxos = UtxoSet::default();
    let mut coordinator = SyncCoordinator::new();

    match coordinator.connect_block_wire(
        storage.blocks().as_ref(),
        &protocol,
        &storage,
        &[],
        0,
        &mut utxos,
    ) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Failed to parse block from wire format"),
                "connect_block_wire returned {message}"
            );
        }
        Ok(accepted) => panic!("empty payload was connected: {accepted}"),
    }
}

//! Resume gap stall: local block load + coordinator inject (PR1).

use blvm_node::node::parallel_ibd::local_block::{
    LocalBlockMiss, coordinator_inject_local_gap, ibd_local_gap_fill_enabled,
    try_load_local_ibd_block_with_reason,
};
use blvm_node::storage::blockstore::BlockStore;
use blvm_node::storage::database::{create_database, default_backend};
use blvm_protocol::ProtocolVersion;
use rustc_hash::FxHashSet;
use std::sync::Arc;
use tempfile::TempDir;

fn temp_blockstore() -> BlockStore {
    let dir = TempDir::new().unwrap();
    let db: Arc<dyn blvm_node::storage::database::Database> =
        Arc::from(create_database(dir.path(), default_backend(), None).unwrap());
    std::mem::forget(dir);
    BlockStore::new(db).unwrap()
}

#[test]
fn local_gap_fill_enabled_by_default() {
    assert!(ibd_local_gap_fill_enabled());
}

#[test]
fn coordinator_inject_returns_false_when_body_missing() {
    // Eligibility is uncapped by default; missing height→hash / body still fails inject.
    let mut buf = std::collections::BTreeMap::new();
    let mut logged = FxHashSet::default();
    let blockstore = temp_blockstore();
    let mut already = FxHashSet::default();
    let ok = coordinator_inject_local_gap(
        &blockstore,
        ProtocolVersion::BitcoinV1,
        100,
        50,
        99,
        &mut buf,
        &already,
        &mut logged,
        false,
    )
    .unwrap();
    assert!(!ok);
    assert!(buf.is_empty());
}

#[test]
fn try_load_reports_not_in_store_for_missing_block() {
    let blockstore = temp_blockstore();
    let hash = [0xABu8; 32];
    let r = try_load_local_ibd_block_with_reason(&blockstore, 1, hash, ProtocolVersion::BitcoinV1)
        .unwrap();
    assert_eq!(r, Err(LocalBlockMiss::NotInStore));
}

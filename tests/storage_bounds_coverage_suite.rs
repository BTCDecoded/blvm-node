//! Storage bounds, disk metrics, and index stats.

use blvm_node::storage::Storage;
use tempfile::TempDir;

mod common;
use common::setup_mining_chain_on;

#[test]
fn test_check_storage_bounds_on_empty_db() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    assert!(storage.check_storage_bounds().unwrap());
}

#[test]
fn test_check_storage_bounds_after_chain_seed() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    setup_mining_chain_on(&storage, 32).unwrap();
    assert!(storage.check_storage_bounds().unwrap());
    assert!(storage.blocks().block_count().unwrap() > 0);
}

#[test]
fn test_disk_size_and_transaction_count() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    setup_mining_chain_on(&storage, 8).unwrap();
    assert!(storage.disk_size().unwrap() > 0);
    let stats = storage.transactions().get_index_stats().unwrap();
    assert!(stats.total_transactions >= 0);
}

#[test]
fn test_is_pruning_disabled_by_default() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    assert!(!storage.is_pruning_enabled());
}

#[test]
fn test_check_storage_bounds_with_pruning_enabled() {
    use blvm_node::config::{PruningConfig, PruningMode};
    use blvm_node::storage::database::default_backend;

    let temp_dir = TempDir::new().unwrap();
    let pruning = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 50,
        },
        auto_prune: true,
        auto_prune_interval: 100,
        min_blocks_to_keep: 50,
        prune_on_startup: false,
        incremental_prune_during_ibd: false,
        prune_window_size: 50,
        min_blocks_for_incremental_prune: 288,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        bip158_filters: None,
    };
    let storage = Storage::with_backend_pruning_and_indexing(
        temp_dir.path(),
        default_backend(),
        Some(pruning),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    setup_mining_chain_on(&storage, 64).unwrap();
    assert!(storage.is_pruning_enabled());
    assert!(storage.check_storage_bounds().unwrap());
}

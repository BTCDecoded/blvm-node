//! Pruning manager policy and error paths.

use blvm_node::config::{PruningConfig, PruningMode};
use blvm_node::storage::pruning::PruningManager;
use blvm_node::storage::Storage;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::setup_mining_chain;

fn normal_pruning_config() -> PruningConfig {
    PruningConfig {
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
    }
}

#[test]
fn test_pruning_manager_disabled_rejects_prune() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mut config = normal_pruning_config();
    config.mode = PruningMode::Disabled;
    let pm = PruningManager::new(config, storage.blocks());
    assert!(!pm.is_enabled());
    assert!(pm.prune_to_height(10, 200, false).is_err());
}

#[test]
fn test_pruning_manager_should_auto_prune_interval() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let pm = PruningManager::new(normal_pruning_config(), storage.blocks());
    assert!(!pm.should_auto_prune(50, None));
    assert!(pm.should_auto_prune(100, None));
    assert!(!pm.should_auto_prune(150, Some(100)));
    assert!(pm.should_auto_prune(200, Some(100)));
}

#[test]
fn test_pruning_manager_incremental_ibd_noop_when_disabled() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let pm = PruningManager::new(normal_pruning_config(), storage.blocks());
    let result = pm.incremental_prune_during_ibd(500, true).unwrap();
    assert!(result.is_none());
}

fn chain_storage(blocks: u64) -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, blocks).unwrap();
    (temp_dir, storage)
}

#[test]
fn test_prune_to_height_rejects_during_ibd_without_incremental() {
    let (_dir, storage) = chain_storage(120);
    let pm = PruningManager::new(normal_pruning_config(), storage.blocks());
    assert!(pm.prune_to_height(20, 119, true).is_err());
}

#[test]
fn test_prune_to_height_normal_mode_smoke() {
    let (_dir, storage) = chain_storage(120);
    let pm = PruningManager::new(normal_pruning_config(), storage.blocks());
    let stats = pm.prune_to_height(20, 119, false).unwrap();
    assert!(stats.blocks_pruned > 0 || stats.blocks_kept > 0);
    assert_eq!(pm.get_stats().last_prune_height, Some(20));
}

#[test]
fn test_storage_with_pruning_config_enables_manager() {
    use blvm_node::storage::database::default_backend;

    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::with_backend_pruning_and_indexing(
        temp_dir.path(),
        default_backend(),
        Some(normal_pruning_config()),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(storage.is_pruning_enabled());
    assert!(storage.pruning().is_some());
}

#[test]
fn test_pruning_custom_mode_keeps_metadata() {
    let (_dir, storage) = chain_storage(80);
    let config = PruningConfig {
        mode: PruningMode::Custom {
            keep_headers: true,
            keep_bodies_from_height: 10,
            keep_commitments: false,
            keep_filters: false,
            keep_filtered_blocks: false,
            keep_witnesses: false,
            keep_tx_index: false,
        },
        auto_prune: false,
        auto_prune_interval: 100,
        min_blocks_to_keep: 20,
        prune_on_startup: false,
        incremental_prune_during_ibd: false,
        prune_window_size: 50,
        min_blocks_for_incremental_prune: 288,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        bip158_filters: None,
    };
    let pm = PruningManager::new(config, storage.blocks());
    let stats = pm.prune_to_height(15, 79, false).unwrap();
    assert!(stats.blocks_pruned > 0);
    assert!(stats.blocks_kept > 0);
}

#[test]
fn test_prune_to_height_rejects_future_prune_height() {
    let (_dir, storage) = chain_storage(40);
    let pm = PruningManager::new(normal_pruning_config(), storage.blocks());
    assert!(pm.prune_to_height(50, 39, false).is_err());
}

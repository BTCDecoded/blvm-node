//! Tests for pruning manager

use blvm_node::config::{PruningConfig, PruningMode};
use blvm_node::storage::blockstore::BlockStore;
use blvm_node::storage::database::create_database;
use blvm_node::storage::pruning::PruningManager;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_blockstore() -> (TempDir, Arc<BlockStore>) {
    let temp_dir = TempDir::new().unwrap();
    // Use default-stack backend (`rocksdb` in default features; use `Redb` if testing with `--features redb`).
    let db = Arc::from(
        create_database(
            temp_dir.path(),
            blvm_node::storage::database::default_backend(),
            None,
        )
        .unwrap(),
    );
    let blockstore = Arc::new(BlockStore::new(db).unwrap());
    (temp_dir, blockstore)
}

#[test]
fn test_pruning_manager_creation() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        ..Default::default()
    };

    let _manager = PruningManager::new(config, blockstore);
    // Should create successfully
}

#[test]
fn test_pruning_manager_with_network() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        ..Default::default()
    };

    let manager = PruningManager::new(config, blockstore)
        .with_network(blvm_protocol::types::Network::Regtest);
    assert!(!manager.is_enabled());
}

#[test]
fn test_pruning_manager_disabled_mode() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        ..Default::default()
    };

    let manager = PruningManager::new(config, blockstore);
    assert!(!manager.is_enabled());
}

#[test]
fn test_pruning_manager_normal_mode() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Normal,
        keep_blocks: 288,
        ..Default::default()
    };

    let manager = PruningManager::new(config, blockstore);
    assert!(manager.is_enabled());
}

#[test]
fn test_pruning_manager_aggressive_mode() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Aggressive,
        keep_blocks: 144,
        ..Default::default()
    };

    let manager = PruningManager::new(config, blockstore);
    assert!(manager.is_enabled());
}

#[test]
fn test_pruning_stats_default() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        ..Default::default()
    };

    let manager = PruningManager::new(config, blockstore);
    let stats = manager.get_stats();

    assert_eq!(stats.blocks_pruned, 0);
    assert_eq!(stats.blocks_kept, 0);
    assert_eq!(stats.headers_kept, 0);
    assert_eq!(stats.storage_freed, 0);
    assert_eq!(stats.last_prune_height, None);
}

#[test]
fn test_pruning_config_clone() {
    let config = PruningConfig {
        mode: PruningMode::Normal,
        keep_blocks: 288,
        ..Default::default()
    };

    let config2 = config.clone();
    assert_eq!(config2.mode, PruningMode::Normal);
    assert_eq!(config2.keep_blocks, 288);
}

#[test]
fn test_pruning_mode_variants() {
    // Test all pruning mode variants
    let modes = [
        PruningMode::Disabled,
        PruningMode::Normal,
        PruningMode::Aggressive,
    ];

    for mode in modes {
        let (_temp_dir, blockstore) = create_test_blockstore();
        let config = PruningConfig {
            mode,
            ..Default::default()
        };
        let manager = PruningManager::new(config, blockstore);

        match mode {
            PruningMode::Disabled => assert!(!manager.is_enabled()),
            _ => assert!(manager.is_enabled()),
        }
    }
}

//! Tests for pruning manager

use bllvm_node::config::{PruningConfig, PruningMode};
use bllvm_node::storage::blockstore::BlockStore;
use bllvm_node::storage::database::create_database;
use bllvm_node::storage::pruning::PruningManager;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_blockstore() -> (TempDir, Arc<BlockStore>) {
    let temp_dir = TempDir::new().unwrap();
    let db = Arc::from(create_database(temp_dir.path(), bllvm_node::storage::database::DatabaseBackend::Redb).unwrap());
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
    assert!(true);
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
        mode: PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 288, // ~2 days
        },
        ..Default::default()
    };

    let manager = PruningManager::new(config, blockstore);
    assert!(manager.is_enabled());
}

#[test]
fn test_pruning_manager_aggressive_mode() {
    let (_temp_dir, blockstore) = create_test_blockstore();
    let config = PruningConfig {
        mode: PruningMode::Aggressive {
            keep_from_height: 0,
            keep_commitments: false,
            keep_filtered_blocks: false,
            min_blocks: 144, // ~1 day
        },
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
        mode: PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 288,
        },
        ..Default::default()
    };

    // Should be able to clone
    let config2 = config.clone();
    match config2.mode {
        PruningMode::Normal {
            min_recent_blocks, ..
        } => assert_eq!(min_recent_blocks, 288),
        _ => panic!("Expected Normal mode"),
    }
}

#[test]
fn test_pruning_mode_variants() {
    // Test all pruning mode variants
    let modes = vec![
        PruningMode::Disabled,
        PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 288,
        },
        PruningMode::Aggressive {
            keep_from_height: 0,
            keep_commitments: false,
            keep_filtered_blocks: false,
            min_blocks: 144,
        },
    ];

    for mode in modes {
        let (_temp_dir, blockstore) = create_test_blockstore();
        let config = PruningConfig {
            mode: mode.clone(),
            ..Default::default()
        };
        let manager = PruningManager::new(config, blockstore);

        match mode {
            PruningMode::Disabled => assert!(!manager.is_enabled()),
            _ => assert!(manager.is_enabled()),
        }
    }
}


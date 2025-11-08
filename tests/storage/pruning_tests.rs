//! Unit tests for pruning functionality

use bllvm_node::config::{PruningConfig, PruningMode};
use bllvm_node::storage::blockstore::BlockStore;
use bllvm_node::storage::pruning::{PruningManager, PruningStats};
use bllvm_node::storage::Storage;
use bllvm_protocol::{Block, BlockHeader, Hash};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_block(height: u64) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505 + (height * 600), // 10 min per block
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![],
    }
}

#[test]
fn test_pruning_manager_creation() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 100,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 144,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    assert!(manager.is_enabled());
}

#[test]
fn test_pruning_disabled() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Disabled,
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 144,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    assert!(!manager.is_enabled());
    
    // Should fail to prune when disabled
    let result = manager.prune_to_height(100, 200, false);
    assert!(result.is_err());
}

#[test]
fn test_should_auto_prune() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 100,
            min_recent_blocks: 50,
        },
        auto_prune: true,
        auto_prune_interval: 144,
        min_blocks_to_keep: 144,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    
    // Should not prune before interval
    assert!(!manager.should_auto_prune(100, None));
    
    // Should prune after interval
    assert!(manager.should_auto_prune(144, None));
    
    // Should not prune if recently pruned
    assert!(!manager.should_auto_prune(200, Some(100)));
    
    // Should prune if interval passed since last prune
    assert!(manager.should_auto_prune(250, Some(100)));
}

#[test]
fn test_pruning_ibd_protection() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 100,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 144,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    
    // Should fail during IBD
    let result = manager.prune_to_height(50, 100, true);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("initial block download"));
}

#[test]
fn test_pruning_height_validation() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 100,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 144,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    
    // Should fail if prune height >= current height
    let result = manager.prune_to_height(200, 200, false);
    assert!(result.is_err());
    
    let result = manager.prune_to_height(300, 200, false);
    assert!(result.is_err());
}

#[test]
fn test_pruning_min_blocks_validation() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 100,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 144,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    
    // Should fail if pruning would leave fewer than min_blocks_to_keep
    // Current height: 200, prune to: 100, would leave 100 blocks (less than 144)
    let result = manager.prune_to_height(100, 200, false);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("minimum"));
}

#[test]
fn test_pruning_stats() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 50,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 50, // Lower for testing
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    
    // Get initial stats
    let initial_stats = manager.get_stats();
    assert_eq!(initial_stats.blocks_pruned, 0);
    assert_eq!(initial_stats.blocks_kept, 0);
    
    // Note: Actual pruning would require blocks to be stored first
    // This test just verifies stats structure
}

#[test]
fn test_pruning_mode_normal() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 100,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 50,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    assert!(manager.is_enabled());
    
    // Verify config matches
    match manager.config.mode {
        PruningMode::Normal { keep_from_height, min_recent_blocks } => {
            assert_eq!(keep_from_height, 100);
            assert_eq!(min_recent_blocks, 50);
        }
        _ => panic!("Expected Normal mode"),
    }
}

#[test]
fn test_pruning_mode_custom() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let blockstore = storage.blocks();
    
    let config = PruningConfig {
        mode: PruningMode::Custom {
            keep_headers: true,
            keep_bodies_from_height: 100,
            keep_commitments: false,
            keep_filters: false,
            keep_filtered_blocks: false,
            keep_witnesses: false,
            keep_tx_index: false,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 50,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let manager = PruningManager::new(config, blockstore);
    assert!(manager.is_enabled());
    
    // Verify config matches
    match manager.config.mode {
        PruningMode::Custom { keep_headers, keep_bodies_from_height, .. } => {
            assert!(keep_headers);
            assert_eq!(keep_bodies_from_height, 100);
        }
        _ => panic!("Expected Custom mode"),
    }
}


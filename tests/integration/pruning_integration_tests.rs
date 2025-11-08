//! Integration tests for pruning functionality

use bllvm_node::config::{PruningConfig, PruningMode};
use bllvm_node::storage::Storage;
use bllvm_protocol::{Block, BlockHeader};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_block(height: u64, prev_hash: [u8; 32]) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: prev_hash,
            merkle_root: [0u8; 32],
            timestamp: 1231006505 + (height * 600), // 10 min per block
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![],
    }
}

#[tokio::test]
async fn test_pruning_integration_normal_mode() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create storage with pruning config
    let pruning_config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 5,
            min_recent_blocks: 3,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 3, // Low for testing
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        bllvm_node::storage::database::DatabaseBackend::Sled,
        Some(pruning_config),
    ).unwrap();
    
    // Store some test blocks
    let mut prev_hash = [0u8; 32];
    for height in 0..10 {
        let block = create_test_block(height, prev_hash);
        let block_hash = storage.blocks().get_block_hash(&block);
        storage.blocks().store_block(&block).unwrap();
        storage.blocks().store_height(height, &block_hash).unwrap();
        prev_hash = block_hash;
    }
    
    // Update chain state
    let tip_hash = storage.blocks().get_block_hash(&create_test_block(9, prev_hash));
    storage.chain().set_tip_hash(&tip_hash).unwrap();
    storage.chain().set_height(9).unwrap();
    
    // Get pruning manager
    let pruning_manager = storage.pruning().unwrap();
    
    // Verify pruning is enabled
    assert!(pruning_manager.is_enabled());
    
    // Prune blocks up to height 5
    let stats = pruning_manager.prune_to_height(5, 9, false).unwrap();
    
    // Verify stats
    assert!(stats.blocks_pruned > 0);
    assert!(stats.blocks_kept > 0);
    assert_eq!(stats.last_prune_height, Some(5));
}

#[tokio::test]
async fn test_pruning_integration_auto_prune() {
    let temp_dir = TempDir::new().unwrap();
    
    let pruning_config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 5,
            min_recent_blocks: 3,
        },
        auto_prune: true,
        auto_prune_interval: 5, // Low for testing
        min_blocks_to_keep: 3,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        bllvm_node::storage::database::DatabaseBackend::Sled,
        Some(pruning_config),
    ).unwrap();
    
    let pruning_manager = storage.pruning().unwrap();
    
    // Should not prune before interval
    assert!(!pruning_manager.should_auto_prune(3, None));
    
    // Should prune after interval
    assert!(pruning_manager.should_auto_prune(5, None));
    
    // Should not prune if recently pruned
    assert!(!pruning_manager.should_auto_prune(7, Some(5)));
    
    // Should prune if interval passed
    assert!(pruning_manager.should_auto_prune(10, Some(5)));
}

#[tokio::test]
async fn test_pruning_integration_ibd_protection() {
    let temp_dir = TempDir::new().unwrap();
    
    let pruning_config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 5,
            min_recent_blocks: 3,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 3,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        bllvm_node::storage::database::DatabaseBackend::Sled,
        Some(pruning_config),
    ).unwrap();
    
    let pruning_manager = storage.pruning().unwrap();
    
    // Should fail during IBD
    let result = pruning_manager.prune_to_height(5, 10, true);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("initial block download"));
}

#[tokio::test]
async fn test_pruning_integration_custom_mode() {
    let temp_dir = TempDir::new().unwrap();
    
    let pruning_config = PruningConfig {
        mode: PruningMode::Custom {
            keep_headers: true,
            keep_bodies_from_height: 5,
            keep_commitments: false,
            keep_filters: false,
            keep_filtered_blocks: false,
            keep_witnesses: false,
            keep_tx_index: false,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 3,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        bllvm_node::storage::database::DatabaseBackend::Sled,
        Some(pruning_config),
    ).unwrap();
    
    let pruning_manager = storage.pruning().unwrap();
    assert!(pruning_manager.is_enabled());
    
    // Verify custom mode
    match pruning_manager.config.mode {
        PruningMode::Custom { keep_headers, keep_bodies_from_height, .. } => {
            assert!(keep_headers);
            assert_eq!(keep_bodies_from_height, 5);
        }
        _ => panic!("Expected Custom mode"),
    }
}

#[tokio::test]
async fn test_pruning_integration_get_stats() {
    let temp_dir = TempDir::new().unwrap();
    
    let pruning_config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 5,
            min_recent_blocks: 3,
        },
        auto_prune: false,
        auto_prune_interval: 144,
        min_blocks_to_keep: 3,
        prune_on_startup: false,
        #[cfg(feature = "utxo-commitments")]
        utxo_commitments: None,
        #[cfg(feature = "bip158")]
        bip158_filters: None,
    };
    
    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        bllvm_node::storage::database::DatabaseBackend::Sled,
        Some(pruning_config),
    ).unwrap();
    
    let pruning_manager = storage.pruning().unwrap();
    
    // Get initial stats
    let stats = pruning_manager.get_stats();
    assert_eq!(stats.blocks_pruned, 0);
    assert_eq!(stats.blocks_kept, 0);
    assert_eq!(stats.last_prune_height, None);
}

#[tokio::test]
async fn test_pruning_integration_disabled_mode() {
    let temp_dir = TempDir::new().unwrap();
    
    let pruning_config = PruningConfig {
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
    
    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        bllvm_node::storage::database::DatabaseBackend::Sled,
        Some(pruning_config),
    ).unwrap();
    
    let pruning_manager = storage.pruning().unwrap();
    
    // Should be disabled
    assert!(!pruning_manager.is_enabled());
    
    // Should fail to prune
    let result = pruning_manager.prune_to_height(5, 10, false);
    assert!(result.is_err());
}


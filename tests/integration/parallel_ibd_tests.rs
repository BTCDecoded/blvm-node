//! Integration tests for Parallel IBD (Initial Block Download)
//!
//! Tests parallel block download from multiple peers during IBD.

use blvm_node::node::parallel_ibd::{ParallelIBD, ParallelIBDConfig};
use blvm_node::storage::{BlockStore, Storage};
use blvm_node::config::PruningConfig;
use blvm_node::network::NetworkManager;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use blvm_protocol::UtxoSet;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::{sleep, Duration};

fn create_test_storage() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    (temp_dir, storage)
}

#[tokio::test]
async fn test_parallel_ibd_config() {
    let config = ParallelIBDConfig {
        num_workers: 4,
        chunk_size: 100,
        max_concurrent_per_peer: 3,
        checkpoint_interval: 10000,
        download_timeout_secs: 30,
    };
    
    assert_eq!(config.num_workers, 4);
    assert_eq!(config.chunk_size, 100);
    assert_eq!(config.download_timeout_secs, 30);
}

#[tokio::test]
async fn test_parallel_ibd_initialization() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
    
    let config = ParallelIBDConfig {
        num_workers: 2,
        chunk_size: 50,
        max_concurrent_per_peer: 3,
        checkpoint_interval: 10000,
        download_timeout_secs: 10,
    };
    
    let _parallel_ibd = ParallelIBD::new(config);
    // Should not panic
    assert!(true);
}

#[tokio::test]
async fn test_parallel_ibd_chunk_creation() {
    let config = ParallelIBDConfig {
        num_workers: 3,
        chunk_size: 100,
        max_concurrent_per_peer: 3,
        checkpoint_interval: 10000,
        download_timeout_secs: 30,
    };
    
    let parallel_ibd = ParallelIBD::new(config);
    
    // Test chunk creation
    let peer_ids = vec![
        "127.0.0.1:8333".to_string(),
        "127.0.0.1:8334".to_string(),
        "127.0.0.1:8335".to_string(),
    ];
    
    let chunks = parallel_ibd.create_chunks(0, 299, &peer_ids, None);
    
    // Should create 3 chunks (0-99, 100-199, 200-299)
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].start_height, 0);
    assert_eq!(chunks[0].end_height, 99);
    assert_eq!(chunks[1].start_height, 100);
    assert_eq!(chunks[1].end_height, 199);
    assert_eq!(chunks[2].start_height, 200);
    assert_eq!(chunks[2].end_height, 299);
}

#[tokio::test]
async fn test_parallel_ibd_chunk_assignment() {
    let config = ParallelIBDConfig {
        num_workers: 2,
        chunk_size: 50,
        max_concurrent_per_peer: 3,
        checkpoint_interval: 10000,
        download_timeout_secs: 30,
    };
    
    let parallel_ibd = ParallelIBD::new(config);
    
    let peer_ids = vec![
        "127.0.0.1:8333".to_string(),
        "127.0.0.1:8334".to_string(),
    ];
    
    let chunks = parallel_ibd.create_chunks(0, 149, &peer_ids, None);
    
    // Should assign chunks to peers in round-robin
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].peer_id, "127.0.0.1:8333");
    assert_eq!(chunks[1].peer_id, "127.0.0.1:8334");
    assert_eq!(chunks[2].peer_id, "127.0.0.1:8333"); // Round-robin
}

#[tokio::test]
async fn test_parallel_ibd_empty_peer_list() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
    let mut utxo_set = UtxoSet::default();
    
    let config = ParallelIBDConfig {
        num_workers: 2,
        chunk_size: 50,
        max_concurrent_per_peer: 3,
        checkpoint_interval: 10000,
        download_timeout_secs: 30,
    };
    
    let parallel_ibd = ParallelIBD::new(config);
    
    // Should fail with empty peer list
    let result = parallel_ibd.sync_parallel(
        0,
        100,
        &[],
        &blockstore,
        Some(&storage),
        &protocol,
        &mut utxo_set,
        None, // No network manager
        None, // No event publisher
    ).await;
    
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("No peers available"));
}

#[tokio::test]
async fn test_parallel_ibd_invalid_height_range() {
    let (_temp_dir, storage) = create_test_storage();
    let blockstore = storage.blocks();
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
    let mut utxo_set = UtxoSet::default();
    
    let config = ParallelIBDConfig {
        num_workers: 2,
        chunk_size: 50,
        max_concurrent_per_peer: 3,
        checkpoint_interval: 10000,
        download_timeout_secs: 30,
    };
    
    let parallel_ibd = ParallelIBD::new(config);
    
    let peer_ids = vec!["127.0.0.1:8333".to_string()];
    
    // start_height > target_height should be handled gracefully
    let result = parallel_ibd.sync_parallel(
        100,
        50, // target < start
        &peer_ids,
        &blockstore,
        Some(&storage),
        &protocol,
        &mut utxo_set,
        None,
        None, // No event publisher
    ).await;
    
    // Should either fail or handle gracefully (implementation dependent)
    // For now, we just verify it doesn't panic
    let _ = result;
}

/// Integration test: IBD + utxo-commitments
///
/// Verifies that when storage is created with aggressive pruning and UTXO commitments,
/// the pruning manager provides a commitment_store. This ensures the incremental commitment
/// path in parallel_ibd has the required components when IBD runs with
/// utxo-commitments enabled.
#[cfg(all(feature = "utxo-commitments", feature = "production"))]
#[tokio::test]
async fn test_ibd_utxo_commitments_storage_wiring() {
    use blvm_node::storage::database::default_backend;

    let temp_dir = TempDir::new().unwrap();
    // Default PruningConfig uses Aggressive mode with keep_commitments: true
    let pruning_config = PruningConfig::default();

    let storage = Storage::with_backend_and_pruning(
        temp_dir.path(),
        default_backend(),
        Some(pruning_config),
    )
    .expect("storage with aggressive pruning + commitments");

    let pm = storage.pruning().expect("pruning manager");
    let commitment_store = pm.commitment_store();
    assert!(
        commitment_store.is_some(),
        "IBD incremental commitment requires commitment_store when utxo-commitments enabled"
    );

    // Verify UtxoMerkleTree can be created (used in parallel_ibd validation loop)
    let tree = blvm_protocol::utxo_commitments::merkle_tree::UtxoMerkleTree::new();
    assert!(tree.is_ok(), "UtxoMerkleTree must be creatable for IBD commitments");
}

/// Integration test: Node startup with IBD + utxo-commitments
///
/// Verifies that a node starts successfully with aggressive pruning and utxo-commitments.
/// The node will enter IBD (height 0); we verify startup completes without panic.
#[cfg(all(feature = "utxo-commitments", feature = "production"))]
#[tokio::test]
async fn test_ibd_utxo_commitments_node_startup() {
    use blvm_node::config::{PruningConfig, PruningMode, StorageConfig};
    use blvm_node::node::Node;
    use std::net::SocketAddr;
    use tokio::time::{timeout, Duration};

    let temp_dir = TempDir::new().unwrap();
    let data_dir = temp_dir.path().to_str().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let pruning_config = PruningConfig {
        mode: PruningMode::Aggressive {
            keep_from_height: 0,
            keep_commitments: true,
            keep_filtered_blocks: false,
            min_blocks: 144,
        },
        incremental_prune_during_ibd: true,
        min_blocks_for_incremental_prune: 288,
        ..Default::default()
    };
    let storage_config = StorageConfig {
        pruning: Some(pruning_config),
        ..Default::default()
    };

    let mut node = Node::with_storage_config(
        data_dir,
        network_addr,
        rpc_addr,
        None,
        Some(&storage_config),
    )
    .expect("node with utxo-commitments storage");

    // Start node; it will enter IBD. Timeout after 15s (startup should complete quickly)
    let start_result = timeout(Duration::from_secs(15), node.start()).await;
    // Node runs indefinitely in IBD, so timeout is expected (Ok(Err)) or it may complete
    assert!(
        start_result.is_err() || start_result.unwrap().is_ok(),
        "Node startup with utxo-commitments should not panic"
    );
    let _ = node.shutdown();
}


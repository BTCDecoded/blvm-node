//! Headless node `run_once` / health with seeded chain.

use blvm_node::node::health::HealthStatus;
use blvm_node::node::Node;
use blvm_protocol::ProtocolVersion;
use serial_test::serial;
use std::net::SocketAddr;
use tempfile::TempDir;

mod common;
use common::setup_mining_chain_on;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_run_once_with_seeded_chain_exercises_storage_health_path() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap();

    setup_mining_chain_on(node.storage(), 16).unwrap();
    assert!(node.storage().blocks().block_count().unwrap() > 0);

    node.run_once().await.unwrap();
    assert_eq!(node.network().peer_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_health_check_report_with_seeded_chain() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap();

    setup_mining_chain_on(node.storage(), 8).unwrap();
    let report = node.health_check();
    assert!(
        report.overall_status == HealthStatus::Healthy
            || report.overall_status == HealthStatus::Degraded
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_run_once_multiple_ticks() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap();

    for _ in 0..3 {
        node.run_once().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_stop_flushes_storage() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap();

    setup_mining_chain_on(node.storage(), 4).unwrap();
    node.stop().await.unwrap();
    assert!(node.storage().blocks().block_count().unwrap() >= 4);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_with_pruning_storage_config() {
    use blvm_node::config::{PruningConfig, PruningMode, StorageConfig};

    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut storage_config = StorageConfig::default();
    storage_config.pruning = Some(PruningConfig {
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
    });

    let mut node = Node::with_storage_config(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
        Some(&storage_config),
    )
    .unwrap();

    assert!(node.storage().is_pruning_enabled());
    setup_mining_chain_on(node.storage(), 8).unwrap();
    node.run_once().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_network_queue_block_bytes_smoke() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap();

    let payload = vec![0x01, 0x02, 0x03];
    node.network().queue_block(payload.clone());
    assert_eq!(node.network().try_recv_block(), Some(payload));
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_run_once_after_disk_size_query() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap();

    setup_mining_chain_on(node.storage(), 24).unwrap();
    assert!(node.storage().disk_size().unwrap() > 0);
    node.run_once().await.unwrap();
}

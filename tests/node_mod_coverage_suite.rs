//! `Node` accessors, config builders, and health report paths.

use blvm_node::config::NodeConfig;
use blvm_node::node::Node;
use blvm_node::node::health::HealthStatus;
use blvm_protocol::ProtocolVersion;
use serial_test::serial;
use std::net::SocketAddr;
use tempfile::TempDir;

mod common;
use common::setup_mining_chain_on;

fn base_node() -> (TempDir, Node) {
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
    (temp_dir, node)
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_accessors_and_protocol() {
    let (_dir, node) = base_node();
    assert_eq!(node.network().peer_count(), 0);
    assert!(node.protocol().supports_feature("fast_mining"));
    assert!(!node.storage().is_pruning_enabled());
    assert!(node.module_manager().is_none());
    assert!(node.event_publisher().is_none());
    let _rpc = node.rpc();
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_with_modules_enables_manager() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    let socket_dir = temp_dir.path().join("sockets");
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();

    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap()
    .with_modules(&modules_dir, &socket_dir)
    .unwrap();

    assert!(node.module_manager().is_some());
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_with_modules_from_config_disabled() {
    let temp_dir = TempDir::new().unwrap();
    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut config = NodeConfig::default();
    config.modules = Some(blvm_node::config::ModuleConfig {
        enabled: false,
        modules_dir: temp_dir.path().join("modules").to_string_lossy().into(),
        data_dir: temp_dir.path().join("data").to_string_lossy().into(),
        socket_dir: temp_dir.path().join("sockets").to_string_lossy().into(),
        ..Default::default()
    });

    let node = Node::new(
        temp_dir.path().to_str().unwrap(),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .unwrap()
    .with_modules_from_config(&config)
    .unwrap();

    assert!(node.module_manager().is_none());
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_testnet_and_mainnet_protocol_init() {
    use blvm_node::config::StorageConfig;

    for version in [ProtocolVersion::Testnet3, ProtocolVersion::BitcoinV1] {
        let temp_dir = TempDir::new().unwrap();
        let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let rpc_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let node = Node::with_storage_config(
            temp_dir.path().to_str().unwrap(),
            network_addr,
            rpc_addr,
            Some(version),
            Some(&StorageConfig::default()),
        )
        .unwrap();
        assert!(node.protocol().get_network_params().magic_bytes.len() == 4);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_node_with_config_applies_settings() {
    let (_dir, mut node) = base_node();
    let mut config = NodeConfig::default();
    config.max_outbound_peers = Some(4);
    node = node.with_config(config).unwrap();
    setup_mining_chain_on(node.storage(), 6).unwrap();
    let report = node.health_check();
    assert!(
        report.overall_status == HealthStatus::Healthy
            || report.overall_status == HealthStatus::Degraded
    );
    assert!(report.uptime_seconds >= 0);
}

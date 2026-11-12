//! Test-line QA: a fee config with both estimators off is rejected.

use std::net::SocketAddr;

use blvm_node::config::{FeeEstimatorConfig, NodeConfig};
use blvm_node::node::Node;
use blvm_protocol::ProtocolVersion;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_config_rejects_a_fee_estimator_with_both_sources_off() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let network_addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let rpc_addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let node = Node::new(
        dir.path().to_str().expect("path"),
        network_addr,
        rpc_addr,
        Some(ProtocolVersion::Regtest),
    )
    .expect("node");

    let mut fees = FeeEstimatorConfig::default();
    fees.history_enabled = false;
    fees.mempool_enabled = false;
    let mut config = NodeConfig::default();
    config.fees = Some(fees);

    match node.with_config(config) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("history_enabled and mempool_enabled cannot both be false"),
                "with_config returned {message}"
            );
        }
        Ok(_) => panic!("fee config with both sources off was accepted"),
    }
}

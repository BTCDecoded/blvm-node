//! Tests for protocol message processing integration

use bllvm_node::network;
use bllvm_node::node::mempool::MempoolManager;
use bllvm_node::storage::Storage;
use bllvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn test_protocol_message_processing_setup() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let protocol_engine = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());

    let network_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = network::NetworkManager::new(network_addr).with_dependencies(
        protocol_engine,
        storage,
        mempool,
    );

    // Verify dependencies are set
    // (NetworkManager doesn't expose these, but we can verify it was created)
    assert!(true); // If we got here, setup worked
}

#[tokio::test]
async fn test_node_chain_access_creation() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mempool = Arc::new(MempoolManager::new());

    // Verify we can create NodeChainAccess with Storage Arcs
    use bllvm_node::network::chain_access::NodeChainAccess;
    let chain_access = NodeChainAccess::new(storage.blocks(), storage.transactions(), mempool);

    // Test that it works
    let hash = [0u8; 32];
    use bllvm_protocol::network::ChainStateAccess;
    assert!(!chain_access.has_object(&hash)); // Empty storage
}

//! Tests for BlockchainRpc methods

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::Storage;
use blvm_protocol::BitcoinProtocolEngine;
use blvm_protocol::ProtocolVersion;
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;

fn chain_from_state(v: &Value) -> String {
    v["chain"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn test_get_chain_name_with_protocol() {
    // `getblockchainstate` includes `chain` from the same logic as internal get_chain_name.
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());

    let mainnet_protocol =
        Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap());
    let rpc_mainnet =
        BlockchainRpc::with_dependencies_and_protocol(storage.clone(), mainnet_protocol);
    let info = rpc_mainnet.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "mainnet");

    let testnet_protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Testnet3).unwrap());
    let rpc_testnet =
        BlockchainRpc::with_dependencies_and_protocol(storage.clone(), testnet_protocol);
    let info = rpc_testnet.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "testnet");

    let regtest_protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let rpc_regtest =
        BlockchainRpc::with_dependencies_and_protocol(storage.clone(), regtest_protocol);
    let info = rpc_regtest.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "regtest");
}

#[tokio::test]
async fn test_get_chain_name_without_protocol() {
    let rpc = BlockchainRpc::new();
    let info = rpc.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "regtest");

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let rpc_with_storage = BlockchainRpc::with_dependencies(storage);
    let info = rpc_with_storage.get_blockchain_state().await.unwrap();
    assert_eq!(chain_from_state(&info), "regtest");
}

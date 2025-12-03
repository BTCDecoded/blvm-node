//! Tests for BlockchainRpc methods

use bllvm_node::rpc::blockchain::BlockchainRpc;
use bllvm_node::storage::Storage;
use bllvm_protocol::BitcoinProtocolEngine;
use bllvm_protocol::ProtocolVersion;
use std::sync::Arc;
use tempfile::TempDir;

#[test]
fn test_get_chain_name_with_protocol() {
    // Test get_chain_name() with different protocol versions
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());

    // Test mainnet
    let mainnet_protocol = Arc::new(
        BitcoinProtocolEngine::new(ProtocolVersion::BitcoinV1).unwrap(),
    );
    let rpc_mainnet = BlockchainRpc::with_dependencies_and_protocol(
        storage.clone(),
        mainnet_protocol,
    );
    assert_eq!(rpc_mainnet.get_chain_name(), "mainnet");

    // Test testnet
    let testnet_protocol = Arc::new(
        BitcoinProtocolEngine::new(ProtocolVersion::Testnet3).unwrap(),
    );
    let rpc_testnet = BlockchainRpc::with_dependencies_and_protocol(
        storage.clone(),
        testnet_protocol,
    );
    assert_eq!(rpc_testnet.get_chain_name(), "testnet");

    // Test regtest
    let regtest_protocol = Arc::new(
        BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap(),
    );
    let rpc_regtest = BlockchainRpc::with_dependencies_and_protocol(
        storage.clone(),
        regtest_protocol,
    );
    assert_eq!(rpc_regtest.get_chain_name(), "regtest");
}

#[test]
fn test_get_chain_name_without_protocol() {
    // Test get_chain_name() fallback when protocol is not set
    let rpc = BlockchainRpc::new();
    assert_eq!(
        rpc.get_chain_name(),
        "regtest",
        "Should default to regtest when protocol is not set"
    );

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let rpc_with_storage = BlockchainRpc::with_dependencies(storage);
    assert_eq!(
        rpc_with_storage.get_chain_name(),
        "regtest",
        "Should default to regtest when protocol is not set even with storage"
    );
}


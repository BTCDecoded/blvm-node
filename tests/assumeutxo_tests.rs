//! AssumeUTXO functional tests
//!
//! Verifies -assumeutxo CLI/config wiring, chainparams lookup, snapshot load path,
//! load_snapshot_from_path, and loadtxoutset RPC.

use blvm_node::config::NodeConfig;
use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::assumeutxo::{
    assumeutxo_data_for_blockhash, chainstate_snapshot_dir, clear_assumeutxo_marker,
    height_for_blockhash, is_background_validated, read_base_blockhash_marker,
    write_background_validated_marker, write_base_blockhash_marker, AssumeUtxoManager,
};
use serde_json::json;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn test_height_for_blockhash_regtest() {
    // Regtest chainparams has (100, [0x01; 32])
    let block_hash = [0x01u8; 32];
    assert_eq!(height_for_blockhash("regtest", &block_hash), Some(100));
    assert_eq!(height_for_blockhash("Regtest", &block_hash), Some(100));
}

#[test]
fn test_height_for_blockhash_mainnet_empty() {
    let block_hash = [0x01u8; 32];
    assert_eq!(height_for_blockhash("mainnet", &block_hash), None);
    assert_eq!(height_for_blockhash("bitcoinv1", &block_hash), None);
}

#[test]
fn test_height_for_blockhash_unknown_network() {
    let block_hash = [0x01u8; 32];
    assert_eq!(height_for_blockhash("unknown", &block_hash), None);
}

#[test]
fn test_assumeutxo_data_for_blockhash() {
    let block_hash = [0x01u8; 32];
    let data = assumeutxo_data_for_blockhash("regtest", &block_hash).unwrap();
    assert_eq!(data.height, 100);
    assert_eq!(data.block_hash, block_hash);
    assert_eq!(data.chain_tx_count, 101);
    assert!(assumeutxo_data_for_blockhash("mainnet", &block_hash).is_none());
}

#[test]
fn test_chainstate_snapshot_marker() {
    let dir = tempdir().unwrap();
    let block_hash = [0xab; 32];

    assert!(read_base_blockhash_marker(dir.path()).unwrap().is_none());

    write_base_blockhash_marker(dir.path(), &block_hash).unwrap();
    let snapshot_dir = chainstate_snapshot_dir(dir.path());
    assert!(snapshot_dir.join("base_blockhash").exists());

    let read = read_base_blockhash_marker(dir.path()).unwrap().unwrap();
    assert_eq!(read, block_hash);

    clear_assumeutxo_marker(dir.path()).unwrap();
    assert!(read_base_blockhash_marker(dir.path()).unwrap().is_none());
}

#[test]
fn test_background_validated_marker() {
    let dir = tempdir().unwrap();
    let block_hash = [0xab; 32];
    let other_hash = [0xcd; 32];

    assert!(!is_background_validated(dir.path(), &block_hash));

    write_base_blockhash_marker(dir.path(), &block_hash).unwrap();
    assert!(!is_background_validated(dir.path(), &block_hash));

    write_background_validated_marker(dir.path(), &block_hash).unwrap();
    assert!(is_background_validated(dir.path(), &block_hash));
    assert!(!is_background_validated(dir.path(), &other_hash));
}

#[test]
fn test_node_config_assumeutxo_blockhash() {
    let hash = [0xab; 32];
    let config = NodeConfig {
        assumeutxo_blockhash: Some(hash),
        ..Default::default()
    };
    assert_eq!(config.assumeutxo_blockhash, Some(hash));
}

#[test]
fn test_assumeutxo_snapshot_chainparams_match() {
    // Create snapshot at height 100 with block_hash [0x01; 32] (matches regtest chainparams)
    let dir = tempdir().unwrap();
    let mut manager = AssumeUtxoManager::new(dir.path());

    let mut utxo_set = blvm_protocol::UtxoSet::default();
    let outpoint = blvm_protocol::OutPoint {
        hash: [0u8; 32],
        index: 0,
    };
    let utxo = blvm_protocol::UTXO {
        value: 50_000_000,
        script_pubkey: vec![0x76, 0xa9, 0x14, 0x00, 0x88, 0xac].into(),
        is_coinbase: true,
        height: 100,
    };
    utxo_set.insert(outpoint, std::sync::Arc::new(utxo));

    let block_hash = [0x01u8; 32];
    manager.create_snapshot(&utxo_set, block_hash, 100).unwrap();

    // Verify height_for_blockhash finds it
    assert_eq!(height_for_blockhash("regtest", &block_hash), Some(100));

    // Load snapshot (verifies file format and chainparams alignment)
    let (loaded, metadata) = manager.load_snapshot(100).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(metadata.block_height, 100);
    assert_eq!(metadata.block_hash, block_hash);
}

#[test]
fn test_load_snapshot_from_path() {
    let dir = tempdir().unwrap();
    let manager = AssumeUtxoManager::new(dir.path());

    let mut utxo_set = blvm_protocol::UtxoSet::default();
    let outpoint = blvm_protocol::OutPoint {
        hash: [0x42u8; 32],
        index: 1,
    };
    let utxo = blvm_protocol::UTXO {
        value: 21_000_000,
        script_pubkey: vec![0x76, 0xa9, 0x14, 0x00, 0x88, 0xac].into(),
        is_coinbase: false,
        height: 50,
    };
    utxo_set.insert(outpoint, std::sync::Arc::new(utxo));

    let block_hash = [0xab; 32];
    manager.create_snapshot(&utxo_set, block_hash, 50).unwrap();

    let snapshot_path = dir.path().join("utxo_snapshot_50.dat");
    assert!(snapshot_path.exists());

    let manager2 = AssumeUtxoManager::new(Path::new("."));
    let (loaded, metadata) = manager2.load_snapshot_from_path(&snapshot_path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(metadata.block_height, 50);
    assert_eq!(metadata.block_hash, block_hash);
    assert_eq!(metadata.utxo_count, 1);
}

#[test]
fn test_load_snapshot_from_path_nonexistent() {
    let manager = AssumeUtxoManager::new(Path::new("."));
    let result = manager.load_snapshot_from_path(Path::new("/nonexistent/utxo_snapshot_999.dat"));
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Snapshot file not found"));
}

#[tokio::test]
async fn test_loadtxoutset_rpc() {
    let dir = tempdir().unwrap();
    let manager = AssumeUtxoManager::new(dir.path());

    let mut utxo_set = blvm_protocol::UtxoSet::default();
    let outpoint = blvm_protocol::OutPoint {
        hash: [0xcc; 32],
        index: 2,
    };
    let utxo = blvm_protocol::UTXO {
        value: 10_000_000,
        script_pubkey: vec![0x76, 0xa9, 0x14, 0x00, 0x88, 0xac].into(),
        is_coinbase: true,
        height: 200,
    };
    utxo_set.insert(outpoint, std::sync::Arc::new(utxo));

    let block_hash = [0xde; 32];
    manager.create_snapshot(&utxo_set, block_hash, 200).unwrap();

    let snapshot_path = dir.path().join("utxo_snapshot_200.dat");
    let path_str = snapshot_path.to_string_lossy().to_string();

    let rpc = BlockchainRpc::new();
    let params = json!([path_str]);
    let result = rpc.load_txout_set(&params).await.unwrap();

    assert_eq!(result["base_blockhash"], hex::encode(block_hash));
    assert_eq!(result["height"], 200);
    assert_eq!(result["txout_count"], 1);
    assert!(result.get("utxo_hash").is_some());
}

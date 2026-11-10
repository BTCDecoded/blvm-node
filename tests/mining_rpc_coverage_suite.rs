//! Mining RPC smoke with seeded chain.

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::setup_mining_chain;

fn ckpool_gbt_params() -> serde_json::Value {
    json!([{
        "capabilities": ["coinbasetxn", "workid", "coinbase/append"],
        "rules": ["segwit"]
    }])
}

fn rpc_with_chain() -> (TempDir, MiningRpc) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, 2016).unwrap();
    let mempool = Arc::new(MempoolManager::new());
    (temp_dir, MiningRpc::with_dependencies(storage, mempool))
}

fn rpc_with_regtest_protocol() -> (TempDir, MiningRpc, Arc<BitcoinProtocolEngine>, Arc<Storage>) {
    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    storage.chain().initialize(&genesis_header).unwrap();
    let mempool = Arc::new(MempoolManager::new());
    let rpc = MiningRpc::with_dependencies(Arc::clone(&storage), mempool)
        .with_protocol_engine(Arc::clone(&protocol));
    (temp_dir, rpc, protocol, storage)
}

#[tokio::test]
async fn test_get_mining_info_with_chain() {
    let (_dir, rpc) = rpc_with_chain();
    let info = rpc.get_mining_info().await.unwrap();
    assert!(info.get("blocks").unwrap().as_u64().unwrap() >= 2015);
    assert!(info.get("difficulty").is_some());
    assert!(info.get("pooledtx").is_some());
}

#[tokio::test]
async fn test_get_mining_info_without_storage() {
    let rpc = MiningRpc::new();
    let info = rpc.get_mining_info().await.unwrap();
    assert_eq!(info.get("blocks").unwrap().as_u64(), Some(0));
}

#[tokio::test]
async fn test_estimate_smart_fee_economical_mode() {
    let (_dir, rpc) = rpc_with_chain();
    let fee = rpc
        .estimate_smart_fee(&json!([12, "economical"]))
        .await
        .unwrap();
    assert!(fee.get("feerate").is_some() || fee.get("errors").is_some());
}

#[tokio::test]
async fn test_get_block_template_with_difficulty_interval_chain() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let (_dir, rpc) = rpc_with_chain();
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let template = rpc.get_block_template(&ckpool_gbt_params()).await.unwrap();
    assert!(template.get("coinbasevalue").unwrap().as_u64().unwrap() > 0);
    assert!(template.get("previousblockhash").is_some());
    assert!(template.get("target").is_some());

    // REV-TN-01: GBT must expose curtime/mintime and use live template timestamp.
    let curtime = template.get("curtime").unwrap().as_u64().unwrap();
    let mintime = template.get("mintime").unwrap().as_u64().unwrap();
    assert!(curtime >= before.saturating_sub(5));
    assert!(mintime <= curtime);
    assert!(template.get("height").unwrap().as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_generatetoaddress_regtest_smoke() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let (_dir, rpc, protocol, storage) = rpc_with_regtest_protocol();
    let genesis_ts = protocol.get_network_params().genesis_block.header.timestamp;
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let hashes = rpc
        .generate_to_address(&json!([
            2u64,
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
            2_000_000u64
        ]))
        .await
        .unwrap();
    let arr = hashes.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr[0].as_str().unwrap().len() >= 64);

    // REV-TN-01 / REV-C-32: mined blocks use wall-clock time via create_new_block_with_time.
    let tip_header = storage
        .chain()
        .get_tip_header()
        .unwrap()
        .expect("tip after generatetoaddress");
    assert!(tip_header.timestamp > genesis_ts);
    assert_ne!(tip_header.timestamp, 1_231_006_505);
    assert!(tip_header.timestamp >= before.saturating_sub(5));
}

#[tokio::test]
async fn test_prioritise_transaction_without_mempool_errors() {
    let (_dir, rpc) = rpc_with_chain();
    let missing = "00".repeat(32);
    assert!(
        rpc.prioritise_transaction(&json!([missing, 0.0, 0]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn test_submitblock_invalid_hex_errors() {
    let (_dir, rpc) = rpc_with_chain();
    assert!(rpc.submit_block(&json!(["not-hex"])).await.is_err());
}

//! Mempool RPC methods with populated mempool fixture.

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::mempool::MempoolRpc;
use blvm_node::storage::Storage;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::valid_transaction;

fn rpc_with_mempool() -> (TempDir, Arc<MempoolManager>, MempoolRpc, String) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let txid = blvm_node::storage::hashing::hash_to_rpc_hex(&calculate_tx_id(&tx));
    mempool.add_transaction(tx).unwrap();
    let rpc = MempoolRpc::with_dependencies(mempool.clone(), storage);
    (temp_dir, mempool, rpc, txid)
}

#[tokio::test]
async fn test_getmempoolinfo_without_dependencies() {
    let rpc = MempoolRpc::new();
    let info = rpc.getmempoolinfo(&json!([])).await.unwrap();
    assert_eq!(info.get("loaded").unwrap().as_bool(), Some(false));
    assert_eq!(info.get("size").unwrap().as_u64(), Some(0));
}

#[tokio::test]
async fn test_getmempoolinfo_with_transaction() {
    let (_dir, mempool, rpc, _txid) = rpc_with_mempool();
    let info = rpc.getmempoolinfo(&json!([])).await.unwrap();
    assert_eq!(info.get("loaded").unwrap().as_bool(), Some(true));
    assert_eq!(
        info.get("size").unwrap().as_u64().unwrap(),
        mempool.size() as u64
    );
    assert!(info.get("bytes").unwrap().as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_getrawmempool_non_verbose() {
    let (_dir, _mempool, rpc, _txid) = rpc_with_mempool();
    let ids = rpc.getrawmempool(&json!([])).await.unwrap();
    let arr = ids.as_array().expect("txid list");
    assert_eq!(arr.len(), 1);
    assert!(arr[0].as_str().unwrap().len() >= 64);
}

#[tokio::test]
async fn test_getrawmempool_verbose() {
    let (_dir, _mempool, rpc, _txid) = rpc_with_mempool();
    let entries = rpc.getrawmempool(&json!([true])).await.unwrap();
    let map = entries.as_object().expect("verbose map");
    assert_eq!(map.len(), 1);
    let entry = map.values().next().unwrap();
    assert!(entry.get("size").is_some());
    assert!(entry.get("fees").is_some());
}

#[tokio::test]
async fn test_savemempool_smoke() {
    let (dir, _mempool, rpc, _txid) = rpc_with_mempool();
    let prev = std::env::var("DATA_DIR").ok();
    unsafe { std::env::set_var("DATA_DIR", dir.path()) };
    let result = rpc.savemempool(&json!([])).await.unwrap();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DATA_DIR", v),
            None => std::env::remove_var("DATA_DIR"),
        }
    }
    assert!(result.is_string() || result.is_null());
    assert!(dir.path().join("mempool.dat").exists());
}

#[tokio::test]
async fn test_getmempoolentry_for_known_tx() {
    let (_dir, _mempool, rpc, txid) = rpc_with_mempool();
    let entry = rpc.getmempoolentry(&json!([txid])).await.unwrap();
    assert!(entry.get("size").is_some());
    assert!(entry.get("fee").is_some());
    assert_eq!(
        entry.get("ancestorfees").unwrap().as_f64(),
        entry.get("fee").unwrap().as_f64()
    );
}

#[tokio::test]
async fn test_getmempoolentry_modified_fee_in_package_totals() {
    let (_dir, mempool, rpc, txid) = rpc_with_mempool();
    let hash = blvm_node::storage::hashing::hash_from_rpc_hex(&txid).unwrap();
    assert!(mempool.prioritise_transaction(&hash, 50_000));

    let entry = rpc.getmempoolentry(&json!([txid])).await.unwrap();
    let base = entry.get("fee").unwrap().as_f64().unwrap();
    let modified = entry.get("modifiedfee").unwrap().as_f64().unwrap();
    assert!(modified >= base);
    assert_eq!(
        entry.get("ancestorfees").unwrap().as_f64().unwrap(),
        modified
    );
    assert_eq!(
        entry.get("descendantfees").unwrap().as_f64().unwrap(),
        modified
    );
    assert_eq!(
        entry.get("fees").unwrap().get("ancestor").unwrap().as_f64(),
        Some(modified)
    );
}

#[tokio::test]
async fn test_getmempoolancestors_and_descendants() {
    let (_dir, _mempool, rpc, txid) = rpc_with_mempool();
    let ancestors = rpc.getmempoolancestors(&json!([txid])).await.unwrap();
    assert!(ancestors.is_array());
    let descendants = rpc.getmempooldescendants(&json!([txid])).await.unwrap();
    assert!(descendants.is_array());
}

#[tokio::test]
async fn test_getmempoolentry_missing_tx_errors() {
    let (_dir, _mempool, rpc, _txid) = rpc_with_mempool();
    let missing = "00".repeat(32);
    assert!(rpc.getmempoolentry(&json!([missing])).await.is_err());
}

#[tokio::test]
async fn test_getrawmempool_without_mempool_verbose_fallback() {
    let rpc = MempoolRpc::new();
    let map = rpc.getrawmempool(&json!([true])).await.unwrap();
    assert!(map.as_object().unwrap().is_empty());
}

#[tokio::test]
async fn test_getmempoolancestors_verbose_without_mempool() {
    let rpc = MempoolRpc::new();
    let txid = "00".repeat(32);
    let map = rpc.getmempoolancestors(&json!([txid, true])).await.unwrap();
    assert!(map.as_object().unwrap().is_empty());
}

#[tokio::test]
async fn test_getmempooldescendants_verbose_with_storage() {
    let (_dir, _mempool, rpc, txid) = rpc_with_mempool();
    let map = rpc
        .getmempooldescendants(&json!([txid, true]))
        .await
        .unwrap();
    assert!(map.is_object());
}

#[tokio::test]
async fn test_getmempoolancestors_verbose_with_storage() {
    let (_dir, _mempool, rpc, txid) = rpc_with_mempool();
    let map = rpc.getmempoolancestors(&json!([txid, true])).await.unwrap();
    assert!(map.is_object());
}

#[tokio::test]
async fn test_savemempool_without_mempool_errors() {
    let rpc = MempoolRpc::new();
    assert!(rpc.savemempool(&json!([])).await.is_err());
}

#[tokio::test]
async fn test_getmempoolentry_invalid_hex_errors() {
    let (_dir, _mempool, rpc, _txid) = rpc_with_mempool();
    assert!(rpc.getmempoolentry(&json!(["not-hex"])).await.is_err());
}

fn spend(prev: [u8; 32]) -> blvm_protocol::Transaction {
    let mut tx = valid_transaction();
    tx.inputs[0].prevout.hash = prev;
    tx.inputs[0].prevout.index = 0;
    tx.inputs[0].sequence = 0xfffffffd;
    // Bare OP_TRUE so a child script_sig of OP_1 can spend the in-pool parent.
    tx.outputs[0].script_pubkey = vec![0x51].into();
    tx.outputs[0].value = 1_000;
    tx
}

#[tokio::test]
async fn test_ancestor_and_descendant_walks_and_signed_fee_delta() {
    use blvm_protocol::block::calculate_tx_id;
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mut policy = blvm_node::config::mempool::MempoolPolicyConfig::default();
    policy.max_mempool_mb = 1;
    policy.min_relay_fee_rate = 5;
    mempool.set_policy_config(Some(policy));

    let a = spend([0x11; 32]);
    let a_hash = calculate_tx_id(&a);
    let b = spend(a_hash);
    let b_hash = calculate_tx_id(&b);
    let c = spend(b_hash);
    assert!(mempool.add_transaction(a).unwrap());
    assert!(mempool.add_transaction(b).unwrap());
    assert!(mempool.add_transaction(c.clone()).unwrap());
    let c_hash = calculate_tx_id(&c);
    storage
        .utxos()
        .add_utxo(
            &blvm_node::OutPoint {
                hash: [0x11; 32],
                index: 0,
            },
            &blvm_node::UTXO {
                value: 2_000,
                script_pubkey: vec![0x51].into(),
                height: 1,
                is_coinbase: false,
            },
        )
        .unwrap();
    let rpc = MempoolRpc::with_dependencies(mempool.clone(), storage);
    let a_rpc = blvm_node::storage::hashing::hash_to_rpc_hex(&a_hash);
    let c_rpc = blvm_node::storage::hashing::hash_to_rpc_hex(&c_hash);
    let descendants = rpc.getmempooldescendants(&json!([a_rpc])).await.unwrap();
    let names: Vec<&str> = descendants
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(names.contains(&blvm_node::storage::hashing::hash_to_rpc_hex(&b_hash).as_str()));
    assert!(names.contains(&c_rpc.as_str()));
    let ancestors = rpc.getmempoolancestors(&json!([c_rpc])).await.unwrap();
    let ancestor_names: Vec<&str> = ancestors
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(ancestor_names.contains(&a_rpc.as_str()));
    assert!(
        ancestor_names.contains(&blvm_node::storage::hashing::hash_to_rpc_hex(&b_hash).as_str())
    );

    let entry = rpc.getmempoolentry(&json!([a_rpc])).await.unwrap();
    let verbose = rpc.getrawmempool(&json!([true])).await.unwrap();
    assert_eq!(entry["descendantcount"], 3);
    assert_eq!(verbose[&a_rpc]["descendantcount"], 3);
    assert_eq!(entry["time"], verbose[&a_rpc]["time"]);
    assert_eq!(entry["bip125-replaceable"], true);
    assert_eq!(verbose[&a_rpc]["bip125-replaceable"], true);

    mempool.prioritise_transaction(&a_hash, -1000);
    let entry = rpc.getmempoolentry(&json!([a_rpc])).await.unwrap();
    let verbose = rpc.getrawmempool(&json!([true])).await.unwrap();
    assert_eq!(entry["modifiedfee"].as_f64().unwrap(), 0.0);
    assert_eq!(verbose[&a_rpc]["modifiedfee"].as_f64().unwrap(), 0.0);

    let info = rpc.getmempoolinfo(&json!([])).await.unwrap();
    assert_eq!(info["maxmempool"].as_u64(), Some(1_048_576));
    assert!((info["minrelaytxfee"].as_f64().unwrap() - 0.00005).abs() < 1e-12);
}

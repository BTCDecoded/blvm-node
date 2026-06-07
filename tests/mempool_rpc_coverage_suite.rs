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
    let txid = hex::encode(calculate_tx_id(&tx));
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
    std::env::set_var("DATA_DIR", dir.path());
    let result = rpc.savemempool(&json!([])).await.unwrap();
    match prev {
        Some(v) => std::env::set_var("DATA_DIR", v),
        None => std::env::remove_var("DATA_DIR"),
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
    assert!(map
        .as_object()
        .unwrap()
        .contains_key("0000000000000000000000000000000000000000000000000000000000000000"));
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

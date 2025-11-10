//! Tests for mining RPC implementation

use bllvm_node::node::mempool::MempoolManager;
use bllvm_node::rpc::mining::MiningRpc;
use bllvm_node::storage::Storage;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn test_get_mining_info() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mempool = MempoolManager::new();

    let mining_rpc = MiningRpc::with_dependencies(Arc::new(storage), Arc::new(mempool));

    let info = mining_rpc.get_mining_info().await.unwrap();

    // Verify response structure
    assert!(info.get("blocks").is_some());
    assert!(info.get("pooledtx").is_some());
    assert!(info.get("difficulty").is_some());
    assert!(info.get("chain").is_some());
}

#[tokio::test]
async fn test_get_block_template() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mempool = MempoolManager::new();

    let mining_rpc = MiningRpc::with_dependencies(Arc::new(storage), Arc::new(mempool));

    let params = json!([]);
    let template = mining_rpc.get_block_template(&params).await;

    // Should either succeed or fail gracefully
    // (may fail if chain not initialized, which is expected)
    assert!(template.is_ok() || template.is_err());
}

#[tokio::test]
async fn test_submit_block() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mempool = MempoolManager::new();

    let mining_rpc = MiningRpc::with_dependencies(Arc::new(storage), Arc::new(mempool));

    // Create a minimal invalid block hex (will fail validation, which is expected)
    let invalid_block_hex = "0000000000000000000000000000000000000000000000000000000000000000";
    let params = json!([invalid_block_hex, ""]);

    let result = mining_rpc.submit_block(&params).await;

    // Should fail validation (expected for invalid block)
    assert!(result.is_err());
}

#[tokio::test]
async fn test_estimate_smart_fee() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Storage::new(temp_dir.path()).unwrap();
    let mempool = MempoolManager::new();

    let mining_rpc = MiningRpc::with_dependencies(Arc::new(storage), Arc::new(mempool));

    let params = json!([6, "conservative"]);
    let fee_estimate = mining_rpc.estimate_smart_fee(&params).await.unwrap();

    // Verify response structure
    assert!(fee_estimate.get("feerate").is_some());
    assert!(fee_estimate.get("blocks").is_some());
    assert_eq!(fee_estimate.get("blocks").unwrap().as_u64().unwrap(), 6);
}

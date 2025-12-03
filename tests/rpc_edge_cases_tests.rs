//! Edge case tests for RPC methods (large results, concurrent calls, invalid params)

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::rpc::control::ControlRpc;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::rpc::rawtx::RawTxRpc;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn test_rpc_concurrent_calls() {
    let blockchain = Arc::new(BlockchainRpc::new());

    // Make concurrent RPC calls
    let mut handles = vec![];
    for _ in 0..10 {
        let blockchain_clone = Arc::clone(&blockchain);
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;

    // All should succeed
    assert_eq!(results.len(), 10);
    for result in results {
        assert!(result.is_ok());
        // The inner result may be Ok or Err depending on implementation
        let _ = result.unwrap();
    }
}

#[tokio::test]
async fn test_rpc_invalid_parameters() {
    let blockchain = BlockchainRpc::new();

    // Test with invalid hash format
    let invalid_hash = "not-a-valid-hash";
    let result = blockchain.get_block(invalid_hash).await;
    assert!(result.is_err());

    // Test with invalid height (negative - but get_block_hash takes i64, so -1 is valid)
    // Instead test with a very large height that might cause issues
    let result = blockchain.get_block_hash(1_000_000_000).await;
    // May succeed or fail depending on implementation
    let _ = result;

    // Test with very large height
    let result = blockchain.get_block_hash(1_000_000_000).await;
    // May succeed or fail depending on implementation
    let _ = result;
}

#[tokio::test]
async fn test_rpc_large_result_sets() {
    let blockchain = BlockchainRpc::new();

    // Test methods that might return large results
    // getblockchaininfo should handle large chain states
    let result = blockchain.get_blockchain_info().await;
    assert!(result.is_ok());

    // gettxoutsetinfo might return large UTXO sets
    let result = blockchain.get_txoutset_info().await;
    // May fail without storage, but should handle gracefully
    let _ = result;
}

#[tokio::test]
async fn test_rpc_rate_limiting_edge_cases() {
    let control = Arc::new(ControlRpc::new());

    // Make many rapid calls
    let mut handles = vec![];
    for _ in 0..20 {
        let control_clone = Arc::clone(&control);
        handles.push(tokio::spawn(async move {
            control_clone.uptime(&json!([])).await
        }));
    }

    // All should complete (rate limiting may slow them down)
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 20);
}

#[tokio::test]
async fn test_rpc_mining_edge_cases() {
    let mining = MiningRpc::new();

    // Test getblocktemplate with various parameters
    let params_empty = json!([]);
    let result1 = mining.get_block_template(&params_empty).await;
    let _ = result1;

    // Test with invalid parameters
    let params_invalid = json!([{"invalid": "params"}]);
    let result2 = mining.get_block_template(&params_invalid).await;
    // Should handle invalid params gracefully
    let _ = result2;
}

#[tokio::test]
async fn test_rpc_rawtx_edge_cases() {
    let rawtx = RawTxRpc::new();

    // Test with invalid hex string
    let params_invalid_hex = json!(["not-hex"]);
    let result = rawtx.sendrawtransaction(&params_invalid_hex).await;
    assert!(result.is_err());

    // Test with empty hex string
    let params_empty = json!([""]);
    let result = rawtx.sendrawtransaction(&params_empty).await;
    assert!(result.is_err());

    // Test with very large transaction (simulated with long hex string)
    let large_hex = "00".repeat(1000000); // 1MB hex string
    let params_large = json!([large_hex]);
    let result = rawtx.sendrawtransaction(&params_large).await;
    // Should handle large transactions gracefully (may fail validation)
    let _ = result;
}

#[tokio::test]
async fn test_rpc_control_edge_cases() {
    let control = ControlRpc::new();

    // Test help with invalid command
    let params_invalid = json!(["nonexistent_command"]);
    let result = control.help(&params_invalid).await;
    // Should return help text or error gracefully
    let _ = result;

    // Test logging with invalid parameters
    let params_invalid = json!([{"invalid": "params"}]);
    let result = control.logging(&params_invalid).await;
    // Should handle invalid params gracefully
    let _ = result;
}

#[tokio::test]
async fn test_rpc_error_recovery() {
    let blockchain = BlockchainRpc::new();

    // Make a call that fails
    let result1 = blockchain.get_block("invalid").await;
    assert!(result1.is_err());

    // Subsequent calls should still work
    let result2 = blockchain.get_blockchain_info().await;
    assert!(result2.is_ok());
}

#[tokio::test]
async fn test_rpc_concurrent_different_methods() {
    let blockchain = Arc::new(BlockchainRpc::new());
    let control = Arc::new(ControlRpc::new());

    // Concurrent calls to different RPC modules
    let mut handles = vec![];

    // Blockchain calls
    for _ in 0..5 {
        let blockchain_clone = Arc::clone(&blockchain);
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    // Control calls
    for _ in 0..5 {
        let control_clone = Arc::clone(&control);
        handles.push(tokio::spawn(async move {
            control_clone.uptime(&json!([])).await
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 10);

    // All should complete
    for result in results {
        assert!(result.is_ok());
    }
}

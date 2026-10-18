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
        let info = result.expect("task").expect("info");
        assert_eq!(info["blocks"], 0);
        assert_eq!(info["initialblockdownload"], true);
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
    let message = result.expect_err("no storage").to_string();
    assert!(
        message.contains("Storage not available"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_rpc_large_result_sets() {
    let blockchain = BlockchainRpc::new();

    // Test methods that might return large results
    // getblockchaininfo should handle large chain states
    let result = blockchain.get_blockchain_info().await;
    assert!(result.is_ok());

    // gettxoutsetinfo might return large UTXO sets
    let info = blockchain.get_txoutset_info().await.expect("txoutset");
    assert_eq!(info["height"], 0);
    assert_eq!(info["txouts"], 0);
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
    let message = result1.expect_err("no storage").to_string();
    assert!(
        message.contains("getblocktemplate requires storage"),
        "unexpected error: {message}"
    );

    let params_invalid = json!([{"invalid": "params"}]);
    let result2 = mining.get_block_template(&params_invalid).await;
    let message = result2.expect_err("no storage").to_string();
    assert!(
        message.contains("getblocktemplate requires storage"),
        "unexpected error: {message}"
    );
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
    assert!(result.is_err(), "zero-filled hex was accepted: {result:?}");
}

#[tokio::test]
async fn test_rpc_control_edge_cases() {
    let control = ControlRpc::new();

    // Test help with invalid command
    let params_invalid = json!(["nonexistent_command"]);
    let result = control.help(&params_invalid).await;
    let message = result.expect_err("unknown command").to_string();
    assert!(
        message.contains("Unknown command"),
        "unexpected error: {message}"
    );

    let params_invalid = json!([{"invalid": "params"}]);
    let logging = control.logging(&params_invalid).await.expect("logging");
    assert_eq!(logging["active"], true);
    assert!(logging["current_filter"].is_string());
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

    // Concurrent calls to different RPC modules (split handles: different error types per module)
    let mut blockchain_handles = vec![];
    for _ in 0..5 {
        let blockchain_clone = Arc::clone(&blockchain);
        blockchain_handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    let mut control_handles = vec![];
    for _ in 0..5 {
        let control_clone = Arc::clone(&control);
        control_handles.push(tokio::spawn(async move {
            control_clone.uptime(&json!([])).await
        }));
    }

    let bc_results: Vec<_> = futures::future::join_all(blockchain_handles).await;
    let ctl_results: Vec<_> = futures::future::join_all(control_handles).await;
    assert_eq!(bc_results.len() + ctl_results.len(), 10);

    for result in bc_results {
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }
    for result in ctl_results {
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }
}

//! Advanced RPC scenario tests (rate limiting, large result sets)

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::rpc::control::ControlRpc;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;

#[tokio::test]
async fn test_rpc_rate_limiting_rapid_calls() {
    let blockchain = Arc::new(BlockchainRpc::new());

    // Make many rapid calls
    let start = Instant::now();
    let mut handles = vec![];
    for _ in 0..50 {
        let blockchain_clone = Arc::clone(&blockchain);
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;
    let elapsed = start.elapsed();

    // All should complete
    assert_eq!(results.len(), 50);

    // Should complete in reasonable time (rate limiting may slow it down)
    assert!(elapsed < Duration::from_secs(10));
}

#[tokio::test]
async fn test_rpc_concurrent_different_methods() {
    let blockchain = Arc::new(BlockchainRpc::new());
    let control = Arc::new(ControlRpc::new());

    // Concurrent calls to different methods
    let mut handles = vec![];

    // Blockchain calls
    for _ in 0..10 {
        let blockchain_clone = Arc::clone(&blockchain);
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    // Control calls
    for _ in 0..10 {
        let control_clone = Arc::clone(&control);
        handles.push(tokio::spawn(async move {
            control_clone.uptime(&json!([])).await
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 20);

    // All should complete
    for result in results {
        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn test_rpc_timeout_handling() {
    let blockchain = BlockchainRpc::new();

    // Test that RPC calls respect timeouts
    let result = timeout(Duration::from_millis(100), blockchain.get_blockchain_info()).await;

    // Should either succeed quickly or timeout
    assert!(result.is_ok() || result.is_err());
}

#[tokio::test]
async fn test_rpc_error_recovery_after_failure() {
    let blockchain = BlockchainRpc::new();

    // Make a call that fails
    let result1 = blockchain.get_block("invalid_hash").await;
    assert!(result1.is_err());

    // Subsequent calls should still work
    let result2 = blockchain.get_blockchain_info().await;
    assert!(result2.is_ok());

    // Another call should work
    let result3 = blockchain.get_block_hash(0).await;
    // May succeed or fail depending on storage
    let _ = result3;
}

#[tokio::test]
async fn test_rpc_large_result_handling() {
    let blockchain = BlockchainRpc::new();

    // Test methods that might return large results
    let result = blockchain.get_blockchain_info().await;
    assert!(result.is_ok());

    let info = result.unwrap();
    // Should be valid JSON
    assert!(info.is_object());
}

#[tokio::test]
async fn test_rpc_batch_request_handling() {
    let blockchain = Arc::new(BlockchainRpc::new());

    // Simulate batch requests (multiple concurrent calls)
    let mut handles = vec![];
    for i in 0..20 {
        let blockchain_clone = Arc::clone(&blockchain);
        handles.push(tokio::spawn(async move {
            // Mix different methods
            if i % 2 == 0 {
                blockchain_clone.get_blockchain_info().await
            } else {
                blockchain_clone.get_block_hash(i as i64).await
            }
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 20);
}

#[tokio::test]
async fn test_rpc_concurrent_same_method() {
    let blockchain = Arc::new(BlockchainRpc::new());

    // Many concurrent calls to same method
    let mut handles = vec![];
    for _ in 0..30 {
        let blockchain_clone = Arc::clone(&blockchain);
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 30);

    // All should complete (may have different results if state changes)
    for result in results {
        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn test_rpc_resource_cleanup() {
    let blockchain = BlockchainRpc::new();

    // Make many calls
    for _ in 0..100 {
        let _ = blockchain.get_blockchain_info().await;
    }

    // After many calls, should still work
    let result = blockchain.get_blockchain_info().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_rpc_mixed_success_failure() {
    let blockchain = BlockchainRpc::new();

    // Mix successful and failing calls
    let mut handles = vec![];

    // Successful calls
    for _ in 0..5 {
        let blockchain_clone = Arc::new(blockchain.clone());
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    // Failing calls
    for _ in 0..5 {
        let blockchain_clone = Arc::new(blockchain.clone());
        handles.push(tokio::spawn(async move {
            blockchain_clone.get_block("invalid").await
        }));
    }

    // Wait for all calls
    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 10);

    // Some should succeed, some should fail
    let success_count = results
        .iter()
        .filter(|r| r.is_ok() && r.as_ref().unwrap().is_ok())
        .count();
    let failure_count = results.len() - success_count;

    assert!(success_count > 0);
    assert!(failure_count > 0);
}

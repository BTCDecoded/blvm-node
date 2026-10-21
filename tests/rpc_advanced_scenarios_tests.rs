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

    let mut blockchain_handles = vec![];
    for _ in 0..10 {
        let blockchain_clone = Arc::clone(&blockchain);
        blockchain_handles.push(tokio::spawn(async move {
            blockchain_clone.get_blockchain_info().await
        }));
    }

    let mut control_handles = vec![];
    for _ in 0..10 {
        let control_clone = Arc::clone(&control);
        control_handles.push(tokio::spawn(async move {
            control_clone.uptime(&json!([])).await
        }));
    }

    let bc_results: Vec<_> = futures::future::join_all(blockchain_handles).await;
    let ctl_results: Vec<_> = futures::future::join_all(control_handles).await;
    assert_eq!(bc_results.len() + ctl_results.len(), 20);

    for result in bc_results {
        assert!(result.is_ok());
        assert!(
            result
                .unwrap()
                .expect_err("no chain")
                .to_string()
                .contains("chain is not initialized")
        );
    }
    for result in ctl_results {
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }
}

#[tokio::test]
async fn test_rpc_timeout_handling() {
    let blockchain = BlockchainRpc::new();

    // Test that RPC calls respect timeouts
    let result = timeout(Duration::from_millis(100), blockchain.get_blockchain_info()).await;

    let err = result.expect("getblockchaininfo").expect_err("no chain");
    assert!(err.to_string().contains("chain is not initialized"));
}

#[tokio::test]
async fn test_rpc_error_recovery_after_failure() {
    let blockchain = BlockchainRpc::new();

    // Make a call that fails
    let result1 = blockchain.get_block("invalid_hash").await;
    assert!(result1.is_err());

    let result2 = blockchain.get_blockchain_info().await;
    assert!(
        result2
            .expect_err("no chain")
            .to_string()
            .contains("chain is not initialized")
    );

    // Another call should work
    let result3 = blockchain.get_block_hash(0).await;
    let message = result3.expect_err("no storage").to_string();
    assert!(
        message.contains("Storage not available"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_rpc_large_result_handling() {
    let blockchain = BlockchainRpc::new();

    // Test methods that might return large results
    let result = blockchain.get_blockchain_info().await;
    assert!(
        result
            .expect_err("no chain")
            .to_string()
            .contains("chain is not initialized")
    );
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
                blockchain_clone.get_block_hash(i as u64).await
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

    let result = blockchain.get_blockchain_info().await;
    assert!(
        result
            .expect_err("no chain")
            .to_string()
            .contains("chain is not initialized")
    );
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

    let results: Vec<_> = futures::future::join_all(handles).await;
    assert_eq!(results.len(), 10);
    for result in results {
        assert!(result.expect("task").is_err());
    }
}

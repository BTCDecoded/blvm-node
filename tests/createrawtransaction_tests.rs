//! Comprehensive tests for createrawtransaction RPC method
//!
//! Tests transaction creation including:
//! - Input/output parsing
//! - Bech32/Bech32m address support
//! - OP_RETURN data outputs
//! - Locktime and RBF settings
//! - Transaction version

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::rawtx::RawTxRpc;
use blvm_node::storage::Storage;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_rawtx_rpc() -> RawTxRpc {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());

    RawTxRpc::with_dependencies(storage, mempool, None, None)
}

/// Test basic transaction creation with simple inputs and outputs
#[tokio::test]
async fn test_createrawtransaction_basic() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678",
            "vout": 0
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "createrawtransaction should succeed");
    let tx_hex = result.unwrap();
    assert!(tx_hex.is_string(), "Result should be hex string");

    let hex_str = tx_hex.as_str().unwrap();
    assert!(!hex_str.is_empty(), "Transaction hex should not be empty");
    assert!(hex_str.len() % 2 == 0, "Hex string should have even length");
}

/// Test transaction creation with Bech32 address (SegWit)
#[tokio::test]
async fn test_createrawtransaction_bech32_address() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    // P2WPKH address (bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4)
    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.0001
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support Bech32 addresses");
}

/// Test transaction creation with Bech32m address (Taproot)
#[tokio::test]
async fn test_createrawtransaction_bech32m_address() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    // P2TR address (bc1p...)
    let outputs = json!({
        "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr": 0.0001
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support Bech32m addresses");
}

/// Test transaction creation with OP_RETURN data output
#[tokio::test]
async fn test_createrawtransaction_op_return() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    // Output with data (OP_RETURN)
    let outputs = json!({
        "data": "48656c6c6f20576f726c64" // "Hello World" in hex
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support OP_RETURN outputs");
}

/// Test transaction creation with locktime
#[tokio::test]
async fn test_createrawtransaction_locktime() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
    });

    let params = json!([inputs, outputs, 500000]); // locktime = 500000
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support locktime parameter");
}

/// Test transaction creation with RBF disabled
#[tokio::test]
async fn test_createrawtransaction_no_rbf() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
    });

    let params = json!([inputs, outputs, 0, false]); // locktime=0, replaceable=false
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support replaceable=false");
}

/// Test transaction creation with custom version
#[tokio::test]
async fn test_createrawtransaction_version() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
    });

    let params = json!([inputs, outputs, 0, true, 1]); // version=1
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support custom version");
}

/// Test transaction creation with multiple inputs
#[tokio::test]
async fn test_createrawtransaction_multiple_inputs() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "1111111111111111111111111111111111111111111111111111111111111111",
            "vout": 0
        },
        {
            "txid": "2222222222222222222222222222222222222222222222222222222222222222",
            "vout": 1
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.002
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support multiple inputs");
}

/// Test transaction creation with multiple outputs
#[tokio::test]
async fn test_createrawtransaction_multiple_outputs() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001,
        "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr": 0.0005
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support multiple outputs");
}

/// Test transaction creation with explicit sequence numbers
#[tokio::test]
async fn test_createrawtransaction_sequence() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0,
            "sequence": 0xFFFFFFFD // MAX_BIP125_RBF_SEQUENCE
        }
    ]);

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_ok(), "Should support explicit sequence numbers");
}

/// Test error handling for missing inputs
#[tokio::test]
async fn test_createrawtransaction_missing_inputs() {
    let rawtx = create_test_rawtx_rpc();

    let outputs = json!({
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4": 0.001
    });

    let params = json!([json!([]), outputs]); // Empty inputs array
    let result = rawtx.createrawtransaction(&params).await;

    // Should succeed with empty inputs (coinbase-like, though not valid for regular txs)
    // Actually, let's test with missing inputs parameter
    let params_missing = json!([outputs]);
    let result_missing = rawtx.createrawtransaction(&params_missing).await;
    assert!(
        result_missing.is_err(),
        "Should error on missing inputs parameter"
    );
}

/// Test error handling for missing outputs
#[tokio::test]
async fn test_createrawtransaction_missing_outputs() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let params = json!([inputs]); // Missing outputs
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_err(), "Should error on missing outputs");
}

/// Test error handling for invalid address
#[tokio::test]
async fn test_createrawtransaction_invalid_address() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let outputs = json!({
        "invalid-address": 0.001
    });

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_err(), "Should error on invalid address");
}

/// Test error handling for empty outputs
#[tokio::test]
async fn test_createrawtransaction_empty_outputs() {
    let rawtx = create_test_rawtx_rpc();

    let inputs = json!([
        {
            "txid": "0000000000000000000000000000000000000000000000000000000000000000",
            "vout": 0
        }
    ]);

    let outputs = json!({});

    let params = json!([inputs, outputs]);
    let result = rawtx.createrawtransaction(&params).await;

    assert!(result.is_err(), "Should error on empty outputs");
}

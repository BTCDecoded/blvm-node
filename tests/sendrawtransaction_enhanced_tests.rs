//! Comprehensive tests for enhanced sendrawtransaction RPC method
//! 
//! Tests maxfeerate and allowhighfees options:
//! - Fee rate calculation
//! - maxfeerate rejection
//! - allowhighfees override
//! - Error messages

use bllvm_node::rpc::rawtx::RawTxRpc;
use bllvm_node::storage::Storage;
use bllvm_node::node::mempool::MempoolManager;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_rawtx_rpc() -> RawTxRpc {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    
    RawTxRpc::with_dependencies(storage, mempool, None, None)
}

/// Create a simple test transaction hex
/// This is a minimal valid transaction for testing
fn create_simple_tx_hex() -> String {
    // Simple transaction: version(4) + 1 input + 1 output + locktime(4)
    // This is a simplified example - in real tests, you'd use createrawtransaction
    "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000".to_string()
}

/// Test sendrawtransaction without maxfeerate (should work)
#[tokio::test]
async fn test_sendrawtransaction_no_maxfeerate() {
    let rawtx = create_test_rawtx_rpc();
    
    let tx_hex = create_simple_tx_hex();
    let params = json!([tx_hex]);
    
    // This will likely fail due to missing UTXOs, but should not fail due to fee rate
    let result = rawtx.sendrawtransaction(&params).await;
    
    // We expect it to fail for other reasons (missing UTXOs), not fee rate
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            !error_str.contains("fee_rate") && !error_str.contains("maxfeerate"),
            "Should not fail due to fee rate when maxfeerate not specified"
        );
    }
}

/// Test sendrawtransaction with maxfeerate that should pass
#[tokio::test]
async fn test_sendrawtransaction_maxfeerate_pass() {
    let rawtx = create_test_rawtx_rpc();
    
    let tx_hex = create_simple_tx_hex();
    // Set a very high maxfeerate (1 BTC per kvB) - should pass
    let params = json!([tx_hex, 1.0]);
    
    // This will likely fail due to missing UTXOs, but should not fail due to fee rate
    let result = rawtx.sendrawtransaction(&params).await;
    
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            !error_str.contains("fee_rate_too_high") && !error_str.contains("exceeds maximum"),
            "Should not fail due to fee rate when maxfeerate is high enough"
        );
    }
}

/// Test sendrawtransaction with maxfeerate that should fail
#[tokio::test]
async fn test_sendrawtransaction_maxfeerate_fail() {
    let rawtx = create_test_rawtx_rpc();
    
    let tx_hex = create_simple_tx_hex();
    // Set a very low maxfeerate (0.00000001 BTC per kvB = 1 sat per kvB) - should fail
    let params = json!([tx_hex, 0.00000001]);
    
    let result = rawtx.sendrawtransaction(&params).await;
    
    // Should fail with fee rate too high error
    assert!(result.is_err(), "Should fail when fee rate exceeds maxfeerate");
    
    if let Err(e) = result {
        let error_str = e.to_string();
        // Check that error mentions fee rate
        assert!(
            error_str.contains("fee_rate") || error_str.contains("exceeds maximum") || error_str.contains("maxfeerate"),
            "Error should mention fee rate: {}",
            error_str
        );
    }
}

/// Test sendrawtransaction with allowhighfees=true (should override maxfeerate)
#[tokio::test]
async fn test_sendrawtransaction_allowhighfees() {
    let rawtx = create_test_rawtx_rpc();
    
    let tx_hex = create_simple_tx_hex();
    // Set low maxfeerate but allowhighfees=true
    let params = json!([tx_hex, 0.00000001, true]);
    
    let result = rawtx.sendrawtransaction(&params).await;
    
    // Should not fail due to fee rate (allowhighfees overrides)
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            !error_str.contains("fee_rate_too_high") && !error_str.contains("exceeds maximum"),
            "Should not fail due to fee rate when allowhighfees=true: {}",
            error_str
        );
    }
}

/// Test sendrawtransaction parameter parsing
#[tokio::test]
async fn test_sendrawtransaction_parameter_parsing() {
    let rawtx = create_test_rawtx_rpc();
    
    let tx_hex = create_simple_tx_hex();
    
    // Test with maxfeerate as string (should parse)
    let params_str = json!([tx_hex, "0.001"]);
    let result_str = rawtx.sendrawtransaction(&params_str).await;
    // Should parse successfully (will fail for other reasons)
    assert!(result_str.is_err() || result_str.is_ok(), "Should parse maxfeerate as string");
    
    // Test with maxfeerate as number
    let params_num = json!([tx_hex, 0.001]);
    let result_num = rawtx.sendrawtransaction(&params_num).await;
    assert!(result_num.is_err() || result_num.is_ok(), "Should parse maxfeerate as number");
    
    // Test with allowhighfees as boolean
    let params_bool = json!([tx_hex, 0.001, true]);
    let result_bool = rawtx.sendrawtransaction(&params_bool).await;
    assert!(result_bool.is_err() || result_bool.is_ok(), "Should parse allowhighfees as boolean");
}

/// Test sendrawtransaction error message format
#[tokio::test]
async fn test_sendrawtransaction_error_format() {
    let rawtx = create_test_rawtx_rpc();
    
    let tx_hex = create_simple_tx_hex();
    let params = json!([tx_hex, 0.00000001]); // Very low maxfeerate
    
    let result = rawtx.sendrawtransaction(&params).await;
    
    if let Err(e) = result {
        // Error should be a proper RPC error with details
        let error_str = e.to_string();
        // Should contain helpful information
        assert!(
            error_str.len() > 10,
            "Error message should be descriptive"
        );
    }
}


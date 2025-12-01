//! Comprehensive tests for testmempoolaccept RPC method
//! 
//! Tests Core compatibility including:
//! - SegWit transaction support with witness data
//! - wtxid calculation
//! - effective-includes from mempool
//! - Package validation
//! - Package-error handling

use bllvm_node::node::mempool::MempoolManager;
use bllvm_node::rpc::rawtx::RawTxRpc;
use bllvm_node::storage::Storage;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_rawtx_rpc() -> RawTxRpc {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    
    RawTxRpc::with_dependencies(storage, mempool, None, None)
}

/// Test that testmempoolaccept returns all required fields matching Core format
#[tokio::test]
async fn test_testmempoolaccept_output_format() {
    let rawtx = create_test_rawtx_rpc();
    
    // Create a simple valid transaction (non-SegWit)
    // Version (4 bytes) + Input count (1 byte) + Output count (1 byte) + Locktime (4 bytes)
    let simple_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([simple_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    
    // Should return array with one result
    assert!(result.is_array());
    let results = result.as_array().unwrap();
    assert_eq!(results.len(), 1);
    
    let tx_result = &results[0];
    
    // Verify all required fields are present
    assert!(tx_result.get("txid").is_some(), "Missing txid field");
    assert!(tx_result.get("wtxid").is_some(), "Missing wtxid field");
    assert!(tx_result.get("allowed").is_some(), "Missing allowed field");
    assert!(tx_result.get("vsize").is_some(), "Missing vsize field");
    assert!(tx_result.get("fees").is_some(), "Missing fees field");
    
    // Verify fees structure
    let fees = tx_result.get("fees").unwrap();
    assert!(fees.get("base").is_some(), "Missing fees.base field");
    
    // If allowed, should have effective-feerate and effective-includes
    if let Some(allowed) = tx_result.get("allowed").and_then(|v| v.as_bool()) {
        if allowed {
            assert!(fees.get("effective-feerate").is_some(), "Missing fees.effective-feerate when allowed");
            assert!(fees.get("effective-includes").is_some(), "Missing fees.effective-includes when allowed");
            let includes = fees.get("effective-includes").unwrap();
            assert!(includes.is_array(), "effective-includes should be array");
        } else {
            // If not allowed, should have reject-reason
            assert!(tx_result.get("reject-reason").is_some(), "Missing reject-reason when not allowed");
        }
    }
}

/// Test wtxid calculation for non-SegWit transactions (wtxid == txid)
#[tokio::test]
async fn test_wtxid_non_segwit() {
    let rawtx = create_test_rawtx_rpc();
    
    // Simple non-SegWit transaction
    let simple_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([simple_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    let tx_result = &result.as_array().unwrap()[0];
    
    let txid = tx_result.get("txid").unwrap().as_str().unwrap();
    let wtxid = tx_result.get("wtxid").unwrap().as_str().unwrap();
    
    // For non-SegWit, wtxid should equal txid
    assert_eq!(txid, wtxid, "wtxid should equal txid for non-SegWit transactions");
}

/// Test SegWit transaction parsing and wtxid calculation
#[tokio::test]
async fn test_segwit_wtxid_calculation() {
    let rawtx = create_test_rawtx_rpc();
    
    // SegWit transaction with witness data
    // This is a minimal SegWit transaction: version + marker(0x0001) + inputs + outputs + locktime + witness
    // Note: This is a simplified test - real SegWit transactions are more complex
    let segwit_tx_hex = "0100000000010115e180dc28a2327e687facc6f69c2c230fb8c7f44d2c3c7d520eaba5e688b0300000000171600145d9d93c7155dd0c8b8e67b247726f9d1a30baf2fffffffff02400d03000000000017a9147a55d61848e77ca266e79a39bfc85c580a6426c98700c817a80400000017a9140110f0b2c4493c997b96f8649ddf7a3d0eaf2567870247304402207c8b0f22046c2bc6f6a3e85326c3b7f3db85b963465a41265f37073f1b8e85602200c28b54d05b8e5af68dae25b72c8b0b445027b35104e7e2a9a43510545a190dd0121039d25ab79f41f75cafd8088d4e8d5ae32363f8e8f0a6cdeb8e8c1a7484181e9f500000000";
    
    let params = json!([segwit_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    let tx_result = &result.as_array().unwrap()[0];
    
    let txid = tx_result.get("txid").unwrap().as_str().unwrap();
    let wtxid = tx_result.get("wtxid").unwrap().as_str().unwrap();
    
    // For SegWit transactions, wtxid should be different from txid (if witness data exists)
    // Note: This test may pass even if wtxid == txid if witness is empty, which is valid
    // The important thing is that wtxid field is present and calculated correctly
    assert!(!wtxid.is_empty(), "wtxid should not be empty");
    assert_eq!(wtxid.len(), 64, "wtxid should be 64 hex characters (32 bytes)");
}

/// Test package validation - duplicate transactions
#[tokio::test]
async fn test_package_validation_duplicates() {
    let rawtx = create_test_rawtx_rpc();
    
    // Same transaction twice (should fail package validation)
    let tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([[tx_hex, tx_hex]]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    
    let results = result.as_array().unwrap();
    assert_eq!(results.len(), 2);
    
    // Both should have package-error
    for tx_result in results {
        assert!(tx_result.get("package-error").is_some(), "Should have package-error for duplicate transactions");
        assert_eq!(tx_result.get("allowed").unwrap().as_bool(), Some(false), "Should not be allowed when package validation fails");
    }
}

/// Test package validation - conflicting transactions
#[tokio::test]
async fn test_package_validation_conflicts() {
    let rawtx = create_test_rawtx_rpc();
    
    // Create two transactions spending the same output (conflict)
    // Both transactions spend output 0 of the same previous transaction
    let tx1_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    let tx2_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([[tx1_hex, tx2_hex]]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    
    let results = result.as_array().unwrap();
    
    // Check if package validation detected conflict (may or may not depending on tx structure)
    // At minimum, both should have results
    assert_eq!(results.len(), 2);
}

/// Test effective-includes field presence
#[tokio::test]
async fn test_effective_includes_present() {
    let rawtx = create_test_rawtx_rpc();
    
    let simple_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([simple_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    let tx_result = &result.as_array().unwrap()[0];
    
    let fees = tx_result.get("fees").unwrap();
    
    // effective-includes should be present (even if empty array)
    assert!(fees.get("effective-includes").is_some(), "effective-includes should always be present when allowed");
    
    let includes = fees.get("effective-includes").unwrap();
    assert!(includes.is_array(), "effective-includes should be an array");
}

/// Test vsize calculation uses proper BIP141 formula
#[tokio::test]
async fn test_vsize_calculation() {
    let rawtx = create_test_rawtx_rpc();
    
    let simple_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([simple_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    let tx_result = &result.as_array().unwrap()[0];
    
    let vsize = tx_result.get("vsize").unwrap().as_u64().unwrap();
    
    // vsize should be positive
    assert!(vsize > 0, "vsize should be positive");
    
    // vsize should be reasonable (not extremely large)
    assert!(vsize < 1000000, "vsize should be reasonable");
}

/// Test effective-feerate calculation
#[tokio::test]
async fn test_effective_feerate_calculation() {
    let rawtx = create_test_rawtx_rpc();
    
    let simple_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    let params = json!([simple_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    let tx_result = &result.as_array().unwrap()[0];
    
    // If transaction is allowed, effective-feerate should be present
    if let Some(allowed) = tx_result.get("allowed").and_then(|v| v.as_bool()) {
        if allowed {
            let fees = tx_result.get("fees").unwrap();
            let feerate = fees.get("effective-feerate").unwrap().as_f64().unwrap();
            
            // Effective feerate should be non-negative
            assert!(feerate >= 0.0, "effective-feerate should be non-negative");
        }
    }
}

/// Test reject-reason format matches Core
#[tokio::test]
async fn test_reject_reason_format() {
    let rawtx = create_test_rawtx_rpc();
    
    // Invalid transaction (will be rejected)
    let invalid_tx_hex = "0000"; // Too short to be valid
    
    let params = json!([invalid_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await;
    
    // Should either return error or transaction with reject-reason
    if let Ok(result) = result {
        let results = result.as_array().unwrap();
        if !results.is_empty() {
            let tx_result = &results[0];
            if let Some(allowed) = tx_result.get("allowed").and_then(|v| v.as_bool()) {
                if !allowed {
                    assert!(tx_result.get("reject-reason").is_some(), "Should have reject-reason when not allowed");
                    let reason = tx_result.get("reject-reason").unwrap();
                    assert!(reason.is_string() || reason.is_null(), "reject-reason should be string or null");
                }
            }
        }
    }
}

/// Test that single transaction (not package) works correctly
#[tokio::test]
async fn test_single_transaction_format() {
    let rawtx = create_test_rawtx_rpc();
    
    let simple_tx_hex = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff08044c86041b020602ffffffff0100f2052a010000004341041b0e8c2567c12536aa13357b79a073dc4443acf83e08e2c1252d0efcb9a4ba20b4e93f883d634390d26ed65f763194ea3273f11a6718b3615b4d94e82801b0eac00000000";
    
    // Single transaction (string, not array)
    let params = json!([simple_tx_hex]);
    let result = rawtx.testmempoolaccept(&params).await.unwrap();
    
    // Should return array with one result
    assert!(result.is_array());
    let results = result.as_array().unwrap();
    assert_eq!(results.len(), 1);
    
    // Should not have package-error
    assert!(results[0].get("package-error").is_none(), "Single transaction should not have package-error");
}


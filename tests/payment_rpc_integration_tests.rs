//! Integration tests for Payment RPC commands
//!
//! Tests RPC methods for vaults, pools, and congestion control:
//! - Vault RPC commands (createvault, unvault, withdrawfromvault, getvaultstate)
//! - Pool RPC commands (createpool, joinpool, distributepool, getpoolstate)
//! - Congestion RPC commands (createbatch, addtobatch, getcongestionmetrics, broadcastbatch)

#![cfg(all(feature = "ctv", feature = "bip70-http"))]

use bllvm_node::config::PaymentConfig;
use bllvm_node::payment::processor::PaymentProcessor;
use bllvm_node::payment::state_machine::PaymentStateMachine;
use bllvm_node::rpc::payment::PaymentRpc;
use bllvm_node::storage::Storage;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;

/// Helper to create test payment RPC handler
fn create_test_payment_rpc() -> PaymentRpc {
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor).with_congestion_manager(
        None, // No mempool for unit tests
        None, // No storage for congestion manager
        bllvm_node::payment::congestion::BatchConfig::default(),
    ));
    PaymentRpc::with_state_machine(state_machine)
}

/// Helper to create test payment RPC handler with storage
fn create_test_payment_rpc_with_storage() -> (PaymentRpc, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let storage_path = temp_dir.path();
    let storage = Storage::new(storage_path).expect("Failed to create storage");
    let storage_arc = Arc::new(storage);

    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(
        PaymentStateMachine::with_storage(processor, Some(storage_arc.clone()))
            .with_congestion_manager(
                None,              // No mempool for unit tests
                Some(storage_arc), // Pass storage for congestion manager
                bllvm_node::payment::congestion::BatchConfig::default(),
            ),
    );
    (PaymentRpc::with_state_machine(state_machine), temp_dir)
}

// ============================================================================
// Vault RPC Tests
// ============================================================================

/// Test createvault RPC command
#[tokio::test]
async fn test_rpc_createvault() {
    let rpc = create_test_payment_rpc();
    let params = json!({
        "vault_id": "test_vault_rpc_1",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087",
        "config": {
            "withdrawal_delay_blocks": 144,
            "require_unvault": true
        }
    });

    let result = rpc.create_vault(&params).await;

    assert!(result.is_ok(), "createvault should succeed");
    let response = result.unwrap();
    assert_eq!(response["vault_id"], "test_vault_rpc_1");
    assert!(response["vault_state"].is_object());
}

/// Test createvault RPC with missing parameters
#[tokio::test]
async fn test_rpc_createvault_missing_params() {
    let rpc = create_test_payment_rpc();
    let params = json!({
        "vault_id": "test_vault_rpc_2"
        // Missing deposit_amount and withdrawal_script
    });

    let result = rpc.create_vault(&params).await;

    assert!(
        result.is_err(),
        "createvault should fail with missing params"
    );
}

/// Test unvault RPC command
#[tokio::test]
async fn test_rpc_unvault() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // First create vault
    let create_params = json!({
        "vault_id": "test_vault_rpc_3",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087",
        "config": {
            "require_unvault": true
        }
    });
    rpc.create_vault(&create_params).await.unwrap();

    // Then unvault
    let unvault_params = json!({
        "vault_id": "test_vault_rpc_3",
        "unvault_script": "5276a914000000000000000000000000000000000000000087"
    });

    let result = rpc.unvault(&unvault_params).await;

    assert!(result.is_ok(), "unvault should succeed");
    let response = result.unwrap();
    assert_eq!(response["vault_id"], "test_vault_rpc_3");
    assert!(response["vault_state"].is_object());
}

/// Test unvault RPC with nonexistent vault
#[tokio::test]
async fn test_rpc_unvault_nonexistent() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();
    let params = json!({
        "vault_id": "nonexistent_vault",
        "unvault_script": "5276a914000000000000000000000000000000000000000087"
    });

    let result = rpc.unvault(&params).await;

    assert!(result.is_err(), "unvault should fail for nonexistent vault");
}

/// Test withdrawfromvault RPC command
#[tokio::test]
async fn test_rpc_withdrawfromvault() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create vault without require_unvault for simpler test
    // Direct withdrawal from Deposited state
    let create_params = json!({
        "vault_id": "test_vault_rpc_4",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087",
        "config": {
            "withdrawal_delay_blocks": 0, // No delay for test
            "require_unvault": false // Direct withdrawal
        }
    });
    rpc.create_vault(&create_params).await.unwrap();

    // Withdraw immediately (no delay, no unvault needed)
    let withdraw_params = json!({
        "vault_id": "test_vault_rpc_4",
        "withdrawal_script": "5376a914000000000000000000000000000000000000000087",
        "current_block_height": 1000 // Any height works with delay=0
    });

    let result = rpc.withdraw_from_vault(&withdraw_params).await;

    assert!(result.is_ok(), "withdrawfromvault should succeed");
    let response = result.unwrap();
    assert_eq!(response["vault_id"], "test_vault_rpc_4");
    assert!(response["vault_state"].is_object());
}

/// Test getvaultstate RPC command
#[tokio::test]
async fn test_rpc_getvaultstate() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create vault
    let create_params = json!({
        "vault_id": "test_vault_rpc_5",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087"
    });
    rpc.create_vault(&create_params).await.unwrap();

    // Get vault state
    let get_params = json!({
        "vault_id": "test_vault_rpc_5"
    });

    let result = rpc.get_vault_state(&get_params).await;

    assert!(result.is_ok(), "getvaultstate should succeed");
    let response = result.unwrap();
    assert_eq!(response["vault_id"], "test_vault_rpc_5");
    assert!(response["vault_state"].is_object());
}

// ============================================================================
// Pool RPC Tests
// ============================================================================

/// Test createpool RPC command
#[tokio::test]
async fn test_rpc_createpool() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();
    let params = json!({
        "pool_id": "test_pool_rpc_1",
        "initial_participants": [
            ["participant_1", 10000, "5176a914000000000000000000000000000000000000000087"],
            ["participant_2", 20000, "5276a914000000000000000000000000000000000000000087"]
        ],
        "config": {
            "min_contribution": 5000
        }
    });

    let result = rpc.create_pool(&params).await;

    assert!(result.is_ok(), "createpool should succeed");
    let response = result.unwrap();
    assert_eq!(response["pool_id"], "test_pool_rpc_1");
    assert!(response["pool_state"].is_object());
}

/// Test joinpool RPC command
#[tokio::test]
async fn test_rpc_joinpool() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create pool
    let create_params = json!({
        "pool_id": "test_pool_rpc_2",
        "initial_participants": [
            ["participant_1", 10000, "5176a914000000000000000000000000000000000000000087"]
        ]
    });
    rpc.create_pool(&create_params).await.unwrap();

    // Join pool
    let join_params = json!({
        "pool_id": "test_pool_rpc_2",
        "participant_id": "participant_2",
        "contribution": 20000,
        "script_pubkey": "5276a914000000000000000000000000000000000000000087"
    });

    let result = rpc.join_pool(&join_params).await;

    assert!(result.is_ok(), "joinpool should succeed");
    let response = result.unwrap();
    assert_eq!(response["pool_id"], "test_pool_rpc_2");
}

/// Test distributepool RPC command
#[tokio::test]
async fn test_rpc_distributepool() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create pool with participants
    let create_params = json!({
        "pool_id": "test_pool_rpc_3",
        "initial_participants": [
            ["participant_1", 10000, "5176a914000000000000000000000000000000000000000087"],
            ["participant_2", 20000, "5276a914000000000000000000000000000000000000000087"]
        ]
    });
    rpc.create_pool(&create_params).await.unwrap();

    // Distribute from pool
    let distribute_params = json!({
        "pool_id": "test_pool_rpc_3",
        "distribution": [
            ["participant_1", 5000],
            ["participant_2", 10000]
        ]
    });

    let result = rpc.distribute_pool(&distribute_params).await;

    assert!(result.is_ok(), "distributepool should succeed");
    let response = result.unwrap();
    assert_eq!(response["pool_id"], "test_pool_rpc_3");
    assert!(response["covenant_proof"].is_object());
}

/// Test getpoolstate RPC command
#[tokio::test]
async fn test_rpc_getpoolstate() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create pool
    let create_params = json!({
        "pool_id": "test_pool_rpc_4",
        "initial_participants": [
            ["participant_1", 10000, "5176a914000000000000000000000000000000000000000087"]
        ]
    });
    rpc.create_pool(&create_params).await.unwrap();

    // Get pool state
    let get_params = json!({
        "pool_id": "test_pool_rpc_4"
    });

    let result = rpc.get_pool_state(&get_params).await;

    assert!(result.is_ok(), "getpoolstate should succeed");
    let response = result.unwrap();
    assert_eq!(response["pool_id"], "test_pool_rpc_4");
    assert!(response["pool_state"].is_object());
}

// ============================================================================
// Congestion RPC Tests
// ============================================================================

/// Test createbatch RPC command
#[tokio::test]
async fn test_rpc_createbatch() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();
    let params = json!({
        "batch_id": "test_batch_rpc_1",
        "target_fee_rate": 10
    });

    let result = rpc.create_batch(&params).await;

    assert!(result.is_ok(), "createbatch should succeed");
    let response = result.unwrap();
    assert_eq!(response["batch_id"], "test_batch_rpc_1");
}

/// Test createbatch RPC with default fee rate
#[tokio::test]
async fn test_rpc_createbatch_default_fee() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();
    let params = json!({
        "batch_id": "test_batch_rpc_2"
        // No target_fee_rate - should use default
    });

    let result = rpc.create_batch(&params).await;

    assert!(
        result.is_ok(),
        "createbatch should succeed with default fee"
    );
    let response = result.unwrap();
    assert_eq!(response["batch_id"], "test_batch_rpc_2");
}

/// Test addtobatch RPC command
#[tokio::test]
async fn test_rpc_addtobatch() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create batch
    let create_params = json!({
        "batch_id": "test_batch_rpc_3"
    });
    rpc.create_batch(&create_params).await.unwrap();

    // Add transaction to batch
    let add_params = json!({
        "batch_id": "test_batch_rpc_3",
        "tx_id": "test_tx_1",
        "outputs": [
            {
                "amount": 10000,
                "script_pubkey": "5176a914000000000000000000000000000000000000000087"
            }
        ],
        "priority": "normal"
    });

    let result = rpc.add_to_batch(&add_params).await;

    assert!(result.is_ok(), "addtobatch should succeed");
    let response = result.unwrap();
    assert_eq!(response["batch_id"], "test_batch_rpc_3");
    assert!(response["batch_size"].as_u64().unwrap() > 0);
}

/// Test getcongestionmetrics RPC command
#[tokio::test]
async fn test_rpc_getcongestionmetrics() {
    let rpc = create_test_payment_rpc();
    let params = json!({});

    // Note: getcongestionmetrics requires mempool manager
    // Without mempool, it will return an error
    let result = rpc.get_congestion_metrics(&params).await;

    // Should return error when mempool not available
    assert!(
        result.is_err(),
        "getcongestionmetrics should fail without mempool"
    );
}

/// Test broadcastbatch RPC command
#[tokio::test]
async fn test_rpc_broadcastbatch() {
    let (rpc, _temp_dir) = create_test_payment_rpc_with_storage();

    // Create batch
    let create_params = json!({
        "batch_id": "test_batch_rpc_4"
    });
    rpc.create_batch(&create_params).await.unwrap();

    // Add a transaction to batch first
    let add_params = json!({
        "batch_id": "test_batch_rpc_4",
        "tx_id": "test_tx_2",
        "outputs": [
            {
                "amount": 20000,
                "script_pubkey": "5276a914000000000000000000000000000000000000000087"
            }
        ],
        "priority": "normal"
    });
    rpc.add_to_batch(&add_params).await.unwrap();

    // Broadcast batch
    let broadcast_params = json!({
        "batch_id": "test_batch_rpc_4"
    });

    let result = rpc.broadcast_batch(&broadcast_params).await;

    assert!(result.is_ok(), "broadcastbatch should succeed");
    let response = result.unwrap();
    assert_eq!(response["batch_id"], "test_batch_rpc_4");
    assert!(response["ready_to_broadcast"].as_bool().unwrap());
    assert!(response["covenant_proof"].is_object());
}

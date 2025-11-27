//! Integration tests for Payment REST endpoints
//!
//! Tests REST API endpoints for vaults, pools, and congestion control:
//! - Vault REST endpoints (POST /api/v1/vaults, POST /api/v1/vaults/{id}/unvault, etc.)
//! - Pool REST endpoints (POST /api/v1/pools, POST /api/v1/pools/{id}/join, etc.)
//! - Congestion REST endpoints (POST /api/v1/batches, GET /api/v1/congestion, etc.)

#![cfg(all(feature = "ctv", feature = "bip70-http", feature = "rest-api"))]

use bllvm_node::config::PaymentConfig;
use bllvm_node::payment::processor::PaymentProcessor;
use bllvm_node::payment::state_machine::PaymentStateMachine;
#[cfg(feature = "rest-api")]
use bllvm_node::rpc::rest::congestion::handle_congestion_request;
#[cfg(feature = "rest-api")]
use bllvm_node::rpc::rest::pool::handle_pool_request;
#[cfg(feature = "rest-api")]
use bllvm_node::rpc::rest::vault::handle_vault_request;
use bllvm_node::storage::Storage;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

/// Helper to create test payment state machine
fn create_test_state_machine() -> Arc<PaymentStateMachine> {
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    Arc::new(PaymentStateMachine::new(processor))
}

/// Helper to create test payment state machine with storage
fn create_test_state_machine_with_storage() -> (Arc<PaymentStateMachine>, TempDir) {
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
                None,
                Some(storage_arc),
                bllvm_node::payment::congestion::BatchConfig::default(),
            ),
    );
    (state_machine, temp_dir)
}

/// Helper to extract response body as JSON
fn extract_response_body(
    response: Response<Full<Bytes>>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let (_, body) = response.into_parts();
    let body_bytes = body.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes)?;
    Ok(json)
}

// ============================================================================
// Vault REST Tests
// ============================================================================

/// Test POST /api/v1/vaults - Create vault
#[tokio::test]
async fn test_rest_create_vault() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();
    let body = Some(json!({
        "vault_id": "test_vault_rest_1",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087"
    }));

    let response = handle_vault_request(
        state_machine,
        &Method::POST,
        "/api/v1/vaults",
        body,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["vault_id"], "test_vault_rest_1");
}

/// Test POST /api/v1/vaults - Missing required fields
#[tokio::test]
async fn test_rest_create_vault_missing_fields() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();
    let body = Some(json!({
        "vault_id": "test_vault_rest_2"
        // Missing deposit_amount and withdrawal_script
    }));

    let response = handle_vault_request(
        state_machine,
        &Method::POST,
        "/api/v1/vaults",
        body,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Test POST /api/v1/vaults/{id}/unvault - Unvault funds
#[tokio::test]
async fn test_rest_unvault_vault() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();

    // First create vault
    let create_body = Some(json!({
        "vault_id": "test_vault_rest_3",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087",
        "config": {
            "require_unvault": true
        }
    }));
    handle_vault_request(
        Arc::clone(&state_machine),
        &Method::POST,
        "/api/v1/vaults",
        create_body,
        request_id.clone(),
    )
    .await;

    // Then unvault
    let unvault_body = Some(json!({
        "unvault_script": "5276a914000000000000000000000000000000000000000087"
    }));

    let response = handle_vault_request(
        state_machine,
        &Method::POST,
        "/api/v1/vaults/test_vault_rest_3/unvault",
        unvault_body,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["vault_id"], "test_vault_rest_3");
}

/// Test GET /api/v1/vaults/{id} - Get vault state
#[tokio::test]
async fn test_rest_get_vault_state() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();

    // Create vault
    let create_body = Some(json!({
        "vault_id": "test_vault_rest_4",
        "deposit_amount": 100000,
        "withdrawal_script": "5176a914000000000000000000000000000000000000000087"
    }));
    handle_vault_request(
        Arc::clone(&state_machine),
        &Method::POST,
        "/api/v1/vaults",
        create_body,
        request_id.clone(),
    )
    .await;

    // Get vault state
    let response = handle_vault_request(
        state_machine,
        &Method::GET,
        "/api/v1/vaults/test_vault_rest_4",
        None,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["vault_id"], "test_vault_rest_4");
}

// ============================================================================
// Pool REST Tests
// ============================================================================

/// Test POST /api/v1/pools - Create pool
#[tokio::test]
async fn test_rest_create_pool() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();
    let body = Some(json!({
        "pool_id": "test_pool_rest_1",
        "initial_participants": [
            {
                "participant_id": "participant_1",
                "contribution": 10000,
                "script_pubkey": "5176a914000000000000000000000000000000000000000087"
            },
            {
                "participant_id": "participant_2",
                "contribution": 20000,
                "script_pubkey": "5276a914000000000000000000000000000000000000000087"
            }
        ]
    }));

    let response = handle_pool_request(
        state_machine,
        &Method::POST,
        "/api/v1/pools",
        body,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["pool_id"], "test_pool_rest_1");
}

/// Test POST /api/v1/pools/{id}/join - Join pool
#[tokio::test]
async fn test_rest_join_pool() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();

    // Create pool
    let create_body = Some(json!({
        "pool_id": "test_pool_rest_2",
        "initial_participants": [
            {
                "participant_id": "participant_1",
                "contribution": 10000,
                "script_pubkey": "5176a914000000000000000000000000000000000000000087"
            }
        ]
    }));
    handle_pool_request(
        Arc::clone(&state_machine),
        &Method::POST,
        "/api/v1/pools",
        create_body,
        request_id.clone(),
    )
    .await;

    // Join pool
    let join_body = Some(json!({
        "participant_id": "participant_2",
        "contribution": 20000,
        "script_pubkey": "5276a914000000000000000000000000000000000000000087"
    }));

    let response = handle_pool_request(
        state_machine,
        &Method::POST,
        "/api/v1/pools/test_pool_rest_2/join",
        join_body,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["pool_id"], "test_pool_rest_2");
}

/// Test GET /api/v1/pools/{id} - Get pool state
#[tokio::test]
async fn test_rest_get_pool_state() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();

    // Create pool
    let create_body = Some(json!({
        "pool_id": "test_pool_rest_3",
        "initial_participants": [
            {
                "participant_id": "participant_1",
                "contribution": 10000,
                "script_pubkey": "5176a914000000000000000000000000000000000000000087"
            }
        ]
    }));
    handle_pool_request(
        Arc::clone(&state_machine),
        &Method::POST,
        "/api/v1/pools",
        create_body,
        request_id.clone(),
    )
    .await;

    // Get pool state
    let response = handle_pool_request(
        state_machine,
        &Method::GET,
        "/api/v1/pools/test_pool_rest_3",
        None,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["pool_id"], "test_pool_rest_3");
}

// ============================================================================
// Congestion REST Tests
// ============================================================================

/// Test POST /api/v1/batches - Create batch
#[tokio::test]
async fn test_rest_create_batch() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();
    let body = Some(json!({
        "batch_id": "test_batch_rest_1",
        "target_fee_rate": 10
    }));

    let response = handle_congestion_request(
        state_machine,
        &Method::POST,
        "/api/v1/batches",
        body,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["batch_id"], "test_batch_rest_1");
}

/// Test GET /api/v1/batches/{id} - Get batch state
#[tokio::test]
async fn test_rest_get_batch_state() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();

    // Create batch
    let create_body = Some(json!({
        "batch_id": "test_batch_rest_2"
    }));
    handle_congestion_request(
        Arc::clone(&state_machine),
        &Method::POST,
        "/api/v1/batches",
        create_body,
        request_id.clone(),
    )
    .await;

    // Get batch state
    let response = handle_congestion_request(
        state_machine,
        &Method::GET,
        "/api/v1/batches/test_batch_rest_2",
        None,
        request_id.clone(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body_json = extract_response_body(response).unwrap();
    assert_eq!(body_json["data"]["batch_id"], "test_batch_rest_2");
}

/// Test GET /api/v1/congestion - Get congestion metrics
#[tokio::test]
async fn test_rest_get_congestion_metrics() {
    let (state_machine, _temp_dir) = create_test_state_machine_with_storage();
    let request_id = Uuid::new_v4().to_string();

    // Note: get_congestion_metrics requires mempool manager
    // Without mempool, it will return an error
    let response = handle_congestion_request(
        state_machine,
        &Method::GET,
        "/api/v1/congestion",
        None,
        request_id.clone(),
    )
    .await;

    // Should return error when mempool not available
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

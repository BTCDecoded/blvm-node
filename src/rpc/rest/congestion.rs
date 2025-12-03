//! REST API endpoints for Congestion Control operations
//!
//! Provides HTTP REST endpoints for transaction batching and congestion management:
//! - Create batches
//! - Add transactions to batches
//! - Get batch state
//! - Broadcast batches
//! - Get congestion metrics

use crate::payment::state_machine::PaymentStateMachine;
use crate::rpc::rest::types::{error_response, success_response};
use blvm_protocol::payment::PaymentOutput;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::debug;

/// Handle congestion REST API requests
///
/// Routes:
/// - POST /api/v1/batches - Create batch
/// - POST /api/v1/batches/{id}/transactions - Add transaction to batch
/// - GET /api/v1/batches/{id} - Get batch state
/// - POST /api/v1/batches/{id}/broadcast - Broadcast batch
/// - GET /api/v1/congestion - Get congestion metrics
#[cfg(feature = "ctv")]
pub async fn handle_congestion_request(
    state_machine: Arc<PaymentStateMachine>,
    method: &Method,
    path: &str,
    body: Option<Value>,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!(
        "Congestion REST request: {} {} (request_id: {})",
        method,
        path,
        &request_id[..8]
    );

    match (method, path) {
        (&Method::POST, "/api/v1/batches") => create_batch(state_machine, body, request_id).await,
        (method, path)
            if method == &Method::POST
                && path.starts_with("/api/v1/batches/")
                && path.ends_with("/transactions") =>
        {
            let batch_id = extract_id(path, "/api/v1/batches/", "/transactions");
            add_to_batch(state_machine, body, &batch_id, request_id).await
        }
        (&Method::GET, path) if path.starts_with("/api/v1/batches/") => {
            let batch_id = extract_id(path, "/api/v1/batches/", "");
            get_batch_state(state_machine, &batch_id, request_id).await
        }
        (method, path)
            if method == &Method::POST
                && path.starts_with("/api/v1/batches/")
                && path.ends_with("/broadcast") =>
        {
            let batch_id = extract_id(path, "/api/v1/batches/", "/broadcast");
            broadcast_batch(state_machine, &batch_id, request_id).await
        }
        (&Method::GET, "/api/v1/congestion") => {
            get_congestion_metrics(state_machine, request_id).await
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("Congestion endpoint not found: {} {}", method, path),
            request_id,
        ),
    }
}

#[cfg(feature = "ctv")]
fn extract_id(path: &str, prefix: &str, suffix: &str) -> String {
    path.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(suffix))
        .unwrap_or("")
        .to_string()
}

#[cfg(feature = "ctv")]
async fn create_batch(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    request_id: String,
) -> Response<Full<Bytes>> {
    let body = match body {
        Some(b) => b,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Request body required",
                request_id,
            );
        }
    };

    match state_machine.congestion_manager() {
        Some(congestion_manager) => {
            let batch_id = match body["batch_id"].as_str() {
                Some(id) => id.to_string(),
                None => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "batch_id required",
                        request_id,
                    );
                }
            };
            let target_fee_rate = body["target_fee_rate"].as_u64();

            let mut manager = congestion_manager.lock().await;
            let batch_id_returned = manager.create_batch(&batch_id, target_fee_rate);

            success_response(json!({ "batch_id": batch_id_returned }), request_id)
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "Congestion manager not available",
            request_id,
        ),
    }
}

#[cfg(feature = "ctv")]
async fn add_to_batch(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    batch_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    use super::types::ApiResponse;
    use crate::payment::congestion::{PendingTransaction, TransactionPriority};
    use bllvm_protocol::payment::PaymentOutput;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    let congestion_manager = match state_machine.congestion_manager() {
        Some(manager) => manager,
        None => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "Congestion manager not available",
                request_id,
            );
        }
    };

    // Parse transaction from body
    let outputs = body
        .and_then(|v| {
            v.get("outputs")
                .and_then(|o| o.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            serde_json::from_value::<PaymentOutput>(item.clone()).ok()
                        })
                        .collect::<Vec<_>>()
                })
        })
        .unwrap_or_default();

    if outputs.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "Transaction outputs are required",
            request_id,
        );
    }

    let priority_str = body
        .and_then(|v| v.get("priority").and_then(|p| p.as_str()))
        .unwrap_or("normal");

    let priority = match priority_str.to_lowercase().as_str() {
        "urgent" => TransactionPriority::Urgent,
        "high" => TransactionPriority::High,
        "low" => TransactionPriority::Low,
        _ => TransactionPriority::Normal,
    };

    let deadline = body
        .and_then(|v| v.get("deadline").and_then(|d| d.as_u64()));

    let tx_id = body
        .and_then(|v| v.get("tx_id").and_then(|id| id.as_str()))
        .unwrap_or("unknown")
        .to_string();

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let pending_tx = PendingTransaction {
        tx_id,
        outputs,
        priority,
        created_at,
        deadline,
    };

    // Add to batch
    let mut manager = congestion_manager.lock().await;
    match manager.add_to_batch(batch_id, pending_tx) {
        Ok(_) => {
            let batch = manager.get_batch(batch_id);
            let response = ApiResponse {
                success: true,
                data: Some(json!({
                    "batch_id": batch_id,
                    "transaction_count": batch.map(|b| b.transactions.len()).unwrap_or(0),
                    "message": "Transaction added to batch successfully"
                })),
                error: None,
            };
            success_response(serde_json::to_string(&response).unwrap(), request_id)
        }
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            &format!("Failed to add to batch: {}", e),
            request_id,
        ),
    }
}

#[cfg(feature = "ctv")]
async fn get_batch_state(
    state_machine: Arc<PaymentStateMachine>,
    batch_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    match state_machine.congestion_manager() {
        Some(congestion_manager) => {
            let manager = congestion_manager.lock().await;
            match manager.get_batch(batch_id) {
                Some(batch) => {
                    let response_data = json!({
                        "batch_id": batch.batch_id,
                        "batch_state": serde_json::to_value(batch)
                            .unwrap_or_else(|_| json!(null)),
                    });
                    success_response(response_data, request_id)
                }
                None => error_response(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    &format!("Batch {} not found", batch_id),
                    request_id,
                ),
            }
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "Congestion manager not available",
            request_id,
        ),
    }
}

#[cfg(feature = "ctv")]
async fn broadcast_batch(
    state_machine: Arc<PaymentStateMachine>,
    batch_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    match state_machine.congestion_manager() {
        Some(congestion_manager) => {
            let mut manager = congestion_manager.lock().await;
            match manager.broadcast_batch(batch_id) {
                Ok(covenant_proof) => {
                    let response_data = json!({
                        "batch_id": batch_id,
                        "covenant_proof": serde_json::to_value(&covenant_proof)
                            .unwrap_or_else(|_| json!(null)),
                        "ready_to_broadcast": true,
                    });
                    success_response(response_data, request_id)
                }
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BROADCAST_FAILED",
                    &format!("Failed to broadcast batch: {}", e),
                    request_id,
                ),
            }
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "Congestion manager not available",
            request_id,
        ),
    }
}

#[cfg(feature = "ctv")]
async fn get_congestion_metrics(
    state_machine: Arc<PaymentStateMachine>,
    request_id: String,
) -> Response<Full<Bytes>> {
    match state_machine.congestion_manager() {
        Some(congestion_manager) => {
            let manager = congestion_manager.lock().await;
            match manager.check_congestion() {
                Ok(metrics) => {
                    let response_data = json!({
                        "mempool_size": metrics.mempool_size,
                        "avg_fee_rate": metrics.avg_fee_rate,
                        "median_fee_rate": metrics.median_fee_rate,
                        "estimated_blocks": metrics.estimated_blocks,
                        "collected_at": metrics.collected_at,
                    });
                    success_response(response_data, request_id)
                }
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CONGESTION_METRICS_FAILED",
                    &format!("Failed to get congestion metrics: {}", e),
                    request_id,
                ),
            }
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "Congestion manager not available",
            request_id,
        ),
    }
}

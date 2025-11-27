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
use bllvm_protocol::payment::PaymentOutput;
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
    // TODO: Implement add_to_batch REST
    error_response(
        StatusCode::NOT_IMPLEMENTED,
        "NOT_IMPLEMENTED",
        "Add to batch REST endpoint not yet implemented",
        request_id,
    )
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

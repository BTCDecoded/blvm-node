//! Payment REST API endpoints
//!
//! Provides REST endpoints for payment operations including:
//! - Creating payment requests
//! - Creating CTV covenant proofs
//! - Querying payment state
//! - Settlement monitoring

use crate::payment::state_machine::{PaymentState, PaymentStateMachine};
use crate::rpc::rest::types::{ApiError, ApiResponse, ErrorDetails, ResponseMeta};
use blvm_protocol::payment::PaymentOutput;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, error};
use uuid::Uuid;

/// Handle payment REST API requests
pub async fn handle_payment_request(
    state_machine: Arc<PaymentStateMachine>,
    method: &Method,
    path: &str,
    body: Option<Value>,
) -> Response<Full<Bytes>> {
    let request_id = Uuid::new_v4().to_string();

    match (method, path) {
        // POST /api/v1/payments - Create payment request
        (&Method::POST, "/api/v1/payments") => {
            create_payment_request(state_machine, body, request_id).await
        }
        // POST /api/v1/payments/{id}/covenant - Create CTV covenant proof
        (method, path)
            if method == Method::POST
                && path.starts_with("/api/v1/payments/")
                && path.ends_with("/covenant") =>
        {
            let payment_id = extract_payment_id(path, "/api/v1/payments/", "/covenant");
            create_covenant_proof(state_machine, &payment_id, request_id).await
        }
        // GET /api/v1/payments/{id} - Get payment state
        (&Method::GET, path)
            if path.starts_with("/api/v1/payments/") && !path.contains("/covenant") =>
        {
            let payment_id = extract_payment_id(path, "/api/v1/payments/", "");
            get_payment_state(state_machine, &payment_id, request_id).await
        }
        // GET /api/v1/payments - List all payments
        (&Method::GET, "/api/v1/payments") => list_payments(state_machine, request_id).await,
        _ => error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("Payment endpoint not found: {} {}", method, path),
            request_id,
        ),
    }
}

/// Extract payment ID from path
fn extract_payment_id(path: &str, prefix: &str, suffix: &str) -> String {
    path.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(suffix))
        .unwrap_or("")
        .to_string()
}

/// Create a payment request
async fn create_payment_request(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!("REST: POST /api/v1/payments");

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

    // Parse outputs
    let outputs: Vec<PaymentOutput> = match body.get("outputs") {
        Some(outputs_value) => match serde_json::from_value(outputs_value.clone()) {
            Ok(o) => o,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "INVALID_OUTPUTS",
                    &format!("Invalid outputs format: {}", e),
                    request_id,
                );
            }
        },
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "MISSING_OUTPUTS",
                "Missing 'outputs' field in request body",
                request_id,
            );
        }
    };

    // Parse merchant_data (optional)
    let merchant_data = body
        .get("merchant_data")
        .and_then(|v| v.as_str())
        .and_then(|s| hex::decode(s).ok());

    // Parse create_covenant (optional, default: false)
    let create_covenant = body
        .get("create_covenant")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Create payment request
    match state_machine
        .create_payment_request(outputs, merchant_data, create_covenant)
        .await
    {
        Ok((payment_id, covenant_proof)) => {
            let mut response_data = json!({
                "payment_id": payment_id,
            });

            #[cfg(feature = "ctv")]
            {
                if let Some(proof) = covenant_proof {
                    response_data["covenant_proof"] =
                        serde_json::to_value(&proof).unwrap_or_else(|_| json!(null));
                }
            }

            success_response(response_data, request_id)
        }
        Err(e) => {
            error!("Failed to create payment request: {}", e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "PAYMENT_CREATION_FAILED",
                &format!("Failed to create payment request: {}", e),
                request_id,
            )
        }
    }
}

/// Create CTV covenant proof for existing payment request
#[cfg(feature = "ctv")]
async fn create_covenant_proof(
    state_machine: Arc<PaymentStateMachine>,
    payment_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!("REST: POST /api/v1/payments/{}/covenant", payment_id);

    if payment_id.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "MISSING_PAYMENT_ID",
            "Payment ID required in path",
            request_id,
        );
    }

    match state_machine.create_covenant_proof(payment_id).await {
        Ok(covenant_proof) => {
            let response_data =
                serde_json::to_value(&covenant_proof).unwrap_or_else(|_| json!(null));
            success_response(response_data, request_id)
        }
        Err(e) => {
            error!("Failed to create covenant proof: {}", e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "COVENANT_CREATION_FAILED",
                &format!("Failed to create covenant proof: {}", e),
                request_id,
            )
        }
    }
}

/// Get payment state
async fn get_payment_state(
    state_machine: Arc<PaymentStateMachine>,
    payment_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!("REST: GET /api/v1/payments/{}", payment_id);

    if payment_id.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "MISSING_PAYMENT_ID",
            "Payment ID required in path",
            request_id,
        );
    }

    match state_machine.get_payment_state(payment_id).await {
        Ok(state) => {
            let state_json = payment_state_to_json(&state);
            success_response(state_json, request_id)
        }
        Err(e) => {
            error!("Failed to get payment state: {}", e);
            error_response(
                StatusCode::NOT_FOUND,
                "PAYMENT_NOT_FOUND",
                &format!("Payment not found: {}", e),
                request_id,
            )
        }
    }
}

/// List all payments
async fn list_payments(
    state_machine: Arc<PaymentStateMachine>,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!("REST: GET /api/v1/payments");

    let states = state_machine.list_payment_states();

    let payments: Vec<Value> = states
        .iter()
        .map(|(payment_id, state)| {
            let state_str = match state {
                PaymentState::RequestCreated { .. } => "request_created",
                #[cfg(feature = "ctv")]
                PaymentState::ProofCreated { .. } => "proof_created",
                #[cfg(feature = "ctv")]
                PaymentState::ProofBroadcast { .. } => "proof_broadcast",
                PaymentState::InMempool { .. } => "in_mempool",
                PaymentState::Settled { .. } => "settled",
                PaymentState::Failed { .. } => "failed",
            };

            json!({
                "payment_id": payment_id,
                "state": state_str,
            })
        })
        .collect();

    let response_data = json!({
        "payments": payments,
        "count": payments.len(),
    });

    success_response(response_data, request_id)
}

/// Convert payment state to JSON
fn payment_state_to_json(state: &PaymentState) -> Value {
    match state {
        PaymentState::RequestCreated { request_id } => {
            json!({
                "state": "request_created",
                "request_id": request_id,
            })
        }
        #[cfg(feature = "ctv")]
        PaymentState::ProofCreated {
            request_id,
            covenant_proof,
        } => {
            json!({
                "state": "proof_created",
                "request_id": request_id,
                "covenant_proof": serde_json::to_value(covenant_proof)
                    .unwrap_or_else(|_| json!(null)),
            })
        }
        #[cfg(feature = "ctv")]
        PaymentState::ProofBroadcast {
            request_id,
            covenant_proof,
            broadcast_peers,
        } => {
            json!({
                "state": "proof_broadcast",
                "request_id": request_id,
                "covenant_proof": serde_json::to_value(covenant_proof)
                    .unwrap_or_else(|_| json!(null)),
                "broadcast_peers": broadcast_peers.len(),
            })
        }
        PaymentState::InMempool {
            request_id,
            tx_hash,
        } => {
            json!({
                "state": "in_mempool",
                "request_id": request_id,
                "tx_hash": hex::encode(tx_hash),
            })
        }
        PaymentState::Settled {
            request_id,
            tx_hash,
            block_hash,
            confirmation_count,
        } => {
            json!({
                "state": "settled",
                "request_id": request_id,
                "tx_hash": hex::encode(tx_hash),
                "block_hash": hex::encode(block_hash),
                "confirmation_count": confirmation_count,
            })
        }
        PaymentState::Failed { request_id, reason } => {
            json!({
                "state": "failed",
                "request_id": request_id,
                "reason": reason,
            })
        }
    }
}

/// Create success response
fn success_response(data: Value, request_id: String) -> Response<Full<Bytes>> {
    let response = ApiResponse::success(data, Some(request_id));
    let body = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len())
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// Create error response
fn error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    let error = ApiError::new(code, message, None, None, Some(request_id));
    let body = serde_json::to_string(&error).unwrap_or_else(|_| "{}".to_string());

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len())
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

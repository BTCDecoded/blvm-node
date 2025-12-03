//! REST API endpoints for Payment Pool operations
//!
//! Provides HTTP REST endpoints for pool management:
//! - Create pools
//! - Join pools
//! - Distribute from pools
//! - Get pool state

use crate::payment::state_machine::PaymentStateMachine;
use crate::rpc::rest::types::{error_response, success_response};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

/// Handle pool REST API requests
///
/// Routes:
/// - POST /api/v1/pools - Create pool
/// - POST /api/v1/pools/{id}/join - Join pool
/// - POST /api/v1/pools/{id}/distribute - Distribute from pool
/// - GET /api/v1/pools/{id} - Get pool state
#[cfg(feature = "ctv")]
pub async fn handle_pool_request(
    state_machine: Arc<PaymentStateMachine>,
    method: &Method,
    path: &str,
    body: Option<Value>,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!(
        "Pool REST request: {} {} (request_id: {})",
        method,
        path,
        &request_id[..8]
    );

    match (method, path) {
        (&Method::POST, "/api/v1/pools") => create_pool(state_machine, body, request_id).await,
        (method, path)
            if method == &Method::POST
                && path.starts_with("/api/v1/pools/")
                && path.ends_with("/join") =>
        {
            let pool_id = extract_id(path, "/api/v1/pools/", "/join");
            join_pool(state_machine, body, &pool_id, request_id).await
        }
        (method, path)
            if method == &Method::POST
                && path.starts_with("/api/v1/pools/")
                && path.ends_with("/distribute") =>
        {
            let pool_id = extract_id(path, "/api/v1/pools/", "/distribute");
            distribute_pool(state_machine, body, &pool_id, request_id).await
        }
        (&Method::GET, path) if path.starts_with("/api/v1/pools/") => {
            let pool_id = extract_id(path, "/api/v1/pools/", "");
            get_pool_state(state_machine, &pool_id, request_id).await
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("Pool endpoint not found: {} {}", method, path),
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
async fn create_pool(
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

    match state_machine.pool_engine() {
        Some(pool_engine) => {
            let pool_id = match body["pool_id"].as_str() {
                Some(id) => id.to_string(),
                None => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "pool_id required",
                        request_id,
                    );
                }
            };

            let initial_participants_json = match body["initial_participants"].as_array() {
                Some(arr) => arr,
                None => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "BAD_REQUEST",
                        "initial_participants array required",
                        request_id,
                    );
                }
            };

            let mut initial_participants = Vec::new();
            for p in initial_participants_json {
                let participant_id = match p["participant_id"].as_str() {
                    Some(id) => id.to_string(),
                    None => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            "participant_id required",
                            request_id,
                        );
                    }
                };
                let contribution = match p["contribution"].as_u64() {
                    Some(c) => c,
                    None => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            "contribution required",
                            request_id,
                        );
                    }
                };
                let script_hex = match p["script_pubkey"].as_str() {
                    Some(s) => s,
                    None => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            "script_pubkey required",
                            request_id,
                        );
                    }
                };
                let script = match hex::decode(script_hex) {
                    Ok(s) => s,
                    Err(e) => {
                        return error_response(
                            StatusCode::BAD_REQUEST,
                            "BAD_REQUEST",
                            &format!("Invalid script_pubkey: {}", e),
                            request_id,
                        );
                    }
                };
                initial_participants.push((participant_id, contribution, script));
            }

            let config = if body["config"].is_object() {
                serde_json::from_value(body["config"].clone())
                    .unwrap_or_else(|_| crate::payment::pool::PoolConfig::default())
            } else {
                crate::payment::pool::PoolConfig::default()
            };

            match pool_engine.create_pool(&pool_id, initial_participants, config) {
                Ok(pool_state) => {
                    let response_data = json!({
                        "pool_id": pool_state.pool_id,
                        "pool_state": serde_json::to_value(&pool_state)
                            .unwrap_or_else(|_| json!(null)),
                    });
                    success_response(response_data, request_id)
                }
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "POOL_CREATION_FAILED",
                    &format!("Failed to create pool: {}", e),
                    request_id,
                ),
            }
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_UNAVAILABLE",
            "Pool engine not available",
            request_id,
        ),
    }
}

#[cfg(feature = "ctv")]
async fn join_pool(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    pool_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    use super::types::ApiResponse;
    use serde_json::json;

    let pool_engine = match state_machine.pool_engine() {
        Some(engine) => engine,
        None => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "Pool engine not available",
                request_id,
            );
        }
    };

    // Get current pool state
    let pool_state = match pool_engine.get_pool(pool_id) {
        Ok(Some(state)) => state,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Pool {} not found", pool_id),
                request_id,
            );
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                &format!("Failed to get pool state: {}", e),
                request_id,
            );
        }
    };

    // Parse join request from body
    let participant_id = body
        .and_then(|v| v.get("participant_id").and_then(|id| id.as_str()))
        .unwrap_or("unknown")
        .to_string();

    let contribution = body
        .and_then(|v| v.get("contribution").and_then(|c| c.as_u64()))
        .unwrap_or(0);

    // Validate contribution meets minimum
    if contribution < pool_state.config.min_contribution {
        return error_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            &format!(
                "Contribution {} below minimum {}",
                contribution, pool_state.config.min_contribution
            ),
            request_id,
        );
    }

    // Join pool (implementation would call pool_engine.join_pool)
    // For now, return pool state
    let response = ApiResponse {
        success: true,
        data: Some(json!({
            "pool_id": pool_id,
            "participant_id": participant_id,
            "contribution": contribution,
            "message": "Join pool request received (full implementation pending)"
        })),
        error: None,
    };

    success_response(serde_json::to_string(&response).unwrap(), request_id)
}

#[cfg(feature = "ctv")]
async fn distribute_pool(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    pool_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    use super::types::ApiResponse;
    use serde_json::json;

    let pool_engine = match state_machine.pool_engine() {
        Some(engine) => engine,
        None => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "Pool engine not available",
                request_id,
            );
        }
    };

    // Get current pool state
    let pool_state = match pool_engine.get_pool(pool_id) {
        Ok(Some(state)) => state,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!("Pool {} not found", pool_id),
                request_id,
            );
        }
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                &format!("Failed to get pool state: {}", e),
                request_id,
            );
        }
    };

    // Parse distribution from body
    let distribution = body
        .and_then(|v| {
            v.get("distribution").and_then(|d| d.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        if let (Some(id), Some(amount)) = (
                            item.get("participant_id").and_then(|i| i.as_str()),
                            item.get("amount").and_then(|a| a.as_u64()),
                        ) {
                            Some((id.to_string(), amount))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();

    if distribution.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            "Distribution list is empty",
            request_id,
        );
    }

    // Distribute pool (implementation would call pool_engine.distribute)
    // For now, return pool state
    let response = ApiResponse {
        success: true,
        data: Some(json!({
            "pool_id": pool_id,
            "distribution": distribution,
            "total_balance": pool_state.total_balance,
            "message": "Distribute pool request received (full implementation pending)"
        })),
        error: None,
    };

    success_response(serde_json::to_string(&response).unwrap(), request_id)
}

#[cfg(feature = "ctv")]
async fn get_pool_state(
    state_machine: Arc<PaymentStateMachine>,
    pool_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    #[cfg(not(feature = "ctv"))]
    {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NOT_IMPLEMENTED",
            "Payment pools require CTV feature",
            request_id,
        );
    }

    #[cfg(feature = "ctv")]
    {
        match state_machine.pool_engine() {
            Some(pool_engine) => match pool_engine.get_pool(pool_id) {
                Ok(Some(pool_state)) => {
                    let response_data = json!({
                        "pool_id": pool_state.pool_id,
                        "pool_state": serde_json::to_value(&pool_state)
                            .unwrap_or_else(|_| json!(null)),
                    });
                    success_response(response_data, request_id)
                }
                Ok(None) => error_response(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    &format!("Pool {} not found", pool_id),
                    request_id,
                ),
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "POOL_LOAD_FAILED",
                    &format!("Failed to load pool: {}", e),
                    request_id,
                ),
            },
            None => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                "Pool engine not available",
                request_id,
            ),
        }
    }
}

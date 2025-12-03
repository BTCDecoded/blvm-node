//! REST API endpoints for Vault operations
//!
//! Provides HTTP REST endpoints for vault management:
//! - Create vaults
//! - Unvault funds
//! - Withdraw from vaults
//! - Get vault state

use crate::payment::state_machine::PaymentStateMachine;
use crate::rpc::rest::types::{error_response, success_response};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;

/// Handle vault REST API requests
///
/// Routes:
/// - POST /api/v1/vaults - Create vault
/// - POST /api/v1/vaults/{id}/unvault - Unvault funds
/// - POST /api/v1/vaults/{id}/withdraw - Withdraw from vault
/// - GET /api/v1/vaults/{id} - Get vault state
#[cfg(feature = "ctv")]
pub async fn handle_vault_request(
    state_machine: Arc<PaymentStateMachine>,
    method: &Method,
    path: &str,
    body: Option<Value>,
    request_id: String,
) -> Response<Full<Bytes>> {
    debug!(
        "Vault REST request: {} {} (request_id: {})",
        method,
        path,
        &request_id[..8]
    );

    // Parse path: /api/v1/vaults/{id}/...
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match (method, path_parts.as_slice()) {
        // POST /api/v1/vaults - Create vault
        (&Method::POST, ["api", "v1", "vaults"]) => {
            create_vault(state_machine, body, request_id).await
        }
        // POST /api/v1/vaults/{id}/unvault - Unvault funds
        (&Method::POST, ["api", "v1", "vaults", vault_id, "unvault"]) => {
            unvault_vault(state_machine, body, vault_id, request_id).await
        }
        // POST /api/v1/vaults/{id}/withdraw - Withdraw from vault
        (&Method::POST, ["api", "v1", "vaults", vault_id, "withdraw"]) => {
            withdraw_from_vault(state_machine, body, vault_id, request_id).await
        }
        // GET /api/v1/vaults/{id} - Get vault state
        (&Method::GET, ["api", "v1", "vaults", vault_id]) => {
            get_vault_state(state_machine, vault_id, request_id).await
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("Vault endpoint not found: {} {}", method, path),
            request_id,
        ),
    }
}

/// Create a new vault
async fn create_vault(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    request_id: String,
) -> Response<Full<Bytes>> {
    #[cfg(not(feature = "ctv"))]
    {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NOT_IMPLEMENTED",
            "Vaults require CTV feature",
            request_id,
        );
    }

    #[cfg(feature = "ctv")]
    {
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
        let vault_engine = match state_machine.vault_engine() {
            Some(engine) => engine,
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "SERVICE_UNAVAILABLE",
                    "Vault engine not available",
                    request_id,
                );
            }
        };

        let vault_id = match body["vault_id"].as_str() {
            Some(id) => id,
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    "vault_id required",
                    request_id,
                );
            }
        };

        let deposit_amount = match body["deposit_amount"].as_u64() {
            Some(amount) => amount,
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    "deposit_amount required",
                    request_id,
                );
            }
        };

        let withdrawal_script_hex = match body["withdrawal_script"].as_str() {
            Some(script) => script,
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    "withdrawal_script required",
                    request_id,
                );
            }
        };
        let withdrawal_script = match hex::decode(withdrawal_script_hex) {
            Ok(script) => script,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    &format!("Invalid withdrawal_script: {}", e),
                    request_id,
                );
            }
        };

        let config = if body["config"].is_object() {
            serde_json::from_value(body["config"].clone())
                .unwrap_or_else(|_| crate::payment::vault::VaultConfig::default())
        } else {
            crate::payment::vault::VaultConfig::default()
        };

        match vault_engine.create_vault(vault_id, deposit_amount, withdrawal_script, config) {
            Ok(vault_state) => {
                let response_data = json!({
                    "vault_id": vault_state.vault_id,
                    "vault_state": serde_json::to_value(&vault_state)
                        .unwrap_or_else(|_| json!(null)),
                });
                success_response(response_data, request_id)
            }
            Err(e) => {
                error!("Failed to create vault: {}", e);
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "VAULT_CREATION_FAILED",
                    &format!("Failed to create vault: {}", e),
                    request_id,
                )
            }
        }
    }
}

/// Unvault funds (first step of withdrawal)
async fn unvault_vault(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    vault_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    #[cfg(not(feature = "ctv"))]
    {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NOT_IMPLEMENTED",
            "Vaults require CTV feature",
            request_id,
        );
    }

    #[cfg(feature = "ctv")]
    {
        use super::types::ApiResponse;
        use serde_json::json;

        let vault_engine = match state_machine.vault_engine() {
            Some(engine) => engine,
            None => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "Vault engine not available",
                    request_id,
                );
            }
        };

        // Get vault state from storage
        let vault_state = match vault_engine.get_vault(vault_id) {
            Ok(Some(state)) => state,
            Ok(None) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    &format!("Vault {} not found", vault_id),
                    request_id,
                );
            }
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get vault state: {}", e),
                    request_id,
                );
            }
        };

        // Parse deposit amount from body
        let amount = body
            .and_then(|v| v.get("amount").and_then(|a| a.as_u64()))
            .unwrap_or(0);

        if amount == 0 {
            return error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Deposit amount must be greater than 0",
                request_id,
            );
        }

        // Deposit to vault (implementation would call vault_engine.deposit)
        let response = ApiResponse {
            success: true,
            data: Some(json!({
                "vault_id": vault_id,
                "amount": amount,
                "current_balance": vault_state.balance,
                "message": "Deposit request received (full implementation pending)"
            })),
            error: None,
        };

        return success_response(serde_json::to_string(&response).unwrap(), request_id);
    }
}

/// Withdraw from vault
async fn withdraw_from_vault(
    state_machine: Arc<PaymentStateMachine>,
    body: Option<Value>,
    vault_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    #[cfg(not(feature = "ctv"))]
    {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NOT_IMPLEMENTED",
            "Vaults require CTV feature",
            request_id,
        );
    }

    #[cfg(feature = "ctv")]
    {
        use super::types::ApiResponse;
        use serde_json::json;

        let vault_engine = match state_machine.vault_engine() {
            Some(engine) => engine,
            None => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    "Vault engine not available",
                    request_id,
                );
            }
        };

        // Get vault state from storage
        let vault_state = match vault_engine.get_vault(vault_id) {
            Ok(Some(state)) => state,
            Ok(None) => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    &format!("Vault {} not found", vault_id),
                    request_id,
                );
            }
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL_ERROR",
                    &format!("Failed to get vault state: {}", e),
                    request_id,
                );
            }
        };

        // Parse withdrawal amount from body
        let amount = body
            .and_then(|v| v.get("amount").and_then(|a| a.as_u64()))
            .unwrap_or(0);

        if amount == 0 {
            return error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                "Withdrawal amount must be greater than 0",
                request_id,
            );
        }

        if amount > vault_state.balance {
            return error_response(
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
                &format!("Insufficient balance: {} > {}", amount, vault_state.balance),
                request_id,
            );
        }

        // Withdraw from vault (implementation would call vault_engine.withdraw)
        let response = ApiResponse {
            success: true,
            data: Some(json!({
                "vault_id": vault_id,
                "amount": amount,
                "remaining_balance": vault_state.balance - amount,
                "message": "Withdrawal request received (full implementation pending)"
            })),
            error: None,
        };

        return success_response(serde_json::to_string(&response).unwrap(), request_id);
    }
}

/// Get vault state
async fn get_vault_state(
    state_machine: Arc<PaymentStateMachine>,
    vault_id: &str,
    request_id: String,
) -> Response<Full<Bytes>> {
    #[cfg(not(feature = "ctv"))]
    {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NOT_IMPLEMENTED",
            "Vaults require CTV feature",
            request_id,
        );
    }

    #[cfg(feature = "ctv")]
    {
        match state_machine.vault_engine() {
            Some(vault_engine) => match vault_engine.get_vault(vault_id) {
                Ok(Some(vault_state)) => {
                    let response_data = json!({
                        "vault_id": vault_state.vault_id,
                        "vault_state": serde_json::to_value(&vault_state)
                            .unwrap_or_else(|_| json!(null)),
                    });
                    success_response(response_data, request_id)
                }
                Ok(None) => error_response(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    &format!("Vault {} not found", vault_id),
                    request_id,
                ),
                Err(e) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "VAULT_LOAD_FAILED",
                    &format!("Failed to load vault: {}", e),
                    request_id,
                ),
            },
            None => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                "Vault engine not available",
                request_id,
            ),
        }
    }
}

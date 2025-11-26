//! RPC Error Types
//!
//! Bitcoin Core-compatible JSON-RPC error codes and error handling

use serde_json::{json, Value};
use std::fmt;

/// JSON-RPC error codes (Bitcoin Core compatible)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcErrorCode {
    /// Parse error (-32700)
    ParseError,
    /// Invalid request (-32600)
    InvalidRequest,
    /// Method not found (-32601)
    MethodNotFound,
    /// Invalid params (-32602)
    InvalidParams,
    /// Internal error (-32603)
    InternalError,
    /// Server error (reserved -32000 to -32099)
    ServerError(i32),
    /// Bitcoin Core specific errors
    /// Transaction already in block chain (-1)
    TxAlreadyInChain,
    /// Transaction rejected (-25)
    TxRejected,
    /// Transaction missing inputs (-1)
    TxMissingInputs,
    /// Transaction already in mempool (-27)
    TxAlreadyInMempool,
    /// Block not found (-5)
    BlockNotFound,
    /// Transaction not found (-5)
    TxNotFound,
    /// UTXO not found (-5)
    UtxoNotFound,
}

impl RpcErrorCode {
    /// Get numeric error code
    pub fn code(&self) -> i32 {
        match self {
            RpcErrorCode::ParseError => -32700,
            RpcErrorCode::InvalidRequest => -32600,
            RpcErrorCode::MethodNotFound => -32601,
            RpcErrorCode::InvalidParams => -32602,
            RpcErrorCode::InternalError => -32603,
            RpcErrorCode::ServerError(code) => *code,
            RpcErrorCode::TxAlreadyInChain => -1,
            RpcErrorCode::TxRejected => -25,
            RpcErrorCode::TxMissingInputs => -1,
            RpcErrorCode::TxAlreadyInMempool => -27,
            RpcErrorCode::BlockNotFound => -5,
            RpcErrorCode::TxNotFound => -5,
            RpcErrorCode::UtxoNotFound => -5,
        }
    }

    /// Get error message
    pub fn message(&self) -> &'static str {
        match self {
            RpcErrorCode::ParseError => "Parse error",
            RpcErrorCode::InvalidRequest => "Invalid Request",
            RpcErrorCode::MethodNotFound => "Method not found",
            RpcErrorCode::InvalidParams => "Invalid params",
            RpcErrorCode::InternalError => "Internal error",
            RpcErrorCode::ServerError(_) => "Server error",
            RpcErrorCode::TxAlreadyInChain => "Transaction already in block chain",
            RpcErrorCode::TxRejected => "Transaction rejected",
            RpcErrorCode::TxMissingInputs => "Missing inputs",
            RpcErrorCode::TxAlreadyInMempool => "Transaction already in mempool",
            RpcErrorCode::BlockNotFound => "Block not found",
            RpcErrorCode::TxNotFound => "Transaction not found",
            RpcErrorCode::UtxoNotFound => "No such UTXO",
        }
    }
}

/// RPC Error structure
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: RpcErrorCode,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    /// Create a new RPC error
    pub fn new(code: RpcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Create an error with additional data
    pub fn with_data(code: RpcErrorCode, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    /// Parse error
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(RpcErrorCode::ParseError, message)
    }

    /// Invalid request
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(RpcErrorCode::InvalidRequest, message)
    }

    /// Method not found
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            RpcErrorCode::MethodNotFound,
            format!("Method not found: {method}"),
        )
    }

    /// Invalid params
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(RpcErrorCode::InvalidParams, message)
    }

    /// Internal error
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(RpcErrorCode::InternalError, message)
    }

    /// Block not found
    pub fn block_not_found(hash: &str) -> Self {
        Self::new(
            RpcErrorCode::BlockNotFound,
            format!("Block not found: {hash}"),
        )
    }

    /// Transaction not found
    pub fn tx_not_found(txid: &str) -> Self {
        Self::new(
            RpcErrorCode::TxNotFound,
            format!("Transaction not found: {txid}"),
        )
    }

    /// UTXO not found
    pub fn utxo_not_found() -> Self {
        Self::new(RpcErrorCode::UtxoNotFound, "No such UTXO")
    }

    /// Transaction already in mempool
    pub fn tx_already_in_mempool(txid: &str) -> Self {
        Self::new(
            RpcErrorCode::TxAlreadyInMempool,
            format!("Transaction already in mempool: {txid}"),
        )
    }

    /// Transaction rejected
    pub fn tx_rejected(reason: impl Into<String>) -> Self {
        Self::new(RpcErrorCode::TxRejected, reason)
    }

    /// Transaction rejected with detailed context
    pub fn tx_rejected_with_context(
        reason: impl Into<String>,
        txid: Option<&str>,
        rejection_code: Option<&str>,
        details: Option<Value>,
    ) -> Self {
        let mut data = json!({});

        if let Some(txid) = txid {
            data["txid"] = json!(txid);
        }

        if let Some(code) = rejection_code {
            data["rejection_code"] = json!(code);
        }

        if let Some(details) = details {
            data["details"] = details;
        }

        Self::with_data(RpcErrorCode::TxRejected, reason, data)
    }

    /// Transaction rejected due to insufficient fee
    pub fn tx_rejected_insufficient_fee(
        txid: Option<&str>,
        required_fee_rate: f64,
        provided_fee_rate: f64,
        required_fee: Option<u64>,
        provided_fee: Option<u64>,
    ) -> Self {
        let mut data = json!({
            "reason": "insufficient_fee",
            "required_fee_rate": required_fee_rate,
            "provided_fee_rate": provided_fee_rate,
        });

        if let Some(txid) = txid {
            data["txid"] = json!(txid);
        }

        if let Some(required) = required_fee {
            data["required_fee_satoshis"] = json!(required);
        }

        if let Some(provided) = provided_fee {
            data["provided_fee_satoshis"] = json!(provided);
        }

        let mut suggestions = vec![
            format!(
                "Increase fee rate to at least {:.2} sat/vB",
                required_fee_rate
            ),
            "Use estimatesmartfee to get current fee recommendations".to_string(),
        ];

        if let Some(required) = required_fee {
            if let Some(provided) = provided_fee {
                let shortfall = required.saturating_sub(provided);
                suggestions.push(format!("Fee shortfall: {shortfall} satoshis"));
            }
        }

        data["suggestions"] = json!(suggestions);

        Self::with_data(
            RpcErrorCode::TxRejected,
            format!("Transaction rejected: insufficient fee (required: {required_fee_rate:.2} sat/vB, provided: {provided_fee_rate:.2} sat/vB)"),
            data,
        )
    }

    /// Invalid hash format error
    pub fn invalid_hash_format(
        hash: &str,
        expected_length: Option<usize>,
        reason: Option<&str>,
    ) -> Self {
        let mut data = json!({
            "hash": hash,
            "reason": reason.unwrap_or("Invalid hash format"),
        });

        if let Some(length) = expected_length {
            data["expected_length"] = json!(length);
            data["actual_length"] = json!(hash.len());
        }

        let mut suggestions = vec!["Hash must be a hexadecimal string".to_string()];
        if let Some(length) = expected_length {
            suggestions.push(format!(
                "Hash must be exactly {} characters ({} bytes)",
                length * 2,
                length
            ));
        }
        suggestions
            .push("Use lowercase or uppercase hexadecimal characters (0-9, a-f, A-F)".to_string());

        data["suggestions"] = json!(suggestions);

        Self::with_data(
            RpcErrorCode::InvalidParams,
            format!(
                "Invalid hash format: {}",
                reason.unwrap_or("Invalid hexadecimal string")
            ),
            data,
        )
    }

    /// Invalid address format error
    pub fn invalid_address_format(
        address: &str,
        reason: Option<&str>,
        expected_format: Option<&str>,
    ) -> Self {
        let mut data = json!({
            "address": address,
            "reason": reason.unwrap_or("Invalid address format"),
        });

        if let Some(format) = expected_format {
            data["expected_format"] = json!(format);
        }

        let mut suggestions = vec![
            "Address must be a valid Bitcoin address".to_string(),
            "Supported formats: P2PKH (starts with '1'), P2SH (starts with '3'), Bech32 (starts with 'bc1')".to_string(),
        ];

        if let Some(format) = expected_format {
            suggestions.push(format!("Expected format: {format}"));
        }

        data["suggestions"] = json!(suggestions);

        Self::with_data(
            RpcErrorCode::InvalidParams,
            format!(
                "Invalid address format: {}",
                reason.unwrap_or("Invalid Bitcoin address")
            ),
            data,
        )
    }

    /// Missing required parameter error
    pub fn missing_parameter(param_name: &str, param_type: Option<&str>) -> Self {
        let mut data = json!({
            "parameter": param_name,
        });

        if let Some(ty) = param_type {
            data["expected_type"] = json!(ty);
        }

        let mut suggestions = vec![format!("Provide the '{}' parameter", param_name)];
        if let Some(ty) = param_type {
            suggestions.push(format!("Parameter type should be: {ty}"));
        }

        data["suggestions"] = json!(suggestions);

        Self::with_data(
            RpcErrorCode::InvalidParams,
            format!("Missing required parameter: {param_name}"),
            data,
        )
    }

    /// Block not found with context
    pub fn block_not_found_with_context(
        hash: &str,
        suggestion: Option<&str>,
        available_height: Option<u64>,
    ) -> Self {
        let mut data = json!({
            "block_hash": hash,
        });

        if let Some(suggestion) = suggestion {
            data["suggestion"] = json!(suggestion);
        }

        if let Some(height) = available_height {
            data["available_height"] = json!(height);
        }

        Self::with_data(
            RpcErrorCode::BlockNotFound,
            format!("Block not found: {hash}"),
            data,
        )
    }

    /// Transaction not found with context
    pub fn tx_not_found_with_context(
        txid: &str,
        in_mempool: bool,
        suggestion: Option<&str>,
    ) -> Self {
        let mut data = json!({
            "txid": txid,
            "in_mempool": in_mempool,
        });

        if let Some(suggestion) = suggestion {
            data["suggestion"] = json!(suggestion);
        }

        Self::with_data(
            RpcErrorCode::TxNotFound,
            format!("Transaction not found: {txid}"),
            data,
        )
    }

    /// Invalid params with detailed field information
    pub fn invalid_params_with_fields(
        message: impl Into<String>,
        fields: Vec<(&str, &str)>,
        suggestions: Option<Value>,
    ) -> Self {
        let mut data = json!({
            "invalid_fields": fields
                .iter()
                .map(|(field, reason)| json!({
                    "field": field,
                    "reason": reason
                }))
                .collect::<Vec<_>>(),
        });

        if let Some(suggestions) = suggestions {
            data["suggestions"] = suggestions;
        }

        Self::with_data(RpcErrorCode::InvalidParams, message, data)
    }

    /// Add suggestion to error
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        let data = self.data.get_or_insert_with(|| json!({}));
        data["suggestion"] = json!(suggestion.into());
        self
    }

    /// Add context to error
    pub fn with_context(mut self, context: Value) -> Self {
        let data = self.data.get_or_insert_with(|| json!({}));
        if let Some(obj) = data.as_object_mut() {
            if let Some(ctx_obj) = context.as_object() {
                for (k, v) in ctx_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        self
    }

    /// Convert to JSON-RPC error response
    pub fn to_json(&self, id: Option<Value>) -> Value {
        let mut error = json!({
            "code": self.code.code(),
            "message": self.message,
        });

        if let Some(data) = &self.data {
            error["data"] = data.clone();
        } else {
            error["message"] = json!(self.message.clone());
        }

        json!({
            "jsonrpc": "2.0",
            "error": error,
            "id": id
        })
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPC Error {}: {}", self.code.code(), self.message)
    }
}

impl std::error::Error for RpcError {}

/// Result type for RPC operations
pub type RpcResult<T> = Result<T, RpcError>;

/// Convert anyhow error to RPC error
impl From<anyhow::Error> for RpcError {
    fn from(err: anyhow::Error) -> Self {
        RpcError::internal_error(err.to_string())
    }
}

/// Convert consensus error to RPC error
impl From<bllvm_protocol::ConsensusError> for RpcError {
    fn from(err: bllvm_protocol::ConsensusError) -> Self {
        RpcError::tx_rejected(format!("Consensus error: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_codes() {
        assert_eq!(RpcErrorCode::ParseError.code(), -32700);
        assert_eq!(RpcErrorCode::MethodNotFound.code(), -32601);
        assert_eq!(RpcErrorCode::BlockNotFound.code(), -5);
    }

    #[test]
    fn test_error_creation() {
        let err = RpcError::block_not_found("abc123");
        assert_eq!(err.code.code(), -5);
        assert!(err.message.contains("abc123"));
    }

    #[test]
    fn test_error_to_json() {
        let err = RpcError::method_not_found("test");
        let json = err.to_json(Some(json!(1)));

        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["error"]["code"], -32601);
        assert_eq!(json["id"], 1);
    }
}

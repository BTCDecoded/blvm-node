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
            format!("Method not found: {}", method),
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
            format!("Block not found: {}", hash),
        )
    }

    /// Transaction not found
    pub fn tx_not_found(txid: &str) -> Self {
        Self::new(
            RpcErrorCode::TxNotFound,
            format!("Transaction not found: {}", txid),
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
            format!("Transaction already in mempool: {}", txid),
        )
    }

    /// Transaction rejected
    pub fn tx_rejected(reason: impl Into<String>) -> Self {
        Self::new(RpcErrorCode::TxRejected, reason)
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
impl From<protocol_engine::error::ConsensusError> for RpcError {
    fn from(err: protocol_engine::error::ConsensusError) -> Self {
        RpcError::tx_rejected(format!("Consensus error: {}", err))
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

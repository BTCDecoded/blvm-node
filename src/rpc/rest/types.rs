//! REST API response types
//!
//! Standardized response format for all REST API endpoints

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Standard REST API response wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    /// Response data
    pub data: T,
    /// Metadata about the response
    pub meta: ResponseMeta,
    /// Related resource links
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<HashMap<String, String>>,
}

/// Response metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMeta {
    /// Timestamp of the response
    pub timestamp: String,
    /// API version
    pub version: String,
    /// Request ID for tracing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Error response format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    /// Error information
    pub error: ErrorDetails,
    /// Response metadata
    pub meta: ResponseMeta,
}

/// Error details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetails {
    /// Machine-readable error code
    pub code: String,
    /// Human-readable error message
    pub message: String,
    /// Additional error context
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Actionable suggestions for resolving the error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<Vec<String>>,
}

impl ApiResponse<Value> {
    /// Create a successful response
    pub fn success(data: Value, request_id: Option<String>) -> Self {
        Self {
            data,
            meta: ResponseMeta {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .to_string(),
                version: "1.0".to_string(),
                request_id,
            },
            links: None,
        }
    }

    /// Create a response with links
    pub fn with_links(mut self, links: HashMap<String, String>) -> Self {
        self.links = Some(links);
        self
    }
}

impl ApiError {
    /// Create an error response
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        details: Option<Value>,
        suggestions: Option<Vec<String>>,
        request_id: Option<String>,
    ) -> Self {
        Self {
            error: ErrorDetails {
                code: code.into(),
                message: message.into(),
                details,
                suggestions,
            },
            meta: ResponseMeta {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    .to_string(),
                version: "1.0".to_string(),
                request_id,
            },
        }
    }
}

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

// Helper functions for REST responses
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

/// Maximum request body size (1MB) - matches RPC server
const MAX_REQUEST_SIZE: usize = 1_048_576;

/// Read and parse JSON request body with size limit enforcement
pub async fn read_json_body(req: Request<Incoming>) -> Result<Option<Value>, String> {
    use http_body_util::BodyExt;
    let (_, body) = req.into_parts();
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => return Err(format!("Failed to read request body: {}", e)),
    };

    // Enforce maximum request size
    if body_bytes.len() > MAX_REQUEST_SIZE {
        return Err(format!(
            "Request body too large: {} bytes (max: {} bytes)",
            body_bytes.len(),
            MAX_REQUEST_SIZE
        ));
    }

    if body_bytes.is_empty() {
        Ok(None)
    } else {
        match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(v) => Ok(Some(v)),
            Err(_) => Ok(None), // Or return an error if body is malformed JSON
        }
    }
}

/// Create success response
pub fn success_response(data: Value, request_id: String) -> Response<Full<Bytes>> {
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
pub fn error_response(
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

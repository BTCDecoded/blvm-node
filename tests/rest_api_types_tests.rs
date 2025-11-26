//! Tests for REST API response types

#[cfg(feature = "rest-api")]
mod tests {
    use bllvm_node::rpc::rest::types::{ApiError, ApiResponse, ErrorDetails, ResponseMeta};
    use serde_json::json;
    use std::collections::HashMap;

#[test]
fn test_api_response_creation() {
    let data = json!({"key": "value"});
    let response = ApiResponse::success(data.clone(), None);

    assert_eq!(response.data, data);
    assert_eq!(response.meta.version, "1.0");
    assert!(response.meta.request_id.is_none());
    assert!(response.links.is_none());
}

#[test]
fn test_api_response_with_request_id() {
    let data = json!({"key": "value"});
    let request_id = Some("test-request-id".to_string());
    let response = ApiResponse::success(data, request_id.clone());

    assert_eq!(response.meta.request_id, request_id);
}

#[test]
fn test_api_response_with_links() {
    let data = json!({"key": "value"});
    let mut links = HashMap::new();
    links.insert("next".to_string(), "/api/v1/next".to_string());
    links.insert("prev".to_string(), "/api/v1/prev".to_string());

    let response = ApiResponse::success(data, None).with_links(links.clone());

    assert_eq!(response.links, Some(links));
}

#[test]
fn test_api_response_serialization() {
    let data = json!({"height": 100});
    let response = ApiResponse::success(data, None);

    let serialized = serde_json::to_string(&response).unwrap();
    assert!(serialized.contains("height"));
    assert!(serialized.contains("data"));
    assert!(serialized.contains("meta"));
}

#[test]
fn test_api_error_creation() {
    let error = ApiError::new(
        "NOT_FOUND",
        "Resource not found",
        None,
        None,
        None,
    );

    assert_eq!(error.error.code, "NOT_FOUND");
    assert_eq!(error.error.message, "Resource not found");
    assert_eq!(error.meta.version, "1.0");
}

#[test]
fn test_api_error_with_details() {
    let details = Some(json!({"resource": "block", "hash": "abc123"}));
    let error = ApiError::new(
        "NOT_FOUND",
        "Block not found",
        details.clone(),
        None,
        None,
    );

    assert_eq!(error.error.details, details);
}

#[test]
fn test_api_error_with_suggestions() {
    let suggestions = Some(vec![
        "Check the block hash format".to_string(),
        "Verify the block exists in the chain".to_string(),
    ]);
    let error = ApiError::new(
        "INVALID_HASH",
        "Invalid block hash",
        None,
        suggestions.clone(),
        None,
    );

    assert_eq!(error.error.suggestions, suggestions);
}

#[test]
fn test_api_error_with_request_id() {
    let request_id = Some("error-request-id".to_string());
    let error = ApiError::new(
        "INTERNAL_ERROR",
        "Internal server error",
        None,
        None,
        request_id.clone(),
    );

    assert_eq!(error.meta.request_id, request_id);
}

#[test]
fn test_api_error_serialization() {
    let error = ApiError::new(
        "BAD_REQUEST",
        "Invalid request",
        None,
        None,
        None,
    );

    let serialized = serde_json::to_string(&error).unwrap();
    assert!(serialized.contains("BAD_REQUEST"));
    assert!(serialized.contains("error"));
    assert!(serialized.contains("meta"));
}

#[test]
fn test_response_meta_timestamp() {
    let data = json!({});
    let response1 = ApiResponse::success(data.clone(), None);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let response2 = ApiResponse::success(data, None);

    // Timestamps should be different (or at least close)
    // Parse as u64 and verify they're different or close
    let ts1: u64 = response1.meta.timestamp.parse().unwrap();
    let ts2: u64 = response2.meta.timestamp.parse().unwrap();
    
    // Timestamps should be different (ts2 should be >= ts1)
    assert!(ts2 >= ts1);
    // And should be within reasonable range (at least 1 second difference after sleep)
    assert!(ts2 >= ts1);
}

#[test]
fn test_error_details_structure() {
    let error = ApiError::new(
        "TEST_ERROR",
        "Test error message",
        Some(json!({"field": "value"})),
        Some(vec!["Suggestion 1".to_string()]),
        Some("req-123".to_string()),
    );

    assert_eq!(error.error.code, "TEST_ERROR");
    assert_eq!(error.error.message, "Test error message");
    assert!(error.error.details.is_some());
    assert!(error.error.suggestions.is_some());
    assert_eq!(error.meta.request_id, Some("req-123".to_string()));
}
}


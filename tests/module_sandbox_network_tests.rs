//! Tests for network sandbox

use bllvm_node::module::sandbox::NetworkSandbox;
use bllvm_node::module::traits::ModuleError;

#[test]
fn test_network_sandbox_creation() {
    let sandbox = NetworkSandbox::new();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_network_sandbox_default() {
    let sandbox = NetworkSandbox::default();
    // Should create successfully
    assert!(true);
}

#[test]
fn test_network_sandbox_no_access_by_default() {
    let sandbox = NetworkSandbox::new();

    // Network access should be denied by default
    assert!(!sandbox.is_network_allowed());
}

#[test]
fn test_network_sandbox_with_allowed_endpoints() {
    let endpoints = vec!["https://api.example.com".to_string()];
    let sandbox = NetworkSandbox::with_allowed_endpoints(endpoints);

    // Network access should be allowed if endpoints are provided
    assert!(sandbox.is_network_allowed());
}

#[test]
fn test_network_sandbox_empty_endpoints() {
    let sandbox = NetworkSandbox::with_allowed_endpoints(vec![]);

    // Network access should be denied if no endpoints
    assert!(!sandbox.is_network_allowed());
}

#[test]
fn test_validate_network_operation_denied() {
    let sandbox = NetworkSandbox::new();

    // Network operations should be denied
    let result = sandbox.validate_network_operation("connect");
    assert!(result.is_err());

    match result.unwrap_err() {
        ModuleError::OperationError(msg) => {
            assert!(msg.contains("Network access denied") || msg.contains("denied"));
        }
        _ => panic!("Expected OperationError"),
    }
}

#[test]
fn test_validate_network_operation_allowed() {
    let endpoints = vec!["https://api.example.com".to_string()];
    let sandbox = NetworkSandbox::with_allowed_endpoints(endpoints);

    // Network operations should be allowed
    let result = sandbox.validate_network_operation("connect");
    assert!(result.is_ok());
}

#[test]
fn test_validate_endpoint_denied() {
    let sandbox = NetworkSandbox::new();

    // Endpoint validation should fail
    let result = sandbox.validate_endpoint("https://api.example.com");
    assert!(result.is_err());
}

#[test]
fn test_validate_endpoint_allowed() {
    let endpoints = vec!["https://api.example.com".to_string()];
    let sandbox = NetworkSandbox::with_allowed_endpoints(endpoints);

    // Allowed endpoint should validate successfully
    let result = sandbox.validate_endpoint("https://api.example.com");
    assert!(result.is_ok());
}

#[test]
fn test_validate_endpoint_not_in_list() {
    let endpoints = vec!["https://api.example.com".to_string()];
    let sandbox = NetworkSandbox::with_allowed_endpoints(endpoints);

    // Different endpoint should fail
    let result = sandbox.validate_endpoint("https://other.example.com");
    assert!(result.is_err());

    match result.unwrap_err() {
        ModuleError::OperationError(msg) => {
            assert!(msg.contains("not allowed") || msg.contains("not in allowed list"));
        }
        _ => panic!("Expected OperationError"),
    }
}

#[test]
fn test_validate_endpoint_prefix_match() {
    let endpoints = vec!["https://api.example.com".to_string()];
    let sandbox = NetworkSandbox::with_allowed_endpoints(endpoints);

    // Endpoint with path should match prefix
    let result = sandbox.validate_endpoint("https://api.example.com/v1/endpoint");
    assert!(result.is_ok());
}

#[test]
fn test_validate_endpoint_multiple_allowed() {
    let endpoints = vec![
        "https://api1.example.com".to_string(),
        "https://api2.example.com".to_string(),
    ];
    let sandbox = NetworkSandbox::with_allowed_endpoints(endpoints);

    // Both endpoints should be allowed
    assert!(sandbox
        .validate_endpoint("https://api1.example.com")
        .is_ok());
    assert!(sandbox
        .validate_endpoint("https://api2.example.com")
        .is_ok());

    // Other endpoint should be denied
    assert!(sandbox
        .validate_endpoint("https://api3.example.com")
        .is_err());
}

#[test]
fn test_validate_endpoint_empty_allowed_list() {
    let sandbox = NetworkSandbox::with_allowed_endpoints(vec![]);

    // Even with empty list, network should be denied
    assert!(!sandbox.is_network_allowed());
    assert!(sandbox
        .validate_endpoint("https://api.example.com")
        .is_err());
}

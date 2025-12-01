//! REST API Security Integration Tests
//!
//! Comprehensive tests for security features:
//! - Authentication (required/optional)
//! - Rate limiting (per-user, per-IP)
//! - Security headers
//! - Input validation

use crate::rpc::auth::{RpcAuthManager, UserId};
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};

// Note: Full integration tests would require starting the HTTP server
// and making actual HTTP requests. These tests focus on the security
// logic itself (authentication, rate limiting, validation) which can
// be tested without a full server instance.

/// Test that authentication is required when enabled
#[tokio::test]
async fn test_rest_api_auth_required() {
    let auth = RpcAuthManager::new(true);
    auth.add_token("test-token".to_string()).await.unwrap();

    let mut headers = hyper::HeaderMap::new();
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Test without token - should fail
    let result = auth.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
    assert!(result.error.is_some());
    assert!(result.requires_auth);

    // Test with invalid token - should fail
    headers.insert("authorization", "Bearer invalid-token".parse().unwrap());
    let result = auth.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
    assert!(result.error.is_some());

    // Test with valid token - should succeed
    headers.insert("authorization", "Bearer test-token".parse().unwrap());
    let result = auth.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
    assert!(result.error.is_none());
}

/// Test that authentication is optional when disabled
#[tokio::test]
async fn test_rest_api_auth_optional() {
    let auth = RpcAuthManager::new(false);
    let headers = hyper::HeaderMap::new();
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    let result = auth.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
    assert!(!result.requires_auth);
    assert!(result.error.is_none());
}

/// Test rate limiting with endpoint information
#[tokio::test]
async fn test_rest_api_rate_limiting_with_endpoint() {
    let auth = RpcAuthManager::with_rate_limits(false, 5, 1); // 5 burst, 1/sec
    let user_id = UserId::Ip("127.0.0.1:8080".parse().unwrap());
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Should allow 5 requests (burst)
    for i in 0..5 {
        assert!(
            auth.check_rate_limit_with_endpoint(&user_id, Some(addr), Some("/api/v1/blocks/123")).await,
            "Request {} should be allowed", i + 1
        );
    }

    // 6th request should be rate limited
    assert!(
        !auth.check_rate_limit_with_endpoint(&user_id, Some(addr), Some("/api/v1/blocks/123")).await,
        "6th request should be rate limited"
    );
}

/// Test IP-based rate limiting for unauthenticated requests
#[tokio::test]
async fn test_rest_api_ip_rate_limiting() {
    let auth = RpcAuthManager::with_rate_limits(false, 10, 2); // 10 burst, 2/sec
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Should allow requests up to IP limit (half of authenticated: 5 burst)
    for i in 0..5 {
        assert!(
            auth.check_ip_rate_limit_with_endpoint(addr, Some("/api/v1/chain/info")).await,
            "Request {} should be allowed", i + 1
        );
    }

    // Next request should be rate limited
    assert!(
        !auth.check_ip_rate_limit_with_endpoint(addr, Some("/api/v1/chain/info")).await,
        "Request should be rate limited after IP limit"
    );
}

/// Test brute force detection
#[tokio::test]
async fn test_brute_force_detection() {
    let auth = RpcAuthManager::new(true);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Make 5 failed authentication attempts
    for i in 0..5 {
        auth.record_auth_failure(addr, format!("Invalid token attempt {}", i)).await;
    }

    // The 5th failure should trigger brute force detection
    // (threshold is 5 failures in 5 minutes)
    // Note: This test verifies the tracking mechanism works
    // In a real scenario, we'd check logs for the ERROR level message
}

/// Test input validation for transaction hex
#[tokio::test]
async fn test_input_validation_transaction_hex() {
    use crate::rpc::rest::validation;

    // Valid hex
    assert!(validation::validate_transaction_hex("deadbeef").is_ok());

    // Invalid: odd length
    assert!(validation::validate_transaction_hex("deadbee").is_err());

    // Invalid: non-hex characters
    assert!(validation::validate_transaction_hex("deadbeef!").is_err());

    // Invalid: empty
    assert!(validation::validate_transaction_hex("").is_err());
}

/// Test input validation for block hash
#[tokio::test]
async fn test_input_validation_block_hash() {
    use crate::rpc::rest::validation;

    // Valid hash (64 hex chars)
    let valid_hash = "0".repeat(64);
    assert!(validation::validate_hash_string(&valid_hash).is_ok());

    // Invalid: wrong length
    assert!(validation::validate_hash_string("deadbeef").is_err());

    // Invalid: odd length
    let odd_hash = "0".repeat(63);
    assert!(validation::validate_hash_string(&odd_hash).is_err());
}

/// Test input validation for address
#[tokio::test]
async fn test_input_validation_address() {
    use crate::rpc::rest::validation;

    // Valid address
    assert!(validation::validate_address_string("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh").is_ok());

    // Invalid: empty
    assert!(validation::validate_address_string("").is_err());
}

/// Test input validation for block height
#[tokio::test]
async fn test_input_validation_block_height() {
    use crate::rpc::rest::validation;

    // Valid heights
    assert!(validation::validate_block_height(0).is_ok());
    assert!(validation::validate_block_height(800000).is_ok());

    // Invalid: too large (exceeds MAX_BLOCK_HEIGHT = 2_000_000_000)
    assert!(validation::validate_block_height(3_000_000_000).is_err());
}

/// Test security event logging
#[tokio::test]
async fn test_security_event_logging() {
    use crate::rpc::auth::SecurityEvent;
    use std::net::SocketAddr;

    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Test that events can be created and logged (doesn't crash)
    let event = SecurityEvent::AuthFailure {
        client_addr: addr,
        reason: "Invalid token".to_string(),
    };
    event.log(); // Should not panic

    let event = SecurityEvent::RateLimitViolation {
        user_id: "test-user".to_string(),
        client_addr: addr,
        endpoint: "/api/v1/blocks/123".to_string(),
    };
    event.log(); // Should not panic

    let event = SecurityEvent::RepeatedAuthFailures {
        client_addr: addr,
        failure_count: 5,
        time_window_seconds: 300,
    };
    event.log(); // Should not panic

    let event = SecurityEvent::AuthSuccess {
        user_id: "test-user".to_string(),
        client_addr: addr,
        auth_method: "token".to_string(),
    };
    event.log(); // Should not panic
}

/// Test that rate limiting respects time windows
#[tokio::test]
async fn test_rate_limiting_time_window() {
    let auth = RpcAuthManager::with_rate_limits(false, 5, 1); // 5 burst, 1/sec
    let user_id = UserId::Ip("127.0.0.1:8080".parse().unwrap());

    // Exhaust burst
    for _ in 0..5 {
        assert!(auth.check_rate_limit(&user_id).await);
    }

    // Should be rate limited immediately
    assert!(!auth.check_rate_limit(&user_id).await);

    // Wait 2 seconds (should refill 2 tokens)
    sleep(Duration::from_secs(2)).await;

    // Should now allow 2 more requests
    assert!(auth.check_rate_limit(&user_id).await);
    assert!(auth.check_rate_limit(&user_id).await);
    assert!(!auth.check_rate_limit(&user_id).await);
}


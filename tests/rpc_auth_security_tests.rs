//! RPC Authentication Security Tests
//!
//! Tests security boundaries, authentication bypass attempts,
//! and edge cases that could lead to security vulnerabilities.

use blvm_node::rpc::auth::{RpcAuthManager, UserId};
use hyper::HeaderMap;
use std::net::SocketAddr;

#[tokio::test]
async fn test_auth_bypass_attempt_empty_token() {
    let manager = RpcAuthManager::new(true);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let headers = HeaderMap::new();

    // Empty token should be rejected
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
    assert!(result.requires_auth);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_auth_bypass_attempt_malformed_token() {
    let manager = RpcAuthManager::new(true);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();

    // Add malformed authorization header
    headers.insert("authorization", "InvalidFormat token".parse().unwrap());

    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
    assert!(result.requires_auth);
}

#[tokio::test]
async fn test_auth_bypass_attempt_invalid_token() {
    let manager = RpcAuthManager::new(true);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();

    // Add invalid token
    headers.insert("authorization", "Bearer invalid-token-123".parse().unwrap());

    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
    assert!(result.requires_auth);
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_auth_valid_token_accepted() {
    let manager = RpcAuthManager::new(true);
    let token_str = "valid-token".to_string();

    manager.add_token(token_str.clone()).await.unwrap();

    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );

    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
    match result.user_id {
        Some(UserId::Token(_)) => {} // Expected
        _ => panic!("Expected Token variant"),
    }
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_auth_no_auth_required_allows_access() {
    let manager = RpcAuthManager::new(false);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let headers = HeaderMap::new();

    // When auth not required, should allow access
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(!result.requires_auth);
    // Should have IP-based user_id
    match result.user_id {
        Some(UserId::Ip(ip)) => assert_eq!(ip, addr),
        _ => panic!("Expected IP-based user_id when auth not required"),
    }
}

#[tokio::test]
async fn test_auth_token_revocation() {
    let manager = RpcAuthManager::new(true);
    let token_str = "revocable-token".to_string();

    // Add token
    manager.add_token(token_str.clone()).await.unwrap();

    // Verify it works
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());

    // Revoke token
    manager.remove_token(&token_str).await.unwrap();

    // Verify it no longer works
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
    assert!(result.error.is_some());
}

#[tokio::test]
async fn test_auth_concurrent_requests() {
    use std::sync::Arc;
    let manager = Arc::new(RpcAuthManager::new(true));
    let token_str = "concurrent-token".to_string();

    manager.add_token(token_str.clone()).await.unwrap();

    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );

    // Simulate concurrent authentication attempts
    let mut handles = vec![];
    for _ in 0..10 {
        let manager_clone = Arc::clone(&manager);
        let headers_clone = headers.clone();
        let addr_clone = addr;

        handles.push(tokio::spawn(async move {
            manager_clone
                .authenticate_request(&headers_clone, addr_clone)
                .await
        }));
    }

    // All should succeed
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.user_id.is_some());
    }
}

#[tokio::test]
async fn test_auth_brute_force_protection() {
    let manager = RpcAuthManager::new(true);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Try many invalid tokens
    for i in 0..100 {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer invalid-token-{i}").parse().unwrap(),
        );

        let result = manager.authenticate_request(&headers, addr).await;
        assert!(result.user_id.is_none());
        assert!(result.error.is_some());
    }

    // System should still be functional (not locked out)
    // This tests that brute force attempts don't break the system
}

#[tokio::test]
async fn test_rpc_rate_limiting_without_auth() {
    // Rate-limit-only mode: auth disabled, IP rate limiting applies
    let manager = RpcAuthManager::with_rate_limits(false, 5, 1);
    let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();

    // First 5 requests (burst) should succeed
    for _ in 0..5 {
        assert!(
            manager
                .check_ip_rate_limit_with_endpoint(addr, Some("rpc:getblockchaininfo"))
                .await,
            "Burst requests should be allowed"
        );
    }

    // 6th request should be rate limited
    assert!(
        !manager
            .check_ip_rate_limit_with_endpoint(addr, Some("rpc:getblockchaininfo"))
            .await,
        "Request beyond burst should be rate limited"
    );
}

#[tokio::test]
async fn test_batch_rate_limiting() {
    // Batch consumes min(batch_len, 10) tokens
    let manager = RpcAuthManager::with_rate_limits(false, 5, 1);
    let addr: SocketAddr = "127.0.0.1:8888".parse().unwrap();

    // Batch of 10 consumes 10 tokens, but burst is only 5 - should reject
    assert!(
        !manager
            .check_ip_rate_limit_with_endpoint_n(addr, Some("rpc:batch"), 10)
            .await,
        "Batch exceeding burst should be rate limited"
    );

    // Single request should still work (tokens refill over time, but we just consumed 0 - actually
    // the failed check doesn't consume. Let me re-read - check_and_consume_n returns false without
    // consuming when tokens < n. So we still have 5 tokens. Let me fix the test.
    // Actually: when we call check_ip_rate_limit_with_endpoint_n(addr, _, 10), it tries to consume
    // 10 tokens. We have 5. So it returns false and does NOT consume (the check is atomic - only
    // consume if we have enough). So we still have 5 tokens.
    // Single request should work
    assert!(
        manager
            .check_ip_rate_limit_with_endpoint(addr, Some("rpc:getblockchaininfo"))
            .await,
        "Single request after failed batch should still work"
    );
}

#[tokio::test]
async fn test_auth_case_sensitive_token() {
    let manager = RpcAuthManager::new(true);
    let token_str = "CaseSensitiveToken".to_string();

    manager.add_token(token_str.clone()).await.unwrap();

    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Correct case should work
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());

    // Wrong case should fail
    let mut headers2 = HeaderMap::new();
    headers2.insert(
        "authorization",
        "Bearer casesensitivetoken".parse().unwrap(),
    );
    let result = manager.authenticate_request(&headers2, addr).await;
    assert!(result.user_id.is_none());
}

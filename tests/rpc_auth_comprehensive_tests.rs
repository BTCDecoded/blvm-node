//! Comprehensive RPC Authentication Tests
//!
//! Tests token-based authentication, certificate-based authentication,
//! and all authentication edge cases.

use blvm_node::rpc::auth::{AuthToken, RpcAuthManager, RpcRateLimiter, UserId};
use std::net::SocketAddr;

#[test]
fn test_auth_token_creation() {
    let token = AuthToken::new("test-token-123".to_string());
    assert_eq!(token.as_str(), "test-token-123");
}

#[test]
fn test_user_id_token() {
    let token = AuthToken::new("token-123".to_string());
    let user_id = UserId::Token(token);

    match user_id {
        UserId::Token(t) => assert_eq!(t.as_str(), "token-123"),
        _ => panic!("Expected Token variant"),
    }
}

#[test]
fn test_user_id_certificate() {
    let user_id = UserId::Certificate("fingerprint-abc".to_string());

    match user_id {
        UserId::Certificate(f) => assert_eq!(f, "fingerprint-abc"),
        _ => panic!("Expected Certificate variant"),
    }
}

#[test]
fn test_user_id_ip() {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let user_id = UserId::Ip(addr);

    match user_id {
        UserId::Ip(a) => assert_eq!(a, addr),
        _ => panic!("Expected Ip variant"),
    }
}

#[test]
fn test_rate_limiter_creation() {
    let limiter = RpcRateLimiter::new(10, 5);
    assert_eq!(limiter.tokens_remaining(), 10);
}

#[test]
fn test_rate_limiter_consume_token() {
    let mut limiter = RpcRateLimiter::new(10, 5);

    // Should allow first request
    assert!(limiter.check_and_consume());
    assert_eq!(limiter.tokens_remaining(), 9);
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_exhaust_tokens() {
    let mut limiter = RpcRateLimiter::new(3, 1);

    // Consume all tokens
    assert!(limiter.check_and_consume());
    assert!(limiter.check_and_consume());
    assert!(limiter.check_and_consume());

    // Should be exhausted
    assert_eq!(limiter.tokens_remaining(), 0);
    assert!(!limiter.check_and_consume());
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_refill() {
    let mut limiter = RpcRateLimiter::new(10, 2);

    // Exhaust tokens
    for _ in 0..10 {
        assert!(limiter.check_and_consume());
    }
    assert_eq!(limiter.tokens_remaining(), 0);

    // 2/sec: one token every 500ms
    tokio::time::advance(std::time::Duration::from_millis(499)).await;
    assert!(!limiter.check_and_consume());
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert!(limiter.check_and_consume());
    assert!(!limiter.check_and_consume());
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_burst_limit() {
    let mut limiter = RpcRateLimiter::new(5, 10);

    for _ in 0..5 {
        assert!(limiter.check_and_consume());
    }
    assert!(!limiter.check_and_consume());

    // A long idle period refills to the burst limit and no further
    tokio::time::advance(std::time::Duration::from_secs(86_400)).await;
    for _ in 0..5 {
        assert!(limiter.check_and_consume());
    }
    assert!(!limiter.check_and_consume());
}

#[tokio::test]
async fn test_auth_manager_creation() {
    let manager = RpcAuthManager::new(false);
    // Manager should be created successfully
    // Test by authenticating without token - should work when auth not required
    let headers = hyper::HeaderMap::new();
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(!result.requires_auth);
}

#[tokio::test]
async fn test_auth_manager_auth_required() {
    let manager = RpcAuthManager::new(true);
    // Test by authenticating without token - should fail when auth required
    let headers = hyper::HeaderMap::new();
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.requires_auth);
    assert!(result.user_id.is_none());
}

#[tokio::test]
async fn test_auth_manager_http_basic_password_only() {
    let manager = RpcAuthManager::new(true);
    manager
        .set_basic_auth(None, "pool-secret".to_string())
        .await;

    let mut headers = hyper::HeaderMap::new();
    // anyuser:pool-secret
    headers.insert(
        "authorization",
        "Basic YW55dXNlcjpwb29sLXNlY3JldA==".parse().unwrap(),
    );
    let addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_auth_manager_add_token() {
    let manager = RpcAuthManager::new(false);
    let token_str = "test-token".to_string();

    manager.add_token(token_str.clone()).await.unwrap();

    // Verify token was added by trying to authenticate
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
}

#[tokio::test]
async fn test_auth_manager_remove_token() {
    let manager = RpcAuthManager::new(false);
    let token_str = "test-token".to_string();

    manager.add_token(token_str.clone()).await.unwrap();

    // Verify token works
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());

    // Remove token
    manager.remove_token(&token_str).await.unwrap();

    // Verify token no longer works
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
}

#[tokio::test]
async fn test_auth_manager_add_certificate() {
    let manager = RpcAuthManager::new(false);
    let fingerprint = "abc123".to_string();

    manager.add_certificate(fingerprint.clone()).await.unwrap();

    // Verify certificate was added by trying to authenticate
    let mut headers = hyper::HeaderMap::new();
    headers.insert("x-client-cert-fingerprint", fingerprint.parse().unwrap());
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
}

#[tokio::test(start_paused = true)]
async fn test_auth_manager_set_user_rate_limit() {
    let manager = RpcAuthManager::new(false);
    let token_str = "token".to_string();
    manager.add_token(token_str.clone()).await.unwrap();

    // Authenticate to get user_id
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token_str}").parse().unwrap(),
    );
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    let user_id = result.user_id.unwrap();

    manager.set_user_rate_limit(&user_id, 50, 5).await;

    // Verify rate limit was set by checking it's used
    // User should be able to make 50 requests
    for _ in 0..50 {
        assert!(manager.check_rate_limit(&user_id).await);
    }
    // 51st should be rate limited
    assert!(!manager.check_rate_limit(&user_id).await);
}

#[tokio::test(start_paused = true)]
async fn test_auth_manager_set_method_rate_limit() {
    let manager = RpcAuthManager::new(false);

    manager.set_method_rate_limit("getblock", 20, 2).await;

    // Verify method rate limit was set by checking it's enforced
    // Should allow 20 requests then rate limit
    for _ in 0..20 {
        assert!(manager.check_method_rate_limit("getblock").await);
    }
    assert!(!manager.check_method_rate_limit("getblock").await);
}

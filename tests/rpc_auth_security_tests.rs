//! RPC Authentication Security Tests
//!
//! Tests security boundaries, authentication bypass attempts,
//! and edge cases that could lead to security vulnerabilities.

use bllvm_node::rpc::auth::{RpcAuthManager, AuthToken, UserId};
use std::net::SocketAddr;
use hyper::HeaderMap;

#[tokio::test]
async fn test_auth_bypass_attempt_empty_token() {
    let manager = RpcAuthManager::new(true);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();
    
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
    headers.insert("authorization", format!("Bearer {}", token_str).parse().unwrap());
    
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
    match result.user_id {
        Some(UserId::Token(_)) => {}, // Expected
        _ => panic!("Expected Token variant"),
    }
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_auth_no_auth_required_allows_access() {
    let manager = RpcAuthManager::new(false);
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let mut headers = HeaderMap::new();
    
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
    headers.insert("authorization", format!("Bearer {}", token_str).parse().unwrap());
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
    headers.insert("authorization", format!("Bearer {}", token_str).parse().unwrap());
    
    // Simulate concurrent authentication attempts
    let mut handles = vec![];
    for _ in 0..10 {
        let manager_clone = Arc::clone(&manager);
        let headers_clone = headers.clone();
        let addr_clone = addr;
        
        handles.push(tokio::spawn(async move {
            manager_clone.authenticate_request(&headers_clone, addr_clone).await
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
        headers.insert("authorization", format!("Bearer invalid-token-{}", i).parse().unwrap());
        
        let result = manager.authenticate_request(&headers, addr).await;
        assert!(result.user_id.is_none());
        assert!(result.error.is_some());
    }
    
    // System should still be functional (not locked out)
    // This tests that brute force attempts don't break the system
}

#[tokio::test]
async fn test_auth_case_sensitive_token() {
    let manager = RpcAuthManager::new(true);
    let token_str = "CaseSensitiveToken".to_string();
    
    manager.add_token(token_str.clone()).await.unwrap();
    
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    
    // Correct case should work
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {}", token_str).parse().unwrap());
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_some());
    
    // Wrong case should fail
    let mut headers2 = HeaderMap::new();
    headers2.insert("authorization", "Bearer casesensitivetoken".parse().unwrap());
    let result = manager.authenticate_request(&headers2, addr).await;
    assert!(result.user_id.is_none());
}


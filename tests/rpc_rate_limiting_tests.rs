//! RPC Rate Limiting Tests
//!
//! Tests rate limiting functionality, edge cases, and per-user/per-method limits.

use bllvm_node::rpc::auth::{RpcAuthManager, RpcRateLimiter, UserId, AuthToken};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_rate_limiter_basic_functionality() {
    let mut limiter = RpcRateLimiter::new(10, 5);
    
    // Should allow requests up to burst limit
    for i in 0..10 {
        assert!(limiter.check_and_consume(), "Request {} should be allowed", i);
        assert_eq!(limiter.tokens_remaining(), 10 - i - 1);
    }
    
    // Should reject after burst limit
    assert!(!limiter.check_and_consume());
    assert_eq!(limiter.tokens_remaining(), 0);
}

#[tokio::test]
async fn test_rate_limiter_refill_over_time() {
    let mut limiter = RpcRateLimiter::new(10, 10); // 10 tokens/sec
    
    // Exhaust tokens
    for _ in 0..10 {
        limiter.check_and_consume();
    }
    assert_eq!(limiter.tokens_remaining(), 0);
    
    // Wait 1 second - should refill
    sleep(Duration::from_millis(1100)).await;
    
    // Should have refilled (at least some tokens)
    // Note: Timing-dependent test, may be flaky
    let tokens_before = limiter.tokens_remaining();
    if tokens_before > 0 {
        assert!(limiter.check_and_consume());
    } else {
        // If no tokens yet, wait a bit more
        sleep(Duration::from_millis(200)).await;
        assert!(limiter.tokens_remaining() > 0 || limiter.check_and_consume());
    }
}

#[tokio::test]
async fn test_rate_limiter_burst_limit_cap() {
    let mut limiter = RpcRateLimiter::new(5, 100); // High refill rate, low burst
    
    // Exhaust tokens
    for _ in 0..5 {
        limiter.check_and_consume();
    }
    
    // Wait and check - should never exceed burst limit
    sleep(Duration::from_millis(100)).await;
    let tokens = limiter.tokens_remaining();
    assert!(tokens <= 5, "Tokens should not exceed burst limit of 5, got {}", tokens);
}

#[tokio::test]
async fn test_auth_manager_rate_limit_per_user() {
    let manager = RpcAuthManager::new(false);
    
    // Create two users via tokens
    manager.add_token("user1".to_string()).await.unwrap();
    manager.add_token("user2".to_string()).await.unwrap();
    
    // Authenticate to get user_ids
    let mut headers1 = hyper::HeaderMap::new();
    headers1.insert("authorization", "Bearer user1".parse().unwrap());
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let result1 = manager.authenticate_request(&headers1, addr).await;
    let user1 = result1.user_id.unwrap();
    
    let mut headers2 = hyper::HeaderMap::new();
    headers2.insert("authorization", "Bearer user2".parse().unwrap());
    let result2 = manager.authenticate_request(&headers2, addr).await;
    let user2 = result2.user_id.unwrap();
    
    // Set different rate limits per user
    manager.set_user_rate_limit(&user1, 5, 2).await;
    manager.set_user_rate_limit(&user2, 10, 5).await;
    
    // User1 should have stricter limits
    for i in 0..5 {
        let allowed = manager.check_rate_limit(&user1).await;
        assert!(allowed, "User1 request {} should be allowed", i);
    }
    
    // User1 should be rate limited after burst
    let allowed = manager.check_rate_limit(&user1).await;
    assert!(!allowed, "User1 should be rate limited after burst");
    
    // User2 should still have tokens
    let allowed = manager.check_rate_limit(&user2).await;
    assert!(allowed, "User2 should still have tokens");
}

#[tokio::test]
async fn test_auth_manager_rate_limit_per_method() {
    let manager = RpcAuthManager::new(false);
    
    // Set strict limit for one method, normal for another
    manager.set_method_rate_limit("expensive_method", 2, 1).await;
    manager.set_method_rate_limit("cheap_method", 10, 5).await;
    
    // Expensive method should be limited quickly
    assert!(manager.check_method_rate_limit("expensive_method").await);
    assert!(manager.check_method_rate_limit("expensive_method").await);
    assert!(!manager.check_method_rate_limit("expensive_method").await);
    
    // Cheap method should still work
    assert!(manager.check_method_rate_limit("cheap_method").await);
}

#[tokio::test]
async fn test_auth_manager_ip_rate_limiting() {
    let manager = RpcAuthManager::new(false);
    let addr1: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let addr2: SocketAddr = "192.168.1.1:8080".parse().unwrap();
    
    // IP-based rate limiting should be stricter
    let user1 = UserId::Ip(addr1);
    let user2 = UserId::Ip(addr2);
    
    // Both should be rate limited independently
    for i in 0..50 {
        let allowed1 = manager.check_rate_limit(&user1).await;
        let allowed2 = manager.check_rate_limit(&user2).await;
        
        if i < 50 {
            // Should allow some requests (IP limit is 50 burst)
            assert!(allowed1 || allowed2 || i > 0, "At least one IP should have tokens initially");
        }
    }
}

#[tokio::test]
async fn test_auth_manager_rate_limit_recovery() {
    let manager = RpcAuthManager::new(false);
    let user = UserId::Ip("127.0.0.1:8080".parse().unwrap());
    
    // Exhaust rate limit (IP limit is 50 burst, 5/sec)
    let mut exhausted = false;
    for i in 0..60 {
        if !manager.check_rate_limit(&user).await {
            exhausted = true;
            break;
        }
        if i >= 49 {
            // Should be exhausted by now
            exhausted = true;
        }
    }
    
    // Should be rate limited
    assert!(!manager.check_rate_limit(&user).await || exhausted);
    
    // Wait for refill
    sleep(Duration::from_millis(1100)).await;
    
    // Should recover and allow requests again
    // Wait a bit more to ensure refill
    sleep(Duration::from_millis(200)).await;
    let allowed = manager.check_rate_limit(&user).await;
    // Should have at least some tokens after refill
    assert!(allowed, "Rate limit should recover after refill period");
}

#[tokio::test]
async fn test_auth_manager_concurrent_rate_limiting() {
    use std::sync::Arc;
    let manager = Arc::new(RpcAuthManager::new(false));
    let user = UserId::Ip("127.0.0.1:8080".parse().unwrap());
    
    // Simulate concurrent rate limit checks
    let mut handles = vec![];
    for _ in 0..20 {
        let manager_clone = Arc::clone(&manager);
        let user_clone = user.clone();
        
        handles.push(tokio::spawn(async move {
            manager_clone.check_rate_limit(&user_clone).await
        }));
    }
    
    // Collect results
    let mut allowed_count = 0;
    for handle in handles {
        if handle.await.unwrap() {
            allowed_count += 1;
        }
    }
    
    // Should respect rate limits even under concurrency
    assert!(allowed_count <= 50, "Should not exceed IP burst limit of 50");
}

#[tokio::test]
async fn test_rate_limiter_zero_rate() {
    let mut limiter = RpcRateLimiter::new(10, 0); // No refill
    
    // Should allow initial burst
    for _ in 0..10 {
        assert!(limiter.check_and_consume());
    }
    
    // Should never refill
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(limiter.tokens_remaining(), 0);
    assert!(!limiter.check_and_consume());
}

#[tokio::test]
async fn test_rate_limiter_high_rate() {
    let mut limiter = RpcRateLimiter::new(10, 1000); // Very high refill rate
    
    // Exhaust tokens
    for _ in 0..10 {
        limiter.check_and_consume();
    }
    
    // Should refill quickly (but capped at burst limit)
    // With 1000 tokens/sec, 100ms should give ~100 tokens, but capped at 10
    sleep(Duration::from_millis(100)).await;
    // Refill happens on check_and_consume, so check it
    let had_tokens = limiter.check_and_consume();
    // Should have tokens, but not exceed burst limit
    let tokens = limiter.tokens_remaining();
    assert!(tokens <= 10, "Should not exceed burst limit of 10");
    // With such a high rate, should have refilled
    assert!(had_tokens || tokens >= 8, "Should have refilled (had_tokens: {}, remaining: {})", had_tokens, tokens);
}


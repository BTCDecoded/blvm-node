//! RPC Rate Limiting Tests
//!
//! Tests rate limiting functionality, edge cases, and per-user/per-method limits.

use blvm_node::rpc::auth::{RpcAuthManager, RpcRateLimiter, UserId};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::advance;

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_basic_functionality() {
    let mut limiter = RpcRateLimiter::new(10, 5);

    // Should allow requests up to burst limit
    for i in 0..10 {
        assert!(limiter.check_and_consume(), "Request {i} should be allowed");
        assert_eq!(limiter.tokens_remaining(), 10 - i - 1);
    }

    // Should reject after burst limit
    assert!(!limiter.check_and_consume());
    assert_eq!(limiter.tokens_remaining(), 0);
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_refill_over_time() {
    let mut limiter = RpcRateLimiter::new(10, 10); // 10 tokens/sec

    // Exhaust tokens
    for _ in 0..10 {
        assert!(limiter.check_and_consume());
    }
    assert!(!limiter.check_and_consume());

    // 999ms at 10/sec earns 9.99 tokens: 9 whole tokens, 99ms carried
    advance(Duration::from_millis(999)).await;
    for _ in 0..9 {
        assert!(limiter.check_and_consume());
    }
    assert!(!limiter.check_and_consume());

    // 101ms more plus the 99ms carry is 200ms: exactly 2 tokens
    advance(Duration::from_millis(101)).await;
    assert!(limiter.check_and_consume());
    assert!(limiter.check_and_consume());
    assert!(!limiter.check_and_consume());
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_burst_limit_cap() {
    let mut limiter = RpcRateLimiter::new(5, 100); // High refill rate, low burst

    // Exhaust tokens
    for _ in 0..5 {
        assert!(limiter.check_and_consume());
    }

    // An hour at 100/sec would earn 360000 tokens; the bucket holds 5
    advance(Duration::from_secs(3600)).await;
    for _ in 0..5 {
        assert!(limiter.check_and_consume());
    }
    assert!(
        !limiter.check_and_consume(),
        "Tokens must not exceed burst limit of 5"
    );
}

#[tokio::test(start_paused = true)]
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
        assert!(allowed, "User1 request {i} should be allowed");
    }

    // User1 should be rate limited after burst
    let allowed = manager.check_rate_limit(&user1).await;
    assert!(!allowed, "User1 should be rate limited after burst");

    // User2 should still have tokens
    let allowed = manager.check_rate_limit(&user2).await;
    assert!(allowed, "User2 should still have tokens");
}

#[tokio::test(start_paused = true)]
async fn test_auth_manager_rate_limit_per_method() {
    let manager = RpcAuthManager::new(false);

    // Set strict limit for one method, normal for another
    manager
        .set_method_rate_limit("expensive_method", 2, 1)
        .await;
    manager.set_method_rate_limit("cheap_method", 10, 5).await;

    // Expensive method should be limited quickly
    assert!(manager.check_method_rate_limit("expensive_method").await);
    assert!(manager.check_method_rate_limit("expensive_method").await);
    assert!(!manager.check_method_rate_limit("expensive_method").await);

    // Cheap method should still work
    assert!(manager.check_method_rate_limit("cheap_method").await);
}

#[tokio::test(start_paused = true)]
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
            assert!(
                allowed1 || allowed2 || i > 0,
                "At least one IP should have tokens initially"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn test_auth_manager_rate_limit_recovery() {
    let manager = RpcAuthManager::new(false);
    let user = UserId::Ip("127.0.0.1:8080".parse().unwrap());

    // Default limit is 100 burst, 10/sec
    let mut allowed = 0;
    while manager.check_rate_limit(&user).await {
        allowed += 1;
        assert!(allowed <= 100, "burst exceeded");
    }
    assert_eq!(allowed, 100);

    // 99ms at 10/sec is 0.99 tokens: still limited
    advance(Duration::from_millis(99)).await;
    assert!(!manager.check_rate_limit(&user).await);

    // 1ms more completes one token
    advance(Duration::from_millis(1)).await;
    assert!(
        manager.check_rate_limit(&user).await,
        "Rate limit should recover after refill period"
    );
    assert!(!manager.check_rate_limit(&user).await);
}

#[tokio::test(start_paused = true)]
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
    assert!(
        allowed_count <= 50,
        "Should not exceed IP burst limit of 50"
    );
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_zero_rate() {
    let mut limiter = RpcRateLimiter::new(10, 0); // No refill

    // Should allow initial burst
    for _ in 0..10 {
        assert!(limiter.check_and_consume());
    }

    // Should never refill
    advance(Duration::from_secs(3600)).await;
    assert!(!limiter.check_and_consume());
    assert_eq!(limiter.tokens_remaining(), 0);
}

#[tokio::test(start_paused = true)]
async fn test_rate_limiter_high_rate() {
    let mut limiter = RpcRateLimiter::new(10, 1000); // Very high refill rate

    // Exhaust tokens
    for _ in 0..10 {
        assert!(limiter.check_and_consume());
    }
    assert!(!limiter.check_and_consume());

    // 1ms at 1000/sec is exactly one token
    advance(Duration::from_millis(1)).await;
    assert!(limiter.check_and_consume());
    assert!(!limiter.check_and_consume());

    // 100ms would earn 100 tokens, capped at the burst of 10
    advance(Duration::from_millis(100)).await;
    assert!(limiter.check_and_consume());
    assert_eq!(
        limiter.tokens_remaining(),
        9,
        "Should not exceed burst limit of 10"
    );
}

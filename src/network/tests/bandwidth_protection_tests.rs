//! Integration tests for bandwidth protection system
//!
//! Tests bandwidth exhaustion attack mitigations:
//! - Per-peer bandwidth limits
//! - Per-IP bandwidth limits
//! - Per-subnet bandwidth limits
//! - Rate limiting
//! - CPU time limits
//! - Attack scenarios (multiple peers from same IP, rapid requests)
//! - Legitimate use cases (should not be blocked)

use crate::network::bandwidth_protection::{BandwidthProtectionManager, ServiceType};
use crate::network::ibd_protection::IbdProtectionManager;
use std::sync::Arc;
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};

/// Create a test bandwidth protection manager
fn create_test_manager() -> Arc<BandwidthProtectionManager> {
    let ibd_protection = Arc::new(IbdProtectionManager::new());
    Arc::new(BandwidthProtectionManager::new(ibd_protection))
}

#[tokio::test]
async fn test_filter_service_bandwidth_limit() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // First request should succeed
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap(), "First filter request should be allowed");

    // Record bandwidth up to limit (5 GB/day)
    let limit_bytes = 5 * 1024 * 1024 * 1024;
    manager
        .record_service_bandwidth(ServiceType::Filters, peer_addr, limit_bytes)
        .await;

    // Next request should fail (limit exceeded)
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Request should be rejected after limit exceeded");
}

#[tokio::test]
async fn test_filter_service_rate_limit() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Make 50 requests (the limit)
    for i in 0..50 {
        let result = manager
            .check_service_request(ServiceType::Filters, peer_addr)
            .await;
        assert!(result.is_ok());
        assert!(
            result.unwrap(),
            "Request {} should be allowed",
            i
        );
        manager
            .record_service_request(ServiceType::Filters, peer_addr)
            .await;
    }

    // 51st request should fail (rate limit exceeded)
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "51st request should be rejected (rate limit)");
}

#[tokio::test]
async fn test_per_ip_bandwidth_limit() {
    let manager = create_test_manager();
    let peer1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let peer2: SocketAddr = "127.0.0.1:8334".parse().unwrap(); // Same IP, different port

    // Both peers from same IP should share IP limit
    let result1 = manager
        .check_service_request(ServiceType::Filters, peer1)
        .await;
    assert!(result1.is_ok());
    assert!(result1.unwrap());

    let result2 = manager
        .check_service_request(ServiceType::Filters, peer2)
        .await;
    assert!(result2.is_ok());
    assert!(result2.unwrap());

    // Record bandwidth from peer1 (should count against IP limit)
    let limit_bytes = 10 * 1024 * 1024 * 1024; // 10 GB (IP limit for filters)
    manager
        .record_service_bandwidth(ServiceType::Filters, peer1, limit_bytes)
        .await;

    // peer2 should now be rejected (IP limit exceeded)
    let result = manager
        .check_service_request(ServiceType::Filters, peer2)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "peer2 should be rejected (IP limit exceeded)");
}

#[tokio::test]
async fn test_per_subnet_bandwidth_limit() {
    let manager = create_test_manager();
    // All peers from same /24 subnet
    let peer1: SocketAddr = "192.168.1.1:8333".parse().unwrap();
    let peer2: SocketAddr = "192.168.1.2:8333".parse().unwrap();
    let peer3: SocketAddr = "192.168.1.3:8333".parse().unwrap();

    // All should initially succeed
    for peer in &[peer1, peer2, peer3] {
        let result = manager
            .check_service_request(ServiceType::Filters, *peer)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // Record bandwidth from multiple peers (should count against subnet limit)
    let limit_bytes = 50 * 1024 * 1024 * 1024; // 50 GB (subnet limit for filters)
    manager
        .record_service_bandwidth(ServiceType::Filters, peer1, limit_bytes)
        .await;

    // All peers from same subnet should now be rejected
    for peer in &[peer2, peer3] {
        let result = manager
            .check_service_request(ServiceType::Filters, *peer)
            .await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Peer from same subnet should be rejected");
    }
}

#[tokio::test]
async fn test_utxo_set_very_restrictive_limit() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // First request should succeed
    let result = manager
        .check_service_request(ServiceType::UtxoSet, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap());

    // Record request (UTXO set has very restrictive rate limit: 1 request/hour)
    manager
        .record_service_request(ServiceType::UtxoSet, peer_addr)
        .await;

    // Second request immediately should fail (rate limit: 1/hour)
    let result = manager
        .check_service_request(ServiceType::UtxoSet, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Second UTXO set request should be rejected (rate limit)");
}

#[tokio::test]
async fn test_cpu_time_limit() {
    let manager = create_test_manager();

    // Filter service has CPU time limit of 100ms
    assert!(
        manager.check_cpu_time_limit(ServiceType::Filters, 50),
        "50ms should be within limit"
    );
    assert!(
        manager.check_cpu_time_limit(ServiceType::Filters, 100),
        "100ms should be at limit"
    );
    assert!(
        !manager.check_cpu_time_limit(ServiceType::Filters, 101),
        "101ms should exceed limit"
    );

    // Package relay has no CPU time limit
    assert!(
        manager.check_cpu_time_limit(ServiceType::PackageRelay, 1000),
        "Package relay should have no CPU limit"
    );
}

#[tokio::test]
async fn test_different_services_independent_limits() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Exceed filter service limit
    let filter_limit = 5 * 1024 * 1024 * 1024;
    manager
        .record_service_bandwidth(ServiceType::Filters, peer_addr, filter_limit)
        .await;

    // Filter service should be blocked
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Filter service should be blocked");

    // But package relay should still work (different service, different limits)
    let result = manager
        .check_service_request(ServiceType::PackageRelay, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap(), "Package relay should still work (different service)");
}

#[tokio::test]
async fn test_attack_scenario_multiple_peers_same_ip() {
    let manager = create_test_manager();
    // Simulate attack: multiple peers from same IP trying to bypass per-peer limits
    let peers: Vec<SocketAddr> = (8333..8353)
        .map(|port| format!("127.0.0.1:{}", port).parse().unwrap())
        .collect();

    // All peers should initially succeed
    for peer in &peers {
        let result = manager
            .check_service_request(ServiceType::Filters, *peer)
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    // Record bandwidth from multiple peers (should count against IP limit)
    let bytes_per_peer = 2 * 1024 * 1024 * 1024; // 2 GB per peer
    for peer in &peers[..5] {
        // 5 peers * 2 GB = 10 GB (IP limit for filters)
        manager
            .record_service_bandwidth(ServiceType::Filters, *peer, bytes_per_peer)
            .await;
    }

    // All remaining peers from same IP should now be blocked
    for peer in &peers[5..] {
        let result = manager
            .check_service_request(ServiceType::Filters, *peer)
            .await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Peer from same IP should be blocked (IP limit)");
    }
}

#[tokio::test]
async fn test_attack_scenario_rapid_requests() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Rapid-fire requests should hit rate limit
    let mut allowed = 0;
    for _ in 0..60 {
        let result = manager
            .check_service_request(ServiceType::Filters, peer_addr)
            .await;
        if result.is_ok() && result.unwrap() {
            allowed += 1;
            manager
                .record_service_request(ServiceType::Filters, peer_addr)
                .await;
        } else {
            break; // Hit rate limit
        }
    }

    // Should allow exactly 50 requests (the rate limit)
    assert_eq!(allowed, 50, "Should allow exactly 50 requests per hour");
}

#[tokio::test]
async fn test_legitimate_use_not_blocked() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Legitimate use: moderate bandwidth usage
    let moderate_bytes = 100 * 1024 * 1024; // 100 MB
    manager
        .record_service_bandwidth(ServiceType::Filters, peer_addr, moderate_bytes)
        .await;

    // Should still be allowed (well under limit)
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(result.unwrap(), "Legitimate moderate use should be allowed");
}

#[tokio::test]
async fn test_package_relay_limits() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Package relay has higher limits than filters
    let limit_bytes = 10 * 1024 * 1024 * 1024; // 10 GB/day
    manager
        .record_service_bandwidth(ServiceType::PackageRelay, peer_addr, limit_bytes)
        .await;

    // Should be blocked after limit
    let result = manager
        .check_service_request(ServiceType::PackageRelay, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Should be blocked after package relay limit");
}

#[tokio::test]
async fn test_module_serving_limits() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Module serving has very high limits (modules can be large)
    let limit_bytes = 100 * 1024 * 1024 * 1024; // 100 GB/day
    manager
        .record_service_bandwidth(ServiceType::ModuleServing, peer_addr, limit_bytes)
        .await;

    // Should be blocked after limit
    let result = manager
        .check_service_request(ServiceType::ModuleServing, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Should be blocked after module serving limit");
}

#[tokio::test]
async fn test_window_reset() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Exceed hourly limit
    let hourly_limit = 1 * 1024 * 1024 * 1024; // 1 GB/hour for filters
    manager
        .record_service_bandwidth(ServiceType::Filters, peer_addr, hourly_limit)
        .await;

    // Should be blocked
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap());

    // Note: In real implementation, windows reset automatically after time passes
    // This test verifies the blocking works, actual reset would require time manipulation
    // or waiting for real time to pass (which is slow for tests)
}

#[tokio::test]
async fn test_multiple_services_same_peer() {
    let manager = create_test_manager();
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Use different services from same peer
    let services = [
        ServiceType::Filters,
        ServiceType::PackageRelay,
        ServiceType::FilteredBlocks,
    ];

    // All should initially work
    for service in &services {
        let result = manager.check_service_request(*service, peer_addr).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Service {:?} should be allowed", service);
    }

    // Exceed limit for one service
    let filter_limit = 5 * 1024 * 1024 * 1024;
    manager
        .record_service_bandwidth(ServiceType::Filters, peer_addr, filter_limit)
        .await;

    // Filter service should be blocked
    let result = manager
        .check_service_request(ServiceType::Filters, peer_addr)
        .await;
    assert!(result.is_ok());
    assert!(!result.unwrap(), "Filter service should be blocked");

    // But other services should still work
    for service in &[ServiceType::PackageRelay, ServiceType::FilteredBlocks] {
        let result = manager.check_service_request(*service, peer_addr).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Service {:?} should still work", service);
    }
}

#[tokio::test]
async fn test_concurrent_requests() {
    let manager = Arc::clone(&create_test_manager());
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Spawn multiple concurrent requests
    let mut handles = vec![];
    for _ in 0..10 {
        let manager_clone = Arc::clone(&manager);
        let peer = peer_addr;
        let handle = tokio::spawn(async move {
            manager_clone
                .check_service_request(ServiceType::Filters, peer)
                .await
        });
        handles.push(handle);
    }

    // All should complete without deadlock
    let results = futures::future::join_all(handles).await;
    for result in results {
        assert!(result.is_ok(), "Concurrent request should complete");
        let check_result = result.unwrap();
        assert!(check_result.is_ok(), "Check should succeed");
    }
}


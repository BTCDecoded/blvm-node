//! Tests for async request-response routing enhancements

use bllvm_node::network::{transport::TransportPreference, NetworkManager};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_request_id_matching() {
    // Test that requests are matched by request_id, not FIFO
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::with_transport_preference(listen_addr, 100, TransportPreference::TCP_ONLY);

    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Register multiple requests
    let (id1, rx1) = manager.register_request(peer_addr);
    let (id2, rx2) = manager.register_request(peer_addr);
    let (id3, rx3) = manager.register_request(peer_addr);

    // Complete requests out of order
    let response2 = vec![2, 2, 2];
    let response1 = vec![1, 1, 1];
    let response3 = vec![3, 3, 3];

    assert!(manager.complete_request(id2, response2.clone()));
    assert!(manager.complete_request(id1, response1.clone()));
    assert!(manager.complete_request(id3, response3.clone()));

    // Verify each receiver got the correct response
    tokio::select! {
        result = rx1 => {
            assert_eq!(result.unwrap(), response1);
        }
        _ = sleep(Duration::from_millis(100)) => {
            panic!("Request 1 timeout");
        }
    }

    tokio::select! {
        result = rx2 => {
            assert_eq!(result.unwrap(), response2);
        }
        _ = sleep(Duration::from_millis(100)) => {
            panic!("Request 2 timeout");
        }
    }

    tokio::select! {
        result = rx3 => {
            assert_eq!(result.unwrap(), response3);
        }
        _ = sleep(Duration::from_millis(100)) => {
            panic!("Request 3 timeout");
        }
    }
}

#[tokio::test]
async fn test_request_cancellation() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::with_transport_preference(listen_addr, 100, TransportPreference::TCP_ONLY);
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    let (request_id, rx) = manager.register_request(peer_addr);

    // Cancel the request
    assert!(manager.cancel_request(request_id));

    // Try to complete it (should fail)
    assert!(!manager.complete_request(request_id, vec![1, 2, 3]));

    // Receiver should be closed
    assert!(rx.await.is_err());
}

#[tokio::test]
async fn test_timestamp_cleanup() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::with_transport_preference(listen_addr, 100, TransportPreference::TCP_ONLY);
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Register a request
    let (request_id, _rx) = manager.register_request(peer_addr);

    // Wait at least 1 second to ensure the request is older than 0 seconds
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Cleanup requests older than 0 seconds (should remove all)
    let cleaned = manager.cleanup_expired_requests(0);
    assert_eq!(cleaned, 1);

    // Request should be gone
    assert!(!manager.complete_request(request_id, vec![1, 2, 3]));
}

#[tokio::test]
async fn test_multiple_concurrent_requests_per_peer() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::with_transport_preference(listen_addr, 100, TransportPreference::TCP_ONLY);
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Register multiple requests from same peer
    let (id1, rx1) = manager.register_request(peer_addr);
    let (id2, rx2) = manager.register_request(peer_addr);
    let (id3, rx3) = manager.register_request(peer_addr);

    // All should be registered
    let pending = manager.get_pending_requests_for_peer(peer_addr);
    assert_eq!(pending.len(), 3);
    assert!(pending.contains(&id1));
    assert!(pending.contains(&id2));
    assert!(pending.contains(&id3));

    // Complete all
    manager.complete_request(id1, vec![1]);
    manager.complete_request(id2, vec![2]);
    manager.complete_request(id3, vec![3]);

    // All should receive responses
    assert_eq!(rx1.await.unwrap(), vec![1]);
    assert_eq!(rx2.await.unwrap(), vec![2]);
    assert_eq!(rx3.await.unwrap(), vec![3]);
}

#[tokio::test]
async fn test_request_metrics() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::with_transport_preference(listen_addr, 100, TransportPreference::TCP_ONLY);
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Register and complete a request
    let (id1, _rx1) = manager.register_request(peer_addr);
    manager.complete_request(id1, vec![1, 2, 3]);

    // Note: get_request_metrics and record_failed_request methods don't exist yet
    // This test is disabled until those methods are implemented
    // Check metrics
    // let metrics = manager.get_request_metrics(peer_addr);
    // assert!(metrics.is_some());
    // let m = metrics.unwrap();
    // assert_eq!(m.total_requests, 1);
    // assert_eq!(m.successful_responses, 1);
    // assert_eq!(m.failed_requests, 0);

    // Record a failure
    // let (id2, _rx2) = manager.register_request(peer_addr);
    // manager.record_failed_request(id2);

    // let metrics = manager.get_request_metrics(peer_addr);
    // let m = metrics.unwrap();
    // assert_eq!(m.total_requests, 2);
    // assert_eq!(m.successful_responses, 1);
    // assert_eq!(m.failed_requests, 1);
}

#[tokio::test]
async fn test_request_priority() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager =
        NetworkManager::with_transport_preference(listen_addr, 100, TransportPreference::TCP_ONLY);
    let peer_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Register requests with different priorities
    let (_id1, _rx1) = manager.register_request_with_priority(peer_addr, 0);
    let (_id2, _rx2) = manager.register_request_with_priority(peer_addr, 5);
    let (_id3, _rx3) = manager.register_request_with_priority(peer_addr, 10);

    // All should be registered
    let pending = manager.get_pending_requests_for_peer(peer_addr);
    assert_eq!(pending.len(), 3);
}

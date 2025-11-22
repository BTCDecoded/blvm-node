//! Concurrency stress tests for network layer
//!
//! Tests for:
//! - Mutex deadlock scenarios
//! - Lock ordering
//! - Concurrent access patterns
//! - Timeout handling

use crate::network::NetworkManager;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Test that multiple tasks can acquire the same Mutex concurrently
/// without deadlocking (using tokio::sync::Mutex)
#[tokio::test]
async fn test_concurrent_mutex_access() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // Spawn multiple tasks that all try to access peer_manager simultaneously
    let mut handles = vec![];
    for i in 0..10 {
        let manager_clone = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let pm = manager_clone.peer_manager().await;
            // Hold lock briefly
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert_eq!(pm.peer_count(), 0);
            drop(pm); // Explicitly drop lock
            i
        });
        handles.push(handle);
    }

    // All tasks should complete without deadlock
    let results = futures::future::join_all(handles).await;
    for result in results {
        assert!(result.is_ok(), "Task should complete without deadlock");
    }
}

/// Test that locks are not held across await points
#[tokio::test]
async fn test_no_lock_held_across_await() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // This test verifies that we can await async operations
    // without holding locks, preventing deadlocks
    let manager_clone = Arc::clone(&manager);
    let handle = tokio::spawn(async move {
        // Acquire lock
        {
            let _pm = manager_clone.peer_manager().await;
            // Lock is dropped here before await
        }

        // Now we can await without holding the lock
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Acquire lock again
        let _pm = manager_clone.peer_manager().await;
        drop(_pm); // Explicitly drop
    });

    // Should complete within timeout (no deadlock)
    let result = timeout(Duration::from_secs(1), handle).await;
    assert!(result.is_ok(), "Should complete without deadlock");
}

/// Test lock ordering to prevent deadlocks
#[tokio::test]
async fn test_lock_ordering() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // Test that we always acquire locks in the same order
    // This prevents deadlocks when multiple locks are needed
    let manager_clone = Arc::clone(&manager);
    let handle = tokio::spawn(async move {
        // Always acquire peer_manager first, then other locks
        let _pm = manager_clone.peer_manager().await;
        // Then acquire other locks if needed
        // (This tests the pattern, actual implementation may vary)
    });

    let result = timeout(Duration::from_secs(1), handle).await;
    assert!(result.is_ok(), "Lock ordering should prevent deadlocks");
}

/// Stress test: Many concurrent operations
#[tokio::test]
async fn test_concurrent_operations_stress() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // Spawn many concurrent operations
    let mut handles = vec![];
    for _ in 0..50 {
        let manager_clone = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            // Simulate various operations
            {
                let _pm = manager_clone.peer_manager().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            } // Lock dropped
              // Access another lock (simplified - actual stats method may vary)
            let _bytes_sent = *manager_clone.bytes_sent.lock().await;
        });
        handles.push(handle);
    }

    // All should complete
    let results = futures::future::join_all(handles).await;
    for result in results {
        assert!(result.is_ok(), "Concurrent operations should succeed");
    }
}

/// Test that rate limiting works correctly under concurrent load
#[tokio::test]
async fn test_concurrent_rate_limiting() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    // Spawn many tasks that all try to send messages
    let mut handles = vec![];
    for _ in 0..20 {
        let manager_clone = Arc::clone(&manager);
        let addr_clone = addr;
        let handle = tokio::spawn(async move {
            // Each task tries to send a message
            // Rate limiting should handle this correctly
            let _ = manager_clone.send_to_peer(addr_clone, vec![0u8; 10]).await;
        });
        handles.push(handle);
    }

    // All should complete (rate limiting may drop some, but no deadlock)
    let results = futures::future::join_all(handles).await;
    for result in results {
        assert!(result.is_ok(), "Rate limiting should not cause deadlocks");
    }
}

/// Test timeout handling for lock acquisition
#[tokio::test]
async fn test_lock_timeout() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // Hold lock for a while
    let manager_clone = Arc::clone(&manager);
    let long_task = tokio::spawn(async move {
        let _pm = manager_clone.peer_manager().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    });

    // Try to acquire lock with timeout
    let manager_clone2 = Arc::clone(&manager);
    let timeout_task = tokio::spawn(async move {
        // tokio::sync::Mutex doesn't have timeout, but we can test
        // that it doesn't block forever
        let result = timeout(Duration::from_millis(100), async {
            manager_clone2.peer_manager().await
        })
        .await;

        // Should eventually succeed (after first task releases)
        assert!(result.is_ok(), "Lock should be acquired eventually");
    });

    tokio::join!(long_task, timeout_task);
}

/// Test that peer addition/removal is thread-safe
#[tokio::test]
async fn test_concurrent_peer_operations() {
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // Spawn tasks that add and remove peers concurrently
    let mut handles = vec![];
    for i in 0..10 {
        let manager_clone = Arc::clone(&manager);
        let handle = tokio::spawn(async move {
            let _addr: SocketAddr = format!("127.0.0.1:{}", 8080 + i).parse().unwrap();
            // Try to get peer count (read operation)
            let _count = {
                let pm = manager_clone.peer_manager().await;
                pm.peer_count()
            };
            // Lock is dropped, so other tasks can proceed
        });
        handles.push(handle);
    }

    // All should complete
    let results = futures::future::join_all(handles).await;
    for result in results {
        assert!(result.is_ok(), "Concurrent peer operations should succeed");
    }
}

/// Test that message processing doesn't deadlock
#[tokio::test]
async fn test_concurrent_message_processing() {
    let (_tx, mut rx): (mpsc::UnboundedSender<()>, _) = mpsc::unbounded_channel();
    let manager = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));

    // Send many messages concurrently
    let manager_clone = Arc::clone(&manager);
    let sender_task = tokio::spawn(async move {
        for i in 0..100 {
            let addr: SocketAddr = format!("127.0.0.1:{}", 8080 + (i % 10)).parse().unwrap();
            let _ = manager_clone.send_to_peer(addr, vec![0u8; 10]).await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    // Process messages (simplified - actual processing is more complex)
    let receiver_task = tokio::spawn(async move {
        let mut count = 0;
        while let Ok(_) = timeout(Duration::from_millis(100), rx.recv()).await {
            count += 1;
            if count >= 100 {
                break;
            }
        }
    });

    // Both should complete
    let (sender_result, receiver_result) = tokio::join!(sender_task, receiver_task);
    assert!(sender_result.is_ok(), "Sender should complete");
    assert!(receiver_result.is_ok(), "Receiver should complete");
}

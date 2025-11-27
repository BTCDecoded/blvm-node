//! Module API Events Edge Cases Tests
//!
//! Stress tests and edge cases for event publishing: queue overflow, concurrent publishing.

use bllvm_node::module::api::events::EventManager;
use bllvm_node::module::ipc::protocol::EventPayload;
use bllvm_node::module::traits::EventType;
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_event_queue_overflow() {
    // Test event queue with many events
    let manager = Arc::new(EventManager::new());

    // Subscribe a module
    let (tx, rx) = mpsc::channel(100);
    manager
        .subscribe_module("test_module".to_string(), vec![EventType::NewBlock], tx)
        .await
        .unwrap();

    // Publish many events rapidly
    for i in 0..100 {
        let result = manager
            .publish_event(
                EventType::NewBlock,
                EventPayload::NewBlock {
                    block_hash: [i as u8; 32],
                    height: i,
                },
            )
            .await;
        assert!(result.is_ok());
    }

    // Manager should handle many events gracefully
    // (some may be dropped if channel is full, which is acceptable)
}

#[tokio::test]
async fn test_concurrent_event_publishing() {
    // Test concurrent event publishing
    let manager = Arc::new(EventManager::new());

    // Subscribe a module
    let (tx, _rx) = mpsc::channel(1000);
    manager
        .subscribe_module("test_module".to_string(), vec![EventType::NewBlock], tx)
        .await
        .unwrap();

    let mut handles = vec![];
    for i in 0..10 {
        let mgr_clone = manager.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..10 {
                let result = mgr_clone
                    .publish_event(
                        EventType::NewBlock,
                        EventPayload::NewBlock {
                            block_hash: [(i * 10 + j) as u8; 32],
                            height: i * 10 + j,
                        },
                    )
                    .await;
                assert!(result.is_ok());
            }
        }));
    }

    futures::future::join_all(handles).await;

    // All events should be published
}

#[tokio::test]
async fn test_event_subscription_edge_cases() {
    // Test event subscription edge cases
    let manager = Arc::new(EventManager::new());

    // Subscribe to events
    let (tx, mut rx) = mpsc::channel(10);
    manager
        .subscribe_module("test_module".to_string(), vec![EventType::NewBlock], tx)
        .await
        .unwrap();

    // Publish event
    let result = manager
        .publish_event(
            EventType::NewBlock,
            EventPayload::NewBlock {
                block_hash: [0x42; 32],
                height: 100,
            },
        )
        .await;
    assert!(result.is_ok());

    // Should receive event (with timeout)
    let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
    assert!(received.is_ok());
}

#[tokio::test]
async fn test_multiple_subscribers() {
    // Test multiple subscribers to same event type
    let manager = Arc::new(EventManager::new());

    let mut receivers = vec![];
    for i in 0..5 {
        let (tx, rx) = mpsc::channel(10);
        manager
            .subscribe_module(format!("module_{i}"), vec![EventType::NewBlock], tx)
            .await
            .unwrap();
        receivers.push(rx);
    }

    // Publish event
    let result = manager
        .publish_event(
            EventType::NewBlock,
            EventPayload::NewBlock {
                block_hash: [0x42; 32],
                height: 100,
            },
        )
        .await;
    assert!(result.is_ok());

    // All subscribers should receive event (with timeout)
    for receiver in &mut receivers {
        let received =
            tokio::time::timeout(std::time::Duration::from_millis(100), receiver.recv()).await;
        assert!(received.is_ok());
    }
}

#[tokio::test]
async fn test_event_subscription_drop() {
    // Test that dropped subscribers don't block publisher
    let manager = Arc::new(EventManager::new());

    {
        let (tx, _rx) = mpsc::channel(10);
        manager
            .subscribe_module("temp_module".to_string(), vec![EventType::NewBlock], tx)
            .await
            .unwrap();
        // Receiver dropped here
    }

    // Publisher should still work after subscriber is dropped
    let result = manager
        .publish_event(
            EventType::NewBlock,
            EventPayload::NewBlock {
                block_hash: [0x42; 32],
                height: 100,
            },
        )
        .await;
    assert!(result.is_ok());
}

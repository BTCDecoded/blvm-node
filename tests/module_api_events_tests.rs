//! Tests for Module API Events

use bllvm_node::module::api::events::EventManager;
use bllvm_node::module::ipc::protocol::{EventMessage, EventPayload, ModuleMessage};
use bllvm_node::module::traits::{EventType, ModuleError};
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_event_manager_new() {
    let manager = EventManager::new();
    // Manager should be created successfully
    assert!(true);
}

#[tokio::test]
async fn test_event_manager_default() {
    let manager = EventManager::default();
    // Default should work
    assert!(true);
}

#[tokio::test]
async fn test_event_manager_subscribe_module() {
    let manager = EventManager::new();
    
    let (tx, _rx) = mpsc::channel(10);
    let event_types = vec![EventType::NewBlock, EventType::NewTransaction];
    
    let result = manager.subscribe_module("test-module".to_string(), event_types, tx).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_event_manager_subscribe_multiple_event_types() {
    let manager = EventManager::new();
    
    let (tx, _rx) = mpsc::channel(10);
    let event_types = vec![
        EventType::NewBlock,
        EventType::NewTransaction,
    ];
    
    let result = manager.subscribe_module("test-module".to_string(), event_types, tx).await;
    assert!(result.is_ok());
    
    // Check subscribers
    let block_subscribers = manager.get_subscribers(EventType::NewBlock).await;
    assert!(block_subscribers.contains(&"test-module".to_string()));
    
    let tx_subscribers = manager.get_subscribers(EventType::NewTransaction).await;
    assert!(tx_subscribers.contains(&"test-module".to_string()));
}

#[tokio::test]
async fn test_event_manager_unsubscribe_module() {
    let manager = EventManager::new();
    
    let (tx, _rx) = mpsc::channel(10);
    let event_types = vec![EventType::NewBlock];
    
    // Subscribe
    manager.subscribe_module("test-module".to_string(), event_types, tx).await.unwrap();
    
    // Unsubscribe
    let result = manager.unsubscribe_module("test-module").await;
    assert!(result.is_ok());
    
    // Check subscribers are removed
    let subscribers = manager.get_subscribers(EventType::NewBlock).await;
    assert!(!subscribers.contains(&"test-module".to_string()));
}

#[tokio::test]
async fn test_event_manager_publish_event_no_subscribers() {
    let manager = EventManager::new();
    
    // Publish event with no subscribers
    use bllvm_protocol::Hash;
    let result = manager.publish_event(
        EventType::NewBlock,
        EventPayload::NewBlock { 
            block_hash: Hash::default(),
            height: 0,
        },
    ).await;
    
    assert!(result.is_ok()); // Should succeed even with no subscribers
}

#[tokio::test]
async fn test_event_manager_publish_event_with_subscribers() {
    let manager = EventManager::new();
    
    let (tx, mut rx) = mpsc::channel(10);
    let event_types = vec![EventType::NewBlock];
    
    // Subscribe
    manager.subscribe_module("test-module".to_string(), event_types, tx).await.unwrap();
    
    // Publish event
    use bllvm_protocol::Hash;
    let result = manager.publish_event(
        EventType::NewBlock,
        EventPayload::NewBlock { 
            block_hash: Hash::default(),
            height: 100,
        },
    ).await;
    
    assert!(result.is_ok());
    
    // Small delay to ensure event is sent
    tokio::time::sleep(Duration::from_millis(10)).await;
    
    // Check event was received (with timeout to prevent hanging)
    let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    assert!(received.is_ok());
    let msg_opt = received.unwrap();
    assert!(msg_opt.is_some());
    
    if let Some(ModuleMessage::Event(EventMessage { event_type, payload })) = msg_opt {
        assert_eq!(event_type, EventType::NewBlock);
        match payload {
            EventPayload::NewBlock { block_hash: _, height } => {
                assert_eq!(height, 100);
            }
            _ => panic!("Expected NewBlock payload"),
        }
    } else {
        panic!("Expected Event message");
    }
}

#[tokio::test]
async fn test_event_manager_publish_to_multiple_subscribers() {
    let manager = EventManager::new();
    
    let (tx1, mut rx1) = mpsc::channel(10);
    let (tx2, mut rx2) = mpsc::channel(10);
    
    // Subscribe two modules
    manager.subscribe_module("module1".to_string(), vec![EventType::NewBlock], tx1).await.unwrap();
    manager.subscribe_module("module2".to_string(), vec![EventType::NewBlock], tx2).await.unwrap();
    
    // Publish event
    use bllvm_protocol::Hash;
    let result = manager.publish_event(
        EventType::NewBlock,
        EventPayload::NewBlock { 
            block_hash: Hash::default(),
            height: 200,
        },
    ).await;
    
    assert!(result.is_ok());
    
    // Both should receive the event (with timeout to prevent hanging)
    let received1 = tokio::time::timeout(Duration::from_secs(1), rx1.recv()).await;
    let received2 = tokio::time::timeout(Duration::from_secs(1), rx2.recv()).await;
    
    assert!(received1.is_ok());
    assert!(received1.unwrap().is_some());
    assert!(received2.is_ok());
    assert!(received2.unwrap().is_some());
}

#[tokio::test]
async fn test_event_manager_publish_different_event_types() {
    let manager = EventManager::new();
    
    let (tx, mut rx) = mpsc::channel(10);
    
    // Subscribe to Block events only
    manager.subscribe_module("test-module".to_string(), vec![EventType::NewBlock], tx).await.unwrap();
    
    // Publish Transaction event (should not be received)
    use bllvm_protocol::Hash;
    manager.publish_event(
        EventType::NewTransaction,
        EventPayload::NewTransaction { tx_hash: Hash::default() },
    ).await.unwrap();
    
    // Publish Block event (should be received)
    manager.publish_event(
        EventType::NewBlock,
        EventPayload::NewBlock { 
            block_hash: Hash::default(),
            height: 400,
        },
    ).await.unwrap();
    
    // Small delay to ensure event is sent
    tokio::time::sleep(Duration::from_millis(10)).await;
    
    // Should only receive Block event (with timeout to prevent hanging)
    let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    assert!(received.is_ok());
    let msg_opt = received.unwrap();
    assert!(msg_opt.is_some());
    
    if let Some(ModuleMessage::Event(EventMessage { event_type, .. })) = msg_opt {
        assert_eq!(event_type, EventType::NewBlock);
    } else {
        panic!("Expected Event message");
    }
    
    // Should not receive Transaction event
    let timeout_result = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
    assert!(timeout_result.is_err()); // Should timeout (no more messages)
}

#[tokio::test]
async fn test_event_manager_cleanup_failed_channels() {
    let manager = EventManager::new();
    
    let (tx, rx) = mpsc::channel(1); // Small buffer
    let event_types = vec![EventType::NewBlock];
    
    // Subscribe
    manager.subscribe_module("test-module".to_string(), event_types, tx).await.unwrap();
    
    // Drop receiver to simulate channel closure
    drop(rx);
    
    // Publish event (should handle failed channel gracefully)
    use bllvm_protocol::Hash;
    let result = manager.publish_event(
        EventType::NewBlock,
        EventPayload::NewBlock { 
            block_hash: Hash::default(),
            height: 500,
        },
    ).await;
    
    assert!(result.is_ok()); // Should succeed even if channel is closed
    
    // Module should be removed from subscribers
    let subscribers = manager.get_subscribers(EventType::NewBlock).await;
    assert!(!subscribers.contains(&"test-module".to_string()));
}

#[tokio::test]
async fn test_event_manager_get_subscribers() {
    let manager = EventManager::new();
    
    let (tx1, _rx1) = mpsc::channel(10);
    let (tx2, _rx2) = mpsc::channel(10);
    
    // Subscribe two modules to Block events
    manager.subscribe_module("module1".to_string(), vec![EventType::NewBlock], tx1).await.unwrap();
    manager.subscribe_module("module2".to_string(), vec![EventType::NewBlock], tx2).await.unwrap();
    
    // Get subscribers
    let subscribers = manager.get_subscribers(EventType::NewBlock).await;
    assert_eq!(subscribers.len(), 2);
    assert!(subscribers.contains(&"module1".to_string()));
    assert!(subscribers.contains(&"module2".to_string()));
    
    // Get subscribers for different event type
    let tx_subscribers = manager.get_subscribers(EventType::NewTransaction).await;
    assert!(tx_subscribers.is_empty());
}

#[tokio::test]
async fn test_event_manager_unsubscribe_nonexistent() {
    let manager = EventManager::new();
    
    // Try to unsubscribe non-existent module
    let result = manager.unsubscribe_module("nonexistent").await;
    assert!(result.is_ok()); // Should succeed (idempotent)
}


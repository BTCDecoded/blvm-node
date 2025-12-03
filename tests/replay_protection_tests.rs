//! Comprehensive tests for replay protection
//!
//! Tests message ID deduplication, timestamp validation, request ID tracking,
//! and cleanup functionality.

use blvm_node::network::replay_protection::{ReplayError, ReplayProtection};
use blvm_node::utils::current_timestamp;
use std::time::Duration;

#[tokio::test]
async fn test_message_id_deduplication_basic() {
    let protection = ReplayProtection::new();
    let message_id = "test-message-id-123";

    // First check should succeed
    assert!(protection
        .check_message_id(message_id, current_timestamp() as i64)
        .await
        .is_ok());

    // Second check should fail (duplicate)
    let result = protection
        .check_message_id(message_id, current_timestamp() as i64)
        .await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err().downcast_ref::<ReplayError>(),
        Some(ReplayError::DuplicateMessageId(_))
    ));
}

#[tokio::test]
async fn test_message_id_deduplication_multiple() {
    let protection = ReplayProtection::new();

    // Add multiple different message IDs
    for i in 0..10 {
        let msg_id = format!("msg-{}", i);
        assert!(protection
            .check_message_id(&msg_id, current_timestamp() as i64)
            .await
            .is_ok());
    }

    // Try to reuse one
    assert!(protection
        .check_message_id("msg-5", current_timestamp() as i64)
        .await
        .is_err());

    // But new ones should still work
    assert!(protection
        .check_message_id("msg-new", current_timestamp() as i64)
        .await
        .is_ok());
}

#[tokio::test]
async fn test_request_id_deduplication_basic() {
    let protection = ReplayProtection::new();
    let request_id = 12345u64;

    // First check should succeed
    assert!(protection.check_request_id(request_id).await.is_ok());

    // Second check should fail (duplicate)
    let result = protection.check_request_id(request_id).await;
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err().downcast_ref::<ReplayError>(),
        Some(ReplayError::DuplicateRequestId(_))
    ));
}

#[tokio::test]
async fn test_request_id_deduplication_multiple() {
    let protection = ReplayProtection::new();

    // Add multiple different request IDs
    for i in 1000..1010 {
        assert!(protection.check_request_id(i).await.is_ok());
    }

    // Try to reuse one
    assert!(protection.check_request_id(1005).await.is_err());

    // But new ones should still work
    assert!(protection.check_request_id(2000).await.is_ok());
}

#[test]
fn test_timestamp_validation_current() {
    let now = current_timestamp() as i64;

    // Valid timestamp (current time)
    assert!(ReplayProtection::validate_timestamp(now, 3600).is_ok());
}

#[test]
fn test_timestamp_validation_recent_past() {
    let now = current_timestamp() as i64;

    // Valid timestamp (1 minute ago)
    assert!(ReplayProtection::validate_timestamp(now - 60, 3600).is_ok());

    // Valid timestamp (30 minutes ago)
    assert!(ReplayProtection::validate_timestamp(now - 1800, 3600).is_ok());
}

#[test]
fn test_timestamp_validation_too_old() {
    let now = current_timestamp() as i64;

    // Invalid timestamp (too old - 2 hours ago with 1 hour max age)
    let result = ReplayProtection::validate_timestamp(now - 7200, 3600);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err().downcast_ref::<ReplayError>(),
        Some(ReplayError::TimestampTooOld(_, _))
    ));
}

#[test]
fn test_timestamp_validation_too_future() {
    let now = current_timestamp() as i64;

    // Invalid timestamp (too far in future - 10 minutes ahead)
    let result = ReplayProtection::validate_timestamp(now + 600, 3600);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err().downcast_ref::<ReplayError>(),
        Some(ReplayError::TimestampTooFuture(_, _))
    ));
}

#[test]
fn test_timestamp_validation_with_custom_tolerance() {
    let now = current_timestamp() as i64;

    // Valid with custom tolerance (1 minute future tolerance)
    assert!(ReplayProtection::validate_timestamp_with_tolerance(now + 30, 3600, 60).is_ok());

    // Invalid with custom tolerance (exceeds 1 minute)
    assert!(ReplayProtection::validate_timestamp_with_tolerance(now + 90, 3600, 60).is_err());
}

#[tokio::test]
async fn test_cleanup_message_ids() {
    let protection = ReplayProtection::with_config(
        Duration::from_millis(100), // cleanup every 100ms
        Duration::from_millis(200), // message IDs expire after 200ms
        Duration::from_millis(500), // request IDs expire after 500ms
        300,
    );

    // Add some message IDs
    protection
        .check_message_id("msg1", current_timestamp() as i64)
        .await
        .unwrap();
    protection
        .check_message_id("msg2", current_timestamp() as i64)
        .await
        .unwrap();

    // Wait for cleanup
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Check that message IDs are cleaned up
    let (msg_count, req_count) = protection.stats().await;
    assert_eq!(msg_count, 0);
    assert_eq!(req_count, 0);
}

#[tokio::test]
async fn test_cleanup_request_ids() {
    let protection = ReplayProtection::with_config(
        Duration::from_millis(100), // cleanup every 100ms
        Duration::from_millis(500), // message IDs expire after 500ms
        Duration::from_millis(200), // request IDs expire after 200ms
        300,
    );

    // Add some request IDs
    protection.check_request_id(1).await.unwrap();
    protection.check_request_id(2).await.unwrap();

    // Wait for cleanup
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Check that request IDs are cleaned up
    let (msg_count, req_count) = protection.stats().await;
    assert_eq!(msg_count, 0);
    assert_eq!(req_count, 0);
}

#[tokio::test]
async fn test_stats_tracking() {
    let protection = ReplayProtection::new();

    // Initially empty
    let (msg_count, req_count) = protection.stats().await;
    assert_eq!(msg_count, 0);
    assert_eq!(req_count, 0);

    // Add some entries
    protection
        .check_message_id("msg1", current_timestamp() as i64)
        .await
        .unwrap();
    protection
        .check_message_id("msg2", current_timestamp() as i64)
        .await
        .unwrap();
    protection.check_request_id(100).await.unwrap();
    protection.check_request_id(200).await.unwrap();

    // Check stats
    let (msg_count, req_count) = protection.stats().await;
    assert_eq!(msg_count, 2);
    assert_eq!(req_count, 2);
}

#[tokio::test]
async fn test_concurrent_message_ids() {
    let protection = ReplayProtection::new();
    let protection = std::sync::Arc::new(protection);

    // Spawn multiple tasks trying to add the same message ID
    let mut handles = vec![];
    for i in 0..10 {
        let protection = Arc::clone(&protection);
        let handle = tokio::spawn(async move {
            protection
                .check_message_id("concurrent-msg", current_timestamp() as i64)
                .await
        });
        handles.push(handle);
    }

    // Wait for all tasks
    let mut results = vec![];
    for handle in handles {
        results.push(handle.await);
    }

    // Only one should succeed, the rest should fail
    let successes = results
        .iter()
        .filter(|r| if let Ok(Ok(_)) = r { true } else { false })
        .count();
    let failures = results
        .iter()
        .filter(|r| if let Ok(Err(_)) = r { true } else { false })
        .count();

    assert_eq!(successes, 1, "Only one concurrent check should succeed");
    assert_eq!(failures, 9, "Nine concurrent checks should fail");
}

#[tokio::test]
async fn test_concurrent_request_ids() {
    let protection = ReplayProtection::new();
    let protection = std::sync::Arc::new(protection);

    // Spawn multiple tasks trying to add the same request ID
    let mut handles = vec![];
    for _ in 0..10 {
        let protection = Arc::clone(&protection);
        let handle = tokio::spawn(async move { protection.check_request_id(9999).await });
        handles.push(handle);
    }

    // Wait for all tasks
    let mut results = vec![];
    for handle in handles {
        results.push(handle.await);
    }

    // Only one should succeed, the rest should fail
    let successes = results
        .iter()
        .filter(|r| if let Ok(Ok(_)) = r { true } else { false })
        .count();
    let failures = results
        .iter()
        .filter(|r| if let Ok(Err(_)) = r { true } else { false })
        .count();

    assert_eq!(successes, 1, "Only one concurrent check should succeed");
    assert_eq!(failures, 9, "Nine concurrent checks should fail");
}

#[tokio::test]
async fn test_message_id_expires_after_cleanup() {
    let protection = ReplayProtection::with_config(
        Duration::from_millis(100), // cleanup every 100ms
        Duration::from_millis(200), // message IDs expire after 200ms
        Duration::from_millis(500), // request IDs expire after 500ms
        300,
    );

    let message_id = "expiring-msg";

    // Add message ID
    protection
        .check_message_id(message_id, current_timestamp() as i64)
        .await
        .unwrap();

    // Wait for cleanup (message ID should expire)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Should be able to reuse the message ID now
    assert!(protection
        .check_message_id(message_id, current_timestamp() as i64)
        .await
        .is_ok());
}

#[tokio::test]
async fn test_request_id_expires_after_cleanup() {
    let protection = ReplayProtection::with_config(
        Duration::from_millis(100), // cleanup every 100ms
        Duration::from_millis(500), // message IDs expire after 500ms
        Duration::from_millis(200), // request IDs expire after 200ms
        300,
    );

    let request_id = 8888u64;

    // Add request ID
    protection.check_request_id(request_id).await.unwrap();

    // Wait for cleanup (request ID should expire)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Should be able to reuse the request ID now
    assert!(protection.check_request_id(request_id).await.is_ok());
}

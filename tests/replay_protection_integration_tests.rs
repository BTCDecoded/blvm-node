//! Integration tests for replay protection in message handlers
//!
//! Exercises replay protection with custom message IDs, ban list timestamps,
//! and async request IDs (GetFilteredBlock, GetModule).

use blvm_node::network::replay_protection::ReplayProtection;
use blvm_node::utils::current_timestamp;
use std::sync::Arc;

#[tokio::test]
async fn test_custom_message_id_replay_protection() {
    let protection = Arc::new(ReplayProtection::new());
    let message_id = "custom-msg-123";
    let timestamp = current_timestamp() as i64;

    assert!(protection
        .check_message_id(message_id, timestamp)
        .await
        .is_ok());
    assert!(ReplayProtection::validate_timestamp(timestamp, 3600).is_ok());

    assert!(protection
        .check_message_id(message_id, timestamp)
        .await
        .is_err());
}

#[tokio::test]
async fn test_custom_message_timestamp_validation() {
    let protection = Arc::new(ReplayProtection::new());
    let message_id = "custom-msg-456";
    let now = current_timestamp() as i64;

    assert!(protection.check_message_id(message_id, now).await.is_ok());
    assert!(ReplayProtection::validate_timestamp(now, 3600).is_ok());

    let old_timestamp = now - 4000;
    assert!(ReplayProtection::validate_timestamp(old_timestamp, 3600).is_err());

    let future_timestamp = now + 400;
    assert!(ReplayProtection::validate_timestamp(future_timestamp, 3600).is_err());
}

#[tokio::test]
async fn test_ban_list_timestamp_validation() {
    let now = current_timestamp() as i64;

    assert!(ReplayProtection::validate_timestamp(now, 86400).is_ok());
    assert!(ReplayProtection::validate_timestamp(now - 43200, 86400).is_ok());
    assert!(ReplayProtection::validate_timestamp(now - 90000, 86400).is_err());
    assert!(ReplayProtection::validate_timestamp(now + 400, 86400).is_err());
}

#[tokio::test]
async fn test_get_filtered_block_request_id_deduplication() {
    let protection = Arc::new(ReplayProtection::new());
    let request_id = 12345u64;

    assert!(protection.check_request_id(request_id).await.is_ok());
    assert!(protection.check_request_id(request_id).await.is_err());
}

#[tokio::test]
async fn test_get_module_request_id_deduplication() {
    let protection = Arc::new(ReplayProtection::new());
    let request_id = 67890u64;

    assert!(protection.check_request_id(request_id).await.is_ok());
    assert!(protection.check_request_id(request_id).await.is_err());
}

#[tokio::test]
async fn test_multiple_async_requests_different_ids() {
    let protection = Arc::new(ReplayProtection::new());

    for i in 1000..1010 {
        assert!(protection.check_request_id(i).await.is_ok());
    }

    assert!(protection.check_request_id(1005).await.is_err());
}

#[tokio::test]
async fn test_custom_messages_different_ids() {
    let protection = Arc::new(ReplayProtection::new());
    let now = current_timestamp() as i64;

    for i in 0..10 {
        let message_id = format!("custom-msg-{}", i);
        assert!(protection.check_message_id(&message_id, now).await.is_ok());
    }

    assert!(protection
        .check_message_id("custom-msg-5", now)
        .await
        .is_err());
}

#[tokio::test]
async fn test_replay_protection_prevents_duplicate_ban_lists() {
    let now = current_timestamp() as i64;

    assert!(ReplayProtection::validate_timestamp(now, 86400).is_ok());

    let old_timestamp = now - 90000;
    assert!(ReplayProtection::validate_timestamp(old_timestamp, 86400).is_err());
}

#[tokio::test]
async fn test_replay_protection_cleanup_allows_reuse() {
    let protection = ReplayProtection::with_config(
        std::time::Duration::from_millis(100),
        std::time::Duration::from_millis(200),
        std::time::Duration::from_millis(200),
        300,
    );

    let message_id = "cleanup-test";
    let request_id = 9999u64;
    let timestamp = current_timestamp() as i64;

    protection
        .check_message_id(message_id, timestamp)
        .await
        .unwrap();
    protection.check_request_id(request_id).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(protection
        .check_message_id(message_id, current_timestamp() as i64)
        .await
        .is_ok());
    assert!(protection.check_request_id(request_id).await.is_ok());
}

//! Integration tests for replay protection in message handlers
//!
//! Tests that replay protection is properly integrated with:
//! - Governance messages (EconomicNodeRegistration, EconomicNodeVeto, EconomicNodeForkDecision)
//! - Ban list messages
//! - Async request messages (GetFilteredBlock, GetModule)

use bllvm_node::network::protocol::{
    BanListMessage, EconomicNodeForkDecisionMessage, EconomicNodeRegistrationMessage,
    EconomicNodeVetoMessage, GetFilteredBlockMessage, GetModuleMessage,
};
use bllvm_node::network::replay_protection::ReplayProtection;
use bllvm_node::utils::current_timestamp;
use std::net::SocketAddr;
use std::sync::Arc;

// Note: These are unit tests for the replay protection logic itself.
// Full integration tests would require setting up a NetworkManager instance,
// which is complex. These tests verify that the replay protection checks
// work correctly with the message types.

#[tokio::test]
#[cfg(feature = "governance")]
async fn test_governance_message_replay_protection() {
    let protection = Arc::new(ReplayProtection::new());

    // Simulate EconomicNodeRegistration message
    let message_id = "gov-msg-123";
    let timestamp = current_timestamp() as i64;

    // First message should pass
    assert!(protection
        .check_message_id(message_id, timestamp)
        .await
        .is_ok());
    assert!(ReplayProtection::validate_timestamp(timestamp, 3600).is_ok());

    // Replay should fail
    assert!(protection
        .check_message_id(message_id, timestamp)
        .await
        .is_err());
}

#[tokio::test]
#[cfg(feature = "governance")]
async fn test_governance_message_timestamp_validation() {
    let protection = Arc::new(ReplayProtection::new());

    let message_id = "gov-msg-456";
    let now = current_timestamp() as i64;

    // Valid timestamp (current)
    assert!(protection
        .check_message_id(message_id, now)
        .await
        .is_ok());
    assert!(ReplayProtection::validate_timestamp(now, 3600).is_ok());

    // Invalid timestamp (too old)
    let old_timestamp = now - 4000;
    assert!(ReplayProtection::validate_timestamp(old_timestamp, 3600).is_err());

    // Invalid timestamp (too future)
    let future_timestamp = now + 400;
    assert!(ReplayProtection::validate_timestamp(future_timestamp, 3600).is_err());
}

#[tokio::test]
async fn test_ban_list_timestamp_validation() {
    // Ban lists use 24 hour max age
    let now = current_timestamp() as i64;

    // Valid timestamp (current)
    assert!(ReplayProtection::validate_timestamp(now, 86400).is_ok());

    // Valid timestamp (12 hours ago)
    assert!(ReplayProtection::validate_timestamp(now - 43200, 86400).is_ok());

    // Invalid timestamp (too old - 25 hours ago)
    assert!(ReplayProtection::validate_timestamp(now - 90000, 86400).is_err());

    // Invalid timestamp (too future)
    assert!(ReplayProtection::validate_timestamp(now + 400, 86400).is_err());
}

#[tokio::test]
async fn test_get_filtered_block_request_id_deduplication() {
    let protection = Arc::new(ReplayProtection::new());

    // Simulate GetFilteredBlock request
    let request_id = 12345u64;

    // First request should pass
    assert!(protection.check_request_id(request_id).await.is_ok());

    // Replay should fail
    assert!(protection.check_request_id(request_id).await.is_err());
}

#[tokio::test]
async fn test_get_module_request_id_deduplication() {
    let protection = Arc::new(ReplayProtection::new());

    // Simulate GetModule request
    let request_id = 67890u64;

    // First request should pass
    assert!(protection.check_request_id(request_id).await.is_ok());

    // Replay should fail
    assert!(protection.check_request_id(request_id).await.is_err());
}

#[tokio::test]
async fn test_multiple_async_requests_different_ids() {
    let protection = Arc::new(ReplayProtection::new());

    // Multiple different request IDs should all pass
    for i in 1000..1010 {
        assert!(protection.check_request_id(i).await.is_ok());
    }

    // But replaying any of them should fail
    assert!(protection.check_request_id(1005).await.is_err());
}

#[tokio::test]
#[cfg(feature = "governance")]
async fn test_governance_messages_different_ids() {
    let protection = Arc::new(ReplayProtection::new());

    let now = current_timestamp() as i64;

    // Multiple different governance messages should all pass
    for i in 0..10 {
        let message_id = format!("gov-msg-{}", i);
        assert!(protection
            .check_message_id(&message_id, now)
            .await
            .is_ok());
    }

    // But replaying any of them should fail
    assert!(protection
        .check_message_id("gov-msg-5", now)
        .await
        .is_err());
}

#[tokio::test]
async fn test_replay_protection_prevents_duplicate_ban_lists() {
    // Ban lists don't use message ID deduplication, only timestamp validation
    // This test verifies timestamp validation works for ban lists
    let now = current_timestamp() as i64;

    // Valid ban list timestamp
    assert!(ReplayProtection::validate_timestamp(now, 86400).is_ok());

    // Old ban list should be rejected
    let old_timestamp = now - 90000; // 25 hours ago
    assert!(ReplayProtection::validate_timestamp(old_timestamp, 86400).is_err());
}

#[tokio::test]
async fn test_replay_protection_cleanup_allows_reuse() {
    let protection = ReplayProtection::with_config(
        std::time::Duration::from_millis(100), // cleanup every 100ms
        std::time::Duration::from_millis(200), // message IDs expire after 200ms
        std::time::Duration::from_millis(200), // request IDs expire after 200ms
        300,
    );

    let message_id = "cleanup-test";
    let request_id = 9999u64;
    let timestamp = current_timestamp() as i64;

    // Add entries
    protection
        .check_message_id(message_id, timestamp)
        .await
        .unwrap();
    protection.check_request_id(request_id).await.unwrap();

    // Wait for cleanup
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Should be able to reuse after cleanup
    assert!(protection
        .check_message_id(message_id, current_timestamp() as i64)
        .await
        .is_ok());
    assert!(protection.check_request_id(request_id).await.is_ok());
}



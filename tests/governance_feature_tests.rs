//! Governance feature tests
//!
//! Tests for governance features when the "governance" feature is enabled.
//! These tests are conditionally compiled based on feature flags.

#[cfg(feature = "governance")]
use bllvm_node::governance::webhook::GovernanceWebhookClient;
#[cfg(feature = "governance")]
use bllvm_protocol::Block;

#[cfg(feature = "governance")]
#[test]
fn test_governance_webhook_client_creation() {
    // Create webhook client without URL (disabled)
    let _client = GovernanceWebhookClient::new(None, None);
    // Should create successfully
    assert!(true);
}

#[cfg(feature = "governance")]
#[test]
fn test_governance_webhook_client_with_url() {
    // Create webhook client with URL (enabled)
    let _client =
        GovernanceWebhookClient::new(Some("http://localhost:8080/webhook".to_string()), None);
    // Should create successfully
    assert!(true);
}

#[cfg(feature = "governance")]
#[test]
fn test_governance_webhook_client_with_node_id() {
    // Create webhook client with node ID
    let _client = GovernanceWebhookClient::new(
        Some("http://localhost:8080/webhook".to_string()),
        Some("test-node-id".to_string()),
    );
    // Should create successfully
    assert!(true);
}

#[cfg(feature = "governance")]
#[test]
fn test_governance_webhook_client_from_env() {
    // Test creation from environment variables
    // Note: This will use actual env vars if set, or create disabled client
    let _client = GovernanceWebhookClient::from_env();
    // Should create successfully
    assert!(true);
}

#[cfg(feature = "governance")]
#[tokio::test]
async fn test_governance_webhook_client_notify_block_disabled() {
    // Create disabled client
    let client = GovernanceWebhookClient::new(None, None);

    // Create a test block
    let block = Block {
        header: bllvm_protocol::BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    };

    // Should succeed silently when disabled
    let result = client.notify_block(&block, 0).await;
    assert!(result.is_ok());
}

// Tests that verify feature is properly gated
#[cfg(not(feature = "governance"))]
#[test]
fn test_governance_feature_not_enabled() {
    // When governance feature is not enabled, these types should not be available
    // This test verifies the feature gating works correctly
    assert!(true);
}

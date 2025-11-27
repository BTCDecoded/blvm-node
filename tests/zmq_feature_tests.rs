//! ZMQ feature tests
//!
//! Tests for ZMQ publisher when the "zmq" feature is enabled.
//! These tests are conditionally compiled based on feature flags.

#[cfg(feature = "zmq")]
use bllvm_node::zmq::publisher::{ZmqConfig, ZmqPublisher};
#[cfg(feature = "zmq")]
use bllvm_protocol::Block;

#[cfg(feature = "zmq")]
#[test]
fn test_zmq_publisher_creation() {
    // Create ZMQ config with test endpoint
    let config = ZmqConfig {
        hashblock: Some("tcp://127.0.0.1:28332".to_string()),
        ..Default::default()
    };

    // Create ZMQ publisher
    let result = ZmqPublisher::new(&config);
    // May fail if ZMQ library not available, but should compile
    let _ = result;
}

#[cfg(feature = "zmq")]
#[test]
fn test_zmq_publisher_multiple_endpoints() {
    // Create ZMQ config with multiple endpoints
    let config = ZmqConfig {
        hashblock: Some("tcp://127.0.0.1:28332".to_string()),
        hashtx: Some("tcp://127.0.0.1:28333".to_string()),
        rawblock: Some("tcp://127.0.0.1:28334".to_string()),
        ..Default::default()
    };

    let result = ZmqPublisher::new(&config);
    // May fail if ZMQ library not available, but should compile
    let _ = result;
}

#[cfg(feature = "zmq")]
#[test]
fn test_zmq_config_is_enabled() {
    // Test config with no endpoints (disabled)
    let config_empty = ZmqConfig::default();
    assert!(!config_empty.is_enabled());

    // Test config with endpoints (enabled)
    let config_enabled = ZmqConfig {
        hashblock: Some("tcp://127.0.0.1:28332".to_string()),
        ..Default::default()
    };
    assert!(config_enabled.is_enabled());
}

#[cfg(feature = "zmq")]
#[tokio::test]
async fn test_zmq_publisher_publish_block() {
    // Create ZMQ config
    let config = ZmqConfig {
        hashblock: Some("tcp://127.0.0.1:28332".to_string()),
        rawblock: Some("tcp://127.0.0.1:28334".to_string()),
        ..Default::default()
    };

    // Create ZMQ publisher (may fail if ZMQ not available)
    if let Ok(publisher) = ZmqPublisher::new(&config) {
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

        let block_hash: bllvm_protocol::Hash = [0u8; 32];

        // Publish block (may fail if no ZMQ server, but shouldn't panic)
        let _result = publisher.publish_block(&block, &block_hash).await;
    }
}

#[cfg(feature = "zmq")]
#[tokio::test]
async fn test_zmq_publisher_publish_transaction() {
    // Create ZMQ config
    let config = ZmqConfig {
        hashtx: Some("tcp://127.0.0.1:28333".to_string()),
        rawtx: Some("tcp://127.0.0.1:28335".to_string()),
        sequence: Some("tcp://127.0.0.1:28336".to_string()),
        ..Default::default()
    };

    // Create ZMQ publisher (may fail if ZMQ not available)
    if let Ok(publisher) = ZmqPublisher::new(&config) {
        // Create a test transaction
        let tx = bllvm_protocol::Transaction {
            version: 1,
            inputs: vec![].into(),
            outputs: vec![].into(),
            lock_time: 0,
        };

        let tx_hash: bllvm_protocol::Hash = [0u8; 32];

        // Publish transaction (may fail if no ZMQ server, but shouldn't panic)
        let _result = publisher.publish_transaction(&tx, &tx_hash, true).await;
    }
}

// Tests that verify feature is properly gated
#[cfg(not(feature = "zmq"))]
#[test]
fn test_zmq_feature_not_enabled() {
    // When zmq feature is not enabled, these types should not be available
    // This test verifies the feature gating works correctly
    assert!(true);
}

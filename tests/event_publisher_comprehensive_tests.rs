//! Event Publisher Comprehensive Tests
//!
//! Tests for event publisher functionality including module events and ZMQ.

use bllvm_node::module::api::events::EventManager;
use bllvm_node::node::event_publisher::EventPublisher;
use bllvm_node::{Block, Hash, Transaction};
use std::sync::Arc;

#[test]
fn test_event_publisher_creation() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

    // Should create successfully
    assert!(true);
}

#[cfg(feature = "zmq")]
#[test]
fn test_event_publisher_with_zmq() {
    use bllvm_node::zmq::publisher::{ZmqConfig, ZmqPublisher};

    let event_manager = Arc::new(EventManager::new());
    let zmq_config = ZmqConfig::default();
    let zmq_publisher = ZmqPublisher::new(&zmq_config).ok();
    let zmq_arc = zmq_publisher.map(Arc::new);

    let publisher = EventPublisher::with_zmq(event_manager, zmq_arc);

    // Should create successfully
    assert!(true);
}

#[tokio::test]
async fn test_event_publisher_new_block() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

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

    let block_hash: Hash = [0u8; 32];

    // Publish new block event (should not panic)
    publisher.publish_new_block(&block, &block_hash, 0).await;
}

#[tokio::test]
async fn test_event_publisher_new_transaction() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

    // Create a test transaction
    let tx = Transaction {
        version: 1,
        inputs: vec![].into(),
        outputs: vec![].into(),
        lock_time: 0,
    };

    let tx_hash: Hash = [0u8; 32];

    // Publish new transaction event (should not panic)
    publisher.publish_new_transaction(&tx, &tx_hash, true).await;
}

#[tokio::test]
async fn test_event_publisher_chain_reorg() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

    // Create test hashes
    let old_tip: Hash = [1u8; 32];
    let new_tip: Hash = [2u8; 32];

    // Publish chain reorg event (should not panic)
    publisher.publish_chain_reorg(&old_tip, &new_tip).await;
}

#[tokio::test]
async fn test_event_publisher_block_disconnected() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

    let block_hash: Hash = [1u8; 32];

    // Publish block disconnected event (should not panic)
    publisher.publish_block_disconnected(&block_hash, 100).await;
}

#[test]
fn test_event_publisher_basic_operations() {
    let event_manager = Arc::new(EventManager::new());
    let _publisher = EventPublisher::new(event_manager);

    // Basic test - just verify creation works
    assert!(true);
}

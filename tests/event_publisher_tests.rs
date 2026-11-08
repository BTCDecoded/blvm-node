//! Tests for event publisher

use blvm_node::module::api::events::EventManager;
use blvm_node::node::event_publisher::EventPublisher;
use blvm_node::{Block, BlockHeader, Hash};
use std::sync::Arc;

fn create_test_block() -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1231006505,
            bits: 0x1d00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    }
}

#[tokio::test]
async fn test_event_publisher_creation() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);
    // Should create successfully
}

#[tokio::test]
async fn test_publish_new_block() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);
    let block = create_test_block();
    let block_hash: Hash = [0xaa; 32];
    let height = 100;

    // Should publish without error
    publisher
        .publish_new_block(&block, &block_hash, height)
        .await;
}

#[tokio::test]
async fn test_publish_multiple_blocks() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

    for i in 0..5 {
        let block = create_test_block();
        let mut block_hash = [0u8; 32];
        block_hash[0] = i;
        let height = i as u64;

        publisher
            .publish_new_block(&block, &block_hash, height)
            .await;
    }
}

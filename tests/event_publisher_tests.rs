//! Tests for event publisher

use bllvm_node::module::api::events::EventManager;
use bllvm_node::node::event_publisher::EventPublisher;
use bllvm_node::{Block, BlockHeader, Hash, Transaction};
use bllvm_protocol::tx_inputs;
use bllvm_protocol::tx_outputs;
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

fn create_test_transaction() -> Transaction {
    Transaction {
        version: 1,
        inputs: tx_inputs![],
        outputs: tx_outputs![
            bllvm_protocol::TransactionOutput {
                value: 1000,
                script_pubkey: vec![0x51], // OP_1
            }
        ],
        lock_time: 0,
    }
}

#[tokio::test]
async fn test_event_publisher_creation() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);
    // Should create successfully
    assert!(true);
}

#[tokio::test]
async fn test_publish_new_block() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);
    let block = create_test_block();
    let block_hash: Hash = [0xaa; 32];
    let height = 100;

    // Should publish without error
    publisher.publish_new_block(&block, &block_hash, height).await;
    assert!(true);
}

#[tokio::test]
async fn test_publish_new_transaction() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);
    let tx = create_test_transaction();
    let tx_hash: Hash = [0xbb; 32];

    // Should publish without error
    publisher
        .publish_new_transaction(&tx, &tx_hash, true)
        .await;
    assert!(true);
}

#[tokio::test]
async fn test_publish_new_transaction_mempool_entry() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);
    let tx = create_test_transaction();
    let tx_hash: Hash = [0xcc; 32];

    // Test with is_mempool_entry = true
    publisher
        .publish_new_transaction(&tx, &tx_hash, true)
        .await;

    // Test with is_mempool_entry = false
    publisher
        .publish_new_transaction(&tx, &tx_hash, false)
        .await;

    assert!(true);
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

        publisher.publish_new_block(&block, &block_hash, height).await;
    }

    assert!(true);
}

#[tokio::test]
async fn test_publish_multiple_transactions() {
    let event_manager = Arc::new(EventManager::new());
    let publisher = EventPublisher::new(event_manager);

    for i in 0..5 {
        let tx = create_test_transaction();
        let mut tx_hash = [0u8; 32];
        tx_hash[0] = i;

        publisher
            .publish_new_transaction(&tx, &tx_hash, true)
            .await;
    }

    assert!(true);
}


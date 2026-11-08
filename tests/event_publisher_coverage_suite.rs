//! Exercise EventPublisher publish paths (smoke coverage).

use blvm_node::module::api::events::EventManager;
use blvm_node::node::event_publisher::EventPublisher;
use blvm_node::{Block, Hash};
use std::sync::Arc;

fn publisher() -> EventPublisher {
    EventPublisher::new(Arc::new(EventManager::new()))
}

fn sample_block() -> Block {
    Block {
        header: blvm_protocol::BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_231_006_505,
            bits: 0x0f00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    }
}

#[tokio::test]
async fn test_publish_chain_tip_events() {
    let p = publisher();
    let block = sample_block();
    let hash: Hash = [0x22; 32];
    let old_tip: Hash = [0x44; 32];
    let new_tip: Hash = [0x55; 32];

    p.publish_new_block(&block, &hash, 100).await;
    p.publish_block_disconnected(&hash, 99).await;
    p.publish_chain_reorg(&old_tip, &new_tip).await;
}

#[tokio::test]
async fn test_publish_sync_and_network_events() {
    let p = publisher();
    let hash: Hash = [0xab; 32];

    p.publish_headers_sync_started(0).await;
    p.publish_headers_sync_progress(100, 1000, 10.0).await;
    p.publish_headers_sync_completed(1000, 5).await;
    p.publish_block_sync_started(0, 1000).await;
    p.publish_block_sync_progress(500, 1000, 50.0, 2.5).await;
    p.publish_block_sync_completed(1000, 10).await;
    p.publish_sync_state_changed("ibd", "synced").await;

    p.publish_peer_connected("127.0.0.1:8333", "outbound", 1, 70015)
        .await;
    p.publish_peer_disconnected("127.0.0.1:8333", "closed")
        .await;
    p.publish_peer_banned("127.0.0.1:8333", "misbehaving", 3600)
        .await;
    p.publish_peer_unbanned("127.0.0.1:8333").await;
    p.publish_message_received("127.0.0.1:8333", "inv", 128, 70015)
        .await;
    p.publish_message_sent("127.0.0.1:8333", "headers", 256)
        .await;

    p.publish_mempool_transaction_added(&hash, 1.5, 1000).await;
    p.publish_mempool_transaction_removed(&hash, "replaced".to_string(), 999)
        .await;
    p.publish_fee_rate_changed(1.0, 2.0, 1000).await;
    p.publish_mempool_threshold_exceeded(900, 1000).await;
    p.publish_mempool_cleared(10).await;
}

#[tokio::test]
async fn test_publish_validation_and_mining_events() {
    let p = publisher();
    let hash: Hash = [0xcd; 32];

    p.publish_block_validation_started(&hash, 1).await;
    p.publish_block_validation_completed(&hash, 1, true, 5, None)
        .await;
    p.publish_difficulty_adjusted(0x1d00ffff, 0x1d00ffff, 2016)
        .await;

    p.publish_block_mined(&hash, 1, Some("miner-1".to_string()))
        .await;
    p.publish_block_template_updated(&hash, 2, 3).await;
    p.publish_mining_difficulty_changed(0x1d00ffff, 0x1d00ffff, 2016)
        .await;
}

#[tokio::test]
async fn test_publish_ops_and_module_events() {
    let p = publisher();

    p.publish_config_loaded(vec!["network".to_string()], None)
        .await;
    p.publish_node_shutdown("user".into(), 30).await;
    p.publish_data_maintenance(
        "prune".to_string(),
        "low".to_string(),
        "periodic".to_string(),
        Some(7),
        Some(60),
    )
    .await;
    p.publish_disk_space_low(1_000_000, 10_000_000, 10.0, "/data".to_string())
        .await;
    p.publish_health_check("full".to_string(), true, Some("healthy".to_string()))
        .await;
    p.publish_companion_udp_peer_registered("127.0.0.1:8333", "127.0.0.1:8334")
        .await;
    p.publish_companion_udp_peer_unregistered("127.0.0.1:8333")
        .await;
    p.publish_event(
        blvm_node::module::traits::EventType::NewBlock,
        blvm_node::module::ipc::protocol::EventPayload::NewBlock {
            block_hash: [0x77; 32],
            height: 42,
        },
    )
    .await
    .unwrap();
}

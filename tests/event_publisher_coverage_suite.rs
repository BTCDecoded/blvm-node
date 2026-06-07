//! Exercise EventPublisher publish paths (smoke coverage).

use blvm_node::module::api::events::EventManager;
use blvm_node::node::event_publisher::EventPublisher;
use blvm_node::{Block, Hash, Transaction};
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

fn sample_tx() -> Transaction {
    Transaction {
        version: 1,
        inputs: vec![].into(),
        outputs: vec![].into(),
        lock_time: 0,
    }
}

#[tokio::test]
async fn test_publish_chain_tip_events() {
    let p = publisher();
    let block = sample_block();
    let hash: Hash = [0x22; 32];
    let tx = sample_tx();
    let tx_hash: Hash = [0x33; 32];
    let old_tip: Hash = [0x44; 32];
    let new_tip: Hash = [0x55; 32];

    p.publish_new_block(&block, &hash, 100).await;
    p.publish_new_transaction(&tx, &tx_hash, true).await;
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
    p.publish_broadcast_started("inv", 3).await;
    p.publish_broadcast_completed("inv", 3, 0).await;
    p.publish_connection_attempt("127.0.0.1:8333", true, None)
        .await;
    p.publish_address_discovered("127.0.0.1:8334", "addr").await;
    p.publish_address_expired("127.0.0.1:8334").await;
    let partition_id = [1u8; 32];
    let peers = vec!["127.0.0.1:8333".to_string()];
    p.publish_network_partition(&partition_id, &peers, 2).await;
    p.publish_network_reconnected(&partition_id, &peers).await;
    p.publish_dos_attack_detected("127.0.0.1:8333", "rate", "high")
        .await;
    p.publish_rate_limit_exceeded("127.0.0.1:8333", "rpc", 150.0, 100.0)
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
    let tx_hash: Hash = [0xef; 32];

    p.publish_block_validation_started(&hash, 1).await;
    p.publish_block_validation_completed(&hash, 1, true, 5, None)
        .await;
    p.publish_script_verification_started(&tx_hash, 0).await;
    p.publish_script_verification_completed(&tx_hash, 0, true, 1)
        .await;
    p.publish_utxo_validation_started(&hash, 1).await;
    p.publish_utxo_validation_completed(&hash, 1, true).await;
    p.publish_difficulty_adjusted(0x1d00ffff, 0x1d00ffff, 2016)
        .await;
    p.publish_soft_fork_activated("segwit", 481_824).await;
    p.publish_soft_fork_locked_in("taproot", 709_632).await;
    p.publish_consensus_rule_violation("test", Some(&hash), None, "detail")
        .await;

    p.publish_block_mined(&hash, 1, Some("miner-1".to_string()))
        .await;
    p.publish_block_template_updated(&hash, 2, 3).await;
    p.publish_mining_difficulty_changed(0x1d00ffff, 0x1d00ffff, 2016)
        .await;
    p.publish_mining_job_created("job-1", &hash, 2).await;
    let share_hash: Hash = [0x01; 32];
    p.publish_share_submitted("job-1", &share_hash, Some("worker-1"))
        .await;
    p.publish_merge_mining_reward("aux", 1000, &hash).await;
    p.publish_mining_pool_connected("stratum+tcp://pool", Some("pool1"))
        .await;
    p.publish_mining_pool_disconnected("stratum+tcp://pool", "timeout")
        .await;
}

#[tokio::test]
async fn test_publish_ops_and_module_events() {
    let p = publisher();
    let hash: Hash = [0x11; 32];

    p.publish_ibd_block_filtered(&hash, 100, "policy").await;
    p.publish_selective_sync_policy_applied("headers-only", 1000)
        .await;
    p.publish_action_executed("restart", "system", "node", true)
        .await;
    p.publish_module_purchase_completed("zmq", "txid", 1000)
        .await;
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
    let dest = [1u8; 32];
    let route = vec!["hop1".to_string(), "hop2".to_string()];
    p.publish_route_discovered(&dest, &route, 3).await;
    p.publish_route_failed(&[3u8; 32], "unreachable").await;
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

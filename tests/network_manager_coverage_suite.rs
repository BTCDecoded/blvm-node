//! NetworkManager request tracking and peer policy smoke coverage.

use blvm_node::network::network_manager::NetworkManager;
use blvm_node::network::transport::TransportPreference;
use blvm_protocol::BlockHeader;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

fn localhost() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18444)
}

#[tokio::test(flavor = "multi_thread")]
async fn network_manager_request_register_complete_cancel() {
    let nm = NetworkManager::new(localhost());
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 8333);

    let id1 = nm.generate_request_id();
    let id2 = nm.generate_request_id();
    assert_ne!(id1, id2);

    let (req_id, mut rx) = nm.register_request_with_priority(peer, 1);
    assert_eq!(nm.get_pending_requests_for_peer(peer), vec![req_id]);
    assert!(nm.complete_request(req_id, b"ok".to_vec()));
    assert!(rx.try_recv().is_ok());

    let (cancel_id, _rx) = nm.register_request(peer);
    assert!(nm.cancel_request(cancel_id));
    assert!(!nm.complete_request(cancel_id, vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn network_manager_headers_and_block_request_queues() {
    let nm = NetworkManager::new(localhost());
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), 8333);

    let mut headers_rx = nm.register_headers_request(peer);
    assert_eq!(nm.pending_headers_count(peer), 1);

    let header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [1u8; 32],
        timestamp: 1,
        bits: 0x0f00ffff,
        nonce: 0,
    };
    assert!(nm.complete_headers_request(peer, vec![header.clone()]));
    assert!(headers_rx.try_recv().is_ok());

    let mut block_rx = nm.register_block_request(peer, [0xcd; 32]);
    let block = blvm_protocol::Block {
        header,
        transactions: vec![].into_boxed_slice(),
    };
    assert!(nm.complete_block_request(peer, [0xcd; 32], block, vec![]));
    assert!(block_rx.try_recv().is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn network_manager_peer_ban_and_block_queue_smoke() {
    let nm =
        NetworkManager::with_transport_preference(localhost(), 8, TransportPreference::TCP_ONLY);

    assert_eq!(nm.transport_preference(), TransportPreference::TCP_ONLY);
    assert_eq!(nm.peer_count(), 0);
    assert!(nm.peer_addresses().is_empty());
    let _active = nm.is_network_active();

    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 8333);
    nm.add_persistent_peer(peer);
    assert!(nm.get_persistent_peers_sync().contains(&peer));
    nm.remove_persistent_peer(peer);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    nm.ban_peer(peer, now);
    assert!(nm.is_banned(peer));
    assert!(!nm.get_banned_peers().is_empty());
    nm.unban_peer(peer);
    nm.clear_bans();

    nm.queue_block(vec![0x01, 0x02]);
    assert!(nm.try_recv_block().is_some());

    let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 42));
    assert!(nm.check_eclipse_prevention(ip));
    let _prefix = nm.get_ip_prefix(ip);
    nm.remove_peer_diversity(ip);

    let _sender = nm.message_sender();
    let _ = nm.filter_service();
    let _ = nm.dos_protection();
    let _ = nm.bytes_sent();
    let _ = nm.get_network_stats_legacy();
}

#[tokio::test(flavor = "multi_thread")]
async fn network_manager_create_version_message_sets_services() {
    use blvm_node::network::protocol::NetworkAddress;

    let nm = NetworkManager::new(localhost());
    let addr = NetworkAddress {
        services: 0,
        ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 127, 0, 0, 1],
        port: 8333,
    };
    let msg = nm.create_version_message(
        70016,
        0,
        1_700_000_000,
        addr.clone(),
        addr,
        0xdeadbeef,
        "blvm-cov".into(),
        100,
        true,
    );
    assert_eq!(msg.version, 70016);
    assert!(msg.services > 0);
    assert_eq!(msg.user_agent, "blvm-cov");
}

#[tokio::test(flavor = "multi_thread")]
async fn network_manager_peer_transport_and_start_height() {
    let nm = NetworkManager::with_config(localhost(), 8, TransportPreference::TCP_ONLY, None);
    assert!(nm.get_highest_peer_start_height().is_none());
    assert!(nm.peer_transport_addresses().is_empty());
    let _utxo = nm.utxo_set_arc();
}

#[tokio::test(flavor = "multi_thread")]
async fn network_manager_cleanup_expired_requests() {
    let nm = NetworkManager::with_config(localhost(), 8, TransportPreference::TCP_ONLY, None);
    let removed = nm.cleanup_expired_requests(0);
    assert_eq!(removed, 0);
}

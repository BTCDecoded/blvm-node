//! Integration tests for ban list functionality

use bllvm_node::network::NetworkManager;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test(flavor = "multi_thread")]
async fn test_ban_list_operations() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);

    let test_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Test ban
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let future_timestamp = now + 3600; // 1 hour from now
    manager.ban_peer(test_addr, future_timestamp);

    assert!(manager.is_banned(test_addr));

    // Test unban
    manager.unban_peer(test_addr);
    assert!(!manager.is_banned(test_addr));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ban_list_expiration() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);

    let test_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Ban with expiration in the past
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let past_timestamp = now - 3600; // 1 hour ago
    manager.ban_peer(test_addr, past_timestamp);

    // is_banned should automatically check expiration
    assert!(!manager.is_banned(test_addr));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ban_list_permanent() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);

    let test_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();

    // Permanent ban (timestamp = 0)
    manager.ban_peer(test_addr, 0);
    assert!(manager.is_banned(test_addr));

    // Clear all bans
    manager.clear_bans();
    assert!(!manager.is_banned(test_addr));
    assert_eq!(manager.get_banned_peers().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ban_list_multiple_peers() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);

    let addr1: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.1:8334".parse().unwrap();
    let addr3: SocketAddr = "127.0.0.1:8335".parse().unwrap();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    manager.ban_peer(addr1, now + 3600);
    manager.ban_peer(addr2, 0); // Permanent
    manager.ban_peer(addr3, now + 7200);

    assert_eq!(manager.get_banned_peers().len(), 3);
    assert!(manager.is_banned(addr1));
    assert!(manager.is_banned(addr2));
    assert!(manager.is_banned(addr3));

    // Unban one
    manager.unban_peer(addr2);
    assert_eq!(manager.get_banned_peers().len(), 2);
    assert!(!manager.is_banned(addr2));
}

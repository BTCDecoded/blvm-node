//! Test-line QA: a global IPv6 dial is refused when this host has no IPv6 route.

use std::net::SocketAddr;
use std::time::Duration;

use blvm_node::network::network_manager::NetworkManager;

#[tokio::test]
async fn connect_to_peer_rejects_global_ipv6_without_routing() {
    let manager = NetworkManager::new("127.0.0.1:0".parse().expect("addr"));
    let peer: SocketAddr = "[2001:db8::1]:8333".parse().expect("peer");

    match tokio::time::timeout(Duration::from_secs(2), manager.connect_to_peer(peer)).await {
        Ok(Err(err)) => {
            let message = err.to_string();
            assert!(
                message.contains(
                    "Skipping IPv6 peer [2001:db8::1]:8333 — no global IPv6 routing on this host"
                ),
                "connect_to_peer returned {message}"
            );
        }
        Ok(Ok(())) => panic!("global IPv6 peer was connected"),
        Err(_) => panic!("connect_to_peer did not return"),
    }
}

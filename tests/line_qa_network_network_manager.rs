//! Test-line QA: a send with no peers is rejected.

use blvm_node::network::network_manager::NetworkManager;

#[tokio::test]
async fn send_to_best_peer_rejects_an_empty_peer_list() {
    let manager = NetworkManager::new("127.0.0.1:0".parse().expect("addr"));

    match manager.send_to_best_peer(Vec::new()).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("No peers available"),
                "send_to_best_peer returned {message}"
            );
        }
        Ok(addr) => panic!("sent to a peer when none are connected: {addr:?}"),
    }
}

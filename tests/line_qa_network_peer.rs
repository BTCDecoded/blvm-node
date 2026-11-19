//! Test-line QA: a peer with no outbound reader rejects a send.

use std::net::SocketAddr;

use blvm_node::network::peer::Peer;

#[tokio::test]
async fn send_message_rejects_a_closed_outbound_channel() {
    let addr: SocketAddr = "127.0.0.1:8333".parse().expect("addr");
    let peer = Peer::new_for_testing(addr);

    match peer.send_message(b"ping".to_vec()).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Failed to send message to peer 127.0.0.1:8333: channel closed"),
                "send_message returned {message}"
            );
        }
        Ok(()) => panic!("message was queued with no reader"),
    }
}

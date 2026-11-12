//! Test-line QA: a LAN checkpoint with no internet peers is rejected.

use std::net::SocketAddr;
use std::pin::Pin;

use blvm_node::network::lan_security::InternetCheckpointValidator;

#[tokio::test]
async fn validate_lan_block_rejects_an_empty_peer_list() {
    let validator = InternetCheckpointValidator::new();
    let lan: SocketAddr = "192.168.1.2:8333".parse().expect("addr");
    let fetch = |_peer: SocketAddr, _height: u64| {
        Box::pin(async { None })
            as Pin<Box<dyn std::future::Future<Output = Option<[u8; 32]>> + Send>>
    };

    match validator
        .validate_lan_block(lan, [0xab; 32], 1000, &[], fetch, None)
        .await
    {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Insufficient internet peers for checkpoint: have 0, need 3"),
                "validate_lan_block returned {message}"
            );
        }
        Ok(()) => panic!("empty peer list passed the checkpoint"),
    }
}

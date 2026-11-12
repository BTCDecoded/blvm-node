//! Test-line QA: RPC refuses a public bind when authentication is off.

use std::net::SocketAddr;
use std::time::Duration;

use blvm_node::rpc::RpcManager;

#[tokio::test]
async fn start_rejects_a_public_address_without_authentication() {
    let addr: SocketAddr = "203.0.113.1:8332".parse().expect("addr");
    let mut manager = RpcManager::new(addr);

    match tokio::time::timeout(Duration::from_secs(2), manager.start()).await {
        Ok(Err(err)) => {
            let message = err.to_string();
            assert!(
                message.contains(
                    "RPC server would bind to public address 203.0.113.1:8332 without authentication"
                ),
                "start returned {message}"
            );
        }
        Ok(Ok(())) => {
            let _ = manager.stop();
            panic!("public RPC bind was accepted");
        }
        Err(_) => panic!("start did not return"),
    }
}

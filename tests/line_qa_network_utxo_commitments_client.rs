//! Test-line QA: a UTXO proof request rejects a peer id that is not an address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use blvm_node::network::NetworkManager;
use blvm_node::network::utxo_commitments_client::UtxoCommitmentsClient;
use blvm_protocol::types::Natural;
use tokio::sync::RwLock;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_utxo_proof_rejects_an_invalid_peer_id() {
    let manager = NetworkManager::new("127.0.0.1:0".parse::<SocketAddr>().expect("addr"));
    let client = UtxoCommitmentsClient::new(Arc::new(RwLock::new(manager)));

    let call =
        client.request_utxo_proof("not-a-peer", [0xab; 32], 0, Natural::from(0u64), [0xcd; 32]);
    match tokio::time::timeout(Duration::from_secs(2), call).await {
        Ok(Err(err)) => {
            let message = err.to_string();
            assert!(
                message.contains("Invalid peer ID: not-a-peer"),
                "request_utxo_proof returned {message}"
            );
        }
        Ok(Ok(_)) => panic!("invalid peer id returned a proof"),
        Err(_) => panic!("request_utxo_proof did not return"),
    }
}

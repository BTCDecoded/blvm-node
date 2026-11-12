//! Test-line QA: the REST server refuses an address that is already bound.

use std::sync::Arc;
use std::time::Duration;

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::rpc::mempool::MempoolRpc;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::rpc::network::NetworkRpc;
use blvm_node::rpc::rawtx::RawTxRpc;
use blvm_node::rpc::rest::RestApiServer;

#[tokio::test]
async fn start_rejects_an_address_already_in_use() {
    let hold = std::net::TcpListener::bind("127.0.0.1:0").expect("hold");
    let addr = hold.local_addr().expect("addr");
    let server = RestApiServer::new(
        addr,
        Arc::new(BlockchainRpc::new()),
        Arc::new(NetworkRpc::new()),
        Arc::new(MempoolRpc::new()),
        Arc::new(MiningRpc::new()),
        Arc::new(RawTxRpc::new()),
    );

    match tokio::time::timeout(Duration::from_secs(2), server.start()).await {
        Ok(Err(err)) => {
            let message = err.to_string();
            assert!(
                message.contains("Address already in use"),
                "RestApiServer::start returned {message}"
            );
        }
        Ok(Ok(())) => panic!("REST server stopped after binding {addr}"),
        Err(_) => panic!("REST server accepted the busy address {addr}"),
    }
}

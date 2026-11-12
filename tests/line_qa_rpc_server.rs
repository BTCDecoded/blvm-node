//! Test-line QA: removing a core RPC override that was never registered fails.

use blvm_node::rpc::server::RpcServer;

#[tokio::test]
async fn unregister_core_override_rejects_a_missing_method() {
    let server = RpcServer::new("127.0.0.1:0".parse().expect("addr"));

    match server.unregister_core_rpc_override("getblock").await {
        Err(message) => {
            assert!(
                message.contains("No core RPC override registered for 'getblock'"),
                "unregister_core_rpc_override returned {message}"
            );
        }
        Ok(()) => panic!("removed a core RPC override that was never registered"),
    }
}

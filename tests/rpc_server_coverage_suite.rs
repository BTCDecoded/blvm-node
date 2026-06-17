//! RpcServer request dispatch and module endpoint registration.

use async_trait::async_trait;
use blvm_node::module::rpc::handler::ModuleRpcHandler;
use blvm_node::rpc::errors::RpcError;
use blvm_node::rpc::server::RpcServer;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;

struct EchoHandler;

#[async_trait]
impl ModuleRpcHandler for EchoHandler {
    async fn handle(&self, params: Value) -> Result<Value, RpcError> {
        Ok(params)
    }
}

fn localhost() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

#[tokio::test]
async fn rpc_server_process_request_help_and_invalid_json() {
    let help_req = r#"{"jsonrpc":"2.0","method":"help","params":[],"id":1}"#;
    let help_resp = RpcServer::process_request(help_req).await;
    assert!(help_resp.contains("\"result\""));
    assert!(help_resp.contains("\"id\":1"));

    let bad = RpcServer::process_request("not-json").await;
    assert!(bad.contains("error"));
}

#[tokio::test]
async fn rpc_server_process_batch_request() {
    let batch = r#"[
        {"jsonrpc":"2.0","method":"uptime","params":[],"id":1},
        {"jsonrpc":"2.0","method":"getrpcinfo","params":[],"id":2}
    ]"#;
    let resp = RpcServer::process_request(batch).await;
    assert!(resp.starts_with('['));
    assert!(resp.contains("\"id\":1"));
    assert!(resp.contains("\"id\":2"));
}

#[tokio::test]
async fn rpc_server_module_endpoint_register_and_unregister() {
    let server = Arc::new(RpcServer::new(localhost()).with_batch_rate_multiplier_cap(8));

    server
        .register_module_endpoint("covmod_ping".into(), "covmod".into(), Arc::new(EchoHandler))
        .await
        .unwrap();

    server
        .unregister_module_endpoint("covmod_ping")
        .await
        .unwrap();
    assert!(
        server
            .unregister_module_endpoint("missing_method")
            .await
            .is_err()
    );
    assert!(
        server
            .register_module_endpoint("getblock".into(), "bad".into(), Arc::new(EchoHandler),)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rpc_server_core_override_register_and_cleanup() {
    let server = Arc::new(RpcServer::new(localhost()));

    server
        .register_core_rpc_override(
            "getdescriptorinfo".into(),
            "covmod".into(),
            Arc::new(EchoHandler),
        )
        .await
        .unwrap();

    server
        .unregister_core_rpc_override("getdescriptorinfo")
        .await
        .unwrap();
    server.unregister_all_for_module("covmod").await;
    assert!(
        server
            .register_core_rpc_override("getblock".into(), "covmod".into(), Arc::new(EchoHandler))
            .await
            .is_err()
    );
}

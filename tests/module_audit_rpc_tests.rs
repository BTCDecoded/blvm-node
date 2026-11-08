//! `getmoduleaudit` reads the hub audit log.

#[path = "stub_node_api.rs"]
mod stub_node_api;

use std::sync::Arc;

use blvm_node::module::ipc::protocol::{MessageType, RequestMessage, RequestPayload};
use blvm_node::module::manager::ModuleManager;
use blvm_node::module::security::permissions::{Permission, PermissionSet};
use blvm_node::rpc::control::ControlRpc;
use serde_json::json;
use stub_node_api::MockNodeAPI;
use tempfile::TempDir;
use tokio::sync::Mutex;

#[tokio::test]
async fn getmoduleaudit_requires_the_module_manager() {
    let rpc = ControlRpc::new();
    let err = rpc.getmoduleaudit(&json!([])).await.unwrap_err();
    assert!(err.to_string().contains("Module system not available"));
}

#[tokio::test]
async fn getmoduleaudit_rejects_a_non_numeric_limit() {
    let temp = TempDir::new().unwrap();
    let manager = ModuleManager::new(
        temp.path().join("modules"),
        temp.path().join("data"),
        temp.path().join("sockets"),
    );
    let rpc = ControlRpc::new().with_module_manager(Arc::new(Mutex::new(manager)));
    let err = rpc.getmoduleaudit(&json!(["nope"])).await.unwrap_err();
    assert!(err.to_string().contains("limit must be a number"));
}

#[tokio::test]
async fn getmoduleaudit_returns_hub_entries() {
    let temp = TempDir::new().unwrap();
    let modules_dir = temp.path().join("modules");
    let data_dir = temp.path().join("data");
    let socket_dir = temp.path().join("sockets");
    std::fs::create_dir_all(&modules_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&socket_dir).unwrap();
    let mut manager = ModuleManager::new(&modules_dir, &data_dir, &socket_dir);
    manager
        .start(temp.path().join("node.sock"), Arc::new(MockNodeAPI))
        .await
        .unwrap();

    let hub = manager.api_hub_arc().expect("hub");
    {
        let mut hub = hub.lock().await;
        let mut permissions = PermissionSet::new();
        permissions.add(Permission::ReadChainState);
        hub.register_module_permissions("test-module".to_string(), permissions);
        let request = RequestMessage {
            correlation_id: 1,
            request_type: MessageType::GetChainTip,
            payload: RequestPayload::GetChainTip,
        };
        hub.handle_request("test-module", request).await.unwrap();
    }

    let rpc = ControlRpc::new().with_module_manager(Arc::new(Mutex::new(manager)));
    let value = rpc.getmoduleaudit(&json!([])).await.unwrap();
    let entries = value.as_array().expect("array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["module_id"], "test-module");
    assert_eq!(entries[0]["api_call"], "get_chain_tip");
    assert_eq!(entries[0]["success"], true);

    let one = rpc.getmoduleaudit(&json!([0])).await.unwrap();
    assert!(one.as_array().expect("array").is_empty());
}

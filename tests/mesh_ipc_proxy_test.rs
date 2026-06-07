//! IPC ModuleAPI proxy registration for subprocess modules (C0).

use std::sync::Arc;

use async_trait::async_trait;
use blvm_node::module::inter_module::api::ModuleAPI;
use blvm_node::module::inter_module::{IpcForwardingModuleAPI, ModuleApiRegistry};
use blvm_node::module::ipc::server::ModuleIpcServer;
use blvm_node::module::traits::ModuleError;
use tokio::sync::Mutex;

struct EchoApi;

#[async_trait]
impl ModuleAPI for EchoApi {
    async fn handle_request(
        &self,
        method: &str,
        params: &[u8],
        _caller: &str,
    ) -> Result<Vec<u8>, ModuleError> {
        match method {
            "echo" => Ok(params.to_vec()),
            _ => Err(ModuleError::OperationError(format!(
                "unknown method {method}"
            ))),
        }
    }

    fn list_methods(&self) -> Vec<String> {
        vec!["echo".to_string()]
    }

    fn api_version(&self) -> u32 {
        1
    }
}

#[tokio::test]
async fn ipc_forwarding_module_api_lists_registered_methods() {
    let ipc = Arc::new(Mutex::new(ModuleIpcServer::new(
        "/tmp/blvm-mesh-ipc-test.sock",
    )));
    let proxy = IpcForwardingModuleAPI::new(
        "blvm-mesh_test".to_string(),
        ipc,
        vec![
            "send_packet".to_string(),
            "poll_local_deliveries".to_string(),
        ],
        1,
    );
    assert!(proxy.list_methods().contains(&"send_packet".to_string()));
    assert_eq!(proxy.api_version(), 1);
}

#[tokio::test]
async fn module_api_registry_accepts_ipc_proxy() {
    let registry = ModuleApiRegistry::new();
    let ipc = Arc::new(Mutex::new(ModuleIpcServer::new(
        "/tmp/blvm-mesh-reg-test.sock",
    )));
    let proxy = IpcForwardingModuleAPI::new(
        "blvm-mesh_abc".to_string(),
        ipc,
        vec!["send_packet".to_string()],
        1,
    );
    registry
        .register_api("blvm-mesh_abc".to_string(), Arc::new(proxy))
        .await
        .unwrap();
    assert!(registry.get_api("blvm-mesh_abc").await.is_some());
}

#[tokio::test]
async fn in_process_api_still_registers_for_tests() {
    let registry = ModuleApiRegistry::new();
    registry
        .register_api("echo_mod".to_string(), Arc::new(EchoApi))
        .await
        .unwrap();
    let api = registry.get_api("echo_mod").await.unwrap();
    let out = api.handle_request("echo", b"hi", "caller").await.unwrap();
    assert_eq!(out, b"hi");
}

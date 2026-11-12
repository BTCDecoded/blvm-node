//! Test-line QA: a module API call is rejected when that module is not connected.

use blvm_node::module::ipc::server::ModuleIpcServer;

#[tokio::test]
async fn invoke_module_api_rejects_a_missing_module() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let server = ModuleIpcServer::new(dir.path().join("modules.sock"));

    match server
        .handle()
        .invoke_module_api("ghost", "ping", b"{}", "caller")
        .await
    {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Module 'ghost' is not connected"),
                "invoke_module_api returned {message}"
            );
        }
        Ok(bytes) => panic!("disconnected module returned {bytes:?}"),
    }
}

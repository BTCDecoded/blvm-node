//! Test-line QA: a WASM host height call is denied before permissions are registered.

use std::sync::Arc;

use blvm_node::module::api::NodeApiImpl;
use blvm_node::module::api::hub::ModuleApiHub;
use blvm_node::storage::Storage;

#[tokio::test]
async fn wasm_host_get_block_height_rejects_an_unregistered_module() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Arc::new(Storage::new(dir.path()).expect("storage"));
    let hub = ModuleApiHub::new(Arc::new(NodeApiImpl::new(storage)));

    match hub.wasm_host_get_block_height("guest").await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("WASM host call requires the permission set registered at load"),
                "wasm_host_get_block_height returned {message}"
            );
        }
        Ok(height) => panic!("unregistered module read height {height}"),
    }
}

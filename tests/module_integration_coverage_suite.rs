//! `ModuleIntegration` in-process and IPC connect helpers.

#[cfg(unix)]
#[path = "common/ipc_harness.rs"]
mod ipc_harness;

#[cfg(unix)]
#[path = "common/mod.rs"]
mod common;

mod in_process {
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::integration::ModuleIntegration;
    use blvm_node::storage::Storage;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[test]
    fn from_node_api_exposes_module_id() {
        let temp = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let api = Arc::new(NodeApiImpl::new(storage));
        let mut integration = ModuleIntegration::from_node_api("in-proc-mod".into(), api);
        assert_eq!(integration.module_id(), "in-proc-mod");
        assert!(integration.invocation_receiver().is_none());
    }
}

#[cfg(unix)]
mod ipc {
    use super::common;
    use super::ipc_harness;
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::integration::ModuleIntegration;
    use blvm_node::module::traits::NodeAPI;
    use blvm_node::storage::Storage;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration, Instant};

    async fn wait_for_socket(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            if Instant::now() > deadline {
                panic!("timeout waiting for IPC socket at {path:?}");
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn connect_via_ipc_handshake_and_query_height() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        common::setup_mining_chain(&storage, 8).unwrap();
        let node_api = Arc::new(NodeApiImpl::new(storage));

        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        wait_for_socket(&socket_path).await;

        let integration = ModuleIntegration::connect(
            socket_path,
            "cov-int".into(),
            "CoverageMod".into(),
            "0.0.1".into(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(integration.module_id(), "cov-int");
        let height = integration.node_api().get_block_height().await.unwrap();
        assert!(height >= 7);
        server.abort();
    }
}

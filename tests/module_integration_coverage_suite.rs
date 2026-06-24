//! `ModuleIntegration` in-process and IPC connect helpers.

#[cfg(unix)]
#[path = "common/ipc_harness.rs"]
mod ipc_harness;

#[cfg(unix)]
#[path = "common/mod.rs"]
mod common;

mod in_process {
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::api::events::EventManager;
    use blvm_node::module::integration::ModuleIntegration;
    use blvm_node::module::ipc::protocol::{EventPayload, ModuleMessage};
    use blvm_node::module::traits::EventType;
    use blvm_node::storage::Storage;
    use std::sync::Arc;
    use std::time::Duration;
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

    #[tokio::test]
    async fn from_node_api_forwards_subscribed_events() {
        let temp = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp.path()).unwrap());
        let event_manager = Arc::new(EventManager::new());
        let mut api = NodeApiImpl::new(storage);
        api.set_event_manager(Arc::clone(&event_manager), "in-proc-mod".into());
        let api = Arc::new(api);

        let integration = ModuleIntegration::from_node_api("in-proc-mod".into(), api);
        integration
            .subscribe_events(vec![EventType::NewBlock])
            .await
            .expect("subscribe");

        event_manager
            .publish_event(
                EventType::NewBlock,
                EventPayload::NewBlock {
                    block_hash: [0xab; 32],
                    height: 42,
                },
            )
            .await
            .expect("publish");

        let mut rx = integration.event_receiver();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let Ok(ModuleMessage::Event(event)) = rx.recv().await else {
                    continue;
                };
                if event.event_type == EventType::NewBlock {
                    return;
                }
            }
        })
        .await
        .expect("timed out waiting for NewBlock event");
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
    use tokio::time::{Duration, Instant, sleep};

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

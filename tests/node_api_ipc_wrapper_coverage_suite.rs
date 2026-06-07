//! `NodeApiIpc` wrapper — exercise `NodeAPI` trait via IPC client.

#[cfg(unix)]
#[path = "common/ipc_harness.rs"]
mod ipc_harness;

#[cfg(unix)]
#[path = "common/mod.rs"]
mod common;

#[cfg(unix)]
mod tests {
    use super::common;
    use super::ipc_harness;
    use blvm_node::module::api::events::EventManager;
    use blvm_node::module::api::node_api_ipc::NodeApiIpc;
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::ipc::protocol::{MessageType, RequestMessage, RequestPayload};
    use blvm_node::module::metrics::manager::{Metric, MetricValue, MetricsManager};
    use blvm_node::module::traits::NodeAPI;
    use blvm_node::network::NetworkManager;
    use blvm_node::node::mempool::MempoolManager;
    use blvm_node::node::sync::SyncCoordinator;
    use blvm_node::storage::Storage;
    use blvm_node::{Block, BlockHeader, Hash, OutPoint, UTXO};
    use common::{setup_mining_chain, valid_transaction};
    use serial_test::serial;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;

    struct WrapperHarness {
        _dir: TempDir,
        _server: JoinHandle<Result<(), blvm_node::module::traits::ModuleError>>,
        api: NodeApiIpc,
        outpoint: OutPoint,
        mempool_tx_hash: Hash,
        wire_hash: Hash,
    }

    async fn wrapper_harness() -> WrapperHarness {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        setup_mining_chain(&storage, 16).unwrap();
        let wire_block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [14u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1_231_006_505 + 15 * 600,
                bits: 0x0f00ffff,
                nonce: 0,
            },
            transactions: vec![].into(),
        };
        let wire_height = 15u64;
        let wire_hash = storage.blocks().get_block_hash(&wire_block);
        storage.blocks().store_block(&wire_block).unwrap();
        storage
            .blocks()
            .store_height(wire_height, &wire_hash)
            .unwrap();
        let outpoint = OutPoint {
            hash: [0xcd; 32],
            index: 0,
        };
        storage
            .utxos()
            .add_utxo(
                &outpoint,
                &UTXO {
                    value: 50_000,
                    script_pubkey: vec![0x51].into(),
                    height: 0,
                    is_coinbase: false,
                },
            )
            .unwrap();
        let mempool = Arc::new(MempoolManager::new());
        let tx = valid_transaction();
        use blvm_protocol::block::calculate_tx_id;
        let mempool_tx_hash = calculate_tx_id(&tx);
        mempool.add_transaction(tx.clone()).unwrap();
        let tip = storage.chain().get_tip_hash().unwrap().unwrap_or([0u8; 32]);
        storage
            .transactions()
            .index_transaction(&tx, &tip, 15, 0)
            .unwrap();
        let network = Arc::new(NetworkManager::new("127.0.0.1:18460".parse().unwrap()));
        let mut node_api =
            NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), Some(network));
        let coordinator = Arc::new(tokio::sync::Mutex::new(SyncCoordinator::new()));
        coordinator.lock().await.start_sync().unwrap();
        node_api.set_sync_coordinator(coordinator);
        node_api.set_metrics_manager(Arc::new(MetricsManager::new()), "cov-wrap".to_string());
        node_api.set_event_manager(Arc::new(EventManager::new()), "cov-wrap".to_string());
        let module_dir = std::path::PathBuf::from("data/modules/cov-wrap");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("note.txt"), b"seed").unwrap();
        let node_api = Arc::new(node_api);

        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;

        let id = client.next_correlation_id();
        let _ = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::Handshake,
                payload: RequestPayload::Handshake {
                    module_id: "cov-wrap".to_string(),
                    module_name: "Wrapper".to_string(),
                    version: "0.0.1".to_string(),
                },
            })
            .await;

        let api = NodeApiIpc::new(Arc::new(Mutex::new(client)), "cov-wrap".into());

        WrapperHarness {
            _dir: temp_dir,
            _server: server,
            api,
            outpoint,
            mempool_tx_hash,
            wire_hash,
        }
    }

    #[tokio::test]
    async fn wrapper_chain_mempool_and_storage_queries() {
        let h = wrapper_harness().await;
        assert!(h.api.get_block_height().await.unwrap() >= 15);
        let tip = h.api.get_chain_tip().await.unwrap();
        assert_eq!(tip.len(), 32);
        let info = h.api.get_chain_info().await.unwrap();
        assert!(info.height >= 15);
        let size = h.api.get_mempool_size().await.unwrap();
        assert!(size.transaction_count >= 1);
        let sync = h.api.get_sync_status().await.unwrap();
        assert!(sync.progress > 0.0);
        assert!(h.api.has_transaction(&h.mempool_tx_hash).await.unwrap());
        assert!(h.api.get_utxo(&h.outpoint).await.unwrap().is_some());
        assert!(h
            .api
            .check_transaction_in_mempool(&h.mempool_tx_hash)
            .await
            .unwrap());
        assert!(h
            .api
            .get_transaction(&h.mempool_tx_hash)
            .await
            .unwrap()
            .is_some());
        assert!(h.api.get_block_by_height(15).await.unwrap().is_some());
        assert!(h
            .api
            .get_mempool_transaction(&h.mempool_tx_hash)
            .await
            .unwrap()
            .is_some());
        let fee = h.api.get_fee_estimate(6).await.unwrap();
        assert!(fee <= 1_000_000);
        let txs = h.api.get_mempool_transactions().await.unwrap();
        assert!(txs.contains(&h.mempool_tx_hash));
        assert!(h.api.get_block(&h.wire_hash).await.unwrap().is_some());
        assert!(h
            .api
            .get_block_header(&h.wire_hash)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrapper_network_lightning_and_denylist() {
        let h = wrapper_harness().await;
        let stats = h.api.get_network_stats().await.unwrap();
        assert_eq!(stats.peer_count, 0);
        let peers = h.api.get_network_peers().await.unwrap();
        assert!(peers.is_empty());
        assert!(h.api.get_lightning_node_url().await.unwrap().is_none());
        assert!(h.api.get_lightning_info().await.unwrap().is_none());

        let deny = [0x48u8; 32];
        h.api.merge_block_serve_denylist(&[deny]).await.unwrap();
        let snap = h.api.get_block_serve_denylist_snapshot().await.unwrap();
        assert!(snap.hashes.contains(&deny));
        h.api.clear_block_serve_denylist().await.unwrap();

        let tx_deny = [0x49u8; 32];
        h.api.merge_tx_serve_denylist(&[tx_deny]).await.unwrap();
        let tx_snap = h.api.get_tx_serve_denylist_snapshot().await.unwrap();
        assert!(tx_snap.hashes.contains(&tx_deny));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrapper_metrics_maintenance_and_queue() {
        let h = wrapper_harness().await;
        h.api
            .report_metric(Metric {
                name: "wrap".into(),
                value: MetricValue::Gauge(1.0),
                labels: HashMap::new(),
                timestamp: 0,
            })
            .await
            .unwrap();
        let metrics = h.api.get_module_metrics("cov-wrap").await.unwrap();
        assert_eq!(metrics.len(), 1);
        let all = h.api.get_all_metrics().await.unwrap();
        assert!(!all.is_empty());

        h.api.ban_peer("127.0.0.1:8333", Some(60)).await.unwrap();
        h.api.set_block_serve_maintenance_mode(true).await.unwrap();
        h.api.set_block_serve_maintenance_mode(false).await.unwrap();
        h.api
            .queue_received_block_bytes(vec![0x01, 0x02])
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrapper_module_and_health_paths() {
        use blvm_node::module::ipc::protocol::EventPayload;
        use blvm_node::module::process::monitor::ModuleHealth;
        use blvm_node::module::traits::EventType;

        let h = wrapper_harness().await;
        h.api
            .report_module_health(ModuleHealth::Healthy)
            .await
            .unwrap();
        h.api
            .publish_event(
                EventType::NewBlock,
                EventPayload::NewBlock {
                    block_hash: [0x43; 32],
                    height: 8,
                },
            )
            .await
            .unwrap();
        let sync = h.api.get_sync_status().await.unwrap();
        assert!(sync.progress > 0.0);
        assert!(h.api.discover_modules().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrapper_module_info_without_manager_errors() {
        let h = wrapper_harness().await;
        assert!(h.api.get_module_info("missing").await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrapper_denylist_replace_and_clear() {
        let h = wrapper_harness().await;
        let h1 = [0x51u8; 32];
        let h2 = [0x52u8; 32];
        h.api.merge_tx_serve_denylist(&[h1]).await.unwrap();
        h.api.replace_tx_serve_denylist(&[h2]).await.unwrap();
        let snap = h.api.get_tx_serve_denylist_snapshot().await.unwrap();
        assert!(snap.hashes.contains(&h2));
        h.api.clear_tx_serve_denylist().await.unwrap();
        h.api.replace_block_serve_denylist(&[h1]).await.unwrap();
        h.api.clear_block_serve_denylist().await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn wrapper_filesystem_round_trip() {
        blvm_node::module::api::node_api::NodeApiImpl::clear_current_module_id();
        let h = wrapper_harness().await;
        let read = h.api.read_file("note.txt".into()).await.unwrap();
        assert_eq!(read, b"seed");
        h.api
            .write_file("note.txt".into(), b"via-ipc".to_vec())
            .await
            .unwrap();
        let updated = h.api.read_file("note.txt".into()).await.unwrap();
        assert_eq!(updated, b"via-ipc");
        let entries = h.api.list_directory(String::new()).await.unwrap();
        assert!(entries.iter().any(|e| e.contains("note.txt")));
        let meta = h.api.get_file_metadata("note.txt".into()).await.unwrap();
        assert!(meta.size >= 7);
        h.api.delete_file("note.txt".into()).await.unwrap();
    }

    #[tokio::test]
    async fn wrapper_get_block_template_with_long_chain() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        common::setup_mining_chain(&storage, 2016).unwrap();
        let mempool = Arc::new(MempoolManager::new());
        let network = Arc::new(NetworkManager::new("127.0.0.1:18472".parse().unwrap()));
        let mut node_api =
            NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), Some(network));
        node_api.set_event_manager(Arc::new(EventManager::new()), "cov-wrap".into());
        let node_api = Arc::new(node_api);

        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;

        let id = client.next_correlation_id();
        let _ = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::Handshake,
                payload: RequestPayload::Handshake {
                    module_id: "cov-wrap".to_string(),
                    module_name: "Wrapper".to_string(),
                    version: "0.0.1".to_string(),
                },
            })
            .await;
        let api = NodeApiIpc::new(Arc::new(Mutex::new(client)), "cov-wrap".into());
        let template = api
            .get_block_template(vec!["segwit".into()], None, None)
            .await
            .unwrap();
        assert!(template.height >= 2015);
        server.abort();
    }
}

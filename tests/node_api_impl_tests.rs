//! Real `NodeApiImpl` backed by storage fixtures (not MockNodeAPI).

use blvm_node::module::api::NodeApiImpl;
use blvm_node::module::traits::NodeAPI;
use blvm_node::network::NetworkManager;
use blvm_node::node::mempool::MempoolManager;
use blvm_node::node::sync::SyncCoordinator;
use blvm_node::storage::Storage;
use blvm_node::{Block, BlockHeader, Hash, OutPoint};
use blvm_protocol::UTXO;
use std::net::SocketAddr;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{setup_mining_chain, valid_transaction};

fn storage_with_chain() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, 32).unwrap();
    (temp_dir, storage)
}

fn store_block_at_height(storage: &Storage, height: u64) -> Hash {
    let block = Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [height as u8; 32],
            merkle_root: [0u8; 32],
            timestamp: 1_231_006_505 + height,
            bits: 0x0f00ffff,
            nonce: 0,
        },
        transactions: vec![].into_boxed_slice(),
    };
    let hash = storage.blocks().get_block_hash(&block);
    storage.blocks().store_block(&block).unwrap();
    storage.blocks().store_height(height, &hash).unwrap();
    hash
}

#[tokio::test]
async fn test_node_api_get_chain_height_and_tip() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);

    let height = api.get_block_height().await.unwrap();
    assert_eq!(height, 31);

    let tip = api.get_chain_tip().await.unwrap();
    assert_ne!(tip, [0u8; 32]);
}

#[tokio::test]
async fn test_node_api_get_block_by_wire_hash() {
    let (_dir, storage) = storage_with_chain();
    let hash = store_block_at_height(&storage, 0);
    let api = NodeApiImpl::new(Arc::clone(&storage));

    let block = api.get_block(&hash).await.unwrap();
    assert!(block.is_some());

    let header = api.get_block_header(&hash).await.unwrap();
    assert!(header.is_some());
    assert_eq!(header.unwrap().version, 1);
}

#[tokio::test]
async fn test_node_api_get_chain_info() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);

    let info = api.get_chain_info().await.unwrap();
    assert_eq!(info.height, 31);
    assert_eq!(info.tip_hash, api.get_chain_tip().await.unwrap());
    assert!(info.is_synced);
}

#[tokio::test]
async fn test_node_api_mempool_queries() {
    let (_dir, storage) = storage_with_chain();
    let mempool = Arc::new(MempoolManager::new());
    mempool.add_transaction(valid_transaction()).unwrap();

    let api = NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), None);

    let txs = api.get_mempool_transactions().await.unwrap();
    assert_eq!(txs.len(), 1);

    let size = api.get_mempool_size().await.unwrap();
    assert_eq!(size.transaction_count, 1);

    assert!(api.check_transaction_in_mempool(&txs[0]).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_network_stats_and_peers() {
    let (_dir, storage) = storage_with_chain();
    let addr: SocketAddr = "127.0.0.1:18333".parse().unwrap();
    let network = Arc::new(NetworkManager::new(addr));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));

    let stats = api.get_network_stats().await.unwrap();
    assert_eq!(stats.peer_count, 0);

    let peers = api.get_network_peers().await.unwrap();
    assert!(peers.is_empty());
}

#[tokio::test]
async fn test_node_api_sync_status_via_coordinator() {
    let (_dir, storage) = storage_with_chain();
    let mut api = NodeApiImpl::new(storage);
    let coordinator = Arc::new(tokio::sync::Mutex::new(SyncCoordinator::new()));
    coordinator.lock().await.mark_chain_current();
    api.set_sync_coordinator(coordinator);

    let status = api.get_sync_status().await.unwrap();
    assert!(status.is_synced);
    assert_eq!(status.phase, "Synced");
}

#[tokio::test]
async fn test_node_api_utxo_lookup() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let outpoint = OutPoint {
        hash: [0xab; 32],
        index: 0,
    };
    let utxo = UTXO {
        value: 50_000,
        script_pubkey: vec![0x76, 0xa9].into(),
        height: 0,
        is_coinbase: false,
    };
    storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

    let api = NodeApiImpl::new(storage);
    let got = api.get_utxo(&outpoint).await.unwrap();
    assert!(got.is_some());
    assert_eq!(got.unwrap().value, 50_000);
}

#[tokio::test]
async fn test_node_api_has_transaction_false_when_missing() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    let missing: Hash = [0xff; 32];
    assert!(!api.has_transaction(&missing).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_fee_estimate_empty_mempool() {
    let (_dir, storage) = storage_with_chain();
    let mempool = Arc::new(MempoolManager::new());
    let api = NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), None);
    let fee = api.get_fee_estimate(6).await.unwrap();
    assert_eq!(fee, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_block_serve_denylist_round_trip() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18336".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    let hash: Hash = [0x99; 32];
    api.replace_tx_serve_denylist(&[hash]).await.unwrap();
    let snap = api.get_tx_serve_denylist_snapshot().await.unwrap();
    assert_eq!(snap.hashes.len(), 1);
    api.clear_tx_serve_denylist().await.unwrap();
    let cleared = api.get_tx_serve_denylist_snapshot().await.unwrap();
    assert!(cleared.hashes.is_empty());
}

#[tokio::test]
async fn test_node_api_get_block_by_height() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    store_block_at_height(&storage, 0);
    let api = NodeApiImpl::new(storage);

    let block = api.get_block_by_height(0).await.unwrap();
    assert!(block.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_get_mempool_transaction_by_hash() {
    let (_dir, storage) = storage_with_chain();
    let mempool = Arc::new(MempoolManager::new());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    mempool.add_transaction(tx).unwrap();
    let api = NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), None);
    let got = api.get_mempool_transaction(&tx_hash).await.unwrap();
    assert!(got.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_ban_peer_smoke() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18404".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    api.ban_peer("127.0.0.1:18405", Some(120)).await.unwrap();
}

#[tokio::test]
async fn test_node_api_has_transaction_true_when_indexed() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    storage
        .transactions()
        .index_transaction(&tx, &[0xbb; 32], 0, 0)
        .unwrap();
    let api = NodeApiImpl::new(storage);
    assert!(api.has_transaction(&tx_hash).await.unwrap());
}

#[tokio::test]
async fn test_node_api_module_filesystem_round_trip() {
    let temp_dir = TempDir::new().unwrap();
    let db_dir = temp_dir.path().join("db");
    let base_data = temp_dir.path().join("base");
    let module_data = base_data.join("cov-mod");
    std::fs::create_dir_all(&module_data).unwrap();
    let storage = Arc::new(Storage::new(&db_dir).unwrap());
    let api = NodeApiImpl::new(storage);
    api.initialize_module("cov-mod".into(), module_data.clone(), base_data)
        .await
        .unwrap();
    std::fs::write(module_data.join("note.txt"), b"seed").unwrap();
    NodeApiImpl::set_current_module_id("cov-mod".into());
    let read = api.read_file("note.txt".into()).await.unwrap();
    assert_eq!(read, b"seed");
    api.write_file("note.txt".into(), b"payload".to_vec())
        .await
        .unwrap();
    let updated = api.read_file("note.txt".into()).await.unwrap();
    assert_eq!(updated, b"payload");
    let entries = api.list_directory(String::new()).await.unwrap();
    assert!(entries.iter().any(|e| e.contains("note.txt")));
    NodeApiImpl::clear_current_module_id();
}

#[tokio::test]
async fn test_node_api_lightning_defaults_none() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.get_lightning_node_url().await.unwrap().is_none());
    assert!(api.get_lightning_info().await.unwrap().is_none());
}

#[tokio::test]
async fn test_node_api_get_transaction_from_index() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let tx = valid_transaction();
    use blvm_protocol::block::calculate_tx_id;
    let tx_hash = calculate_tx_id(&tx);
    storage
        .transactions()
        .index_transaction(&tx, &[0xcc; 32], 0, 0)
        .unwrap();
    let api = NodeApiImpl::new(storage);
    let got = api.get_transaction(&tx_hash).await.unwrap();
    assert!(got.is_some());
    assert_eq!(got.unwrap().version, 1);
}

fn storage_with_long_chain() -> (TempDir, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    setup_mining_chain(&storage, 2016).unwrap();
    (temp_dir, storage)
}

#[tokio::test]
async fn test_node_api_get_block_template_with_chain() {
    let (_dir, storage) = storage_with_long_chain();
    let mempool = Arc::new(MempoolManager::new());
    let api = NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), None);
    let template = api
        .get_block_template(vec!["segwit".into()], None, None)
        .await
        .unwrap();
    assert_eq!(template.coinbase_tx.version, 1);
    assert!(template.height >= 2015);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_merge_tx_serve_denylist() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18406".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    let hash = [0x11u8; 32];
    api.merge_tx_serve_denylist(&[hash]).await.unwrap();
    let snap = api.get_tx_serve_denylist_snapshot().await.unwrap();
    assert!(snap.hashes.contains(&hash));
}

#[tokio::test]
async fn test_node_api_get_sync_status_requires_coordinator() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.get_sync_status().await.is_err());
}

#[tokio::test]
async fn test_node_api_subscribe_events_without_manager() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    let _rx = api
        .subscribe_events(vec![blvm_node::module::traits::EventType::NewBlock])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_node_api_filesystem_delete_smoke() {
    let temp_dir = TempDir::new().unwrap();
    let db_dir = temp_dir.path().join("db");
    let base_data = temp_dir.path().join("base");
    let module_data = base_data.join("cov-mod");
    std::fs::create_dir_all(&module_data).unwrap();
    let storage = Arc::new(Storage::new(&db_dir).unwrap());
    let api = NodeApiImpl::new(storage);
    api.initialize_module("cov-mod".into(), module_data.clone(), base_data)
        .await
        .unwrap();
    std::fs::write(module_data.join("drop.me"), b"x").unwrap();
    NodeApiImpl::set_current_module_id("cov-mod".into());
    api.delete_file("drop.me".into()).await.unwrap();
    let entries = api.list_directory(String::new()).await.unwrap();
    assert!(!entries.iter().any(|e| e == "drop.me"));
    NodeApiImpl::clear_current_module_id();
}

#[tokio::test]
async fn test_node_api_report_metric_via_manager() {
    use blvm_node::module::metrics::manager::{Metric, MetricValue, MetricsManager};
    use std::collections::HashMap;

    let (_dir, storage) = storage_with_chain();
    let mut api = NodeApiImpl::new(storage);
    api.set_metrics_manager(Arc::new(MetricsManager::new()), "cov-mod".into());
    api.report_metric(Metric {
        name: "height".into(),
        value: MetricValue::Gauge(100.0),
        labels: HashMap::new(),
        timestamp: 0,
    })
    .await
    .unwrap();
    let metrics = api.get_module_metrics("cov-mod").await.unwrap();
    assert_eq!(metrics.len(), 1);
}

#[tokio::test]
async fn test_node_api_register_and_cancel_timer() {
    use async_trait::async_trait;
    use blvm_node::module::timers::manager::{TimerCallback, TimerManager};

    struct NoopTimer;

    #[async_trait]
    impl TimerCallback for NoopTimer {
        async fn call(&self) -> Result<(), String> {
            Ok(())
        }
    }

    let (_dir, storage) = storage_with_chain();
    let mut api = NodeApiImpl::new(storage);
    api.set_timer_manager(Arc::new(TimerManager::new()), "cov-mod".into());
    let id = api.register_timer(3600, Arc::new(NoopTimer)).await.unwrap();
    api.cancel_timer(id).await.unwrap();
}

#[tokio::test]
async fn test_node_api_discover_modules_requires_manager() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.discover_modules().await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_queue_received_block_bytes() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18407".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    api.queue_received_block_bytes(vec![0x01, 0x02, 0x03])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_block_serve_maintenance_mode() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18408".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    api.set_block_serve_maintenance_mode(true).await.unwrap();
    api.set_block_serve_maintenance_mode(false).await.unwrap();
}

#[tokio::test]
async fn test_node_api_get_file_metadata() {
    let temp_dir = TempDir::new().unwrap();
    let db_dir = temp_dir.path().join("db");
    let base_data = temp_dir.path().join("base");
    let module_data = base_data.join("cov-mod");
    std::fs::create_dir_all(&module_data).unwrap();
    std::fs::write(module_data.join("meta.txt"), b"12345").unwrap();
    let storage = Arc::new(Storage::new(&db_dir).unwrap());
    let api = NodeApiImpl::new(storage);
    api.initialize_module("cov-mod".into(), module_data, base_data)
        .await
        .unwrap();
    NodeApiImpl::set_current_module_id("cov-mod".into());
    let meta = api.get_file_metadata("meta.txt".into()).await.unwrap();
    assert_eq!(meta.size, 5);
    NodeApiImpl::clear_current_module_id();
}

#[tokio::test]
async fn test_node_api_create_directory_smoke() {
    let temp_dir = TempDir::new().unwrap();
    let db_dir = temp_dir.path().join("db");
    let base_data = temp_dir.path().join("base");
    let module_data = base_data.join("cov-mod");
    std::fs::create_dir_all(module_data.join("existing")).unwrap();
    let storage = Arc::new(Storage::new(&db_dir).unwrap());
    let api = NodeApiImpl::new(storage);
    api.initialize_module("cov-mod".into(), module_data, base_data)
        .await
        .unwrap();
    NodeApiImpl::set_current_module_id("cov-mod".into());
    api.create_directory("existing".into()).await.unwrap();
    let entries = api.list_directory(String::new()).await.unwrap();
    assert!(entries.iter().any(|e| e.contains("existing")));
    NodeApiImpl::clear_current_module_id();
}

#[tokio::test]
async fn test_node_api_schedule_task_smoke() {
    use async_trait::async_trait;
    use blvm_node::module::timers::manager::{TaskCallback, TimerManager};

    struct NoopTask;

    #[async_trait]
    impl TaskCallback for NoopTask {
        async fn call(&self) -> Result<(), String> {
            Ok(())
        }
    }

    let (_dir, storage) = storage_with_chain();
    let mut api = NodeApiImpl::new(storage);
    api.set_timer_manager(Arc::new(TimerManager::new()), "cov-mod".into());
    let _task_id = api.schedule_task(86_400, Arc::new(NoopTask)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_block_serve_denylist_clear_and_replace() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18409".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    let h1 = [0x22u8; 32];
    let h2 = [0x33u8; 32];
    api.merge_block_serve_denylist(&[h1]).await.unwrap();
    api.clear_block_serve_denylist().await.unwrap();
    let snap = api.get_block_serve_denylist_snapshot().await.unwrap();
    assert!(snap.hashes.is_empty());
    api.replace_block_serve_denylist(&[h2]).await.unwrap();
    let snap2 = api.get_block_serve_denylist_snapshot().await.unwrap();
    assert!(snap2.hashes.contains(&h2));
}

#[tokio::test]
async fn test_node_api_submit_block_exercises_mining_path() {
    let (_dir, storage) = storage_with_chain();
    let mempool = Arc::new(MempoolManager::new());
    let api = NodeApiImpl::with_dependencies(Arc::clone(&storage), None, None, Some(mempool), None);
    let hash = store_block_at_height(&storage, 31);
    let block = storage.blocks().get_block(&hash).unwrap().unwrap();
    // NodeAPI serializes with bincode; submitblock expects wire encoding — error path still
    // exercises MiningRpc wiring and mempool dependency checks.
    let err = api.submit_block(block).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("deserialize") || msg.contains("submit") || msg.contains("Serialization"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn test_node_api_get_payment_state_requires_machine() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.get_payment_state("cov-pay").await.is_err());
}

#[tokio::test]
async fn test_node_api_publish_event_with_manager() {
    use blvm_node::module::api::events::EventManager;
    use blvm_node::module::ipc::protocol::EventPayload;
    use blvm_node::module::traits::EventType;

    let (_dir, storage) = storage_with_chain();
    let mut api = NodeApiImpl::new(storage);
    api.set_event_manager(Arc::new(EventManager::new()), "cov-mod".into());
    api.publish_event(
        EventType::NewBlock,
        EventPayload::NewBlock {
            block_hash: [0x99; 32],
            height: 100,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn test_node_api_report_module_health_smoke() {
    use blvm_node::module::process::monitor::ModuleHealth;

    let (_dir, storage) = storage_with_chain();
    let mut api = NodeApiImpl::new(storage);
    api.set_current_module_id_for_api("cov-mod".into());
    api.report_module_health(ModuleHealth::Healthy)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_clear_tx_serve_denylist() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18411".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    let hash = [0x55u8; 32];
    api.merge_tx_serve_denylist(&[hash]).await.unwrap();
    api.clear_tx_serve_denylist().await.unwrap();
    let snap = api.get_tx_serve_denylist_snapshot().await.unwrap();
    assert!(snap.hashes.is_empty());
}

#[tokio::test]
async fn test_node_api_is_module_available_without_manager() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.is_module_available("missing-mod").await.is_err());
}

#[tokio::test]
async fn test_node_api_get_module_info_without_manager() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.get_module_info("missing-mod").await.is_err());
}

#[tokio::test]
async fn test_node_api_get_all_module_health_without_manager() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.get_all_module_health().await.is_err());
}

#[tokio::test]
async fn test_node_api_call_module_without_router() {
    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.call_module(None, "ping", vec![]).await.is_err());
}

#[tokio::test]
async fn test_node_api_register_module_api_without_registry() {
    use async_trait::async_trait;
    use blvm_node::module::inter_module::api::ModuleAPI;
    use blvm_node::module::traits::ModuleError;

    struct DummyApi;

    #[async_trait]
    impl ModuleAPI for DummyApi {
        async fn handle_request(
            &self,
            _method: &str,
            _params: &[u8],
            _caller_module_id: &str,
        ) -> Result<Vec<u8>, ModuleError> {
            Ok(vec![])
        }

        fn list_methods(&self) -> Vec<String> {
            vec![]
        }

        fn api_version(&self) -> u32 {
            1
        }
    }

    let (_dir, storage) = storage_with_chain();
    let api = NodeApiImpl::new(storage);
    assert!(api.register_module_api(Arc::new(DummyApi)).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_node_api_replace_tx_serve_denylist() {
    let (_dir, storage) = storage_with_chain();
    let network = Arc::new(NetworkManager::new("127.0.0.1:18412".parse().unwrap()));
    let api = NodeApiImpl::with_dependencies(storage, None, None, None, Some(network));
    let h1 = [0x77u8; 32];
    let h2 = [0x88u8; 32];
    api.merge_tx_serve_denylist(&[h1]).await.unwrap();
    api.replace_tx_serve_denylist(&[h2]).await.unwrap();
    let snap = api.get_tx_serve_denylist_snapshot().await.unwrap();
    assert!(!snap.hashes.contains(&h1));
    assert!(snap.hashes.contains(&h2));
}

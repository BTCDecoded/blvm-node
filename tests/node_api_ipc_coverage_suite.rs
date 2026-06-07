//! IPC round-trip against real `NodeApiImpl` (not MockNodeAPI).

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
    use blvm_node::module::api::NodeApiImpl;
    use blvm_node::module::ipc::protocol::{
        MessageType, RequestMessage, RequestPayload, ResponsePayload,
    };
    use blvm_node::module::metrics::manager::{Metric, MetricValue, MetricsManager};
    use blvm_node::module::traits::NodeAPI;
    use blvm_node::network::NetworkManager;
    use blvm_node::node::mempool::MempoolManager;
    use blvm_node::node::sync::SyncCoordinator;
    use blvm_node::storage::Storage;
    use blvm_node::{Block, BlockHeader, Hash, OutPoint, UTXO};
    use common::{setup_mining_chain, valid_transaction};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn node_api_fixture_full() -> (TempDir, Arc<NodeApiImpl>, OutPoint, Hash, Hash) {
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
        let network = Arc::new(NetworkManager::new("127.0.0.1:18440".parse().unwrap()));
        let mut api =
            NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), Some(network));
        let coordinator = Arc::new(tokio::sync::Mutex::new(SyncCoordinator::new()));
        coordinator.lock().await.start_sync().unwrap();
        api.set_sync_coordinator(coordinator);
        api.set_metrics_manager(Arc::new(MetricsManager::new()), "cov-test".to_string());
        api.set_event_manager(Arc::new(EventManager::new()), "cov-test".to_string());
        let module_dir = std::path::PathBuf::from("data/modules/cov-test");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("note.txt"), b"seed").unwrap();
        (
            temp_dir,
            Arc::new(api),
            outpoint,
            mempool_tx_hash,
            wire_hash,
        )
    }

    async fn node_api_fixture() -> (TempDir, Arc<NodeApiImpl>) {
        let (dir, api, _, _, _) = node_api_fixture_full().await;
        (dir, api)
    }

    async fn handshake(client: &mut blvm_node::module::ipc::client::ModuleIpcClient) {
        let id = client.next_correlation_id();
        let _ = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::Handshake,
                payload: RequestPayload::Handshake {
                    module_id: "cov-test".to_string(),
                    module_name: "Coverage".to_string(),
                    version: "0.0.1".to_string(),
                },
            })
            .await;
    }

    #[tokio::test]
    async fn ipc_get_block_height_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage::get_block_height(id))
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::U64(h)) => assert_eq!(h, 15),
            other => panic!("expected U64 height, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_chain_tip_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let tip = node_api.get_chain_tip().await.unwrap();
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage::get_chain_tip(id))
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::Hash(hash)) => assert_eq!(hash, tip),
            other => panic!("expected Hash tip, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_mempool_size_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetMempoolSize,
                payload: RequestPayload::GetMempoolSize,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::MempoolSize(size)) => assert_eq!(size.transaction_count, 1),
            other => panic!("expected MempoolSize, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_sync_status_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetSyncStatus,
                payload: RequestPayload::GetSyncStatus,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::NodeSyncStatus(status)) => {
                assert!(status.is_synced);
                assert_eq!(status.phase, "Synced");
            }
            other => panic!("expected NodeSyncStatus, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_chain_info_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetChainInfo,
                payload: RequestPayload::GetChainInfo,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::ChainInfo(info)) => {
                assert_eq!(info.height, 15);
                assert!(info.is_synced);
            }
            other => panic!("expected ChainInfo, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_has_transaction_round_trip() {
        let (_dir, node_api, _, tx_hash, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage::has_transaction(id, tx_hash))
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::Bool(true)) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_utxo_round_trip() {
        let (_dir, node_api, outpoint, _, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage::get_utxo(id, outpoint))
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::Utxo(Some(utxo))) => assert_eq!(utxo.value, 50_000),
            other => panic!("expected Utxo(Some), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_check_transaction_in_mempool_round_trip() {
        let (_dir, node_api, _, tx_hash, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::CheckTransactionInMempool,
                payload: RequestPayload::CheckTransactionInMempool { tx_hash },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::CheckTransactionInMempool(true)) => {}
            other => panic!("expected CheckTransactionInMempool(true), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_transaction_round_trip() {
        let (_dir, node_api, _, tx_hash, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage::get_transaction(id, tx_hash))
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::Transaction(Some(_))) => {}
            other => panic!("expected Transaction(Some), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_block_by_height_round_trip() {
        let (_dir, node_api, _, _, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetBlockByHeight,
                payload: RequestPayload::GetBlockByHeight { height: 15 },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::BlockByHeight(Some(_))) => {}
            other => panic!("expected BlockByHeight(Some), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_mempool_transaction_round_trip() {
        let (_dir, node_api, _, tx_hash, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetMempoolTransaction,
                payload: RequestPayload::GetMempoolTransaction { tx_hash },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::MempoolTransaction(Some(_))) => {}
            other => panic!("expected MempoolTransaction(Some), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_fee_estimate_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetFeeEstimate,
                payload: RequestPayload::GetFeeEstimate { target_blocks: 6 },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::FeeEstimate(_)) => {}
            other => panic!("expected FeeEstimate, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_block_and_header_round_trip() {
        let (_dir, node_api, _, _, wire_hash) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let block_resp = client
            .request(RequestMessage::get_block(id, wire_hash))
            .await
            .unwrap();
        assert!(block_resp.success);
        match block_resp.payload {
            Some(ResponsePayload::Block(Some(_))) => {}
            other => panic!("expected Block(Some), got {other:?}"),
        }

        let id = client.next_correlation_id();
        let header_resp = client
            .request(RequestMessage::get_block_header(id, wire_hash))
            .await
            .unwrap();
        assert!(header_resp.success);
        match header_resp.payload {
            Some(ResponsePayload::BlockHeader(Some(_))) => {}
            other => panic!("expected BlockHeader(Some), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_mempool_transactions_round_trip() {
        let (_dir, node_api, _, tx_hash, _) = node_api_fixture_full().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetMempoolTransactions,
                payload: RequestPayload::GetMempoolTransactions,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::MempoolTransactions(hashes)) => {
                assert_eq!(hashes.len(), 1);
                assert_eq!(hashes[0], tx_hash);
            }
            other => panic!("expected MempoolTransactions, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ipc_get_network_stats_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetNetworkStats,
                payload: RequestPayload::GetNetworkStats,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::NetworkStats(stats)) => assert_eq!(stats.peer_count, 0),
            other => panic!("expected NetworkStats, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_report_metric_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::ReportMetric,
                payload: RequestPayload::ReportMetric {
                    metric: Metric {
                        name: "test_counter".to_string(),
                        value: MetricValue::Counter(42),
                        labels: std::collections::HashMap::new(),
                        timestamp: 0,
                    },
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::MetricReported) => {}
            other => panic!("expected MetricReported, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_module_metrics_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetModuleMetrics,
                payload: RequestPayload::GetModuleMetrics {
                    module_id: "cov-test".to_string(),
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::ModuleMetrics(_)) => {}
            other => panic!("expected ModuleMetrics, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_all_metrics_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetAllMetrics,
                payload: RequestPayload::GetAllMetrics,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::AllMetrics(_)) => {}
            other => panic!("expected AllMetrics, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_network_peers_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetNetworkPeers,
                payload: RequestPayload::GetNetworkPeers,
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::NetworkPeers(peers)) => assert!(peers.is_empty()),
            other => panic!("expected NetworkPeers, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_get_lightning_defaults_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let url_id = client.next_correlation_id();
        let url_resp = client
            .request(RequestMessage {
                correlation_id: url_id,
                request_type: MessageType::GetLightningNodeUrl,
                payload: RequestPayload::GetLightningNodeUrl,
            })
            .await
            .unwrap();
        assert!(url_resp.success);
        match url_resp.payload {
            Some(ResponsePayload::LightningNodeUrl(None)) => {}
            other => panic!("expected LightningNodeUrl(None), got {other:?}"),
        }

        let info_id = client.next_correlation_id();
        let info_resp = client
            .request(RequestMessage {
                correlation_id: info_id,
                request_type: MessageType::GetLightningInfo,
                payload: RequestPayload::GetLightningInfo,
            })
            .await
            .unwrap();
        assert!(info_resp.success);
        match info_resp.payload {
            Some(ResponsePayload::LightningInfo(None)) => {}
            other => panic!("expected LightningInfo(None), got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_block_serve_denylist_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let deny_hash = [0x44u8; 32];
        let merge_id = client.next_correlation_id();
        let merge_resp = client
            .request(RequestMessage {
                correlation_id: merge_id,
                request_type: MessageType::MergeBlockServeDenylist,
                payload: RequestPayload::MergeBlockServeDenylist {
                    block_hashes: vec![deny_hash],
                },
            })
            .await
            .unwrap();
        assert!(merge_resp.success);

        let snap_id = client.next_correlation_id();
        let snap_resp = client
            .request(RequestMessage {
                correlation_id: snap_id,
                request_type: MessageType::GetBlockServeDenylistSnapshot,
                payload: RequestPayload::GetBlockServeDenylistSnapshot,
            })
            .await
            .unwrap();
        assert!(snap_resp.success);
        match snap_resp.payload {
            Some(ResponsePayload::BlockServeDenylistSnapshot(snap)) => {
                assert!(snap.hashes.contains(&deny_hash));
            }
            other => panic!("expected BlockServeDenylistSnapshot, got {other:?}"),
        }

        let clear_id = client.next_correlation_id();
        let clear_resp = client
            .request(RequestMessage {
                correlation_id: clear_id,
                request_type: MessageType::ClearBlockServeDenylist,
                payload: RequestPayload::ClearBlockServeDenylist,
            })
            .await
            .unwrap();
        assert!(clear_resp.success);
        server.abort();
    }

    #[tokio::test]
    async fn ipc_ban_peer_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::BanPeer,
                payload: RequestPayload::BanPeer {
                    peer_addr: "127.0.0.1:8333".into(),
                    ban_duration_seconds: Some(60),
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        server.abort();
    }

    #[tokio::test]
    async fn ipc_set_block_serve_maintenance_mode_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        for enabled in [true, false] {
            let id = client.next_correlation_id();
            let resp = client
                .request(RequestMessage {
                    correlation_id: id,
                    request_type: MessageType::SetBlockServeMaintenanceMode,
                    payload: RequestPayload::SetBlockServeMaintenanceMode { enabled },
                })
                .await
                .unwrap();
            assert!(resp.success);
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_queue_received_block_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::QueueReceivedBlock,
                payload: RequestPayload::QueueReceivedBlock {
                    block_bytes: vec![0x01, 0x02, 0x03],
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        server.abort();
    }

    async fn node_api_fixture_gbt() -> (TempDir, Arc<NodeApiImpl>) {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        setup_mining_chain(&storage, 2016).unwrap();
        let mempool = Arc::new(MempoolManager::new());
        let api = NodeApiImpl::with_dependencies(storage, None, None, Some(mempool), None);
        (temp_dir, Arc::new(api))
    }

    #[tokio::test]
    async fn ipc_get_block_template_round_trip() {
        let (_dir, node_api) = node_api_fixture_gbt().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetBlockTemplate,
                payload: RequestPayload::GetBlockTemplate {
                    rules: vec!["segwit".into()],
                    coinbase_script: None,
                    coinbase_address: None,
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::BlockTemplate(t)) => assert!(t.height >= 2015),
            other => panic!("expected BlockTemplate, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_tx_serve_denylist_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let tx_hash = [0x66u8; 32];
        let merge_id = client.next_correlation_id();
        let merge_resp = client
            .request(RequestMessage {
                correlation_id: merge_id,
                request_type: MessageType::MergeTxServeDenylist,
                payload: RequestPayload::MergeTxServeDenylist {
                    tx_hashes: vec![tx_hash],
                },
            })
            .await
            .unwrap();
        assert!(merge_resp.success);

        let snap_id = client.next_correlation_id();
        let snap_resp = client
            .request(RequestMessage {
                correlation_id: snap_id,
                request_type: MessageType::GetTxServeDenylistSnapshot,
                payload: RequestPayload::GetTxServeDenylistSnapshot,
            })
            .await
            .unwrap();
        assert!(resp_success_with_tx_denylist(&snap_resp, &tx_hash));

        let replace_id = client.next_correlation_id();
        let replace_hash = [0x77u8; 32];
        let replace_resp = client
            .request(RequestMessage {
                correlation_id: replace_id,
                request_type: MessageType::ReplaceTxServeDenylist,
                payload: RequestPayload::ReplaceTxServeDenylist {
                    tx_hashes: vec![replace_hash],
                },
            })
            .await
            .unwrap();
        assert!(replace_resp.success);

        let clear_id = client.next_correlation_id();
        let clear_resp = client
            .request(RequestMessage {
                correlation_id: clear_id,
                request_type: MessageType::ClearTxServeDenylist,
                payload: RequestPayload::ClearTxServeDenylist,
            })
            .await
            .unwrap();
        assert!(clear_resp.success);
        server.abort();
    }

    fn resp_success_with_tx_denylist(
        resp: &blvm_node::module::ipc::protocol::ResponseMessage,
        expected: &Hash,
    ) -> bool {
        if !resp.success {
            return false;
        }
        match &resp.payload {
            Some(ResponsePayload::TxServeDenylistSnapshot(snap)) => snap.hashes.contains(expected),
            _ => false,
        }
    }

    #[tokio::test]
    async fn ipc_get_payment_state_without_machine_errors() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let result = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::GetPaymentState,
                payload: RequestPayload::GetPaymentState {
                    payment_id: "cov-pay".into(),
                },
            })
            .await;
        match result {
            Ok(resp) => assert!(!resp.success),
            Err(_) => {
                // Handler error currently breaks the IPC connection without an error frame.
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_report_module_health_round_trip() {
        use blvm_node::module::process::monitor::ModuleHealth;

        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::ReportModuleHealth,
                payload: RequestPayload::ReportModuleHealth {
                    health: ModuleHealth::Healthy,
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::HealthReported) => {}
            other => panic!("expected HealthReported, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_publish_event_round_trip() {
        use blvm_node::module::ipc::protocol::EventPayload;
        use blvm_node::module::traits::EventType;

        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::PublishEvent,
                payload: RequestPayload::PublishEvent {
                    event_type: EventType::NewBlock,
                    payload: EventPayload::NewBlock {
                        block_hash: [0x42; 32],
                        height: 7,
                    },
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        match resp.payload {
            Some(ResponsePayload::EventPublished) => {}
            other => panic!("expected EventPublished, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_discover_modules_without_manager_errors() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::DiscoverModules,
                payload: RequestPayload::DiscoverModules,
            })
            .await;
        match resp {
            Ok(r) => assert!(!r.success),
            Err(_) => {}
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_is_module_available_without_manager_errors() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::IsModuleAvailable,
                payload: RequestPayload::IsModuleAvailable {
                    module_id: "missing-mod".into(),
                },
            })
            .await;
        match resp {
            Ok(r) => assert!(!r.success),
            Err(_) => {}
        }
        server.abort();
    }

    #[tokio::test]
    async fn ipc_replace_block_denylist_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let hash = [0x59u8; 32];
        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::ReplaceBlockServeDenylist,
                payload: RequestPayload::ReplaceBlockServeDenylist {
                    block_hashes: vec![hash],
                },
            })
            .await
            .unwrap();
        assert!(resp.success);
        server.abort();
    }

    #[tokio::test]
    async fn ipc_read_file_round_trip() {
        let (_dir, node_api) = node_api_fixture().await;
        let (_td, socket_path) = ipc_harness::setup_ipc_socket();
        let mut server = ipc_harness::spawn_ipc_server(socket_path.clone(), node_api);
        let mut client = ipc_harness::wait_bound_then_connect(&socket_path, &mut server).await;
        handshake(&mut client).await;

        let id = client.next_correlation_id();
        let resp = client
            .request(RequestMessage {
                correlation_id: id,
                request_type: MessageType::ReadFile,
                payload: RequestPayload::ReadFile {
                    path: "note.txt".into(),
                },
            })
            .await
            .unwrap();
        assert!(resp.success, "read failed: {:?}", resp.error);
        match resp.payload {
            Some(ResponsePayload::FileData(data)) => assert_eq!(data, b"seed"),
            other => panic!("expected FileData, got {other:?}"),
        }
        server.abort();
    }
}

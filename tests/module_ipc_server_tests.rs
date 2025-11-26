//! Tests for IPC server (node-side communication handling)

#[cfg(unix)]
mod tests {
    use bllvm_node::module::api::events::EventManager;
    use bllvm_node::module::ipc::client::ModuleIpcClient;
    use bllvm_node::module::ipc::protocol::{RequestMessage, ResponsePayload};
    use bllvm_node::module::ipc::server::ModuleIpcServer;
    use bllvm_node::module::traits::{EventType, ModuleError, NodeAPI};
    use bllvm_node::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration};

    // Mock NodeAPI that returns test data
    struct MockNodeAPI {
        test_block: Option<Block>,
        test_tx: Option<Transaction>,
        test_hash: Hash,
        test_height: u64,
    }

    impl MockNodeAPI {
        fn new() -> Self {
            Self {
                test_block: None,
                test_tx: None,
                test_hash: [0xaa; 32],
                test_height: 100,
            }
        }
    }

    #[async_trait::async_trait]
    impl NodeAPI for MockNodeAPI {
        async fn get_block(&self, hash: &Hash) -> Result<Option<Block>, ModuleError> {
            if *hash == self.test_hash {
                Ok(self.test_block.clone())
            } else {
                Ok(None)
            }
        }

        async fn get_block_header(&self, hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
            if *hash == self.test_hash {
                Ok(Some(BlockHeader {
                    version: 1,
                    prev_block_hash: [0u8; 32],
                    merkle_root: [0u8; 32],
                    timestamp: 1231006505,
                    bits: 0x1d00ffff,
                    nonce: 0,
                }))
            } else {
                Ok(None)
            }
        }

        async fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
            if *hash == self.test_hash {
                Ok(self.test_tx.clone())
            } else {
                Ok(None)
            }
        }

        async fn has_transaction(&self, hash: &Hash) -> Result<bool, ModuleError> {
            Ok(*hash == self.test_hash)
        }

        async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
            Ok(self.test_hash)
        }

        async fn get_block_height(&self) -> Result<u64, ModuleError> {
            Ok(self.test_height)
        }

        async fn get_utxo(&self, _outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
            Ok(Some(UTXO {
                value: 1000,
                script_pubkey: vec![0x51], // OP_1
                height: 0,
            }))
        }

        async fn subscribe_events(
            &self,
            _event_types: Vec<EventType>,
        ) -> Result<tokio::sync::mpsc::Receiver<bllvm_node::module::ipc::protocol::ModuleMessage>, ModuleError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(100);
            Ok(rx)
        }
    }

    fn setup_test_socket() -> (TempDir, std::path::PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");
        (temp_dir, socket_path)
    }

    #[tokio::test]
    async fn test_server_handshake_required() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Send handshake
        let correlation_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test-module".to_string(),
                module_name: "Test Module".to_string(),
                version: "1.0.0".to_string(),
            },
        };

        let response = client.request(handshake).await.unwrap();
        assert!(response.success);
        assert_eq!(response.correlation_id, correlation_id);

        match response.payload {
            Some(ResponsePayload::HandshakeAck { node_version }) => {
                assert!(!node_version.is_empty());
            }
            _ => panic!("Expected HandshakeAck"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_get_block_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake first
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Request block
        let hash: Hash = [0xaa; 32];
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_block(correlation_id, hash);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        assert_eq!(response.correlation_id, correlation_id);
        match response.payload {
            Some(ResponsePayload::Block(_)) => {}
            _ => panic!("Expected Block payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_get_block_header_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Request block header
        let hash: Hash = [0xaa; 32];
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_block_header(correlation_id, hash);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::BlockHeader(_)) => {}
            _ => panic!("Expected BlockHeader payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_get_transaction_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Request transaction
        let hash: Hash = [0xaa; 32];
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_transaction(correlation_id, hash);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::Transaction(_)) => {}
            _ => panic!("Expected Transaction payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_has_transaction_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Check if transaction exists
        let hash: Hash = [0xaa; 32];
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::has_transaction(correlation_id, hash);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::Bool(true)) => {}
            _ => panic!("Expected Bool(true) payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_get_chain_tip_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Get chain tip
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_chain_tip(correlation_id);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::Hash(hash)) => {
                assert_eq!(hash, [0xaa; 32]);
            }
            _ => panic!("Expected Hash payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_get_block_height_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Get block height
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_block_height(correlation_id);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::U64(height)) => {
                assert_eq!(height, 100);
            }
            _ => panic!("Expected U64 payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_get_utxo_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Get UTXO
        let hash: Hash = [0xbb; 32];
        let outpoint = OutPoint {
            hash,
            index: 0,
        };
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_utxo(correlation_id, outpoint);
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::Utxo(Some(utxo))) => {
                assert_eq!(utxo.value, 1000);
            }
            _ => panic!("Expected Utxo payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_subscribe_events_request() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());
        let event_manager = Arc::new(EventManager::new());

        let server_path = socket_path.clone();
        let event_mgr_clone = Arc::clone(&event_manager);
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path)
                .with_event_manager(event_mgr_clone);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Subscribe to events
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::subscribe_events(
            correlation_id,
            vec![EventType::NewBlock, EventType::NewTransaction],
        );
        let response = client.request(request).await.unwrap();

        assert!(response.success);
        match response.payload {
            Some(ResponsePayload::SubscribeAck) => {}
            _ => panic!("Expected SubscribeAck payload"),
        }

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_server_multiple_requests() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI::new());

        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: bllvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: bllvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Send multiple requests
        let hash: Hash = [0xaa; 32];
        let requests = vec![
            RequestMessage::get_chain_tip(client.next_correlation_id()),
            RequestMessage::get_block_height(client.next_correlation_id()),
            RequestMessage::has_transaction(client.next_correlation_id(), hash),
        ];

        for request in requests {
            let response = client.request(request).await.unwrap();
            assert!(response.success);
        }

        server_handle.abort();
    }
}


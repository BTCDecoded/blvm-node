//! Tests for IPC client (module-side communication)

#[cfg(unix)]
mod tests {
    use bllvm_node::module::ipc::client::ModuleIpcClient;
    use bllvm_node::module::ipc::protocol::{RequestMessage, ResponseMessage};
    use bllvm_node::module::ipc::server::ModuleIpcServer;
    use bllvm_node::module::traits::{EventType, ModuleError, NodeAPI};
    use bllvm_node::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration};

    // Mock NodeAPI for testing
    struct MockNodeAPI;

    #[async_trait::async_trait]
    impl NodeAPI for MockNodeAPI {
        async fn get_block(&self, _hash: &Hash) -> Result<Option<Block>, ModuleError> {
            Ok(None)
        }

        async fn get_block_header(&self, _hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
            Ok(None)
        }

        async fn get_transaction(&self, _hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
            Ok(None)
        }

        async fn has_transaction(&self, _hash: &Hash) -> Result<bool, ModuleError> {
            Ok(false)
        }

        async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
            Ok([0u8; 32])
        }

        async fn get_block_height(&self) -> Result<u64, ModuleError> {
            Ok(0)
        }

        async fn get_utxo(&self, _outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
            Ok(None)
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
    async fn test_client_connection_failure() {
        let (_temp_dir, socket_path) = setup_test_socket();

        // Try to connect to non-existent socket
        let result = ModuleIpcClient::connect(&socket_path).await;
        assert!(result.is_err());
        if let Err(ModuleError::IpcError(_)) = result {
            // Expected error type
        } else {
            panic!("Expected IpcError");
        }
    }

    #[tokio::test]
    async fn test_client_handshake() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI);

        // Start server in background
        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        // Wait for server to start
        sleep(Duration::from_millis(100)).await;

        // Connect client
        let mut client = match ModuleIpcClient::connect(&socket_path).await {
            Ok(c) => c,
            Err(_) => {
                // Server might not be ready yet, wait a bit more
                sleep(Duration::from_millis(200)).await;
                ModuleIpcClient::connect(&socket_path).await.unwrap()
            }
        };

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

        // Cleanup
        server_handle.abort();
    }

    #[tokio::test]
    async fn test_client_correlation_id_increment() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI);

        // Start server
        let server_path = socket_path.clone();
        let server_handle = tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            let _ = server.start(node_api).await;
        });

        sleep(Duration::from_millis(100)).await;

        let mut client = ModuleIpcClient::connect(&socket_path).await.unwrap();

        // Send handshake first
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

        // Test correlation ID increment
        let id1 = client.next_correlation_id();
        let id2 = client.next_correlation_id();
        assert_eq!(id2, id1.wrapping_add(1));

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_client_request_response_matching() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI);

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

        // Send a request and verify correlation ID matches
        let hash: Hash = [0xab; 32];
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_block(correlation_id, hash);
        let response = client.request(request).await.unwrap();

        assert_eq!(response.correlation_id, correlation_id);
        assert!(response.success);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_client_receive_event_timeout() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI);

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

        // Try to receive event (should timeout and return None)
        let event = client.receive_event().await.unwrap();
        assert!(event.is_none());

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_client_connection_closed_error() {
        let (_temp_dir, socket_path) = setup_test_socket();
        let node_api = Arc::new(MockNodeAPI);

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

        // Close server
        server_handle.abort();
        sleep(Duration::from_millis(100)).await;

        // Try to send request (should fail)
        let hash: Hash = [0xcd; 32];
        let correlation_id = client.next_correlation_id();
        let request = RequestMessage::get_block(correlation_id, hash);
        let result = client.request(request).await;
        assert!(result.is_err());
    }
}


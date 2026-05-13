//! Tests for IPC client (module-side communication)

#[cfg(unix)]
#[path = "stub_node_api.rs"]
mod stub_node_api;

#[cfg(unix)]
mod tests {
    use blvm_node::module::ipc::client::ModuleIpcClient;
    use blvm_node::module::ipc::protocol::RequestMessage;
    use blvm_node::module::ipc::server::ModuleIpcServer;
    use blvm_node::module::traits::ModuleError;
    use blvm_node::Hash;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, Duration, Instant};

    use super::stub_node_api::MockNodeAPI;

    fn setup_test_socket() -> (TempDir, std::path::PathBuf) {
        // Linux `sockaddr_un` paths are short (~108 bytes). CI sets a long TMPDIR/CARGO_TARGET_DIR;
        // `TempDir::new()` there can make Unix socket bind fail with no obvious client error.
        let tmp = std::path::Path::new("/tmp");
        let temp_dir = TempDir::new_in(tmp).expect("temp dir under /tmp");
        let socket_path = temp_dir.path().join("s.sock");
        (temp_dir, socket_path)
    }

    fn spawn_server(
        server_path: std::path::PathBuf,
        node_api: Arc<MockNodeAPI>,
    ) -> JoinHandle<Result<(), ModuleError>> {
        tokio::spawn(async move {
            let mut server = ModuleIpcServer::new(&server_path);
            server.start(node_api).await
        })
    }

    /// Wait until `bind` created the socket file, then connect. If the server task exits first,
    /// surface `ModuleIpcServer::start` errors instead of retrying connect until timeout.
    async fn wait_bound_then_connect(
        socket_path: &Path,
        server: &mut JoinHandle<Result<(), ModuleError>>,
    ) -> ModuleIpcClient {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if socket_path.exists() {
                return ModuleIpcClient::connect(socket_path)
                    .await
                    .unwrap_or_else(|e| {
                        panic!("connect to {socket_path:?} after bind failed: {e:?}");
                    });
            }
            if server.is_finished() {
                match server.await {
                    Ok(Ok(())) => panic!("server returned before binding (unexpected)"),
                    Ok(Err(e)) => panic!("IPC server failed to start: {e:?}"),
                    Err(j) => panic!("IPC server task panicked: {j}"),
                }
            }
            if Instant::now() > deadline {
                panic!(
                    "timeout waiting for socket file {socket_path:?} (path.len={}, dir={:?})",
                    socket_path.as_os_str().len(),
                    socket_path.parent()
                );
            }
            sleep(Duration::from_millis(20)).await;
        }
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

        let mut server_handle = spawn_server(socket_path.clone(), node_api);

        let mut client = wait_bound_then_connect(&socket_path, &mut server_handle).await;

        // Send handshake
        let correlation_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id,
            request_type: blvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: blvm_node::module::ipc::protocol::RequestPayload::Handshake {
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

        let mut server_handle = spawn_server(socket_path.clone(), node_api);

        let mut client = wait_bound_then_connect(&socket_path, &mut server_handle).await;

        // Send handshake first
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: blvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: blvm_node::module::ipc::protocol::RequestPayload::Handshake {
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

        let mut server_handle = spawn_server(socket_path.clone(), node_api);

        let mut client = wait_bound_then_connect(&socket_path, &mut server_handle).await;

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: blvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: blvm_node::module::ipc::protocol::RequestPayload::Handshake {
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

        let mut server_handle = spawn_server(socket_path.clone(), node_api);

        let mut client = wait_bound_then_connect(&socket_path, &mut server_handle).await;

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: blvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: blvm_node::module::ipc::protocol::RequestPayload::Handshake {
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

        let mut server_handle = spawn_server(socket_path.clone(), node_api);

        let mut client = wait_bound_then_connect(&socket_path, &mut server_handle).await;

        // Handshake
        let handshake_id = client.next_correlation_id();
        let handshake = RequestMessage {
            correlation_id: handshake_id,
            request_type: blvm_node::module::ipc::protocol::MessageType::Handshake,
            payload: blvm_node::module::ipc::protocol::RequestPayload::Handshake {
                module_id: "test".to_string(),
                module_name: "Test".to_string(),
                version: "1.0.0".to_string(),
            },
        };
        let _ = client.request(handshake).await;

        // Close server — peer teardown can lag behind `abort` on CI.
        server_handle.abort();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let hash: Hash = [0xcd; 32];
                let correlation_id = client.next_correlation_id();
                let request = RequestMessage::get_block(correlation_id, hash);
                if client.request(request).await.is_err() {
                    return;
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("request should fail after server task aborted");
    }
}

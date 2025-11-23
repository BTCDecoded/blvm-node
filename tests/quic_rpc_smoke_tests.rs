#![cfg(feature = "quinn")]

use bllvm_node::rpc::quinn_server::QuinnRpcServer;
use std::net::SocketAddr;

#[tokio::test]
#[ignore]
async fn quic_rpc_getblockchaininfo_smoke() {
    // Use a fixed port for the smoke test
    let quinn_addr: SocketAddr = "127.0.0.1:18332".parse().unwrap();

    // Start QUIC RPC server in background
    let server = QuinnRpcServer::new(quinn_addr);
    let _server_handle = tokio::spawn(async move {
        let _ = server.start().await;
    });

    // Allow server to start
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Create QUIC client endpoint
    let client_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let endpoint = quinn::Endpoint::client(client_addr).expect("client endpoint");

    // Connect to server (using "localhost" to match self-signed cert CN)
    let connection = endpoint
        .connect(quinn_addr, "localhost")
        .expect("connect")
        .await
        .expect("connection");

    // Open bidirectional stream
    let (mut send, mut recv) = connection.open_bi().await.expect("open_bi");

    // Send JSON-RPC request
    let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}"#;
    send.write_all(request.as_bytes()).await.expect("write");
    send.finish().expect("finish");

    // Read response (quinn 0.10: read() returns Result<Option<usize>, ReadError>)
    let mut response_bytes = Vec::new();
    let mut temp_buf = [0u8; 4096];
    loop {
        match recv.read(&mut temp_buf).await {
            Ok(Some(0)) | Ok(None) => break,
            Ok(Some(n)) => response_bytes.extend_from_slice(&temp_buf[..n]),
            Err(e) => {
                panic!("Error reading from QUIC stream: {}", e);
            }
        }
    }

    let response_str = String::from_utf8(response_bytes).expect("utf8");
    let v: serde_json::Value = serde_json::from_str(&response_str).expect("json");

    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert!(v["result"].is_object());
}

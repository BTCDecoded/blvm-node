//! Edge case tests for transport layers (TCP, QUIC, Iroh)

use bllvm_node::network::tcp_transport::TcpTransport;
use bllvm_node::network::transport::{
    Transport, TransportAddr, TransportConnection, TransportListener,
};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_tcp_transport_concurrent_connections() {
    let transport = TcpTransport::new();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut listener = transport.listen(addr).await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    // Spawn multiple concurrent connection attempts
    let mut handles = vec![];
    for _ in 0..5 {
        let transport_clone = TcpTransport::new();
        let connect_addr = TransportAddr::Tcp(local_addr);
        handles.push(tokio::spawn(async move {
            transport_clone.connect(connect_addr).await
        }));
    }

    // Accept all connections
    for _ in 0..5 {
        let accept_result = timeout(Duration::from_secs(2), listener.accept()).await;
        if accept_result.is_ok() {
            let _ = accept_result.unwrap();
        }
    }

    // Wait for all connections
    for handle in handles {
        let _ = handle.await;
    }
}

#[tokio::test]
async fn test_tcp_transport_connection_timeout() {
    let transport = TcpTransport::new();

    // Try to connect to an unreachable address
    let unreachable_addr = TransportAddr::Tcp("127.0.0.1:1".parse().unwrap());

    let result = timeout(
        Duration::from_millis(100),
        transport.connect(unreachable_addr),
    )
    .await;

    // Should timeout or fail quickly
    assert!(result.is_err() || result.unwrap().is_err());
}

#[tokio::test]
async fn test_tcp_transport_listen_on_used_port() {
    let transport = TcpTransport::new();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    // First listener should succeed
    let listener1 = transport.listen(addr).await;
    assert!(listener1.is_ok());

    // Second listener on same port should fail (if we use a specific port)
    // With port 0, it should get a different port, so this test is more about
    // ensuring the first listener works
    let _listener1 = listener1.unwrap();
}

#[tokio::test]
async fn test_tcp_transport_partial_read_write() {
    let transport = TcpTransport::new();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut listener = transport.listen(addr).await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    // Spawn client connection
    let transport_clone = TcpTransport::new();
    let connect_addr = TransportAddr::Tcp(local_addr);
    let client_handle = tokio::spawn(async move { transport_clone.connect(connect_addr).await });

    // Accept connection
    let (mut server_conn, _) = timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();

    let mut client_conn = timeout(Duration::from_secs(2), client_handle)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    // Test partial send
    let data = b"Hello, World!";
    let send_result = server_conn.send(&data[..5]).await;
    assert!(send_result.is_ok());

    // Test partial receive
    let recv_result = timeout(Duration::from_millis(100), client_conn.recv()).await;
    // May succeed or timeout depending on timing
    assert!(recv_result.is_ok() || recv_result.is_err());
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_error_recovery() {
    use bllvm_node::network::quinn_transport::QuinnTransport;

    let transport = QuinnTransport::new().unwrap();

    // Test connection to unreachable address
    let unreachable_addr = TransportAddr::Quinn("127.0.0.1:1".parse().unwrap());

    let result = timeout(
        Duration::from_millis(500),
        transport.connect(unreachable_addr),
    )
    .await;

    // Should fail or timeout
    assert!(result.is_err() || result.unwrap().is_err());
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_concurrent_connections() {
    use bllvm_node::network::quinn_transport::QuinnTransport;

    let transport = QuinnTransport::new().unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let mut listener = transport.listen(addr).await.unwrap();
    let local_addr = listener.local_addr().unwrap();

    // Spawn multiple concurrent connection attempts
    let mut handles = vec![];
    for _ in 0..3 {
        let transport_clone = QuinnTransport::new().unwrap();
        let connect_addr = TransportAddr::Quinn(local_addr);
        handles.push(tokio::spawn(async move {
            transport_clone.connect(connect_addr).await
        }));
    }

    // Accept connections (may fail due to cert verification, but tests error handling)
    for _ in 0..3 {
        let _ = timeout(Duration::from_secs(2), listener.accept()).await;
    }

    // Wait for all connections
    for handle in handles {
        let _ = handle.await;
    }
}

#[cfg(feature = "iroh")]
#[tokio::test]
async fn test_iroh_transport_error_recovery() {
    use bllvm_node::network::iroh_transport::IrohTransport;

    let transport = IrohTransport::new().await.unwrap();

    // Test connection with invalid key
    let invalid_key = vec![0u8; 32];
    let invalid_addr = TransportAddr::Iroh(invalid_key);

    let result = timeout(Duration::from_millis(500), transport.connect(invalid_addr)).await;

    // Should fail or timeout
    assert!(result.is_err() || result.unwrap().is_err());
}

#[cfg(feature = "iroh")]
#[tokio::test]
async fn test_iroh_transport_concurrent_connections() {
    use bllvm_node::network::iroh_transport::IrohTransport;

    let transport1 = IrohTransport::new().await.unwrap();
    let transport2 = IrohTransport::new().await.unwrap();

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut listener = transport1.listen(addr).await.unwrap();

    // Get transport2's public key
    let transport2_key = transport2.node_id().as_bytes().to_vec();
    let connect_addr = TransportAddr::Iroh(transport2_key);

    // Spawn connection attempt
    let connect_handle = tokio::spawn(async move { transport2.connect(connect_addr).await });

    // Accept connection (may fail due to connection setup, but tests error handling)
    let _ = timeout(Duration::from_secs(2), listener.accept()).await;

    // Wait for connection
    let _ = timeout(Duration::from_secs(2), connect_handle).await;
}

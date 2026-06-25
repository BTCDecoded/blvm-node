//! Tests for Iroh transport

#[cfg(feature = "iroh")]
mod tests {
    use blvm_node::network::iroh_transport::IrohTransport;
    use blvm_node::network::transport::{Transport, TransportAddr, TransportListener};
    use std::net::SocketAddr;

    #[tokio::test]
    async fn test_iroh_transport_new() {
        let transport = IrohTransport::new().await;
        assert!(transport.is_ok());

        let transport = transport.unwrap();
        assert_eq!(
            transport.transport_type(),
            blvm_node::network::transport::TransportType::Iroh
        );
    }

    #[tokio::test]
    async fn test_iroh_transport_node_id() {
        let transport = IrohTransport::new().await.unwrap();
        let _node_id = transport.node_id();

        // Node ID should be a valid endpoint ID
    }

    #[tokio::test]
    async fn test_iroh_transport_secret_key() {
        let transport = IrohTransport::new().await.unwrap();
        let _secret_key = transport.secret_key();

        // Secret key should be available
    }

    #[tokio::test]
    async fn test_iroh_transport_listen() {
        let transport = IrohTransport::new().await.unwrap();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let listener = transport.listen(addr).await;
        assert!(listener.is_ok());

        let listener = listener.unwrap();
        let local_addr = listener.local_addr();
        assert!(local_addr.is_ok());
    }

    #[tokio::test]
    async fn test_iroh_transport_connect_invalid_addr() {
        let transport = IrohTransport::new().await.unwrap();

        // Try to connect with invalid address type
        let invalid_addr = TransportAddr::Tcp("127.0.0.1:8080".parse().unwrap());
        let result = transport.connect(invalid_addr).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_iroh_transport_connect_invalid_key_length() {
        let transport = IrohTransport::new().await.unwrap();

        // Try to connect with invalid key length
        let invalid_key = vec![0u8; 16]; // Should be 32 bytes
        let invalid_addr = TransportAddr::Iroh(invalid_key);
        let result = transport.connect(invalid_addr).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_iroh_connection_multi_stream_roundtrip() {
        use blvm_node::network::transport::{Transport, TransportConnection, TransportListener};
        use std::time::Duration;
        use tokio::time::timeout;

        let server_transport = IrohTransport::new().await.unwrap();
        let client_transport = IrohTransport::new().await.unwrap();
        let server_id = server_transport.node_id();
        let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = server_transport.listen(listen_addr).await.unwrap();
        let direct_addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            let (mut server_conn, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            for _ in 0..3 {
                received.push(server_conn.recv().await.unwrap());
            }
            received
        });

        let mut client_conn = timeout(
            Duration::from_secs(10),
            client_transport.connect_direct(server_id, direct_addr),
        )
        .await
        .unwrap()
        .unwrap();
        for i in 0..3u8 {
            let payload = vec![i; 32];
            client_conn.send(&payload).await.unwrap();
        }

        let received = timeout(Duration::from_secs(10), server_handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.len(), 3);
        assert_eq!(received[0], vec![0u8; 32]);
        assert_eq!(received[2], vec![2u8; 32]);
    }
}

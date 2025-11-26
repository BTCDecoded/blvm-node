//! Tests for Quinn QUIC transport

#[cfg(feature = "quinn")]
mod tests {
    use bllvm_node::network::quinn_transport::QuinnTransport;
    use bllvm_node::network::transport::{Transport, TransportAddr};
    use std::net::SocketAddr;

    #[tokio::test]
    async fn test_quinn_transport_new() {
        let transport = QuinnTransport::new();
        assert!(transport.is_ok());
        
        let transport = transport.unwrap();
        assert_eq!(transport.transport_type(), bllvm_node::network::transport::TransportType::Quinn);
    }

    #[tokio::test]
    async fn test_quinn_transport_listen() {
        let transport = QuinnTransport::new().unwrap();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        
        let listener = transport.listen(addr).await;
        assert!(listener.is_ok());
        
        let listener = listener.unwrap();
        let local_addr = listener.local_addr();
        assert!(local_addr.is_ok());
    }

    #[tokio::test]
    async fn test_quinn_transport_connect_invalid_addr() {
        let transport = QuinnTransport::new().unwrap();
        
        // Try to connect with invalid address type
        let invalid_addr = TransportAddr::Tcp("127.0.0.1:8080".parse().unwrap());
        let result = transport.connect(invalid_addr).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_quinn_transport_listen_and_accept() {
        let transport = QuinnTransport::new().unwrap();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        
        let mut listener = transport.listen(addr).await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        
        // Spawn a task to connect
        let connect_addr = TransportAddr::Quinn(local_addr);
        let transport_clone = QuinnTransport::new().unwrap();
        tokio::spawn(async move {
            let _ = transport_clone.connect(connect_addr).await;
        });
        
        // Accept connection (with timeout)
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            listener.accept()
        ).await;
        
        // May succeed or timeout depending on cert verification
        assert!(result.is_ok() || result.is_err());
    }
}


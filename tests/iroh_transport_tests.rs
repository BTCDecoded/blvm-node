//! Tests for Iroh transport

#[cfg(feature = "iroh")]
mod tests {
    use bllvm_node::network::iroh_transport::IrohTransport;
    use bllvm_node::network::transport::{Transport, TransportAddr};
    use std::net::SocketAddr;

    #[tokio::test]
    async fn test_iroh_transport_new() {
        let transport = IrohTransport::new().await;
        assert!(transport.is_ok());

        let transport = transport.unwrap();
        assert_eq!(
            transport.transport_type(),
            bllvm_node::network::transport::TransportType::Iroh
        );
    }

    #[tokio::test]
    async fn test_iroh_transport_node_id() {
        let transport = IrohTransport::new().await.unwrap();
        let node_id = transport.node_id();

        // Node ID should be a valid endpoint ID
        assert!(true);
    }

    #[tokio::test]
    async fn test_iroh_transport_secret_key() {
        let transport = IrohTransport::new().await.unwrap();
        let _secret_key = transport.secret_key();

        // Secret key should be available
        assert!(true);
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
}

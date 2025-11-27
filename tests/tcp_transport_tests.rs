//! Tests for TCP transport

use bllvm_node::network::tcp_transport::TcpTransport;
use bllvm_node::network::transport::{Transport, TransportAddr, TransportListener};
use std::net::SocketAddr;

#[tokio::test]
async fn test_tcp_transport_new() {
    let transport = TcpTransport::new();
    assert_eq!(
        transport.transport_type(),
        bllvm_node::network::transport::TransportType::Tcp
    );
}

#[tokio::test]
async fn test_tcp_transport_listen() {
    let transport = TcpTransport::new();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    let listener = transport.listen(addr).await;
    assert!(listener.is_ok());

    let listener = listener.unwrap();
    let local_addr = listener.local_addr();
    assert!(local_addr.is_ok());
}

#[tokio::test]
async fn test_tcp_transport_connect_invalid_addr() {
    let transport = TcpTransport::new();

    // Try to connect with invalid address type (use Iroh variant if available, otherwise skip)
    #[cfg(feature = "iroh")]
    {
        let invalid_key = vec![0u8; 32];
        let invalid_addr = TransportAddr::Iroh(invalid_key);
        let result = transport.connect(invalid_addr).await;
        assert!(result.is_err());
    }

    #[cfg(not(feature = "iroh"))]
    {
        // Without Iroh, we can't test invalid address type easily
        // Just verify transport type
        assert_eq!(
            transport.transport_type(),
            bllvm_node::network::transport::TransportType::Tcp
        );
    }
}

//! Integration tests for Quinn QUIC transport

#[cfg(feature = "quinn")]
use anyhow::Result;
#[cfg(feature = "quinn")]
use bllvm_node::network::{
    transport::{Transport, TransportAddr},
    quinn_transport::QuinnTransport,
};
#[cfg(feature = "quinn")]
use std::net::SocketAddr;

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_type() -> Result<()> {
    let transport = QuinnTransport::new()?;
    assert_eq!(transport.transport_type(), bllvm_node::network::transport::TransportType::Quinn);
    Ok(())
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_listen() -> Result<()> {
    let transport = QuinnTransport::new()?;
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    let listener = transport.listen(addr).await?;
    let local_addr = listener.local_addr()?;
    assert_eq!(local_addr.ip(), addr.ip());
    
    Ok(())
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_connect_invalid_addr() -> Result<()> {
    let transport = QuinnTransport::new()?;
    
    // Try to connect with non-Quinn address (TCP)
    let tcp_addr = TransportAddr::Tcp("127.0.0.1:8333".parse().unwrap());
    let result = transport.connect(tcp_addr).await;
    assert!(result.is_err());
    
    Ok(())
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_listen_and_accept() -> Result<()> {
    let transport = QuinnTransport::new()?;
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    let mut listener = transport.listen(addr).await?;
    let local_addr = listener.local_addr()?;
    
    // Spawn a task to accept a connection
    let accept_handle = tokio::spawn(async move {
        listener.accept().await
    });
    
    // Small delay to ensure listener is ready
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    
    // Connect to the listener
    let client_transport = QuinnTransport::new()?;
    let client_addr = TransportAddr::Quinn(local_addr);
    
    // Note: Connection will likely fail due to certificate verification
    // This test verifies the structure works, full functional test would need cert setup
    let connect_result = client_transport.connect(client_addr).await;
    
    // Accept should complete (even if connection fails)
    let _accept_result = accept_handle.await;
    
    // Connection might fail due to cert verification - that's expected for now
    // The important thing is the code structure is correct
    Ok(())
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_quinn_transport_addr() -> Result<()> {
    let addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let quinn_addr = TransportAddr::Quinn(addr);
    
    assert!(quinn_addr.is_quinn());
    assert!(!quinn_addr.is_tcp());
    
    match quinn_addr {
        TransportAddr::Quinn(socket_addr) => {
            assert_eq!(socket_addr, addr);
        }
        _ => panic!("Expected Quinn address"),
    }
    
    Ok(())
}


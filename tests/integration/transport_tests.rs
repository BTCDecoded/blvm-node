//! Integration tests for transport abstraction layer

use anyhow::Result;
use bllvm_node::network::{
    transport::{Transport, TransportType, TransportPreference, TransportAddr},
    tcp_transport::TcpTransport,
    NetworkManager,
};

#[cfg(feature = "quinn")]
use bllvm_node::network::quinn_transport::QuinnTransport;
use std::net::SocketAddr;

#[tokio::test]
async fn test_tcp_transport_listen_and_accept() -> Result<()> {
    let transport = TcpTransport::new();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    let mut listener = transport.listen(addr).await?;
    let local_addr = listener.local_addr()?;
    
    // Spawn a task to accept a connection
    let accept_handle = tokio::spawn(async move {
        listener.accept().await
    });
    
    // Connect to the listener
    let connect_handle = tokio::spawn(async move {
        let client_transport = TcpTransport::new();
        let client_addr = TransportAddr::Tcp(local_addr);
        client_transport.connect(client_addr).await
    });
    
    // Wait for both operations
    let (accept_result, connect_result) = tokio::try_join!(accept_handle, connect_handle)?;
    
    // Both should succeed
    assert!(accept_result.is_ok());
    assert!(connect_result.is_ok());
    
    Ok(())
}

#[tokio::test]
async fn test_tcp_transport_send_recv() -> Result<()> {
    let transport = TcpTransport::new();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    let mut listener = transport.listen(addr).await?;
    let local_addr = listener.local_addr()?;
    
    // Accept connection in background
    let accept_handle = tokio::spawn(async move {
        listener.accept().await.map(|(conn, _)| conn)
    });
    
    // Connect from client
    let client_transport = TcpTransport::new();
    let client_addr = TransportAddr::Tcp(local_addr);
    let mut client_conn = client_transport.connect(client_addr).await?;
    
    // Get server connection
    let mut server_conn = accept_handle.await??;
    
    // Send data from client
    let test_data = b"Hello, server!";
    client_conn.send(test_data).await?;
    
    // Receive on server
    let received = server_conn.recv().await?;
    assert_eq!(received, test_data);
    
    Ok(())
}

#[tokio::test]
async fn test_transport_preference() {
    let preference = TransportPreference::TCP_ONLY;
    assert!(preference.allows_tcp());
    assert!(!preference.allows_iroh());
    
    #[cfg(feature = "iroh")]
    {
        let iroh_pref = TransportPreference::IROH_ONLY;
        assert!(!iroh_pref.allows_tcp());
        assert!(iroh_pref.allows_iroh());
        
        let hybrid_pref = TransportPreference::HYBRID;
        assert!(hybrid_pref.allows_tcp());
        assert!(hybrid_pref.allows_iroh());
    }
    
    #[cfg(feature = "quinn")]
    {
        let quinn_pref = TransportPreference::QUINN_ONLY;
        assert!(!quinn_pref.allows_tcp());
        assert!(quinn_pref.allows_quinn());
        
        // Test combinations
        let tcp_quinn = TransportPreference::TCP | TransportPreference::QUINN;
        assert!(tcp_quinn.allows_tcp());
        assert!(tcp_quinn.allows_quinn());
        
        #[cfg(feature = "iroh")]
        {
            let all = TransportPreference::all_transports();
            assert!(all.allows_tcp());
            assert!(all.allows_iroh());
            assert!(all.allows_quinn());
            
            let transports = all.enabled_transports();
            assert_eq!(transports.len(), 3);
        }
    }
}

#[tokio::test]
async fn test_network_manager_transport_preference() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let manager = NetworkManager::new(addr);
    
    assert_eq!(manager.transport_preference(), TransportPreference::TCP_ONLY);
}

#[tokio::test]
async fn test_transport_addr_conversion() {
    let socket_addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let transport_addr = TransportAddr::from(socket_addr);
    
    match transport_addr {
        TransportAddr::Tcp(addr) => {
            assert_eq!(addr, socket_addr);
            assert!(transport_addr.is_tcp());
        }
        _ => panic!("Expected TCP address"),
    }
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_transport_preference_combinations() {
    // Test all transport combinations
    let tcp = TransportPreference::TCP;
    let quinn = TransportPreference::QUINN;
    
    assert!(tcp.allows_tcp());
    assert!(!tcp.allows_quinn());
    
    assert!(!quinn.allows_tcp());
    assert!(quinn.allows_quinn());
    
    // Test combination
    let tcp_quinn = tcp | quinn;
    assert!(tcp_quinn.allows_tcp());
    assert!(tcp_quinn.allows_quinn());
    
    let transports = tcp_quinn.enabled_transports();
    assert_eq!(transports.len(), 2);
    assert!(transports.contains(&TransportType::Tcp));
    assert!(transports.contains(&TransportType::Quinn));
    
    #[cfg(feature = "iroh")]
    {
        let iroh = TransportPreference::IROH;
        let all = tcp | quinn | iroh;
        assert!(all.allows_tcp());
        assert!(all.allows_quinn());
        assert!(all.allows_iroh());
    }
}

#[cfg(feature = "quinn")]
#[tokio::test]
async fn test_network_manager_with_quinn() -> Result<()> {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    
    // Test Quinn-only preference
    let manager = NetworkManager::with_transport_preference(
        addr,
        100,
        TransportPreference::QUINN_ONLY,
    );
    assert_eq!(manager.transport_preference(), TransportPreference::QUINN_ONLY);
    
    // Test TCP + Quinn combination
    let combined = TransportPreference::TCP | TransportPreference::QUINN;
    let manager2 = NetworkManager::with_transport_preference(addr, 100, combined);
    assert!(manager2.transport_preference().allows_tcp());
    assert!(manager2.transport_preference().allows_quinn());
    
    Ok(())
}


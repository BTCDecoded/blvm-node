//! TCP transport implementation
//!
//! Provides TCP-based transport for Bitcoin P2P protocol compatibility.

use crate::network::transport::{
    Transport, TransportAddr, TransportConnection, TransportListener, TransportType,
};
use anyhow::Result;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tracing::{debug, error};

/// TCP transport implementation
///
/// Implements the Transport trait for traditional TCP connections,
/// providing Bitcoin P2P protocol compatibility.
#[derive(Debug, Clone)]
pub struct TcpTransport;

impl TcpTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Transport for TcpTransport {
    type Connection = TcpConnection;
    type Listener = TcpListener;

    fn transport_type(&self) -> TransportType {
        TransportType::Tcp
    }

    async fn listen(&self, addr: SocketAddr) -> Result<Self::Listener> {
        let listener = TokioTcpListener::bind(addr).await?;
        Ok(TcpListener { listener })
    }

    async fn connect(&self, addr: TransportAddr) -> Result<Self::Connection> {
        #[allow(irrefutable_let_patterns)]
        let TransportAddr::Tcp(socket_addr) = addr
        else {
            return Err(anyhow::anyhow!(
                "TCP transport can only connect to TCP addresses"
            ));
        };

        let stream = TcpStream::connect(socket_addr).await?;
        let peer_addr = stream.peer_addr()?;

        Ok(TcpConnection {
            stream,
            peer_addr: TransportAddr::Tcp(peer_addr),
            connected: true,
        })
    }
}

/// TCP listener implementation
pub struct TcpListener {
    listener: TokioTcpListener,
}

#[async_trait::async_trait]
impl TransportListener for TcpListener {
    type Connection = TcpConnection;

    async fn accept(&mut self) -> Result<(Self::Connection, TransportAddr)> {
        match self.listener.accept().await {
            Ok((stream, addr)) => {
                debug!("Accepted TCP connection from {}", addr);
                let peer_addr = stream.peer_addr()?;
                Ok((
                    TcpConnection {
                        stream,
                        peer_addr: TransportAddr::Tcp(peer_addr),
                        connected: true,
                    },
                    TransportAddr::Tcp(addr),
                ))
            }
            Err(e) => {
                error!("Failed to accept TCP connection: {}", e);
                Err(anyhow::anyhow!("Failed to accept connection: {}", e))
            }
        }
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| anyhow::anyhow!("Failed to get local addr: {}", e))
    }
}

/// TCP connection implementation
pub struct TcpConnection {
    pub(crate) stream: TcpStream,
    pub(crate) peer_addr: TransportAddr,
    pub(crate) connected: bool,
}

#[async_trait::async_trait]
impl TransportConnection for TcpConnection {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if !self.connected {
            return Err(anyhow::anyhow!("Connection closed"));
        }

        // Write length prefix (4 bytes, big-endian)
        let len = data.len() as u32;
        self.stream.write_u32(len).await?;

        // Write data
        self.stream.write_all(data).await?;

        Ok(())
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        if !self.connected {
            return Ok(Vec::new()); // Graceful close
        }

        // Read length prefix (4 bytes)
        let len = match self.stream.read_u32().await {
            Ok(len) => len as usize,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    self.connected = false;
                    return Ok(Vec::new()); // Graceful close
                }
                return Err(anyhow::anyhow!("Failed to read length: {}", e));
            }
        };

        if len == 0 {
            self.connected = false;
            return Ok(Vec::new());
        }

        // Validate message size before allocation (DoS protection)
        use crate::network::protocol::MAX_PROTOCOL_MESSAGE_LENGTH;
        if len > MAX_PROTOCOL_MESSAGE_LENGTH {
            return Err(anyhow::anyhow!(
                "Message too large: {} bytes (max: {} bytes)",
                len,
                MAX_PROTOCOL_MESSAGE_LENGTH
            ));
        }

        // Read data
        let mut buffer = vec![0u8; len];
        let bytes_read = self.stream.read_exact(&mut buffer).await?;

        if bytes_read != len {
            return Err(anyhow::anyhow!(
                "Incomplete read: expected {} bytes, got {}",
                len,
                bytes_read
            ));
        }

        Ok(buffer)
    }

    fn peer_addr(&self) -> TransportAddr {
        self.peer_addr.clone()
    }

    fn is_connected(&self) -> bool {
        self.connected
        // Note: In a real implementation, we might check stream state
        // For now, rely on the connected flag
    }

    async fn close(&mut self) -> Result<()> {
        if self.connected {
            self.stream.shutdown().await?;
            self.connected = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tcp_transport_type() {
        let transport = TcpTransport::new();
        assert_eq!(transport.transport_type(), TransportType::Tcp);
    }

    #[tokio::test]
    async fn test_tcp_transport_listen() {
        let transport = TcpTransport::new();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let listener = transport.listen(addr).await;
        assert!(listener.is_ok());

        if let Ok(listener) = listener {
            let local_addr = listener.local_addr();
            assert!(local_addr.is_ok());
        }
    }

    #[tokio::test]
    async fn test_tcp_transport_connect_invalid_addr() {
        let transport = TcpTransport::new();

        // Try to connect with non-TCP address (if Iroh feature enabled)
        #[cfg(feature = "iroh")]
        {
            let iroh_addr = TransportAddr::Iroh(vec![0u8; 32]);
            let result = transport.connect(iroh_addr).await;
            assert!(result.is_err());
        }
    }
}

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
use tracing::{debug, error, info};

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

        Ok(TcpConnection::new(stream, TransportAddr::Tcp(peer_addr)))
    }
}

impl TcpTransport {
    /// Connect and return raw TcpStream (for use with Peer::from_tcp_stream_split)
    /// 
    /// Includes a 10-second connection timeout to prevent blocking on unresponsive peers.
    pub async fn connect_stream(&self, addr: SocketAddr) -> Result<TcpStream> {
        use tokio::time::{timeout, Duration};
        
        info!("Connecting to peer at {}", addr);
        
        // 10 second connection timeout - prevents blocking on unresponsive peers
        const CONNECT_TIMEOUT_SECS: u64 = 10;
        
        let stream = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow::anyhow!("Connection timeout to {}", addr))?
            .map_err(|e| anyhow::anyhow!("Connection failed to {}: {}", addr, e))?;
        
        Ok(stream)
    }
}

/// TCP listener implementation
pub struct TcpListener {
    listener: TokioTcpListener,
}

impl TcpListener {
    /// Accept a raw TCP stream (for use with Peer::from_tcp_stream_split)
    pub async fn accept_stream(&mut self) -> Result<(TcpStream, SocketAddr)> {
        match self.listener.accept().await {
            Ok((stream, addr)) => {
                debug!("Accepted TCP connection from {}", addr);
                Ok((stream, addr))
            }
            Err(e) => {
                error!("Failed to accept TCP connection: {}", e);
                Err(anyhow::anyhow!("Failed to accept connection: {}", e))
            }
        }
    }
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
                    TcpConnection::new(stream, TransportAddr::Tcp(peer_addr)),
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

/// TCP connection implementation with split read/write halves
/// to allow concurrent read and write operations without deadlock.
/// Uses interior mutability (Mutex) for each half so that external
/// code can share the TcpConnection without deadlocks.
pub struct TcpConnection {
    pub(crate) reader: std::sync::Arc<tokio::sync::Mutex<tokio::io::ReadHalf<TcpStream>>>,
    pub(crate) writer: std::sync::Arc<tokio::sync::Mutex<tokio::io::WriteHalf<TcpStream>>>,
    pub(crate) peer_addr: TransportAddr,
    pub(crate) connected: std::sync::atomic::AtomicBool,
}

impl TcpConnection {
    /// Create a new TCP connection from a stream
    pub fn new(stream: TcpStream, peer_addr: TransportAddr) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self {
            reader: std::sync::Arc::new(tokio::sync::Mutex::new(reader)),
            writer: std::sync::Arc::new(tokio::sync::Mutex::new(writer)),
            peer_addr,
            connected: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl TransportConnection for TcpConnection {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        use std::sync::atomic::Ordering;
        
        if !self.connected.load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("Connection closed"));
        }

        // Lock writer half (doesn't block reader)
        let mut writer = self.writer.lock().await;
        
        // Send data directly - it's already in Bitcoin wire format
        // (magic + command + length + checksum + payload)
        writer.write_all(data).await?;
        writer.flush().await?;

        Ok(())
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        use std::sync::atomic::Ordering;
        
        if !self.connected.load(Ordering::Relaxed) {
            return Ok(Vec::new()); // Graceful close
        }

        // Lock reader half (doesn't block writer)
        let mut reader = self.reader.lock().await;

        // Bitcoin wire format:
        // - Magic (4 bytes)
        // - Command (12 bytes)
        // - Payload length (4 bytes, little-endian)
        // - Checksum (4 bytes)
        // - Payload (variable)
        // Header total: 24 bytes

        // Read header (24 bytes)
        let mut header = [0u8; 24];
        match reader.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) => {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    self.connected.store(false, Ordering::Relaxed);
                    return Ok(Vec::new()); // Graceful close
                }
                return Err(anyhow::anyhow!("Failed to read header: {}", e));
            }
        }

        // Extract payload length from bytes 16-19 (little-endian)
        let payload_len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]) as usize;

        if payload_len == 0 {
            // Message with no payload (like verack)
            return Ok(header.to_vec());
        }

        // Validate message size before allocation (DoS protection)
        use crate::network::protocol::MAX_PROTOCOL_MESSAGE_LENGTH;
        if payload_len > MAX_PROTOCOL_MESSAGE_LENGTH - 24 {
            return Err(anyhow::anyhow!(
                "Message payload too large: {} bytes (max: {} bytes)",
                payload_len,
                MAX_PROTOCOL_MESSAGE_LENGTH - 24
            ));
        }

        // Read payload
        let mut payload = vec![0u8; payload_len];
        reader.read_exact(&mut payload).await?;

        // Combine header and payload
        let mut message = header.to_vec();
        message.extend_from_slice(&payload);

        Ok(message)
    }

    fn peer_addr(&self) -> TransportAddr {
        self.peer_addr.clone()
    }

    fn is_connected(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.connected.load(Ordering::Relaxed)
    }

    async fn close(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        use std::sync::atomic::Ordering;
        
        if self.connected.load(Ordering::Relaxed) {
            // Shutdown the writer half to signal end of connection
            let mut writer = self.writer.lock().await;
            writer.shutdown().await?;
            self.connected.store(false, Ordering::Relaxed);
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

//! Iroh transport implementation
//!
//! Provides QUIC-based transport using Iroh for modern P2P networking.
//! This transport offers encryption, NAT traversal, and public key-based
//! peer identity.

#[cfg(feature = "iroh")]
use crate::network::transport::{
    Transport, TransportAddr, TransportConnection, TransportListener, TransportType,
};
#[cfg(feature = "iroh")]
use anyhow::Result;
#[cfg(feature = "iroh")]
use iroh::endpoint::{Connection, Endpoint, SendStream};
#[cfg(feature = "iroh")]
use iroh::{EndpointAddr, EndpointId, PublicKey, SecretKey};
#[cfg(feature = "iroh")]
use std::net::SocketAddr;
#[cfg(feature = "iroh")]
use tracing::{debug, info};

/// Iroh transport implementation
///
/// Implements the Transport trait for QUIC-based connections using Iroh.
/// Provides modern P2P networking with encryption and NAT traversal.
#[cfg(feature = "iroh")]
#[derive(Debug)]
pub struct IrohTransport {
    endpoint: Endpoint,
}

#[cfg(feature = "iroh")]
impl IrohTransport {
    /// Create a new Iroh transport
    pub async fn new() -> Result<Self> {
        // Create endpoint - this handles QUIC connections with NAT traversal
        // Endpoint::bind() automatically generates a secret key
        let endpoint = Endpoint::bind().await?;

        info!(
            "Iroh transport initialized with endpoint ID: {}",
            endpoint.id()
        );

        Ok(Self { endpoint })
    }

    /// Get the endpoint ID (public key) for this transport
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Get the secret key (for persistence if needed)
    pub fn secret_key(&self) -> &SecretKey {
        self.endpoint.secret_key()
    }
}

#[cfg(feature = "iroh")]
#[async_trait::async_trait]
impl Transport for IrohTransport {
    type Connection = IrohConnection;
    type Listener = IrohListener;

    fn transport_type(&self) -> TransportType {
        TransportType::Iroh
    }

    async fn listen(&self, _addr: SocketAddr) -> Result<Self::Listener> {
        // Iroh uses QUIC which listens on UDP, not TCP
        // The endpoint is already bound in new()
        // We use the endpoint's accept method for incoming connections
        let bound_sockets = self.endpoint.bound_sockets();
        let local_addr = bound_sockets
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("No bound sockets available"))?;
        Ok(IrohListener {
            endpoint: self.endpoint.clone(),
            local_addr,
        })
    }

    async fn connect(&self, addr: TransportAddr) -> Result<Self::Connection> {
        let public_key = match addr {
            TransportAddr::Iroh(key) => {
                // Convert public key bytes to Iroh PublicKey
                // PublicKey is 32 bytes
                if key.len() != 32 {
                    return Err(anyhow::anyhow!(
                        "Invalid Iroh public key length: expected 32 bytes, got {}",
                        key.len()
                    ));
                }
                let mut key_bytes = [0u8; 32];
                key_bytes.copy_from_slice(&key[..32]);
                PublicKey::from_bytes(&key_bytes)
                    .map_err(|e| anyhow::anyhow!("Invalid Iroh public key: {}", e))?
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Iroh transport can only connect to Iroh addresses"
                ))
            }
        };

        // Create endpoint address - can convert directly from PublicKey
        let endpoint_addr: EndpointAddr = public_key.into();

        // Dial peer using endpoint
        // ALPN identifier for Bitcoin protocol over Iroh
        let alpn = b"bitcoin/1.0";
        let conn = self.endpoint.connect(endpoint_addr, alpn).await?;

        // Get peer's public key from connection
        let peer_id = conn.remote_id();
        let peer_addr_bytes = peer_id.as_bytes().to_vec();

        Ok(IrohConnection {
            conn,
            peer_node_id: peer_id,
            peer_addr: TransportAddr::Iroh(peer_addr_bytes),
            connected: true,
            active_streams: std::collections::HashMap::new(),
        })
    }
}

/// Iroh listener implementation
#[cfg(feature = "iroh")]
pub struct IrohListener {
    endpoint: Endpoint,
    local_addr: SocketAddr,
}

#[cfg(feature = "iroh")]
#[async_trait::async_trait]
impl TransportListener for IrohListener {
    type Connection = IrohConnection;

    async fn accept(&mut self) -> Result<(Self::Connection, TransportAddr)> {
        // Accept incoming Iroh connection
        // accept() returns Accept<'_> which yields Option<Incoming>
        let accept_future = self.endpoint.accept();
        let incoming = accept_future
            .await
            .ok_or_else(|| anyhow::anyhow!("Accept stream ended"))?;

        // Accept the incoming connection - returns Accepting future
        let accepting = incoming.accept()?;

        // Await connection establishment
        let conn = accepting.await?;

        // Get peer's endpoint ID from connection
        let peer_id = conn.remote_id();

        // Extract peer node_id from connection (can also use conn.remote_id() after connection)
        let peer_node_id = peer_id; // Already have it from connecting.id()
        let peer_addr = TransportAddr::Iroh(peer_node_id.as_bytes().to_vec());

        debug!(
            "Iroh connection accepted - peer endpoint ID: {}",
            peer_node_id
        );

        Ok((
            IrohConnection {
                conn,
                peer_node_id,
                peer_addr: peer_addr.clone(),
                connected: true,
                active_streams: std::collections::HashMap::new(),
            },
            peer_addr,
        ))
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

/// Iroh connection implementation
#[cfg(feature = "iroh")]
pub struct IrohConnection {
    conn: Connection,
    peer_node_id: EndpointId,
    peer_addr: TransportAddr,
    connected: bool,
    /// Active streams per channel (for QUIC stream multiplexing)
    active_streams: std::collections::HashMap<u32, SendStream>,
}

#[cfg(feature = "iroh")]
#[async_trait::async_trait]
impl TransportConnection for IrohConnection {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if !self.connected {
            return Err(anyhow::anyhow!("Connection closed"));
        }

        // Open a new QUIC stream for sending data
        let mut stream = self.conn.open_uni().await?;

        // Write length prefix (4 bytes, big-endian)
        let len = data.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;

        // Write data
        stream.write_all(data).await?;
        stream.finish()?;

        Ok(())
    }

    /// Send data on a specific channel stream (for QUIC stream multiplexing)
    ///
    /// Opens a dedicated QUIC stream for the channel, enabling parallel operations.
    /// Streams are not reused (they're closed after sending) to avoid complexity.
    /// For true stream reuse, would need async HashMap with proper locking.
    async fn send_on_channel(&mut self, _channel_id: Option<u32>, data: &[u8]) -> Result<()> {
        if !self.connected {
            return Err(anyhow::anyhow!("Connection closed"));
        }

        // Open a new QUIC stream for this channel (parallel, non-blocking)
        let mut stream = self.conn.open_uni().await?;

        // Track active stream (for future reuse if needed)
        // Note: We don't reuse streams here to keep it simple - streams are closed after send
        // For true multiplexing with reuse, would need async-safe HashMap

        // Write length prefix (4 bytes, big-endian)
        let len = data.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;

        // Write data
        stream.write_all(data).await?;
        stream.finish()?;

        Ok(())
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        if !self.connected {
            return Ok(Vec::new()); // Graceful close
        }

        // Accept incoming QUIC stream
        // This is simplified - real implementation would handle multiple streams
        let mut stream = match self.conn.accept_uni().await {
            Ok(stream) => stream,
            Err(e) => {
                self.connected = false;
                return Err(anyhow::anyhow!("Failed to accept stream: {}", e));
            }
        };

        // Read length prefix (4 bytes)
        let mut len_bytes = [0u8; 4];
        stream.read_exact(&mut len_bytes).await?;
        let len = u32::from_be_bytes(len_bytes) as usize;

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
        stream.read_exact(&mut buffer).await?;

        Ok(buffer)
    }

    fn peer_addr(&self) -> TransportAddr {
        self.peer_addr.clone()
    }

    fn is_connected(&self) -> bool {
        self.connected && self.conn.close_reason().is_none()
    }

    async fn close(&mut self) -> Result<()> {
        if self.connected {
            self.conn.close(0u32.into(), b"Connection closed");
            self.connected = false;
        }
        Ok(())
    }
}

// Placeholder implementation when Iroh feature is disabled
#[cfg(not(feature = "iroh"))]
pub struct IrohTransport;

#[cfg(not(feature = "iroh"))]
impl IrohTransport {
    pub async fn new() -> Result<Self> {
        Err(anyhow::anyhow!("Iroh transport requires 'iroh' feature"))
    }
}

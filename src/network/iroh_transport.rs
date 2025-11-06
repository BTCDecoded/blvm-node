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
use futures::StreamExt;
#[cfg(feature = "iroh")]
use std::net::SocketAddr;
#[cfg(feature = "iroh")]
use tokio::io::AsyncReadExt;
#[cfg(feature = "iroh")]
use tracing::{debug, error, info, warn};

/// Iroh transport implementation
///
/// Implements the Transport trait for QUIC-based connections using Iroh.
/// Provides modern P2P networking with encryption and NAT traversal.
#[cfg(feature = "iroh")]
#[derive(Debug)]
pub struct IrohTransport {
    endpoint: iroh_net::magic_endpoint::MagicEndpoint,
    secret_key: iroh_net::key::SecretKey,
}

#[cfg(feature = "iroh")]
impl IrohTransport {
    /// Create a new Iroh transport
    pub async fn new() -> Result<Self> {
        // Generate a new secret key for this node
        let secret_key = iroh_net::key::SecretKey::generate();

        // Create magic endpoint - this handles QUIC connections with NAT traversal
        let endpoint = iroh_net::magic_endpoint::MagicEndpoint::builder()
            .secret_key(secret_key.clone())
            .bind(0) // Bind to any available UDP port
            .await?;

        info!(
            "Iroh transport initialized with node ID: {}",
            endpoint.node_id()
        );

        Ok(Self {
            endpoint,
            secret_key,
        })
    }

    /// Get the node ID (public key) for this transport
    pub fn node_id(&self) -> iroh_net::NodeId {
        self.endpoint.node_id()
    }

    /// Get the secret key (for persistence if needed)
    pub fn secret_key(&self) -> &iroh_net::key::SecretKey {
        &self.secret_key
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
        // Note: accept() returns a future, we'll poll it in accept() method
        let (local_addr, _) = self
            .endpoint
            .local_addr()
            .map_err(|e| anyhow::anyhow!("Failed to get local address: {}", e))?;
        Ok(IrohListener {
            endpoint: self.endpoint.clone(),
            local_addr,
        })
    }

    async fn connect(&self, addr: TransportAddr) -> Result<Self::Connection> {
        let node_id = match addr {
            TransportAddr::Iroh(key) => {
                // Convert public key bytes to Iroh NodeId
                // NodeId is 32 bytes (public key)
                if key.len() != 32 {
                    return Err(anyhow::anyhow!(
                        "Invalid Iroh public key length: expected 32 bytes, got {}",
                        key.len()
                    ));
                }
                let mut node_id_bytes = [0u8; 32];
                node_id_bytes.copy_from_slice(&key[..32]);
                iroh_net::NodeId::from_bytes(&node_id_bytes)
                    .map_err(|e| anyhow::anyhow!("Invalid Iroh public key: {}", e))?
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Iroh transport can only connect to Iroh addresses"
                ))
            }
        };

        // Create node address with ALPN protocol identifier for Bitcoin P2P
        let node_addr = iroh_net::NodeAddr::from_parts(
            node_id,
            None,   // No DERP URL
            vec![], // No direct addresses, use magic endpoint for connection
        );

        // Dial peer using magic endpoint
        // ALPN identifier for Bitcoin protocol over Iroh
        let alpn = b"bitcoin/1.0";
        let conn = self.endpoint.connect(node_addr, alpn).await?;

        // Store node_id separately since quinn::Connection doesn't expose it
        let peer_addr_bytes = node_id.as_bytes().to_vec();

        Ok(IrohConnection {
            conn,
            peer_node_id: node_id,
            peer_addr: TransportAddr::Iroh(peer_addr_bytes),
            connected: true,
        })
    }
}

/// Iroh listener implementation
#[cfg(feature = "iroh")]
pub struct IrohListener {
    endpoint: iroh_net::magic_endpoint::MagicEndpoint,
    local_addr: SocketAddr,
}

#[cfg(feature = "iroh")]
#[async_trait::async_trait]
impl TransportListener for IrohListener {
    type Connection = IrohConnection;

    async fn accept(&mut self) -> Result<(Self::Connection, TransportAddr)> {
        // Accept incoming Iroh connection
        // accept() returns a future that yields Option<Accept>
        let accept_future = self.endpoint.accept();
        let accept = accept_future
            .await
            .ok_or_else(|| anyhow::anyhow!("Accept stream ended"))?;

        // Accept yields a future that becomes Connection when awaited
        let conn = accept.await?;

        // Extract peer node_id from connection
        // Note: Iroh's Accept type doesn't directly expose peer node_id.
        // The peer's node_id is authenticated via QUIC/TLS handshake, but extraction
        // from quinn::Connection requires accessing internal connection state which isn't
        // publicly exposed.
        //
        // Standard approach: Protocol-level node_id exchange in first message.
        // This matches Bitcoin P2P protocol where peers exchange identity via Version message.
        // The node_id will be available once the peer sends its version message.
        //
        // For now, use placeholder that will be updated when first protocol message is received.
        let peer_node_id = self.endpoint.node_id(); // Placeholder until protocol exchange
        let peer_addr = TransportAddr::Iroh(peer_node_id.as_bytes().to_vec());

        debug!("Iroh connection accepted - peer node_id will be extracted from protocol handshake");

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
    conn: quinn::connection::Connection,
    peer_node_id: iroh_net::NodeId,
    peer_addr: TransportAddr,
    connected: bool,
    /// Active streams per channel (for QUIC stream multiplexing)
    active_streams: std::collections::HashMap<u32, quinn::SendStream>,
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
    pub async fn send_on_channel(&mut self, channel_id: u32, data: &[u8]) -> Result<()> {
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

//! Quinn QUIC transport implementation
//!
//! Provides direct QUIC-based transport using Quinn for simple, high-performance
//! connections without NAT traversal. SocketAddr-based addressing (like TCP)
//! makes it ideal for server-to-server connections, mining pools, and UTXO sync.

#[cfg(feature = "quinn")]
use crate::network::transport::{
    Transport, TransportAddr, TransportConnection, TransportListener, TransportType,
};
#[cfg(feature = "quinn")]
use anyhow::Result;
#[cfg(feature = "quinn")]
use std::net::SocketAddr;
#[cfg(feature = "quinn")]
use tracing::{debug, info};

/// Quinn transport implementation
///
/// Implements the Transport trait for direct QUIC connections using Quinn.
/// Provides modern QUIC benefits (encryption, multiplexing, connection migration)
/// without the overhead of NAT traversal (Iroh's MagicEndpoint).
#[cfg(feature = "quinn")]
#[derive(Debug)]
pub struct QuinnTransport {
    endpoint: quinn::Endpoint,
}

#[cfg(feature = "quinn")]
impl QuinnTransport {
    /// Create a new Quinn transport (client-side)
    ///
    /// For client connections. Server endpoints are created in listen().
    pub fn new() -> Result<Self> {
        // Create client endpoint
        // For now, use default client config (will need proper cert verification later)
        let endpoint = quinn::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;

        info!("Quinn transport initialized (client mode)");

        Ok(Self { endpoint })
    }

    // Note: Server certificates are handled in listen() method
    // This transport uses self-signed certs for development
}

#[cfg(feature = "quinn")]
#[async_trait::async_trait]
impl Transport for QuinnTransport {
    type Connection = QuinnConnection;
    type Listener = QuinnListener;

    fn transport_type(&self) -> TransportType {
        TransportType::Quinn
    }

    async fn listen(&self, addr: SocketAddr) -> Result<Self::Listener> {
        // Create server config with self-signed cert
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .map_err(|e| anyhow::anyhow!("Failed to generate certificate: {}", e))?;
        // Convert to formats expected by quinn 0.11
        let cert_der = cert.serialize_der()?;
        let key_der = cert.serialize_private_key_der();

        // quinn 0.11 uses pki_types
        use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = vec![CertificateDer::from(cert_der)];
        let key = PrivateKeyDer::Pkcs8(key_der.into());

        let server_config = quinn::ServerConfig::with_single_cert(certs, key)?;

        let endpoint = quinn::Endpoint::server(server_config, addr)?;

        Ok(QuinnListener {
            endpoint,
            local_addr: addr,
        })
    }

    async fn connect(&self, addr: TransportAddr) -> Result<Self::Connection> {
        let socket_addr = match addr {
            TransportAddr::Quinn(socket_addr) => socket_addr,
            _ => {
                return Err(anyhow::anyhow!(
                    "Quinn transport can only connect to Quinn addresses"
                ))
            }
        };

        // Create a new endpoint for this connection
        // For now, use default client endpoint (will need proper cert verification later)
        let endpoint = quinn::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;

        // Connect to server (use SNI or IP)
        let server_name = socket_addr.ip().to_string();
        let conn = endpoint.connect(socket_addr, &server_name)?.await?;

        Ok(QuinnConnection {
            conn,
            peer_addr: TransportAddr::Quinn(socket_addr),
            connected: true,
        })
    }
}

/// Quinn listener implementation
#[cfg(feature = "quinn")]
pub struct QuinnListener {
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
}

#[cfg(feature = "quinn")]
#[async_trait::async_trait]
impl TransportListener for QuinnListener {
    type Connection = QuinnConnection;

    async fn accept(&mut self) -> Result<(Self::Connection, TransportAddr)> {
        // Accept incoming QUIC connection
        let conn = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow::anyhow!("Endpoint closed"))?;

        // Wait for connection handshake
        let conn = conn.await?;

        // Extract peer address from connection
        let peer_addr = conn.remote_address();
        let transport_addr = TransportAddr::Quinn(peer_addr);

        debug!("Accepted Quinn connection from {}", peer_addr);

        Ok((
            QuinnConnection {
                conn,
                peer_addr: transport_addr.clone(),
                connected: true,
            },
            transport_addr,
        ))
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

/// Quinn connection implementation
#[cfg(feature = "quinn")]
pub struct QuinnConnection {
    conn: quinn::Connection,
    peer_addr: TransportAddr,
    connected: bool,
}

#[cfg(feature = "quinn")]
#[async_trait::async_trait]
impl TransportConnection for QuinnConnection {
    async fn send(&mut self, data: &[u8]) -> Result<()> {
        if !self.connected {
            return Err(anyhow::anyhow!("Connection closed"));
        }

        // Open a new QUIC unidirectional stream for sending data
        let mut stream = self.conn.open_uni().await?;

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

// Placeholder implementation when Quinn feature is disabled
#[cfg(not(feature = "quinn"))]
pub struct QuinnTransport;

#[cfg(not(feature = "quinn"))]
impl QuinnTransport {
    pub async fn new() -> Result<Self> {
        Err(anyhow::anyhow!("Quinn transport requires 'quinn' feature"))
    }
}

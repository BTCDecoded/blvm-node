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

/// Certificate verifier that accepts any server certificate.
///
/// SECURITY: This bypasses TLS chain validation. It is intentional for the BLVM P2P
/// transport where peers use ephemeral self-signed certificates. Trust is established
/// at the application layer via the Bitcoin P2P version handshake and peer scoring,
/// not via a PKI chain. Replace with certificate-pinning once peer identity is stable.
#[cfg(feature = "quinn")]
struct NoServerCertVerification;

#[cfg(feature = "quinn")]
impl quinn::rustls::client::danger::ServerCertVerifier for NoServerCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &quinn::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> std::result::Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error>
    {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        quinn::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Quinn transport implementation
///
/// Implements the Transport trait for direct QUIC connections using Quinn.
/// Provides modern QUIC benefits (encryption, multiplexing, connection migration)
/// without the overhead of NAT traversal (Iroh's MagicEndpoint).
#[cfg(feature = "quinn")]
#[derive(Debug)]
pub struct QuinnTransport {
    endpoint: quinn::Endpoint,
    max_message_length: usize,
}

#[cfg(feature = "quinn")]
impl QuinnTransport {
    /// Create a new Quinn transport (client-side)
    ///
    /// For client connections. Server endpoints are created in listen().
    pub fn new() -> Result<Self> {
        Self::with_max_message_length(crate::network::protocol::MAX_PROTOCOL_MESSAGE_LENGTH)
    }

    /// Create with configurable max message length (for constrained networks).
    pub fn with_max_message_length(max_message_length: usize) -> Result<Self> {
        let endpoint = quinn::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
        info!("Quinn transport initialized (client mode)");
        Ok(Self {
            endpoint,
            max_message_length,
        })
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
            max_message_length: self.max_message_length,
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

        // SECURITY: BLVM Quinn peers use ephemeral self-signed certificates (no shared CA).
        // We cannot use the system root store for verification. Instead we skip TLS chain
        // verification and rely on the Bitcoin-level P2P authentication (version handshake,
        // peer scoring, network magic) to establish trust.
        //
        // TODO: Replace with certificate-pinning / TOFU once peer identity management is
        //       implemented (track peer cert fingerprint on first connection, reject changes).
        let crypto = {
            use quinn::rustls;
            let mut tls = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(NoServerCertVerification))
                .with_no_client_auth();
            tls.alpn_protocols = vec![b"blvm-p2p".to_vec()];
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)
                .map_err(|e| anyhow::anyhow!("Failed to build Quinn TLS config: {e}"))?
        };
        let client_config = quinn::ClientConfig::new(std::sync::Arc::new(crypto));
        let mut endpoint = quinn::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?;
        endpoint.set_default_client_config(client_config);

        let server_name = "blvm-peer";
        let conn = endpoint.connect(socket_addr, server_name)?.await?;

        Ok(QuinnConnection {
            conn,
            peer_addr: TransportAddr::Quinn(socket_addr),
            connected: true,
            max_message_length: self.max_message_length,
        })
    }
}

/// Quinn listener implementation
#[cfg(feature = "quinn")]
pub struct QuinnListener {
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
    max_message_length: usize,
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
                max_message_length: self.max_message_length,
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
    max_message_length: usize,
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
        if len > self.max_message_length {
            return Err(anyhow::anyhow!(
                "Message too large: {} bytes (max: {} bytes)",
                len,
                self.max_message_length
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

//! QUIC RPC Server Implementation
//!
//! Provides JSON-RPC over QUIC using Quinn for improved performance and security.
//! Optional feature alongside the standard TCP RPC server.

#[cfg(feature = "quinn")]
use anyhow::Result;
#[cfg(feature = "quinn")]
use serde_json::{json, Value};
#[cfg(feature = "quinn")]
use std::net::SocketAddr;
#[cfg(feature = "quinn")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(feature = "quinn")]
use tracing::{debug, error, info, warn};

#[cfg(feature = "quinn")]
use super::server;

/// QUIC RPC server using Quinn
#[cfg(feature = "quinn")]
pub struct QuinnRpcServer {
    addr: SocketAddr,
}

#[cfg(feature = "quinn")]
impl QuinnRpcServer {
    /// Create a new QUIC RPC server
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }

    /// Start the QUIC RPC server
    pub async fn start(&self) -> Result<()> {
        // Generate self-signed certificate for QUIC
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .map_err(|e| anyhow::anyhow!("Failed to generate certificate: {}", e))?;

        let cert_der = rustls::pki_types::CertificateDer::from(cert.serialize_der()?);
        let key_der = rustls::pki_types::PrivateKeyDer::from(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.serialize_private_key_der()),
        );

        let server_config = quinn::ServerConfig::with_single_cert(vec![cert_der], key_der)?;
        let endpoint = quinn::Endpoint::server(server_config, self.addr)?;

        info!("QUIC RPC server listening on {}", self.addr);

        // Accept incoming connections
        while let Some(conn) = endpoint.accept().await {
            let connection = match conn.await {
                Ok(conn) => conn,
                Err(e) => {
                    warn!("Failed to accept QUIC connection: {}", e);
                    continue;
                }
            };

            debug!(
                "New QUIC RPC connection from {}",
                connection.remote_address()
            );

            // Handle each connection in a separate task
            tokio::spawn(Self::handle_connection(connection));
        }

        Ok(())
    }

    /// Handle a QUIC connection
    #[cfg(feature = "quinn")]
    async fn handle_connection(connection: quinn::Connection) {
        // Accept bidirectional streams from the connection
        while let Ok(Some(stream)) = connection.accept_bi().await {
            let (mut send, mut recv) = stream;

            // Handle each stream in a separate task
            tokio::spawn(async move {
                // Read full request (QUIC streams can be read like regular streams)
                let mut buffer = Vec::new();
                match recv.read_to_end(&mut buffer).await {
                    Ok(_) => {
                        if buffer.is_empty() {
                            debug!("Empty QUIC RPC request");
                            let _ = send.finish().await;
                            return;
                        }
                    }
                    Err(e) => {
                        warn!("Error reading from QUIC stream: {}", e);
                        let _ = send.finish().await;
                        return;
                    }
                }

                let request = match String::from_utf8(buffer) {
                    Ok(req) if !req.is_empty() => req,
                    Ok(_) => {
                        warn!("Empty QUIC RPC request");
                        let _ = send.finish().await;
                        return;
                    }
                    Err(e) => {
                        warn!("Invalid UTF-8 in QUIC RPC request: {}", e);
                        let _ = send.finish().await;
                        return;
                    }
                };

                debug!("QUIC RPC request: {}", request);

                // Process JSON-RPC request (reuse existing logic)
                let response = server::RpcServer::process_request(&request).await;
                let response_json =
                    serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());

                // Send response
                if let Err(e) = send.write_all(response_json.as_bytes()).await {
                    warn!("Failed to send QUIC RPC response: {}", e);
                }

                // Finish the stream
                if let Err(e) = send.finish().await {
                    warn!("Failed to finish QUIC stream: {}", e);
                }
            });
        }

        debug!("QUIC connection closed");
    }
}

#[cfg(not(feature = "quinn"))]
pub struct QuinnRpcServer {
    _phantom: std::marker::PhantomData<()>,
}

#[cfg(not(feature = "quinn"))]
impl QuinnRpcServer {
    pub fn new(_addr: SocketAddr) -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }

    pub async fn start(&self) -> Result<()> {
        Err(anyhow::anyhow!(
            "QUIC RPC server requires 'quinn' feature flag"
        ))
    }
}

//! QUIC RPC Server Implementation
//!
//! Provides JSON-RPC over QUIC using Quinn for improved performance and security.
//! Optional feature alongside the standard TCP RPC server.

#[cfg(feature = "quinn")]
use anyhow::Result;
#[cfg(feature = "quinn")]
use std::net::SocketAddr;
#[cfg(feature = "quinn")]
use tracing::{debug, info, warn};

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

        // Convert to formats expected by quinn 0.11
        let cert_der = cert.serialize_der()?;
        let key_der = cert.serialize_private_key_der();

        // quinn 0.11 uses pki_types
        use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = vec![CertificateDer::from(cert_der)];
        let key = PrivateKeyDer::Pkcs8(key_der.into());

        let server_config = quinn::ServerConfig::with_single_cert(certs, key)?;
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
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            // Handle each stream in a separate task
            tokio::spawn(async move {
                // Read full request (QUIC streams can be read like regular streams)
                // quinn 0.10: read_to_end takes a limit parameter, so we use read() in a loop
                let mut buffer = Vec::new();
                let mut temp_buf = [0u8; 4096];
                loop {
                    match recv.read(&mut temp_buf).await {
                        Ok(Some(0)) | Ok(None) => break,
                        Ok(Some(n)) => buffer.extend_from_slice(&temp_buf[..n]),
                        Err(e) => {
                            warn!("Error reading from QUIC stream: {}", e);
                            let _ = send.finish();
                            return;
                        }
                    }
                }
                let request = match String::from_utf8(buffer) {
                    Ok(req) if !req.is_empty() => req,
                    Ok(_) => {
                        warn!("Empty QUIC RPC request");
                        let _ = send.finish();
                        return;
                    }
                    Err(e) => {
                        warn!("Invalid UTF-8 in QUIC RPC request: {}", e);
                        let _ = send.finish();
                        return;
                    }
                };

                debug!("QUIC RPC request: {}", request);

                // Process JSON-RPC request (reuse existing logic)
                let response_json = server::RpcServer::process_request(&request).await;

                // Send response
                if let Err(e) = send.write_all(response_json.as_bytes()).await {
                    warn!("Failed to send QUIC RPC response: {}", e);
                }

                // Finish the stream
                if let Err(e) = send.finish() {
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

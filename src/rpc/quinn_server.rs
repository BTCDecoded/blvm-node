//! QUIC RPC Server Implementation
//!
//! Provides JSON-RPC over QUIC using Quinn for improved performance and security.
//! Optional feature alongside the standard TCP RPC server.

#[cfg(feature = "quinn")]
use anyhow::Result;
#[cfg(feature = "quinn")]
use std::net::SocketAddr;
#[cfg(feature = "quinn")]
use std::sync::Arc;
#[cfg(feature = "quinn")]
use tracing::{debug, info, warn};

#[cfg(feature = "quinn")]
use super::auth;
#[cfg(feature = "quinn")]
use super::errors;
#[cfg(feature = "quinn")]
use super::server;

/// QUIC RPC server using Quinn
#[cfg(feature = "quinn")]
pub struct QuinnRpcServer {
    addr: SocketAddr,
    auth_manager: Option<Arc<auth::RpcAuthManager>>,
    max_request_size: usize,
    batch_rate_multiplier_cap: u32,
}

#[cfg(feature = "quinn")]
impl QuinnRpcServer {
    /// Create a new QUIC RPC server
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            auth_manager: None,
            max_request_size: server::DEFAULT_MAX_REQUEST_SIZE,
            batch_rate_multiplier_cap: 10,
        }
    }

    /// Set auth manager for authentication and rate limiting
    pub fn with_auth_manager(mut self, auth_manager: Arc<auth::RpcAuthManager>) -> Self {
        self.auth_manager = Some(auth_manager);
        self
    }

    /// Set maximum request body size
    pub fn with_max_request_size(mut self, bytes: usize) -> Self {
        self.max_request_size = bytes;
        self
    }

    /// Set batch rate multiplier cap (min(batch_len, cap) tokens consumed)
    pub fn with_batch_rate_multiplier_cap(mut self, cap: u32) -> Self {
        self.batch_rate_multiplier_cap = cap;
        self
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

        let auth_manager = self.auth_manager.clone();
        let max_request_size = self.max_request_size;
        let batch_rate_multiplier_cap = self.batch_rate_multiplier_cap;

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
            tokio::spawn(Self::handle_connection(
                connection,
                auth_manager.clone(),
                max_request_size,
                batch_rate_multiplier_cap,
            ));
        }

        Ok(())
    }

    /// Handle a QUIC connection
    #[cfg(feature = "quinn")]
    async fn handle_connection(
        connection: quinn::Connection,
        auth_manager: Option<Arc<auth::RpcAuthManager>>,
        max_request_size: usize,
        batch_rate_multiplier_cap: u32,
    ) {
        let client_addr = connection.remote_address();
        // Accept bidirectional streams from the connection
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let auth_manager = auth_manager.clone();
            let client_addr = client_addr;
            // Handle each stream in a separate task
            tokio::spawn(async move {
                // Read full request with size limit (S-009)
                let max_size = max_request_size;
                let mut buffer = Vec::new();
                let mut temp_buf = [0u8; 4096];
                loop {
                    if buffer.len() >= max_size {
                        warn!("QUIC RPC request exceeds size limit ({} bytes)", max_size);
                        let _ = send.finish();
                        return;
                    }
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

                // Auth and rate limit checks (QUIC has no HTTP headers - use IP-based only)
                if let Some(ref auth_mgr) = auth_manager {
                    use hyper::HeaderMap;
                    let headers = HeaderMap::new();
                    let auth_result = auth_mgr.authenticate_request(&headers, client_addr).await;

                    if let Some(error) = &auth_result.error {
                        let err = errors::RpcError::new(
                            errors::RpcErrorCode::ServerError(-32001),
                            error.clone(),
                        );
                        let response_json = serde_json::to_string(&err.to_json(None))
                            .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","error":{"code":-32001,"message":"Unauthorized"},"id":null}"#.to_string());
                        let _ = send.write_all(response_json.as_bytes()).await;
                        let _ = send.finish();
                        return;
                    }

                    let parsed = serde_json::from_str::<serde_json::Value>(&request).ok();
                    let (method_name, rate_limit_n) =
                        match parsed.as_ref().and_then(|v| v.as_array()) {
                            Some(requests) => (
                                "batch".to_string(),
                                requests.len().min(batch_rate_multiplier_cap as usize) as u32,
                            ),
                            _ => (
                                parsed
                                    .as_ref()
                                    .and_then(|req| {
                                        req.get("method").and_then(|m| m.as_str()).map(String::from)
                                    })
                                    .unwrap_or_else(|| "unknown".to_string()),
                                1u32,
                            ),
                        };
                    let endpoint = format!("quinn:rpc:{method_name}");

                    if let Some(ref user_id) = auth_result.user_id {
                        if !auth_mgr
                            .check_rate_limit_with_endpoint_n(
                                user_id,
                                Some(client_addr),
                                Some(&endpoint),
                                rate_limit_n,
                            )
                            .await
                        {
                            let err = errors::RpcError::new(
                                errors::RpcErrorCode::ServerError(-32002),
                                "Rate limit exceeded",
                            );
                            let response_json = serde_json::to_string(&err.to_json(None))
                                .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","error":{"code":-32002,"message":"Rate limit exceeded"},"id":null}"#.to_string());
                            let _ = send.write_all(response_json.as_bytes()).await;
                            let _ = send.finish();
                            return;
                        }
                    } else if !auth_mgr
                        .check_ip_rate_limit_with_endpoint_n(
                            client_addr,
                            Some(&endpoint),
                            rate_limit_n,
                        )
                        .await
                    {
                        let err = errors::RpcError::new(
                            errors::RpcErrorCode::ServerError(-32002),
                            "IP rate limit exceeded",
                        );
                        let response_json = serde_json::to_string(&err.to_json(None))
                            .unwrap_or_else(|_| r#"{"jsonrpc":"2.0","error":{"code":-32002,"message":"IP rate limit exceeded"},"id":null}"#.to_string());
                        let _ = send.write_all(response_json.as_bytes()).await;
                        let _ = send.finish();
                        return;
                    }

                    if !auth_mgr.check_method_rate_limit(&method_name).await {
                        let err = errors::RpcError::new(
                            errors::RpcErrorCode::ServerError(-32002),
                            format!("Method '{method_name}' rate limit exceeded"),
                        );
                        let response_json = serde_json::to_string(&err.to_json(None))
                            .unwrap_or_else(|_| "{}".to_string());
                        let _ = send.write_all(response_json.as_bytes()).await;
                        let _ = send.finish();
                        return;
                    }
                }

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

    #[allow(dead_code)]
    pub fn with_auth_manager(
        self,
        _auth_manager: std::sync::Arc<super::auth::RpcAuthManager>,
    ) -> Self {
        self
    }

    #[allow(dead_code)]
    pub fn with_max_request_size(self, _bytes: usize) -> Self {
        self
    }

    #[allow(dead_code)]
    pub fn with_batch_rate_multiplier_cap(self, _cap: u32) -> Self {
        self
    }

    pub async fn start(&self) -> Result<()> {
        Err(anyhow::anyhow!(
            "QUIC RPC server requires 'quinn' feature flag"
        ))
    }
}

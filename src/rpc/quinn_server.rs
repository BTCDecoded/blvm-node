//! QUIC JSON-RPC server (HTTP/3 over QUIC).
//!
//! Uses the same [`crate::rpc::server::RpcServer`] instance as TCP HTTP JSON-RPC: authentication,
//! rate limits, and handlers match the HTTP transport.

#[cfg(feature = "quinn")]
use anyhow::Result;
#[cfg(feature = "quinn")]
use bytes::{Buf, Bytes};
#[cfg(feature = "quinn")]
use http::{Method, Response, StatusCode};
#[cfg(feature = "quinn")]
use std::net::SocketAddr;
#[cfg(feature = "quinn")]
use std::sync::{Arc, Once};
#[cfg(feature = "quinn")]
use tracing::{debug, info, warn};

#[cfg(feature = "quinn")]
use super::server::{DispatchJsonRpcPostOutcome, RpcServer};
#[cfg(feature = "quinn")]
use quinn::crypto::rustls::QuicServerConfig;

/// QUIC + HTTP/3 JSON-RPC server (TLS ALPN `h3`).
#[cfg(feature = "quinn")]
pub struct QuinnRpcServer {
    addr: SocketAddr,
    rpc_server: Arc<RpcServer>,
}

#[cfg(feature = "quinn")]
impl QuinnRpcServer {
    /// Create an HTTP/3 RPC server bound to `addr`, sharing handlers/auth with `rpc_server`.
    pub fn new(addr: SocketAddr, rpc_server: Arc<RpcServer>) -> Self {
        Self { addr, rpc_server }
    }

    /// Start the QUIC HTTP/3 RPC server (blocking accept loop).
    pub async fn start(&self) -> Result<()> {
        static INIT_RUSTLS_CRYPTO: Once = Once::new();
        INIT_RUSTLS_CRYPTO.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("install rustls ring CryptoProvider for QUIC RPC TLS");
        });

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .map_err(|e| anyhow::anyhow!("Failed to generate certificate: {}", e))?;

        let cert_der = cert.serialize_der()?;
        let key_der = cert.serialize_private_key_der();

        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = vec![CertificateDer::from(cert_der)];
        let key = PrivateKeyDer::Pkcs8(key_der.into());

        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| anyhow::anyhow!("TLS server config: {}", e))?;
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(tls_config)
                .map_err(|e| anyhow::anyhow!("QuicServerConfig: {}", e))?,
        ));

        let endpoint = quinn::Endpoint::server(server_config, self.addr)?;

        info!(
            "HTTP/3 JSON-RPC (QUIC) listening on {} (ALPN h3)",
            self.addr
        );

        let rpc_server = Arc::clone(&self.rpc_server);

        while let Some(conn) = endpoint.accept().await {
            let incoming = match conn.await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to accept QUIC connection: {}", e);
                    continue;
                }
            };

            let remote_addr = incoming.remote_address();
            let rpc_server = Arc::clone(&rpc_server);
            debug!("New HTTP/3 RPC connection from {}", remote_addr);

            tokio::spawn(async move {
                Self::handle_h3_connection(incoming, rpc_server, remote_addr).await;
            });
        }

        Ok(())
    }

    async fn handle_h3_connection(
        conn: quinn::Connection,
        rpc_server: Arc<RpcServer>,
        remote_addr: SocketAddr,
    ) {
        let mut h3_conn = match h3::server::builder()
            .build(h3_quinn::Connection::new(conn))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!("HTTP/3 connection setup failed: {}", e);
                return;
            }
        };

        loop {
            match h3_conn.accept().await {
                Ok(Some(resolver)) => {
                    let rpc_server = Arc::clone(&rpc_server);
                    tokio::spawn(async move {
                        if let Err(e) =
                            Self::handle_h3_request(resolver, rpc_server, remote_addr).await
                        {
                            debug!("HTTP/3 RPC request handler ended: {}", e);
                        }
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("HTTP/3 accept failed: {}", e);
                    break;
                }
            }
        }
    }

    async fn handle_h3_request(
        resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
        rpc_server: Arc<RpcServer>,
        remote_addr: SocketAddr,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (req, mut stream) = resolver.resolve_request().await?;

        if req.method() != Method::POST {
            let status = StatusCode::METHOD_NOT_ALLOWED;
            let msg = "Only POST method is supported for JSON-RPC";
            Self::send_h3_http_error(&mut stream, status, msg).await?;
            stream.finish().await?;
            return Ok(());
        }

        let headers = req.headers().clone();
        if let Some(ct) = headers.get("content-type") {
            if ct.as_bytes() != b"application/json" {
                warn!(
                    "Invalid Content-Type on HTTP/3 RPC from {}: {:?}",
                    remote_addr, ct
                );
            }
        }

        let max = rpc_server.max_request_body_bytes();
        let mut body_acc: Vec<u8> = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await? {
            while chunk.has_remaining() {
                let n = chunk.remaining();
                if body_acc.len().saturating_add(n) > max {
                    let status = StatusCode::PAYLOAD_TOO_LARGE;
                    let msg = format!(
                        "Request body too large: {} bytes (max: {} bytes)",
                        body_acc.len().saturating_add(n),
                        max
                    );
                    Self::send_h3_http_error(&mut stream, status, &msg).await?;
                    stream.finish().await?;
                    return Ok(());
                }
                body_acc.extend_from_slice(chunk.chunk());
                chunk.advance(n);
            }
        }

        let json_body = String::from_utf8(body_acc)
            .map_err(|e| format!("invalid UTF-8 in request body: {e}"))?;

        match RpcServer::dispatch_json_rpc_post_body(
            Arc::clone(&rpc_server),
            &headers,
            remote_addr,
            &json_body,
        )
        .await
        {
            DispatchJsonRpcPostOutcome::Success {
                response_json,
                request_id_short,
            } => {
                let resp = Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .header("x-request-id", request_id_short)
                    .body(())?;
                stream.send_response(resp).await?;
                stream.send_data(Bytes::from(response_json)).await?;
            }
            DispatchJsonRpcPostOutcome::Error { status, message } => {
                Self::send_h3_http_error(&mut stream, status, &message).await?;
            }
        }

        stream.finish().await?;
        Ok(())
    }

    async fn send_h3_http_error(
        stream: &mut h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        status: StatusCode,
        message: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let body = RpcServer::http_error_json_body(status, message);
        let resp = Response::builder().status(status).body(())?;
        stream.send_response(resp).await?;
        stream.send_data(Bytes::from(body)).await?;
        Ok(())
    }
}

#[cfg(not(feature = "quinn"))]
use std::net::SocketAddr;
#[cfg(not(feature = "quinn"))]
use std::sync::Arc;

#[cfg(not(feature = "quinn"))]
pub struct QuinnRpcServer {
    _phantom: std::marker::PhantomData<()>,
}

#[cfg(not(feature = "quinn"))]
impl QuinnRpcServer {
    pub fn new(_addr: SocketAddr, _rpc_server: Arc<super::server::RpcServer>) -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "QUIC RPC server requires 'quinn' feature flag"
        ))
    }
}

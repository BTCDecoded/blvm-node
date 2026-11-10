#![cfg(feature = "quinn")]

use blvm_node::rpc::auth::RpcAuthManager;
use blvm_node::rpc::quinn_server::QuinnRpcServer;
use blvm_node::rpc::server::RpcServer;
use bytes::Buf;
use futures::future::poll_fn;
use http::Request;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Debug)]
struct SkipServerCert;

impl ServerCertVerifier for SkipServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}

async fn h3_json_rpc(
    quinn_addr: SocketAddr,
    body: &str,
) -> Result<(http::StatusCode, String), Box<dyn std::error::Error + Send + Sync>> {
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerCert))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let client_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_cfg)));

    let connecting = endpoint.connect(quinn_addr, "localhost")?;
    let conn = connecting.await?;

    let h3 = h3_quinn::Connection::new(conn);
    let (mut conn_driver, mut send_req) = h3::client::builder().build(h3).await?;

    let driver = tokio::spawn(async move {
        let _ = poll_fn(|cx| conn_driver.poll_close(cx)).await;
    });

    let req = Request::post("https://localhost/")
        .header("content-type", "application/json")
        .body(())?;

    let mut stream = send_req.send_request(req).await?;
    stream
        .send_data(bytes::Bytes::copy_from_slice(body.as_bytes()))
        .await?;
    stream.finish().await?;

    let resp = stream.recv_response().await?;
    let status = resp.status();

    let mut out = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        while chunk.has_remaining() {
            let n = chunk.remaining();
            out.extend_from_slice(chunk.chunk());
            chunk.advance(n);
        }
    }

    drop(send_req);
    driver.abort();

    Ok((status, String::from_utf8(out)?))
}

#[tokio::test]
#[ignore]
async fn quic_rpc_rate_limit_rejection() {
    let quinn_addr: SocketAddr = "127.0.0.1:18333".parse().unwrap();
    let placeholder: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let auth_manager = Arc::new(RpcAuthManager::with_rate_limits(false, 2, 0)); // no refill: 429 is timing-independent
    let rpc = Arc::new(RpcServer::with_auth(placeholder, auth_manager));
    let server = QuinnRpcServer::new(quinn_addr, rpc);

    let _server_handle = tokio::spawn(async move {
        let _ = server.start().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let request = r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#;

    for i in 0..2 {
        let (status, body) = h3_json_rpc(quinn_addr, request).await.expect("h3");
        assert!(status.is_success(), "status {:?}", status);
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert!(
            v.get("result").is_some(),
            "Request {} should succeed",
            i + 1
        );
    }

    let (status, body) = h3_json_rpc(quinn_addr, request).await.expect("h3");
    assert_eq!(status.as_u16(), 429, "expected 429 Too Many Requests");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert!(v["error"].is_object());
    assert_eq!(v["error"]["code"], 429);
}

#[tokio::test]
#[ignore]
async fn quic_rpc_ping_smoke() {
    let quinn_addr: SocketAddr = "127.0.0.1:18332".parse().unwrap();
    let placeholder: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let rpc = Arc::new(RpcServer::new(placeholder));
    let server = QuinnRpcServer::new(quinn_addr, Arc::clone(&rpc));

    let _server_handle = tokio::spawn(async move {
        let _ = server.start().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let request = r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#;
    let (status, response_str) = h3_json_rpc(quinn_addr, request).await.expect("h3");

    assert!(status.is_success(), "HTTP status {:?}", status);
    let v: serde_json::Value = serde_json::from_str(&response_str).expect("json");

    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert!(v.get("result").is_some());
}

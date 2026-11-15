//! JSON-RPC client for the `blvm` CLI.
//!
//! Posts one HTTP/1 request to the local RPC server. The server already
//! links hyper, so this does not add a second HTTP stack.

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpStream;

/// POST `body` to `http://addr/`. `authorization` is the header value, when set.
pub async fn post_json(
    addr: SocketAddr,
    body: Vec<u8>,
    authorization: Option<&str>,
) -> Result<(u16, Vec<u8>)> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    let io = TokioIo::new(stream);
    let (mut sender, conn) = http1::handshake(io)
        .await
        .with_context(|| format!("HTTP handshake with {addr}"))?;
    tokio::spawn(async move {
        if let Err(err) = conn.await {
            tracing::debug!("local rpc client connection ended: {err}");
        }
    });

    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}/"))
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .context("build RPC request")?;
    let response = sender
        .send_request(request)
        .await
        .with_context(|| format!("send RPC request to {addr}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .context("read RPC response")?
        .to_bytes();
    Ok((status, bytes.to_vec()))
}

/// `Authorization: Basic` value for a JSON-RPC user and password.
pub fn basic_authorization(user: &str, password: &str) -> String {
    format!("Basic {}", base64_encode(&format!("{user}:{password}")))
}

fn base64_encode(input: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let n = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | (bytes[index + 2] as u32);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        index += 3;
    }
    let rest = bytes.len() - index;
    if rest == 1 {
        let n = (bytes[index] as u32) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let n = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

# QUIC RPC Server

## Overview

The reference-node optionally supports JSON-RPC over QUIC using Quinn, providing improved performance and security compared to the standard TCP RPC server.

## Features

- **Encryption**: Built-in TLS encryption via QUIC
- **Multiplexing**: Multiple concurrent requests over single connection
- **Better Performance**: Lower latency, better congestion control
- **Backward Compatible**: TCP RPC server always available

## Usage

### Basic (TCP Only - Default)

```rust
use bllvm_node::rpc::RpcManager;
use std::net::SocketAddr;

let tcp_addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
let mut rpc_manager = RpcManager::new(tcp_addr);
rpc_manager.start().await?;
```

### With QUIC Support

```rust
use bllvm_node::rpc::RpcManager;
use std::net::SocketAddr;

let tcp_addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
let quinn_addr: SocketAddr = "127.0.0.1:18332".parse().unwrap();

// Option 1: Create with both transports
#[cfg(feature = "quinn")]
let mut rpc_manager = RpcManager::with_quinn(tcp_addr, quinn_addr);

// Option 2: Enable QUIC after creation
let mut rpc_manager = RpcManager::new(tcp_addr);
#[cfg(feature = "quinn")]
rpc_manager.enable_quinn(quinn_addr);

rpc_manager.start().await?;
```

## Configuration

QUIC RPC requires the `quinn` feature flag:

```toml
[dependencies]
bllvm-node = { path = "../bllvm-node", features = ["quinn"] }
```

Or when building:

```bash
cargo build --features quinn
```

## Security Notes

- **Self-Signed Certificates**: Currently uses self-signed certificates for development
- **Production**: Should use proper certificate management for production
- **Authentication**: QUIC provides transport encryption but not application-level auth
- **Same Security Boundaries**: QUIC RPC has same security boundaries as TCP RPC (no wallet access)

## Client Usage

Clients need QUIC support. Example with `quinn`:

```rust
use quinn::Endpoint;
use std::net::SocketAddr;

let server_addr: SocketAddr = "127.0.0.1:18332".parse().unwrap();
let endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
let connection = endpoint.connect(server_addr, "localhost")?.await?;

// Open bidirectional stream
let (mut send, mut recv) = connection.open_bi().await?;

// Send JSON-RPC request
let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}"#;
send.write_all(request.as_bytes()).await?;
send.finish().await?;

// Read response
let mut response = Vec::new();
recv.read_to_end(&mut response).await?;
let response_str = String::from_utf8(response)?;
```

## Benefits Over TCP

1. **Encryption**: Built-in TLS, no need for separate TLS layer
2. **Multiplexing**: Multiple requests without head-of-line blocking
3. **Connection Migration**: Survives IP changes
4. **Lower Latency**: Better congestion control
5. **Stream-Based**: Natural fit for request/response patterns

## Limitations

- **Bitcoin Core Compatibility**: Bitcoin Core only supports TCP RPC
- **Client Support**: Requires QUIC-capable clients
- **Certificate Management**: Self-signed certs need proper handling for production
- **Network Requirements**: Some networks may block UDP/QUIC

## When to Use

- **High-Performance Applications**: When you need better performance than TCP
- **Modern Infrastructure**: When all clients support QUIC
- **Enhanced Security**: When you want built-in encryption without extra TLS layer
- **Internal Services**: When you control both client and server

## When Not to Use

- **Bitcoin Core Compatibility**: Need compatibility with Bitcoin Core tooling
- **Legacy Clients**: Clients that only support TCP/HTTP
- **Simple Use Cases**: TCP RPC is simpler and sufficient for most cases


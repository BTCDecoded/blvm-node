# QUIC RPC Server

## Overview

blvm-node optionally supports JSON-RPC over **HTTP/3 on QUIC** using Quinn + **`h3`**, sharing the same **`RpcServer`** instance as TCP HTTP JSON-RPC.

## Features

- **Encryption**: Built-in TLS encryption via QUIC
- **Multiplexing**: Multiple concurrent requests over single connection
- **Better Performance**: Lower latency, better congestion control
- **Backward Compatible**: TCP RPC server always available

## Usage

### Basic (TCP Only - Default)

```rust
use blvm_node::rpc::RpcManager;
use std::net::SocketAddr;

let tcp_addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
let mut rpc_manager = RpcManager::new(tcp_addr);
rpc_manager.start().await?;
```

### With QUIC Support

```rust
use blvm_node::rpc::RpcManager;
use std::net::SocketAddr;

let tcp_addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
let quinn_addr: SocketAddr = "127.0.0.1:18332".parse().unwrap(); // testnet example; regtest default RPC is 18443

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
blvm-node = { path = "../blvm-node", features = ["quinn"] }
```

Or when checking a build locally (prefer `check` over full `build` on resource-constrained hosts):

```bash
cargo check --features quinn
```

## Security Notes

- **Self-Signed Certificates**: The listener generates a short-lived self-signed certificate by default (development-style).
- **Production**: Use operational certificate provisioning appropriate for your deployment (or terminate TLS at a QUIC-aware proxy you trust).
- **HTTP/3 + `[rpc_auth]`**: QUIC JSON-RPC speaks **HTTP/3** (TLS ALPN **`h3`**). **`Authorization: Bearer …`** and all **`RpcAuthManager`** / **`rpc_auth.required`** semantics match **TCP HTTP JSON-RPC** (shared `dispatch_json_rpc_post_body` on the same **`Arc<RpcServer>`**).
- **Same handler surface**: QUIC HTTP/3 hits the live node handlers (storage, mempool, etc.), not a stub server.

## P2P QUIC

Transport preferences for peer connections (e.g. `quinn` feature for P2P) are separate from JSON-RPC over QUIC. This document describes **JSON-RPC over QUIC** only.

## Client Usage

Use an **HTTP/3** stack on top of QUIC (**ALPN `h3`**): issue **`POST /`** with **`Content-Type: application/json`** and the same JSON-RPC body as TCP HTTP. Supply **`Authorization`** headers exactly like HTTP/1.

Development/tests in-tree use **`h3`** + **`h3-quinn`** (see `tests/quic_rpc_smoke_tests.rs` behind **`--features quinn`**).

Raw “JSON bytes only” QUIC streams are **not** the RPC wire format anymore.

## Benefits Over TCP

1. **Encryption**: Built-in TLS, no need for separate TLS layer
2. **Multiplexing**: Multiple requests without head-of-line blocking
3. **Connection Migration**: Survives IP changes
4. **Lower Latency**: Better congestion control
5. **Stream-Based**: Natural fit for request/response patterns

## Limitations

- **Ecosystem tooling**: Most JSON-RPC clients assume TCP RPC
- **Client Support**: Requires QUIC-capable clients
- **Certificate Management**: Self-signed certs need proper handling for production
- **Network Requirements**: Some networks may block UDP/QUIC

## When to Use

- **High-Performance Applications**: When you need better performance than TCP
- **Modern Infrastructure**: When all clients support QUIC
- **Enhanced Security**: When you want built-in encryption without extra TLS layer
- **Internal Services**: When you control both client and server

## When Not to Use

- **Ecosystem tooling**: Need TCP-only RPC scripts
- **Legacy Clients**: Clients that only support TCP/HTTP
- **Simple Use Cases**: TCP RPC is simpler and sufficient for most cases


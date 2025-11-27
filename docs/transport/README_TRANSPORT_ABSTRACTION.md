# Transport Abstraction Layer

This document describes the transport abstraction layer implementation for `reference-node`, enabling support for both traditional TCP (Bitcoin P2P) and modern Iroh (QUIC-based) transports.

## Architecture

The transport abstraction provides a unified interface for different network transports:

```
NetworkManager
    └── Transport Trait (abstraction)
        ├── TcpTransport (Bitcoin P2P compatible)
        └── IrohTransport (QUIC-based, optional)
```

## Key Components

### 1. Transport Traits (`src/network/transport.rs`)

- **Transport**: Main trait for transport implementations
- **TransportConnection**: Trait for active connections (send/recv)
- **TransportListener**: Trait for accepting incoming connections
- **TransportAddr**: Enum supporting both TCP (`SocketAddr`) and Iroh (public key)
- **TransportPreference**: Runtime transport selection (TcpOnly, IrohOnly, Hybrid)

### 2. TCP Transport (`src/network/tcp_transport.rs`)

Fully implemented transport for Bitcoin P2P protocol compatibility:
- Uses traditional TCP sockets
- Maintains Bitcoin wire protocol format
- Compatible with standard Bitcoin nodes

### 3. Iroh Transport (`src/network/iroh_transport.rs`)

Skeleton implementation for QUIC-based transport:
- Uses Iroh for P2P networking
- Public key-based peer identity
- NAT traversal support
- **Status**: Skeleton complete, requires Iroh API integration for full functionality

### 4. Protocol Adapter (`src/network/protocol_adapter.rs`)

Handles message serialization between:
- Consensus-proof `NetworkMessage` types
- Transport-specific wire formats (TCP Bitcoin P2P vs Iroh message format)

### 5. Message Bridge (`src/network/message_bridge.rs`)

Bridges consensus-proof message processing with transport layer:
- Converts messages to/from transport formats
- Processes incoming messages
- Generates responses

### 6. Network Manager (`src/network/mod.rs`)

Updated to support multiple transports:
- Runtime transport selection
- Hybrid mode (both TCP and Iroh simultaneously)
- Backward compatible (defaults to TCP-only)

## Configuration

Transport preference can be configured via `NodeConfig`:

```rust
use bllvm_node::{NodeConfig, TransportPreferenceConfig};

let config = NodeConfig {
    transport_preference: TransportPreferenceConfig::TcpOnly, // or Hybrid, IrohOnly
    // ... other settings
};
```

## Usage

### Basic Usage (TCP-only, default)

```rust
use bllvm_node::network::{NetworkManager, transport::TransportPreference};
use std::net::SocketAddr;

let addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
let mut manager = NetworkManager::new(addr);

// Start with TCP transport
manager.start(addr).await?;
```

### Hybrid Mode (TCP + Iroh)

```rust
use bllvm_node::network::{NetworkManager, transport::TransportPreference};

let manager = NetworkManager::with_transport_preference(
    addr,
    100, // max peers
    TransportPreference::Hybrid, // Prefer Iroh, fallback to TCP
);
```

## Feature Flags

- **Default**: TCP-only (Bitcoin compatible)
- **`--features iroh`**: Enable Iroh transport support

## Status

- ✅ Transport abstraction layer: Complete
- ✅ TCP transport: Fully implemented
- ✅ NetworkManager integration: Complete
- ✅ Protocol adapter: Complete
- ✅ Message bridge: Complete
- ✅ Configuration support: Complete
- ⚠️ Iroh transport: Skeleton ready, needs API integration
- ⚠️ Comprehensive tests: Basic tests added, more needed

## Next Steps

1. Complete Iroh transport implementation (research Iroh 0.12 API)
2. Add comprehensive integration tests
3. Implement hybrid mode peer negotiation
4. Performance benchmarking (TCP vs Iroh)

## Backward Compatibility

The implementation maintains full backward compatibility:
- Default mode is TCP-only (same as before)
- Existing code using `NetworkManager::new()` continues to work
- Iroh is opt-in via feature flag
- TCP peers remain unaware of Iroh capability

## Documentation

- See `docs/IROH_INTEGRATION_ANALYSIS.md` for detailed Iroh integration analysis
- See `production-performance-optimizations.plan.md` for implementation plan


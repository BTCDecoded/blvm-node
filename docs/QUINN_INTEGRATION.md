# Quinn QUIC Transport Integration

## Overview

Quinn QUIC transport has been integrated as a standalone transport option alongside TCP and Iroh, providing direct QUIC connections without NAT traversal overhead.

## Features

- **Direct QUIC**: SocketAddr-based connections (like TCP) using QUIC protocol
- **All Transport Combinations**: Support for TCP/Iroh/Quinn/IrohQuinn/TcpIroh/TcpQuinn/TcpIrohQuinn via bitflags
- **Stratum V2 Support**: `quinn://` URL scheme for mining pool connections
- **Protocol Adapter**: Uses bincode serialization (same as Iroh) for simplified wire format

## Usage

### Basic Transport Selection

```rust
use bllvm_node::network::transport::TransportPreference;

// Single transport
let tcp_only = TransportPreference::TCP_ONLY;
let quinn_only = TransportPreference::QUINN_ONLY;
let iroh_only = TransportPreference::IROH_ONLY;

// Combinations
let tcp_quinn = TransportPreference::TCP | TransportPreference::QUINN;
let all_transports = TransportPreference::ALL; // TCP | IROH | QUINN
```

### Network Manager

```rust
use bllvm_node::network::{NetworkManager, transport::TransportPreference};

let manager = NetworkManager::with_transport_preference(
    listen_addr,
    max_peers,
    TransportPreference::TCP | TransportPreference::QUINN,
);
```

### Stratum V2 Mining

```rust
// Connect to mining pool via Quinn
let client = StratumV2Client::new("quinn://pool.example.com:3333".to_string());
client.connect().await?;
```

## Transport Comparison

| Feature | TCP | Quinn | Iroh |
|---------|-----|-------|------|
| Addressing | SocketAddr | SocketAddr | Public Key |
| NAT Traversal | No | No | Yes (DERP) |
| Encryption | No | Yes (QUIC) | Yes (QUIC) |
| Overhead | Low | Medium | Higher |
| Use Case | Bitcoin P2P | Direct servers | P2P with NAT |

## Implementation Details

### Quinn Transport (`quinn_transport.rs`)

- Implements `Transport` trait
- Uses QUIC unidirectional streams
- Length-prefixed messages (4-byte big-endian)
- Self-signed certificates for development (production needs proper certs)

### Protocol Serialization

Quinn uses bincode serialization (same as Iroh) for simplified wire format:
- Converts `NetworkMessage` → `ProtocolMessage` → bincode bytes
- More compact than JSON
- Compatible with existing protocol adapter

### Transport Preference System

Uses `bitflags` for flexible combinations:
- Individual flags: `TCP`, `IROH`, `QUINN`
- Combine with bitwise OR: `TCP | QUINN`
- Check with: `preference.allows_tcp()`, `preference.allows_quinn()`
- Get list: `preference.enabled_transports()`

## Testing

Comprehensive test coverage includes:

1. **Transport Tests** (`tests/integration/transport_tests.rs`)
   - Transport preference combinations
   - NetworkManager with Quinn
   - Address conversions

2. **Quinn Transport Tests** (`tests/integration/quinn_transport_tests.rs`)
   - Transport type verification
   - Listen/accept functionality
   - Address validation
   - Connection structure

3. **Stratum V2 Tests** (`tests/integration/stratum_v2_quinn_tests.rs`)
   - URL parsing
   - Multiple transport URL formats

## Configuration

### Cargo.toml

```toml
[features]
quinn = ["dep:quinn", "dep:rcgen", "dep:rustls"]

[dependencies]
quinn = { version = "0.11", optional = true }
rcgen = { version = "0.11", optional = true }
rustls = { version = "0.23", optional = true }
bitflags = "2.4"
```

### Runtime Configuration

```rust
use bllvm_node::config::{NodeConfig, TransportPreferenceConfig};

let config = NodeConfig {
    transport_preference: TransportPreferenceConfig::QuinnOnly,
    // ...
};
```

## Backward Compatibility

All existing code continues to work:
- `TransportPreference::TcpOnly` → `TransportPreference::TCP_ONLY`
- `TransportPreference::IrohOnly` → `TransportPreference::IROH_ONLY`
- `TransportPreference::Hybrid` → `TransportPreference::HYBRID`

## Future Improvements

1. **Certificate Verification**: Proper TLS certificate validation for production
2. **Connection Pooling**: Reuse QUIC connections where appropriate
3. **Performance Benchmarks**: Compare TCP vs Quinn vs Iroh
4. **Connection Migration**: Leverage QUIC's connection migration features
5. **Multi-stream Support**: Handle multiple QUIC streams per connection

## See Also

- [Transport Abstraction Documentation](./TRANSPORT_ABSTRACTION_COMPLETE.md)
- [Stratum V2 Implementation](./STRATUM_V2_IMPLEMENTATION_STATUS.md)
- [Iroh Integration](./IROH_COMPLETION_STATUS.md)


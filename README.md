# Reference Node

**Minimal Bitcoin implementation using protocol-engine for protocol abstraction and consensus-proof for consensus decisions.**

This crate provides a minimal, production-ready Bitcoin node implementation that uses the protocol-engine crate for protocol abstraction and consensus-proof for all consensus decisions. It adds only the non-consensus infrastructure: storage, networking, RPC, and orchestration.

## Architecture Position

This is **Tier 4** of the 5-tier Bitcoin Commons architecture (BLLVM technology stack):

```
1. Orange Paper (mathematical foundation)
2. consensus-proof (pure math implementation)
3. protocol-engine (Bitcoin abstraction)
4. reference-node (full node implementation) ← THIS CRATE
5. developer-sdk (governance infrastructure)
```

## Design Principles

1. **Zero Consensus Re-implementation**: All consensus logic from consensus-proof
2. **Protocol Abstraction**: Uses protocol-engine for variant support
3. **Pure Infrastructure**: Only adds storage, networking, RPC, orchestration
4. **Production Ready**: Full Bitcoin node functionality

## Configuration

### Protocol Variants

The reference-node supports multiple Bitcoin protocol variants:

- **Regtest** (default): Regression testing network for development
- **Testnet3**: Bitcoin test network
- **BitcoinV1**: Production Bitcoin mainnet

Usage:
```rust
use reference_node::{ReferenceNode, ProtocolVersion};

// Default: Regtest for safe development
let node = ReferenceNode::new(None)?;

// Explicit testnet
let testnet_node = ReferenceNode::new(Some(ProtocolVersion::Testnet3))?;

// Mainnet (use with caution)
let mainnet_node = ReferenceNode::new(Some(ProtocolVersion::BitcoinV1))?;
```

## Building

### Quick Start

```bash
git clone https://github.com/BTCDecoded/reference-node
cd reference-node
cargo build --release
```

The build automatically fetches consensus-proof from GitHub.

### Local Development

If you're developing both reference-node and consensus-proof:

1. Clone both repos:
   ```bash
   git clone https://github.com/BTCDecoded/consensus-proof
   git clone https://github.com/BTCDecoded/reference-node
   ```

2. Set up local override:
   ```bash
   cd reference-node
   mkdir -p .cargo
   echo '[patch."https://github.com/BTCDecoded/consensus-proof"]' > .cargo/config.toml
   echo 'consensus-proof = { path = "../consensus-proof" }' >> .cargo/config.toml
   ```

3. Build:
   ```bash
   cargo build
   ```

Changes to consensus-proof are now immediately reflected without git push.

## Testing

```bash
# Run all tests
cargo test

# Run with verbose output
cargo test -- --nocapture
```

## Usage

### Basic Node Creation

```rust
use reference_node::{ReferenceNode, ProtocolVersion};

// Default: Regtest for safe development
let node = ReferenceNode::new(None)?;

// Explicit testnet
let testnet_node = ReferenceNode::new(Some(ProtocolVersion::Testnet3))?;

// Mainnet (use with caution)
let mainnet_node = ReferenceNode::new(Some(ProtocolVersion::BitcoinV1))?;
```

### Running the Node

```bash
# Start node in regtest mode (default)
cargo run

# Start in testnet mode
cargo run -- --network testnet

# Start in mainnet mode (use with caution)
cargo run -- --network mainnet
```

## Security

See [SECURITY.md](SECURITY.md) for security policies and [BTCDecoded Security Policy](https://github.com/BTCDecoded/.github/blob/main/SECURITY.md) for organization-wide guidelines.

**Important**: This implementation is designed for pre-production testing and development. Additional hardening is required for production mainnet use.

## Dependencies

- **consensus-proof**: All consensus logic (git dependency)
- **tokio**: Async runtime for networking
- **serde**: Serialization
- **anyhow/thiserror**: Error handling
- **tracing**: Logging
- **clap**: CLI interface

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [BTCDecoded Contribution Guide](https://github.com/BTCDecoded/.github/blob/main/CONTRIBUTING.md).

## License

MIT License - see LICENSE file for details.

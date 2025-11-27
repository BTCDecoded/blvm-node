# Bitcoin Commons Node

Minimal Bitcoin node implementation using bllvm-protocol for protocol abstraction and bllvm-consensus for consensus decisions.

> **For verified system status**: See [SYSTEM_STATUS.md](https://github.com/BTCDecoded/.github/blob/main/SYSTEM_STATUS.md) in the BTCDecoded organization repository.

Provides a minimal Bitcoin node implementation using bllvm-protocol for protocol abstraction and bllvm-consensus for all consensus decisions. Adds only non-consensus infrastructure: storage, networking, RPC, and orchestration.

## Architecture Position

Tier 4 of the 6-tier Bitcoin Commons architecture (BLLVM technology stack):

```
1. bllvm-spec (Orange Paper - mathematical foundation)
2. bllvm-consensus (pure math implementation)
3. bllvm-protocol (Bitcoin abstraction)
4. bllvm-node (full node implementation)
5. bllvm-sdk (developer toolkit)
6. bllvm-commons (governance enforcement)
```

## Design Principles

1. **Zero Consensus Re-implementation**: All consensus logic from bllvm-consensus
2. **Protocol Abstraction**: Uses bllvm-protocol for variant support
3. **Pure Infrastructure**: Only adds storage, networking, RPC, orchestration
4. **Production Ready**: Full Bitcoin node functionality

## Features

- **Consensus Integration**: All consensus logic from bllvm-consensus
- **Protocol Support**: Multiple variants (mainnet, testnet, regtest)
- **RBF Support**: Configurable RBF modes (Disabled, Conservative, Standard, Aggressive)
- **Mempool Policies**: Comprehensive mempool configuration
- **RPC Interface**: Full RPC server implementation
- **Storage**: UTXO set management and chain state
- **Module System**: Process-isolated modules for optional features

See [Security](#security) for production considerations.

## Configuration

### RBF and Mempool Policies

Supports configurable RBF (Replace-By-Fee) modes and comprehensive mempool policies:

- **RBF Modes**: Disabled, Conservative, Standard (default), Aggressive
- **Mempool Policies**: Size limits, fee thresholds, eviction strategies, ancestor/descendant limits

See the [Configuration Guide](docs/CONFIGURATION_GUIDE.md) for details:
- [RBF Configuration](docs/RBF_CONFIGURATION.md) - Detailed RBF mode configuration
- [Mempool Policies](docs/MEMPOOL_POLICIES.md) - Detailed mempool policy configuration

### Protocol Variants

Supports multiple Bitcoin protocol variants:

- **Regtest** (default): Regression testing network for development
- **Testnet3**: Bitcoin test network
- **BitcoinV1**: Production Bitcoin mainnet

```rust
use bllvm_node::{Node, NodeConfig};

// Default: Regtest for safe development
let config = NodeConfig::default();
let node = Node::new(config)?;

// Explicit testnet
let mut config = NodeConfig::default();
config.network = ProtocolVersion::Testnet3;
let testnet_node = Node::new(config)?;
```

## Building

### Quick Start

```bash
git clone https://github.com/BTCDecoded/bllvm-node
cd bllvm-node
cargo build --release
```

The build automatically fetches bllvm-consensus from GitHub.

### Local Development

If you're developing both bllvm-node and bllvm-consensus:

1. Clone both repos:
   ```bash
   git clone https://github.com/BTCDecoded/bllvm-consensus
   git clone https://github.com/BTCDecoded/bllvm-node
   ```

2. Set up local override:
   ```bash
   cd bllvm-node
   mkdir -p .cargo
   echo '[patch."https://github.com/BTCDecoded/bllvm-consensus"]' > .cargo/config.toml
   echo 'bllvm-consensus = { path = "../bllvm-consensus" }' >> .cargo/config.toml
   ```

3. Build:
   ```bash
   cargo build
   ```

Changes to bllvm-consensus are now immediately reflected without git push.

## Testing

```bash
# Run all tests
cargo test

# Run with verbose output
cargo test -- --nocapture
```

## Usage

### Running the Node

```bash
# Start node in regtest mode (default)
cargo run

# Start in testnet mode
cargo run -- --network testnet

# Start in mainnet mode (use with caution)
cargo run -- --network mainnet
```

### Programmatic Usage

```rust
use bllvm_node::{Node, NodeConfig};

// Default: Regtest for safe development
let config = NodeConfig::default();
let node = Node::new(config)?;

// Start the node
node.start().await?;
```

See [docs/](docs/) for detailed documentation including:
- [Configuration Guide](docs/CONFIGURATION_GUIDE.md) - Complete configuration options
- [Module System](modules/README.md) - Process-isolated module system
- [RPC Reference](docs/RPC_REFERENCE.md) - JSON-RPC API documentation

## Security

See [SECURITY.md](SECURITY.md) for security policies and [BTCDecoded Security Policy](https://github.com/BTCDecoded/.github/blob/main/SECURITY.md) for organization-wide guidelines.

Additional hardening required for production mainnet use.

## Dependencies

- **bllvm-consensus**: All consensus logic (git dependency)
- **tokio**: Async runtime for networking
- **serde**: Serialization
- **anyhow/thiserror**: Error handling
- **tracing**: Logging
- **clap**: CLI interface

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [BTCDecoded Contribution Guide](https://github.com/BTCDecoded/.github/blob/main/CONTRIBUTING.md).

## License

MIT License - see LICENSE file for details.

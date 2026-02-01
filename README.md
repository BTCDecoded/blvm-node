# Bitcoin Commons Node

Minimal Bitcoin node implementation using blvm-protocol for protocol abstraction and blvm-consensus for consensus decisions.

> **📚 Comprehensive Documentation**: See [blvm-docs](../blvm-docs/) for complete system documentation.  
> **For verified system status**: See [SYSTEM_STATUS.md](https://github.com/BTCDecoded/.github/blob/main/SYSTEM_STATUS.md) in the BTCDecoded organization repository.

Provides a minimal Bitcoin node implementation using blvm-protocol for protocol abstraction and blvm-consensus for all consensus decisions. Adds only non-consensus infrastructure: storage, networking, RPC, and orchestration.

## Architecture Position

Tier 4 of the 6-tier Bitcoin Commons architecture (BLVM technology stack):

```
1. blvm-spec (Orange Paper - mathematical foundation)
2. blvm-consensus (pure math implementation)
3. blvm-protocol (Bitcoin abstraction)
4. blvm-node (full node implementation)
5. blvm-sdk (developer toolkit)
6. blvm-commons (governance enforcement)
```

## Design Principles

1. **Zero Consensus Re-implementation**: All consensus logic from blvm-consensus
2. **Protocol Abstraction**: Uses blvm-protocol for variant support
3. **Pure Infrastructure**: Only adds storage, networking, RPC, orchestration
4. **Production Ready**: Full Bitcoin node functionality

## Features

- **Consensus Integration**: All consensus logic from blvm-consensus
- **Protocol Support**: Multiple variants (mainnet, testnet, regtest)
- **RBF Support**: Configurable RBF modes (Disabled, Conservative, Standard, Aggressive)
- **Mempool Policies**: Comprehensive mempool configuration with 5 eviction strategies
- **Payment Processing**: CTV (CheckTemplateVerify) support for advanced payment flows
- **Advanced Indexing**: Address and value range indexing for efficient queries
- **RPC Interface**: Full RPC server implementation with Bitcoin Core compatibility
- **Storage**: UTXO set management and chain state with multiple backends (sled, redb, rocksdb)
- **Module System**: Process-isolated modules for optional features
- **P2P Governance**: Governance message relay via P2P protocol

See [Security](#security) for production considerations.

### Storage Backends

Supports multiple database backends via feature flags:

- **sled** (default): Embedded key-value store
- **redb**: Embedded database with ACID transactions
- **rocksdb** (optional): High-performance database with Bitcoin Core compatibility

**RocksDB Features:**
- Automatic Bitcoin Core data directory detection
- Direct access to Bitcoin Core block files (`blk*.dat`)
- Bitcoin Core format parsing and storage integration

Enable RocksDB with the `rocksdb` feature:
```bash
cargo build --features rocksdb
```

**Note**: RocksDB requires `libclang` system dependency. RocksDB and erlay features are mutually exclusive due to dependency conflicts.

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
use blvm_node::{Node, NodeConfig};

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
git clone https://github.com/BTCDecoded/blvm-node
cd blvm-node
cargo build --release
```

The build automatically fetches blvm-consensus from GitHub.

### Local Development

If you're developing both blvm-node and blvm-consensus:

1. Clone both repos:
   ```bash
   git clone https://github.com/BTCDecoded/blvm-consensus
   git clone https://github.com/BTCDecoded/blvm-node
   ```

2. Set up local override:
   ```bash
   cd blvm-node
   mkdir -p .cargo
   echo '[patch."https://github.com/BTCDecoded/blvm-consensus"]' > .cargo/config.toml
   echo 'blvm-consensus = { path = "../blvm-consensus" }' >> .cargo/config.toml
   ```

3. Build:
   ```bash
   cargo build
   ```

Changes to blvm-consensus are now immediately reflected without git push.

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
use blvm_node::{Node, NodeConfig};

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

- **blvm-consensus**: All consensus logic (git dependency)
- **tokio**: Async runtime for networking
- **serde**: Serialization
- **anyhow/thiserror**: Error handling
- **tracing**: Logging
- **clap**: CLI interface

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [BTCDecoded Contribution Guide](https://github.com/BTCDecoded/.github/blob/main/CONTRIBUTING.md).

## License

MIT License - see LICENSE file for details.

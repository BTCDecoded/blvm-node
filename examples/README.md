# bllvm-node Examples

This directory contains example code demonstrating how to use bllvm-node.

## Examples

### electrum-integration.rs

**Purpose**: Generate configuration for Electrum wallet integration

**What it does**:
- Creates a `config.toml` optimized for Electrum
- Configures RPC server on localhost
- Sets up testnet/mainnet ports

**Usage**:
```bash
cargo run --example electrum-integration
# Generates electrum-config.toml

# Then start node with:
bllvm-node --config electrum-config.toml --network testnet
```

**Output**: `electrum-config.toml` file ready to use

---

### wallet-integration.rs

**Purpose**: Show RPC API usage for wallet integration

**What it does**:
- Demonstrates RPC request format
- Shows essential methods for wallets
- Provides integration checklist

**Usage**:
```bash
cargo run --example wallet-integration
```

**Output**: Prints example RPC requests and integration checklist

**Note**: This shows the request format. To test with a running node:
1. Start node: `bllvm-node --network testnet`
2. Use curl or HTTP client to send requests
3. Or use bllvm-sdk for Rust integration

---

## Integration Workflow

### For Electrum

1. **Generate config**:
   ```bash
   cargo run --example electrum-integration
   ```

2. **Start node**:
   ```bash
   bllvm-node --config electrum-config.toml --network testnet
   ```

3. **Configure Electrum**:
   - Tools → Network → Server
   - Enter: `127.0.0.1`
   - Port: `18332` (testnet) or `8332` (mainnet)

### For Custom Wallet

1. **Review RPC examples**:
   ```bash
   cargo run --example wallet-integration
   ```

2. **Implement RPC client**:
   - Use HTTP client (reqwest, curl, etc.)
   - Send JSON-RPC requests
   - Handle responses

3. **Essential methods**:
   - `getblockchaininfo` - Chain state
   - `gettxout` - UTXO queries
   - `getrawtransaction` - Transaction data
   - `sendrawtransaction` - Broadcast transactions
   - `estimatesmartfee` - Fee estimation

---

## See Also

- **Integration Guide**: `docs/INTEGRATION_GUIDE.md`
- **RPC Reference**: `docs/RPC_REFERENCE.md`
- **Quick Start**: `../bLLVM_NODE_QUICK_START.md`


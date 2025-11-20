# Integration Guide: Using bllvm-node with Existing Tools

This guide shows how to integrate bllvm-node with existing Bitcoin tools and services.

---

## Table of Contents

1. [Electrum Wallet Integration](#electrum-wallet-integration)
2. [General Wallet Integration](#general-wallet-integration)
3. [Exchange Integration](#exchange-integration)
4. [Mining Pool Integration](#mining-pool-integration)
5. [Block Explorer Integration](#block-explorer-integration)
6. [Bitcoin Core Config Migration](#bitcoin-core-config-migration)

---

## Electrum Wallet Integration

### Quick Start

1. **Start bllvm-node**:
   ```bash
   bllvm-node --network testnet --rpc-bind 127.0.0.1 --rpc-port 18332
   ```

2. **Configure Electrum**:
   - Open Electrum
   - Go to: Tools → Network → Server
   - Uncheck "Select server automatically"
   - Enter: `127.0.0.1`
   - Port: `18332` (testnet) or `8332` (mainnet)
   - Protocol: TCP
   - Click "Close"

3. **Electrum will now connect to your local node!**

### Required RPC Methods

bllvm-node implements all methods Electrum needs:

- ✅ `gettxout` - Check UTXO existence and value
- ✅ `getrawtransaction` - Get transaction data
- ✅ `getblock` - Get block data
- ✅ `getblockheader` - SPV verification
- ✅ `sendrawtransaction` - Broadcast transactions
- ✅ `getblockchaininfo` - Chain state

### Example Code

See `examples/electrum-integration.rs` for a complete example.

---

## General Wallet Integration

### RPC API Compatibility

bllvm-node implements Bitcoin Core-compatible JSON-RPC API:

```json
{
  "jsonrpc": "2.0",
  "method": "gettxout",
  "params": ["txid", 0, true],
  "id": 1
}
```

### Essential Methods for Wallets

**Balance Checking**:
- `gettxout` - Check if UTXO exists and get value
- `getrawtransaction` - Get transaction details

**Transaction Broadcasting**:
- `sendrawtransaction` - Broadcast signed transaction
- `testmempoolaccept` - Test if transaction would be accepted

**Fee Estimation**:
- `estimatesmartfee` - Get recommended fee rate

**Block Queries**:
- `getblock` - Get block data
- `getblockheader` - Get block header (SPV)
- `getblockhash` - Get block hash by height
- `getblockchaininfo` - Chain state

### Integration Workflow

1. **Wallet creates transaction**:
   - Query UTXOs using `gettxout`
   - Calculate fees using `estimatesmartfee`
   - Create raw transaction (wallet handles signing)

2. **Wallet signs transaction**:
   - Wallet manages private keys
   - Wallet signs transaction
   - Creates final raw transaction hex

3. **Wallet broadcasts transaction**:
   - Send to node via `sendrawtransaction`
   - Node validates and broadcasts to network

### Example Code

See `examples/wallet-integration.rs` for a complete example.

---

## Exchange Integration

### Required Features

**Blockchain Queries**:
- `getblock` - Get block data
- `getrawtransaction` - Get transaction data
- `gettxout` - Verify UTXO existence

**Transaction Broadcasting**:
- `sendrawtransaction` - Broadcast withdrawals

**Mempool Monitoring**:
- `getrawmempool` - List pending transactions
- `getmempoolentry` - Get transaction details

**Fee Estimation**:
- `estimatesmartfee` - Calculate withdrawal fees

### Configuration

```toml
# config.toml
[network]
protocol_version = "bitcoin-v1"  # Mainnet

[rpc_auth]
port = 8332
bind = "127.0.0.1"
# Add authentication for production
username = "exchange_user"
password = "secure_password"
allowed_ips = ["127.0.0.1", "10.0.0.0/8"]  # Internal network
```

### High Availability

For production exchanges, run multiple nodes:

1. **Primary Node**: Handles all requests
2. **Backup Node**: Standby for failover
3. **Load Balancer**: Distributes requests

See `docs/HIGH_AVAILABILITY.md` for details.

---

## Mining Pool Integration

### Required Methods

- ✅ `getblocktemplate` - Get block template for mining
- ✅ `submitblock` - Submit mined block
- ✅ `getmininginfo` - Mining statistics
- ✅ `estimatesmartfee` - Fee estimation

### Configuration

```toml
# config.toml
[network]
protocol_version = "bitcoin-v1"  # Mainnet

[rpc_auth]
port = 8332
bind = "0.0.0.0"  # Allow pool connections
username = "pool_user"
password = "secure_password"
allowed_ips = ["10.0.0.0/8"]  # Pool network
```

### Integration Steps

1. **Pool connects to node**:
   - Uses `getblocktemplate` to get work
   - Distributes work to miners

2. **Miners submit shares**:
   - Pool validates shares
   - Pool submits block via `submitblock`

3. **Node validates and broadcasts**:
   - Node validates block
   - Node broadcasts to network

---

## Block Explorer Integration

### Required Methods

**Block Queries**:
- `getblock` - Get block data
- `getblockhash` - Get block hash by height
- `getblockheader` - Get block header

**Transaction Queries**:
- `getrawtransaction` - Get transaction data
- `gettxout` - Get UTXO information

**Chain Queries**:
- `getblockchaininfo` - Chain state
- `getblockcount` - Current height
- `getbestblockhash` - Best block hash

### Configuration

```toml
# config.toml
[network]
protocol_version = "bitcoin-v1"  # Mainnet

[rpc_auth]
port = 8332
bind = "127.0.0.1"
# No authentication needed for local access
```

---

## Bitcoin Core Config Migration

### Automatic Conversion

Use the provided tool to convert Bitcoin Core config:

```bash
# Shell script
./tools/convert-bitcoin-core-config.sh ~/.bitcoin/bitcoin.conf

# Rust tool (more features)
cargo run --bin convert-bitcoin-core-config -- ~/.bitcoin/bitcoin.conf
```

### Manual Conversion

**Bitcoin Core** (`bitcoin.conf`):
```ini
testnet=1
rpcport=18332
rpcuser=myuser
rpcpassword=mypassword
maxconnections=8
addnode=1.2.3.4
```

**bllvm-node** (`config.toml`):
```toml
[network]
protocol_version = "testnet3"
max_peers = 8
persistent_peers = ["1.2.3.4:18333"]

[rpc_auth]
port = 18332
username = "myuser"
password = "mypassword"
```

### Important Notes

- **Data directories are NOT converted** - configure separately
- Some Bitcoin Core options may not have direct equivalents
- Review generated config and adjust as needed

---

## Testing Integration

### Quick Test

```bash
# Start node
bllvm-node --network testnet

# Test RPC
curl -X POST http://127.0.0.1:18332 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}'
```

### Expected Response

```json
{
  "jsonrpc": "2.0",
  "result": {
    "chain": "test",
    "blocks": 123456,
    "bestblockhash": "...",
    ...
  },
  "id": 1
}
```

---

## Troubleshooting

### Connection Issues

**Problem**: Can't connect to RPC
**Solution**: Check `rpc_auth.bind` and `allowed_ips` in config

**Problem**: Authentication failed
**Solution**: Verify `username` and `password` match

### Compatibility Issues

**Problem**: Method not found
**Solution**: Check `docs/RPC_REFERENCE.md` for available methods

**Problem**: Different response format
**Solution**: bllvm-node matches Bitcoin Core format exactly

---

## Next Steps

- See `examples/` directory for complete integration examples
- See `docs/RPC_REFERENCE.md` for full API documentation
- See `README.md` for general node setup


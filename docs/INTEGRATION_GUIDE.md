# Integration Guide: Using blvm-node with Existing Tools

This guide shows how to integrate blvm-node with existing Bitcoin tools and services.

---

## Table of Contents

1. [Electrum Wallet Integration](#electrum-wallet-integration)
2. [General Wallet Integration](#general-wallet-integration)
3. [Exchange Integration](#exchange-integration)
4. [Mining Pool Integration](#mining-pool-integration)
5. [Block Explorer Integration](#block-explorer-integration)
6. [Migrating from `bitcoin.conf`](#migrating-from-bitcoinconf)

---

## Electrum Wallet Integration

### Quick Start

1. **Start the node** (operator binary is **`blvm`**, not `blvm-node`):
   ```bash
   blvm --network testnet --listen-addr 127.0.0.1:18333 --rpc-addr 127.0.0.1:18332
   ```

2. **Configure Electrum**:
   - Open Electrum
   - Go to: Tools → Network → Server
   - Uncheck "Select server automatically"
   - Enter: `127.0.0.1`
   - Port: `18332` (testnet), `8332` (mainnet), or `18443` (regtest)
   - Protocol: TCP
   - Click "Close"

3. **Electrum will now connect to your local node!**

### Required RPC Methods

blvm-node implements all methods Electrum needs:

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

blvm-node implements JSON-RPC API aligned with common Bitcoin node docs:

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

`blvm.toml` uses **`NodeConfig`** (no `[network]` table). **RPC bind address** is set when starting the **`blvm`** binary (`--rpc-addr` / `BLVM_RPC_ADDR`). **`[rpc_auth]`** carries Bearer tokens, optional HTTP Basic (`username` / `password` for ckpool), certificates, and rate limits — not `port`, `bind`, or `rpcallowip` (see [`RpcAuthConfig`](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/rpc.rs)).

```toml
listen_addr = "0.0.0.0:8333"
protocol_version = "BitcoinV1"
max_peers = 100
transport_preference = "tcponly"

[rpc_auth]
required = true
# Production: use RPC_AUTH_TOKENS or token_file instead of committing secrets here
tokens = []
rate_limit_burst = 100
rate_limit_rate = 10
```

```bash
blvm --config /path/to/blvm.toml --network mainnet --rpc-addr 127.0.0.1:8332
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

- ✅ `getblocktemplate` - Get block template for mining (BIP22/23, `coinbasevalue` format)
- ✅ `submitblock` - Submit mined block
- ✅ `getmininginfo` - Mining statistics
- ✅ `getbestblockhash` / `getblockcount` / `getblockhash` - Chain tip queries
- ✅ `validateaddress` - Solo pool payout address checks (ckpool `-B` mode)
- ✅ `estimatesmartfee` - Fee estimation

### ckpool (solo mining + Bitaxe / Stratum V1)

**Stack:** synced `blvm` node → ckpool (`-B` solo) → miners on `:3333`.

1. **Node** (mainnet example):

```toml
listen_addr = "0.0.0.0:8333"
protocol_version = "BitcoinV1"
transport_preference = "tcponly"

[rpc_auth]
required = true
username = "ckpool"
password = "change-me"
```

```bash
blvm --config /path/to/blvm.toml --network mainnet --rpc-addr 127.0.0.1:8332
```

`[rpc_auth].username` / `password` enable **HTTP Basic** auth (Bitcoin Core / ckpool `auth` + `pass`). The password is automatically granted **admin** RPC access (required for `getblocktemplate` / `submitblock`). Bearer tokens in `tokens = [...]` are read-only unless also listed in `admin_tokens`.

Bind RPC to **loopback** (`--rpc-addr 127.0.0.1:8332`) — Basic auth is cleartext on the wire.

Optional: load **`blvm-zmq`** and set `hashblock = "tcp://127.0.0.1:28332"` for ckpool `zmqblock` (faster template refresh than polling).

2. **ckpool** (`ckpool.conf`):

```json
{
  "btcd": [
    {
      "url": "127.0.0.1:8332",
      "auth": "ckpool",
      "pass": "change-me",
      "notify": true
    }
  ],
  "mindiff": 1,
  "startdiff": 42
}
```

```bash
src/ckpool -B -c ckpool.conf
```

3. **Miners:** pool URL `host:3333`, username = payout address (`bc1…`), password = anything.

**Regtest lab:** use `--network regtest` (default RPC **18443** when `--rpc-addr` is omitted). `getblocktemplate` works after `generatetoaddress` seeds the chain.

**Smoke test** before starting ckpool:

```bash
curl -s -u ckpool:change-me -X POST http://127.0.0.1:18443 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"getblocktemplate","params":[{"rules":["segwit"]}],"id":1}'
```

Or run `examples/ckpool-solo/smoke-rpc.sh` against a live node. In-process regression tests live in `tests/ckpool_integration_tests.rs` (GBT, auth, `validateaddress`, chain-tip RPCs, Core-style block-hash hex, regtest `submitblock`).

Block hashes in RPC responses (`getbestblockhash`, `getblockhash`, GBT `previousblockhash`, `getblockchaininfo.bestblockhash`, etc.) use Bitcoin Core display order (byte-reversed SHA256d digest), matching ckpool’s `hex2bin` + `swap_256` expectations.

### Configuration (generic pools)

Same schema as [Exchange](#configuration): set **`listen_addr` / `protocol_version` / `[rpc_auth]`** in `blvm.toml`. For pool infrastructure exposing JSON-RPC beyond localhost, pass **`--rpc-addr 0.0.0.0:8332`** (or the address your pool uses) to **`blvm`**. Restrict access with **`[rpc_auth]`** tokens and firewall rules. **Bitcoin Core** options such as **`rpcallowip`** / **`rpcwhitelist`** in **`bitcoin.conf`** are not `RpcAuthConfig` fields — use host firewall or reverse-proxy policy (and BLVM token auth).

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
listen_addr = "127.0.0.1:8333"
protocol_version = "BitcoinV1"
transport_preference = "tcponly"

[rpc_auth]
required = false
```

```bash
blvm --config /path/to/blvm.toml --network mainnet --rpc-addr 127.0.0.1:8332
```

---

## Migrating from `bitcoin.conf`

**Scope:** **`bitcoin.conf`** is the **Bitcoin Core** configuration format. **BLVM never loads it natively** — use a converter to draft **`blvm.toml`**, then run **`blvm --config … --rpc-addr …`**.

**RPC auth:** Core’s **`rpcuser`** / **`rpcpassword`** map to **`[rpc_auth].username`** / **`password`** for **HTTP Basic** (ckpool, `curl -u`, legacy tools). The password is auto-granted **admin** RPC access. Alternatively use **`[rpc_auth].tokens`** with **`Authorization: Bearer &lt;token&gt;`** (read-only unless also in **`admin_tokens`**). Generate a strong secret (`openssl rand -hex 32`) rather than reusing weak defaults.

### Automatic conversion

From the **`blvm-node`** repo root (where `Cargo.toml` defines the binary), or using the **`blvm`** CLI:

```bash
# Shell helper — writes to second argument; output must be normalized (see below)
./tools/convert-bitcoin-core-config.sh ~/.bitcoin/bitcoin.conf generated-blvm.toml

cargo run --bin convert-bitcoin-core-config -- ~/.bitcoin/bitcoin.conf generated-blvm.toml

blvm config convert-core ~/.bitcoin/bitcoin.conf blvm.toml
```

Review the output and **normalize to real `NodeConfig` TOML**:

- Remove any **`[network]`** wrapper — use **top-level** `listen_addr`, `protocol_version`, `max_peers`, etc.
- Map Core **`rpcuser`** / **`rpcpassword`** to **`[rpc_auth].username`** / **`password`** (HTTP Basic), or to **`tokens`** / **`admin_tokens`** (Bearer). **Do not** keep **`port` / `bind` / `allowed_ips` under `[rpc_auth]`** — use **`blvm --rpc-addr`** for bind. Core **`rpcallowip`** is not representable here; use firewall rules.
- Replace nested **`[transport_preference]`** with **`transport_preference = "tcponly"`** (or another valid serde variant).
- Ensure **`[network_timing]`** uses keys **`blvm-node`** accepts (e.g. **`target_peer_count`** for outbound target).

**Prefer** `blvm config convert-core` plus the edits above; the older **shell** `convert-bitcoin-core-config.sh` has the same class of legacy output and is **not** a drop-in config.

### Manual conversion (side-by-side)

**Bitcoin Core only** — illustrative `bitcoin.conf` (INI-style):

```ini
# ~/.bitcoin/bitcoin.conf  (Bitcoin Core — not read by BLVM)
testnet=1
rpcport=18332
rpcuser=myuser
rpcpassword=mypassword
maxconnections=8
addnode=1.2.3.4
```

**BLVM `blvm.toml`** (`NodeConfig`):

```toml
protocol_version = "Testnet3"
max_peers = 8
persistent_peers = ["1.2.3.4:18333"]
transport_preference = "tcponly"

[rpc_auth]
required = true
tokens = ["replace-with-long-random-token"]  # or token_file / RPC_AUTH_TOKENS
```

```bash
blvm --config blvm.toml --network testnet --rpc-addr 127.0.0.1:18332
```

### Important notes

- **Data directories:** **`blvm config convert-core`** maps Core **`datadir=`** to **`[storage].data_dir`** when present. If that path is a synced Core tree, **`blvm start`** can auto-migrate to **`<datadir>/blvm/`** (see [Storage Backends — Bitcoin Core drop-in](https://github.com/BTCDecoded/blvm-docs/blob/main/src/node/storage-backends.md#bitcoin-core-drop-in-migrate-on-start) in published docs, or `blvm-docs/src/node/storage-backends.md` in the docs repo).
- **P2P port** on `addnode=` peers must match **their** listening port (e.g. testnet **18333**, not the RPC port).
- Many Core options have **no** BLVM equivalent; treat converter output as a **starting point**.
- **`blvm config convert-core`** / the standalone tools may emit a **`[network]`**-style stub for compatibility — **`NodeConfig`** uses **top-level** keys; merge or edit to match [current `NodeConfig` serde](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/mod.rs).

---

## Testing Integration

### Quick Test

```bash
# Start node (operator binary)
blvm --network testnet --rpc-addr 127.0.0.1:18332

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
**Solution**: blvm-node matches the converted field layout

---

## Next Steps

- See `examples/` directory for complete integration examples
- See `docs/RPC_REFERENCE.md` for full API documentation
- See `README.md` for general node setup


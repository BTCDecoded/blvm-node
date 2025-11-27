# RPC API Reference

## Overview

Implements a JSON-RPC 2.0 API compatible with Bitcoin Core RPC methods. API organized into categories: blockchain, network, mining, mempool, raw transactions, and control.

## Endpoints

**Default RPC Endpoint**: `http://localhost:18332` (regtest) or `http://localhost:8332` (mainnet)

**Protocol**: HTTP POST with JSON-RPC 2.0

**Content-Type**: `application/json`

## Authentication

Authentication is optional. When enabled, use:
- **Token-based**: `Authorization: Bearer <token>`
- **Certificate-based**: TLS client certificates

## Rate Limiting

Rate limiting is enforced per IP, per user, and per method. Default limits:
- Authenticated users: 100 burst, 10 req/sec
- Unauthenticated: 50 burst, 5 req/sec
- Per-method limits may override defaults

---

## Blockchain Methods

### getblockchaininfo

Returns blockchain state information.

**Parameters**: None

**Returns**:
```json
{
  "chain": "regtest",
  "blocks": 123456,
  "headers": 123456,
  "bestblockhash": "0000...",
  "difficulty": 4.656542373906925e-10,
  "mediantime": 1234567890,
  "verificationprogress": 1.0,
  "initialblockdownload": false,
  "chainwork": "0000...",
  "size_on_disk": 1234567890,
  "pruned": false,
  "pruneheight": null,
  "automatic_pruning": false,
  "warnings": ""
}
```

---

### getblock

Returns block information.

**Parameters**:
1. `hash` (string, required) - Block hash
2. `verbosity` (numeric, optional, default=1) - 0=hex, 1=JSON, 2=JSON with transaction details

**Returns**: Block data (format depends on verbosity)

**Example**:
```json
{
  "hash": "0000...",
  "confirmations": 1,
  "size": 285,
  "strippedsize": 285,
  "weight": 1140,
  "height": 123456,
  "version": 536870912,
  "versionHex": "20000000",
  "merkleroot": "0000...",
  "tx": ["txid1", "txid2"],
  "time": 1234567890,
  "mediantime": 1234567890,
  "nonce": 0,
  "bits": "207fffff",
  "difficulty": 4.656542373906925e-10,
  "chainwork": "0000...",
  "nTx": 2,
  "previousblockhash": "0000...",
  "nextblockhash": "0000..."
}
```

---

### getblockhash

Returns block hash for given height.

**Parameters**:
1. `height` (numeric, required) - Block height

**Returns**: Block hash (string)

---

### getblockheader

Returns block header information.

**Parameters**:
1. `hash` (string, required) - Block hash
2. `verbose` (boolean, optional, default=true) - Return JSON object vs hex string

**Returns**: Block header (JSON object or hex string)

---

### getbestblockhash

Returns hash of best (tip) block.

**Parameters**: None

**Returns**: Block hash (string)

---

### getblockcount

Returns current block height.

**Parameters**: None

**Returns**: Block height (numeric)

---

### getdifficulty

Returns current proof-of-work difficulty.

**Parameters**: None

**Returns**: Difficulty (numeric)

---

### gettxoutsetinfo

Returns UTXO set statistics.

**Parameters**: None

**Returns**:
```json
{
  "height": 123456,
  "bestblock": "0000...",
  "transactions": 1234567,
  "txouts": 2345678,
  "bogosize": 123456789,
  "hash_serialized_2": "0000...",
  "disk_size": 1234567890,
  "total_amount": 21000000.0
}
```

---

### verifychain

Verifies blockchain database.

**Parameters**:
1. `checklevel` (numeric, optional, default=3) - Verification level (0-4)
2. `nblocks` (numeric, optional, default=6) - Number of blocks to check (0=all)

**Returns**: `true` if verification passed, `false` otherwise

---

### getchaintips

Returns information about all known chain tips.

**Parameters**: None

**Returns**: Array of chain tip objects

---

### getchaintxstats

Returns statistics about total number and rate of transactions.

**Parameters**:
1. `nblocks` (numeric, optional) - Number of blocks to analyze
2. `blockhash` (string, optional) - Block hash to analyze from

**Returns**: Transaction statistics

---

### getblockstats

Returns per-block statistics.

**Parameters**:
1. `hash_or_height` (string|numeric, required) - Block hash or height
2. `stats` (array, optional) - Specific stats to return

**Returns**: Block statistics

---

### pruneblockchain

Prunes the blockchain to specified height.

**Parameters**:
1. `height` (numeric, required) - Height to prune to

**Returns**: Pruned height (numeric)

---

### getpruneinfo

Returns pruning information.

**Parameters**: None

**Returns**:
```json
{
  "pruned": false,
  "pruneheight": null,
  "automatic_pruning": false,
  "prune_target_size": 0
}
```

---

### invalidateblock

Permanently marks a block as invalid.

**Parameters**:
1. `blockhash` (string, required) - Block hash to invalidate

**Returns**: `null` on success

---

### reconsiderblock

Removes invalidation status of a block.

**Parameters**:
1. `blockhash` (string, required) - Block hash to reconsider

**Returns**: `null` on success

---

### waitfornewblock

Waits for a new block and returns its hash.

**Parameters**:
1. `timeout` (numeric, optional, default=0) - Timeout in seconds (0=no timeout)

**Returns**: Block hash and height

---

### waitforblock

Waits for a specific block.

**Parameters**:
1. `blockhash` (string, required) - Block hash to wait for
2. `timeout` (numeric, optional, default=0) - Timeout in seconds

**Returns**: Block hash and height

---

### waitforblockheight

Waits for a specific block height.

**Parameters**:
1. `height` (numeric, required) - Block height to wait for
2. `timeout` (numeric, optional, default=0) - Timeout in seconds

**Returns**: Block hash and height

---

### getblockfilter

Returns BIP 157 compact block filter.

**Parameters**:
1. `blockhash` (string, required) - Block hash
2. `filtertype` (string, optional, default="basic") - Filter type

**Returns**: Block filter (hex string)

---

### getindexinfo

Returns status of optional indexes.

**Parameters**: None

**Returns**: Index status information

---

## Network Methods

### getnetworkinfo

Returns network information.

**Parameters**: None

**Returns**:
```json
{
  "version": 70015,
  "subversion": "/reference-node:0.1.0/",
  "protocolversion": 70015,
  "localservices": "0000000000000001",
  "localrelay": true,
  "timeoffset": 0,
  "networkactive": true,
  "connections": 8,
  "networks": [...],
  "relayfee": 0.00001000,
  "incrementalfee": 0.00001000,
  "localaddresses": [],
  "warnings": ""
}
```

---

### getpeerinfo

Returns peer connection information.

**Parameters**: None

**Returns**: Array of peer objects

---

### getconnectioncount

Returns number of connections.

**Parameters**: None

**Returns**: Connection count (numeric)

---

### ping

Pings all connected peers.

**Parameters**: None

**Returns**: `null` on success

---

### addnode

Adds a node to the connection list.

**Parameters**:
1. `node` (string, required) - Node address (IP:port)
2. `command` (string, required) - "add", "remove", "onetry"

**Returns**: `null` on success

---

### disconnectnode

Disconnects from a peer.

**Parameters**:
1. `address` (string, optional) - Peer address
2. `nodeid` (numeric, optional) - Peer node ID

**Returns**: `null` on success

---

### getnettotals

Returns network traffic statistics.

**Parameters**: None

**Returns**:
```json
{
  "totalbytesrecv": 1234567890,
  "totalbytessent": 1234567890,
  "timemillis": 1234567890123
}
```

---

### clearbanned

Clears all banned IPs.

**Parameters**: None

**Returns**: `null` on success

---

### setban

Bans or unbans an IP address.

**Parameters**:
1. `subnet` (string, required) - IP address or subnet
2. `command` (string, required) - "add" or "remove"
3. `bantime` (numeric, optional) - Ban duration in seconds
4. `absolute` (boolean, optional) - Use absolute time

**Returns**: `null` on success

---

### listbanned

Lists all banned IPs.

**Parameters**: None

**Returns**: Array of banned IP objects

---

### getaddednodeinfo

Returns information about manually added nodes.

**Parameters**:
1. `dummy` (boolean, required) - Dummy parameter
2. `node` (string, optional) - Specific node address

**Returns**: Array of node information objects

---

### getnodeaddresses

Returns known node addresses.

**Parameters**:
1. `count` (numeric, optional, default=1) - Number of addresses to return

**Returns**: Array of node address objects

---

### setnetworkactive

Enables or disables network activity.

**Parameters**:
1. `state` (boolean, required) - `true` to enable, `false` to disable

**Returns**: `null` on success

---

## Mining Methods

### getmininginfo

Returns mining information.

**Parameters**: None

**Returns**:
```json
{
  "blocks": 123456,
  "currentblocksize": 285,
  "currentblocktx": 1,
  "difficulty": 4.656542373906925e-10,
  "networkhashps": 0.0,
  "pooledtx": 0,
  "chain": "regtest",
  "warnings": ""
}
```

---

### getblocktemplate

Returns block template for mining.

**Parameters**:
1. `template_request` (object, optional) - Template request options

**Returns**:
```json
{
  "capabilities": ["proposal"],
  "version": 536870912,
  "rules": ["segwit"],
  "vbavailable": {},
  "vbrequired": 0,
  "previousblockhash": "0000...",
  "transactions": [...],
  "coinbaseaux": {},
  "coinbasevalue": 5000000000,
  "longpollid": "...",
  "target": "0000...",
  "mintime": 1234567890,
  "mutable": ["time", "transactions", "prevblock"],
  "noncerange": "00000000ffffffff",
  "sigoplimit": 80000,
  "sizelimit": 4000000,
  "weightlimit": 4000000,
  "curtime": 1234567890,
  "bits": "207fffff",
  "height": 123456
}
```

---

### submitblock

Submits a new block to the network.

**Parameters**:
1. `hexdata` (string, required) - Serialized block (hex)
2. `dummy` (string, optional) - Dummy parameter

**Returns**: `null` on success, error string on failure

---

### estimatesmartfee

Estimates fee rate for confirmation target.

**Parameters**:
1. `conf_target` (numeric, required) - Confirmation target (blocks)
2. `estimate_mode` (string, optional) - "UNSET", "ECONOMICAL", "CONSERVATIVE"

**Returns**:
```json
{
  "feerate": 0.00001000,
  "blocks": 2
}
```

---

### prioritisetransaction

Accepts the transaction into mined blocks at a higher priority.

**Parameters**:
1. `txid` (string, required) - Transaction ID
2. `dummy` (numeric, optional) - Dummy parameter
3. `fee_delta` (numeric, required) - Fee delta (satoshis)

**Returns**: `true` on success

---

## Mempool Methods

### getmempoolinfo

Returns mempool information.

**Parameters**: None

**Returns**:
```json
{
  "loaded": true,
  "size": 123,
  "bytes": 30750,
  "usage": 30750,
  "maxmempool": 300000000,
  "mempoolminfee": 0.00001000,
  "minrelaytxfee": 0.00001000
}
```

---

### getrawmempool

Returns transaction IDs in mempool.

**Parameters**:
1. `verbose` (boolean, optional, default=false) - Return verbose information

**Returns**: Array of transaction IDs or objects

---

### savemempool

Saves mempool to disk.

**Parameters**: None

**Returns**: `null` on success

---

### getmempoolancestors

Returns mempool ancestors of a transaction.

**Parameters**:
1. `txid` (string, required) - Transaction ID
2. `verbose` (boolean, optional, default=false) - Return verbose information

**Returns**: Array of transaction IDs or objects

---

### getmempooldescendants

Returns mempool descendants of a transaction.

**Parameters**:
1. `txid` (string, required) - Transaction ID
2. `verbose` (boolean, optional, default=false) - Return verbose information

**Returns**: Array of transaction IDs or objects

---

### getmempoolentry

Returns mempool entry for a transaction.

**Parameters**:
1. `txid` (string, required) - Transaction ID

**Returns**: Mempool entry object

---

## Raw Transaction Methods

### sendrawtransaction

Submits a raw transaction to the network.

**Parameters**:
1. `hexstring` (string, required) - Serialized transaction (hex)
2. `maxfeerate` (numeric, optional) - Maximum fee rate

**Returns**: Transaction ID (string)

---

### testmempoolaccept

Tests if a transaction would be accepted to mempool.

**Parameters**:
1. `rawtxs` (array, required) - Array of raw transactions (hex)
2. `maxfeerate` (numeric, optional) - Maximum fee rate

**Returns**: Array of acceptance results

---

### decoderawtransaction

Decodes a raw transaction.

**Parameters**:
1. `hexstring` (string, required) - Serialized transaction (hex)
2. `iswitness` (boolean, optional) - Whether transaction is SegWit

**Returns**: Decoded transaction object

---

### getrawtransaction

Returns raw transaction data.

**Parameters**:
1. `txid` (string, required) - Transaction ID
2. `verbose` (boolean, optional, default=false) - Return verbose information
3. `blockhash` (string, optional) - Block hash to search in

**Returns**: Transaction data (hex string or JSON object)

---

### gettxout

Returns transaction output information.

**Parameters**:
1. `txid` (string, required) - Transaction ID
2. `n` (numeric, required) - Output index
3. `include_mempool` (boolean, optional, default=true) - Include mempool

**Returns**: Transaction output object or `null`

---

### gettxoutproof

Returns merkle proof for a transaction.

**Parameters**:
1. `txids` (array, required) - Array of transaction IDs
2. `blockhash` (string, optional) - Block hash

**Returns**: Merkle proof (hex string)

---

### verifytxoutproof

Verifies a merkle proof.

**Parameters**:
1. `proof` (string, required) - Merkle proof (hex)

**Returns**: Array of transaction IDs proven by the proof

---

## Control Methods

### stop

Stops the node.

**Parameters**: None

**Returns**: `"bllvm-node stopping"`

---

### uptime

Returns node uptime.

**Parameters**: None

**Returns**: Uptime in seconds (numeric)

---

### getmemoryinfo

Returns memory usage information.

**Parameters**:
1. `mode` (string, optional, default="stats") - "stats" or "mallocinfo"

**Returns**: Memory usage information

---

### getrpcinfo

Returns RPC server information.

**Parameters**: None

**Returns**:
```json
{
  "active_commands": ["getblockchaininfo", "getblock", ...],
  "logpath": ""
}
```

---

### help

Returns help for RPC methods.

**Parameters**:
1. `command` (string, optional) - Specific command to get help for

**Returns**: Help text (string)

---

### logging

Controls logging levels.

**Parameters**:
1. `include` (array, optional) - Categories to include
2. `exclude` (array, optional) - Categories to exclude

**Returns**: Logging configuration

---

### gethealth

Returns node health status.

**Parameters**: None

**Returns**: Health status object

---

### getmetrics

Returns Prometheus metrics.

**Parameters**: None

**Returns**: Prometheus-formatted metrics (text)

---

## Error Responses

All methods return JSON-RPC 2.0 error responses on failure:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32602,
    "message": "Invalid params"
  }
}
```

**Error Codes**:
- `-32700`: Parse error
- `-32600`: Invalid Request
- `-32601`: Method not found
- `-32602`: Invalid params
- `-32603`: Internal error
- `-32000` to `-32099`: Server errors

---

## Related Documentation

- [High Availability](HIGH_AVAILABILITY.md) - Health checks and metrics
- [Configuration](../bllvm-docs/CONFIGURATION.md) - Configuration reference
- [Module System](MODULE_SYSTEM.md) - Module system documentation

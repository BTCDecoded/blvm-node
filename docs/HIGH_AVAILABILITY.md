# High Availability Features

## Overview

blvm-node implements Phase 2 and 3 high availability features for production deployment: Prometheus metrics export, health check endpoints, disk space monitoring, peer reconnection, enhanced rate limiting, and structured logging.

## Metrics Endpoint

### Prometheus Metrics Export

**Endpoint**: `GET /metrics`

**Purpose**: Exports Prometheus-formatted metrics for monitoring.

**Metrics Exported**:
- Block processing metrics (blocks processed, validation time)
- Network metrics (peers connected, bytes sent/received)
- Storage metrics (database size, UTXO count)
- RPC metrics (requests processed, errors)
- Mempool metrics (transaction count, size)

**Example** (use the same host:port as `--rpc-addr`; mainnet **8332**, testnet **18332**, regtest **18443**):

```bash
curl http://localhost:8332/metrics
```

**Response Format**: Prometheus text format

**Usage**: Configure Prometheus to scrape this endpoint for monitoring dashboards.

---

## Health Check Endpoints

### Basic Health Check

**Endpoint**: `GET /health`

**Purpose**: Simple health check for load balancers.

**Response**:
```json
{
  "status": "healthy",
  "timestamp": 1234567890
}
```

**Status Codes**:
- `200 OK`: Node is healthy
- `503 Service Unavailable`: Node is unhealthy

---

### Liveness Probe

**Endpoint**: `GET /health/live`

**Purpose**: Kubernetes liveness probe - indicates if node process is running.

**Response**:
```json
{
  "status": "alive"
}
```

**Status Codes**:
- `200 OK`: Process is alive
- `503 Service Unavailable`: Process is dead/unresponsive

---

### Readiness Probe

**Endpoint**: `GET /health/ready`

**Purpose**: Kubernetes readiness probe - indicates if node is ready to serve requests.

**Response**:
```json
{
  "status": "ready",
  "chain_initialized": true,
  "storage_available": true
}
```

**Status Codes**:
- `200 OK`: Node is ready
- `503 Service Unavailable`: Node is not ready (e.g., initializing chain)

---

### Detailed Health Check

**Endpoint**: `GET /health/detailed`

**Purpose**: Comprehensive health status for debugging.

**Response**:
```json
{
  "status": "healthy",
  "chain": {
    "initialized": true,
    "height": 123456,
    "tip_hash": "0000..."
  },
  "storage": {
    "available": true,
    "size_bytes": 1234567890
  },
  "network": {
    "peers_connected": 8,
    "peers_max": 100
  },
  "rpc": {
    "enabled": true,
    "requests_processed": 12345
  }
}
```

---

## Disk Space Monitoring

Blockchain storage growth is handled primarily via **`[storage.pruning]`** (see [`PruningConfig`](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/storage.rs)). There is **no** `pruning_threshold_gb` / `pruning_target_gb` on `StorageConfig` in this tree.

**Example (normal pruning)**:

```toml
[storage]
data_dir = "/var/lib/blvm"
database_backend = "auto"

[storage.pruning]
mode = { type = "normal", keep_from_height = 0, min_recent_blocks = 288 }
auto_prune = true
min_blocks_to_keep = 144
```

**Behavior**: Depends on pruning mode (Normal / Aggressive / Disabled / Custom); see blvm-docs pruning section and feature gates such as **`utxo-commitments`** for aggressive paths.

---

## Peer Reconnection

The node runs periodic background work including **peer reconnection** intervals (see [`BackgroundTaskConfig`](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/ibd.rs) on `NodeConfig`).

**Configurable interval** (optional):

```toml
[background_tasks]
peer_reconnection_interval_secs = 10
```

There is **no** `[network] reconnect_*` block in `NodeConfig`; reconnection policy is implemented inside the network stack, not via those placeholder keys.

---

## Rate Limiting

RPC rate limits use **`[rpc]`** (IP / connection limits when auth is off) and **`[rpc_auth]`** (token-bucket burst/rate). There is **no** `[rpc.auth]` or `per_method_limits` table in `NodeConfig`.

```toml
[rpc]
rate_limit_when_auth_disabled = true
ip_rate_limit_burst = 50
ip_rate_limit_rate = 5
max_connections_per_ip_per_minute = 10

[rpc_auth]
rate_limit_burst = 100
rate_limit_rate = 10
```

See [`RpcConfig`](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/rpc.rs) and [`RpcAuthConfig`](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/rpc.rs).

---

## Structured Logging

### Request IDs and Tracing

blvm-node uses structured logging with request IDs and tracing spans.

**Features**:
- Request IDs: Unique ID per RPC request
- Tracing spans: Hierarchical tracing context
- Request/response metrics: Logged with each request
- Client address tracking: Logged for each request

**Log Format**:
```
[2025-01-01T00:00:00Z INFO rpc_request] request_id=abc12345 method=getblockhash client_addr=127.0.0.1:12345 request_size=123
```

**Configuration** ([`LoggingConfig`](https://github.com/BTCDecoded/blvm-node/blob/main/src/config/mod.rs)):

```toml
[logging]
filter = "info"   # or use key alias: level = "info"
json_format = true
```

---

## Configuration

### Example: knobs that exist on `NodeConfig`

```toml
listen_addr = "0.0.0.0:8333"
max_peers = 100
transport_preference = "tcponly"

[background_tasks]
peer_reconnection_interval_secs = 10

[storage]
data_dir = "/var/lib/blvm"
database_backend = "auto"

[storage.pruning]
mode = { type = "normal", keep_from_height = 0, min_recent_blocks = 288 }
 
[rpc]
ip_rate_limit_burst = 50
ip_rate_limit_rate = 5

[rpc_auth]
rate_limit_burst = 100
rate_limit_rate = 10

[logging]
filter = "info"
json_format = true
```

Metrics and `/health*` paths depend on the running **`blvm`** / RPC stack build (feature set). Treat endpoint availability as **implementation-defined** and verify against your binary.

---

## Monitoring Setup

### Prometheus Configuration

```yaml
scrape_configs:
  - job_name: 'blvm-node'
    static_configs:
      - targets: ['localhost:8332']   # match --rpc-addr (testnet 18332, regtest 18443)
    metrics_path: '/metrics'
    scrape_interval: 15s
```

### Health Check Configuration

**Kubernetes**:
```yaml
livenessProbe:
  httpGet:
    path: /health/live
    port: 8332   # match --rpc-addr (testnet 18332, regtest 18443)
  initialDelaySeconds: 30
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /health/ready
    port: 8332   # match --rpc-addr
  initialDelaySeconds: 10
  periodSeconds: 5
```

**Load Balancer**:
- Health check endpoint: `/health`
- Health check interval: 10 seconds
- Unhealthy threshold: 3 failures

---

## Related Documentation

- [RPC Reference](RPC_REFERENCE.md) - Complete RPC API
- [Configuration Guide](CONFIGURATION_GUIDE.md) - Node configuration (this repo)
- [Production mainnet node](https://docs.thebitcoincommons.org/getting-started/first-node.html#production-mainnet-node) ([BLVM Documentation](https://docs.thebitcoincommons.org/))

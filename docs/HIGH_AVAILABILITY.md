# High Availability Features

## Overview

bllvm-node implements Phase 2 and 3 high availability features for production deployment: Prometheus metrics export, health check endpoints, disk space monitoring, peer reconnection, enhanced rate limiting, and structured logging.

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

**Example**:
```bash
curl http://localhost:18332/metrics
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

### Automatic Pruning

bllvm-node monitors disk space and automatically prunes old blocks when space is low.

**Configuration**:
```toml
[storage]
pruning_mode = "normal"  # or "aggressive", "custom", "disabled"
pruning_threshold_gb = 100  # Prune when disk usage exceeds this
pruning_target_gb = 80      # Prune down to this size
```

**Pruning Modes**:
- `disabled`: No automatic pruning
- `normal`: Prune old blocks, keep recent blocks
- `aggressive`: Prune aggressively, keep only recent blocks
- `custom`: Custom pruning configuration

**Behavior**:
- Monitors disk space periodically
- Triggers pruning when threshold exceeded
- Prunes to target size
- Logs pruning operations

---

## Peer Reconnection

### Automatic Reconnection

bllvm-node automatically reconnects to disconnected peers with exponential backoff.

**Features**:
- Exponential backoff: Reconnection attempts with increasing delays
- Quality-based prioritization: Reconnect to high-quality peers first
- Connection queue: Manages reconnection queue
- Max retries: Limits reconnection attempts

**Configuration**:
```toml
[network]
reconnect_enabled = true
reconnect_max_retries = 10
reconnect_initial_delay_secs = 5
reconnect_max_delay_secs = 3600
```

**Behavior**:
- Detects peer disconnections
- Adds peer to reconnection queue
- Attempts reconnection with exponential backoff
- Prioritizes high-quality peers
- Stops after max retries

---

## Rate Limiting

### Enhanced Rate Limiting

bllvm-node implements multi-layer rate limiting for RPC requests.

**Layers**:
1. **Per-IP Rate Limiting**: Limits requests per IP address
2. **Per-User Rate Limiting**: Limits requests per authenticated user
3. **Per-Method Rate Limiting**: Limits requests per RPC method

**Configuration**:
```toml
[rpc.auth]
rate_limit_enabled = true
rate_limit_rate = 100      # Requests per second
rate_limit_burst = 200     # Burst capacity
per_method_limits = {      # Per-method overrides
  "getblocktemplate" = { rate = 10, burst = 20 }
  "sendrawtransaction" = { rate = 5, burst = 10 }
}
```

**Rate Limiter**: Token bucket algorithm

**Response**: `429 Too Many Requests` when limit exceeded

---

## Structured Logging

### Request IDs and Tracing

bllvm-node uses structured logging with request IDs and tracing spans.

**Features**:
- Request IDs: Unique ID per RPC request
- Tracing spans: Hierarchical tracing context
- Request/response metrics: Logged with each request
- Client address tracking: Logged for each request

**Log Format**:
```
[2025-01-01T00:00:00Z INFO rpc_request] request_id=abc12345 method=getblockhash client_addr=127.0.0.1:12345 request_size=123
```

**Configuration**:
```toml
[logging]
format = "json"  # or "text"
level = "info"   # trace, debug, info, warn, error
```

---

## Configuration

### Complete HA Configuration

```toml
[network]
reconnect_enabled = true
reconnect_max_retries = 10
reconnect_initial_delay_secs = 5
reconnect_max_delay_secs = 3600

[storage]
pruning_mode = "normal"
pruning_threshold_gb = 100
pruning_target_gb = 80

[rpc]
metrics_enabled = true
health_checks_enabled = true

[rpc.auth]
rate_limit_enabled = true
rate_limit_rate = 100
rate_limit_burst = 200

[logging]
format = "json"
level = "info"
```

---

## Monitoring Setup

### Prometheus Configuration

```yaml
scrape_configs:
  - job_name: 'bllvm-node'
    static_configs:
      - targets: ['localhost:18332']
    metrics_path: '/metrics'
    scrape_interval: 15s
```

### Health Check Configuration

**Kubernetes**:
```yaml
livenessProbe:
  httpGet:
    path: /health/live
    port: 18332
  initialDelaySeconds: 30
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /health/ready
    port: 18332
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
- [Configuration](CONFIGURATION.md) - Configuration reference
- [Deployment](../bllvm-docs/DEPLOYMENT.md) - Production deployment

# blvm-node Configuration Guide

## Overview

Covers all configuration options for blvm-node, including RBF modes, mempool policies, and other node settings.

## Table of Contents

1. [RBF Configuration](#rbf-configuration)
2. [Mempool Policies](#mempool-policies)
3. [Network Configuration](#network-configuration)
4. [Storage Configuration](#storage-configuration)
5. [Module Configuration](#module-configuration)
6. [RPC Configuration](#rpc-configuration)
7. [Example Configurations](#example-configurations)

## RBF Configuration

See [RBF_CONFIGURATION.md](./RBF_CONFIGURATION.md) for detailed RBF configuration guide.

### Quick Reference

```toml
[rbf]
mode = "standard"  # disabled, conservative, standard, aggressive
min_fee_rate_multiplier = 1.1
min_fee_bump_satoshis = 1000
min_confirmations = 0
allow_package_replacements = false
max_replacements_per_tx = 10
cooldown_seconds = 60
```

## Mempool Policies

See [MEMPOOL_POLICIES.md](./MEMPOOL_POLICIES.md) for detailed mempool policy guide.

### Quick Reference

```toml
[mempool]
max_mempool_mb = 300
max_mempool_txs = 100000
min_relay_fee_rate = 1  # sat/vB
min_tx_fee = 1000
incremental_relay_fee = 1000
max_ancestor_count = 25
max_ancestor_size = 101000
max_descendant_count = 25
max_descendant_size = 101000
eviction_strategy = "lowest_fee_rate"
mempool_expiry_hours = 336
persist_mempool = false
mempool_persistence_path = "data/mempool.dat"
```

## Network Configuration

`NodeConfig` has **no** `[network]` table. Use top-level keys (and optional `[network_timing]`, etc.):

```toml
listen_addr = "0.0.0.0:8333"
max_peers = 100
protocol_version = "BitcoinV1"
transport_preference = "tcponly"
enable_self_advertisement = true
```

Advertising an **external** reachable address for inbound peers is **not** a single `external_address` field in current `NodeConfig` — use **`enable_self_advertisement`**, **`persistent_peers`**, and operational discovery (DNS, `addnode`-style peers) as appropriate. **JSON-RPC listen** is set by the **`blvm`** process (`--rpc-addr` / `BLVM_RPC_ADDR`), not a `bind_address` key in this file.

## Storage Configuration

```toml
[storage]
data_dir = "data"
database_backend = "auto"  # typical `blvm` build: heed3 (LMDB); or force "heed3" | "rocksdb" | "redb" | "tidesdb" | "sled"
```

Database backend options:
- **auto** (config default): **heed3 (LMDB)** when the `heed3` feature is enabled (standard release builds), else RocksDB, TidesDB, Redb, Sled — see `default_backend()` in `blvm-node`
- **heed3**: LMDB + rkyv UTXO encoding; zero-copy reads (requires `heed3` feature + liblmdb)
- **rocksdb**: High-performance; common on-disk layout interop
- **redb**: Pure-Rust embedded ACID database (common in minimal / no-RocksDB builds)
- **tidesdb**: LSM-tree store; requires TidesDB feature
- **sled**: Embedded key-value store (fallback tier)

### Advanced Indexing

```toml
[storage.indexing]
enable_address_index = false
enable_value_index = false
strategy = "eager"  # or "lazy"
max_indexed_addresses = 1000000
enable_compression = false
background_indexing = false
```

## Module Configuration

Node config can override module settings via `[modules.<name>]` (e.g. `[modules.selective-sync]`). Node values take precedence over module `config.toml`:

```toml
[modules.selective-sync]
database_backend = "redb"  # Example override; omit to inherit node / module_subprocess preference
```

### WebAssembly in-process modules (`wasm-modules`)

When **`blvm-sdk`** loads guests with wasmtime (`wasm-modules`), optional **`[modules.<name>]`** string keys configure embedder budgets (defaults are suitable for maintainer-signed modules; tighten for untrusted WASM):

| Key | Meaning | Default |
|-----|---------|--------|
| `wasm_max_linear_memory_bytes` | Cap for guest linear memory (bytes) | `33554432` (32 MiB) |
| `wasm_fuel_per_invocation` | Fuel reset before each guest entry (`dispatch_*`, string exports) | `50000000` |
| `wasm_max_instances` | Max Wasm instances in the store | `8` |
| `wasm_max_tables` | Max tables | `64` |
| `wasm_max_memories` | Max memories | `4` |

Example:

```toml
[modules.my-wasm-mod]
wasm_max_linear_memory_bytes = "16777216"
wasm_fuel_per_invocation = "10000000"
```

## RPC configuration

**RPC listen address** is determined by the **`blvm`** binary (`--rpc-addr` and `BLVM_RPC_ADDR`), not by a `bind_address` field in the config file.

The optional **`[rpc]`** table configures **limits and rate limits** on the JSON-RPC server:

```toml
[rpc]
max_request_size_bytes = 1048576
rate_limit_when_auth_disabled = true
ip_rate_limit_burst = 50
ip_rate_limit_rate = 5
max_connections_per_ip_per_minute = 10
batch_rate_multiplier_cap = 10
connection_rate_limit_window_seconds = 60
```

**Authentication and token rate limits** use **`[rpc_auth]`** (documented in the **blvm-docs** configuration reference).

Optional **`rest_api`** / QUIC RPC are separate features; see node docs for `RestApiConfig` / `quinn` when enabled.

**Transport × authentication** (TCP JSON-RPC, QUIC, REST): [RPC transport × authentication](https://docs.thebitcoincommons.org/security/rpc-transport-auth-matrix.html).

**Deployment posture** (exposure classes, P2P, backups, modules): [Deployment posture](https://docs.thebitcoincommons.org/security/deployment-posture.html).

## Example Configurations

### Exchange Node (Conservative)

```toml
[rbf]
mode = "conservative"
min_fee_rate_multiplier = 2.0
min_fee_bump_satoshis = 5000
min_confirmations = 1
max_replacements_per_tx = 3
cooldown_seconds = 300

[mempool]
max_mempool_mb = 500
max_mempool_txs = 200000
min_relay_fee_rate = 2
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 25
max_descendant_count = 25
persist_mempool = true
```

### Mining Pool (Aggressive)

```toml
[rbf]
mode = "aggressive"
min_fee_rate_multiplier = 1.05
min_fee_bump_satoshis = 500
allow_package_replacements = true
max_replacements_per_tx = 10
cooldown_seconds = 60

[mempool]
max_mempool_mb = 1000
max_mempool_txs = 500000
min_relay_fee_rate = 1
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 50
max_descendant_count = 50
```

### Standard Node (Default)

```toml
[rbf]
mode = "standard"

[mempool]
max_mempool_mb = 300
max_mempool_txs = 100000
min_relay_fee_rate = 1
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 25
max_descendant_count = 25
```

## Best Practices

1. **Exchanges**: Use conservative RBF and higher fee thresholds
2. **Miners**: Use aggressive RBF and larger mempool sizes
3. **General Users**: Use standard/default settings
4. **High-Throughput Nodes**: Increase size limits and use aggressive eviction

## See Also

- [RBF_CONFIGURATION.md](./RBF_CONFIGURATION.md) - Detailed RBF configuration
- [MEMPOOL_POLICIES.md](./MEMPOOL_POLICIES.md) - Detailed mempool policy configuration


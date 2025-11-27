# bllvm-node Configuration Guide

## Overview

Covers all configuration options for bllvm-node, including RBF modes, mempool policies, and other node settings.

## Table of Contents

1. [RBF Configuration](#rbf-configuration)
2. [Mempool Policies](#mempool-policies)
3. [Network Configuration](#network-configuration)
4. [Storage Configuration](#storage-configuration)
5. [RPC Configuration](#rpc-configuration)
6. [Example Configurations](#example-configurations)

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

```toml
[network]
max_peers = 100
bind_address = "0.0.0.0:8333"
external_address = "your.external.ip:8333"
```

## Storage Configuration

```toml
[storage]
data_dir = "data"
backend = "redb"  # or "sled"
enable_pruning = false
enable_indexing = false
```

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

## RPC Configuration

```toml
[rpc]
bind_address = "127.0.0.1:8332"
enable_auth = false
rate_limit_burst = 100
rate_limit_rate = 10
rest_api_addr = "127.0.0.1:8080"  # Optional, requires rest-api feature
```

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


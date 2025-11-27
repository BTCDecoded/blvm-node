# Mempool Policy Configuration Guide

## Overview

Supports comprehensive mempool policy configuration to control transaction acceptance, eviction, and limits. Enables fine-grained control over mempool behavior for different use cases.

## Configuration Parameters

### Size Limits

#### `max_mempool_mb`
Maximum mempool size in megabytes. Default: 300 MB

#### `max_mempool_txs`
Maximum number of transactions in mempool. Default: 100,000

### Fee Thresholds

#### `min_relay_fee_rate`
Minimum relay fee rate in satoshis per virtual byte. Transactions with fee rate below this are not relayed. Default: 1 sat/vB

#### `min_tx_fee`
Minimum transaction fee in satoshis (absolute minimum, regardless of size). Default: 1000 satoshis

#### `incremental_relay_fee`
Incremental relay fee for fee bumping. Default: 1000 satoshis

### Ancestor/Descendant Limits

#### `max_ancestor_count`
Maximum ancestor count (transaction + all ancestors). Default: 25

#### `max_ancestor_size`
Maximum ancestor size in virtual bytes. Default: 101,000 bytes (101 kB)

#### `max_descendant_count`
Maximum descendant count (transaction + all descendants). Default: 25

#### `max_descendant_size`
Maximum descendant size in virtual bytes. Default: 101,000 bytes (101 kB)

### Eviction Strategy

#### `eviction_strategy`
Transaction eviction strategy when mempool limits are reached:

- `lowest_fee_rate`: Evict lowest fee rate transactions first (Bitcoin Core default)
- `oldest_first`: Evict oldest transactions first (FIFO)
- `largest_first`: Evict largest transactions first (to free most space)
- `no_descendants_first`: Evict transactions with no descendants first (safest)
- `hybrid`: Combine fee rate and age

Default: `lowest_fee_rate`

### Expiry

#### `mempool_expiry_hours`
Mempool transaction expiry in hours. Transactions older than this are removed from mempool. Default: 336 hours (14 days)

### Persistence

#### `persist_mempool`
Enable mempool persistence (survives restarts). Default: false

#### `mempool_persistence_path`
Mempool persistence file path. Default: `data/mempool.dat`

## Configuration Examples

### Exchange Node (Conservative)
```toml
[mempool]
max_mempool_mb = 500
max_mempool_txs = 200000
min_relay_fee_rate = 2  # 2 sat/vB
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 25
max_descendant_count = 25
mempool_expiry_hours = 336
persist_mempool = true
```

### Mining Pool (Aggressive)
```toml
[mempool]
max_mempool_mb = 1000
max_mempool_txs = 500000
min_relay_fee_rate = 1  # 1 sat/vB
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 50
max_descendant_count = 50
mempool_expiry_hours = 336
persist_mempool = false
```

### Standard Node (Default)
```toml
[mempool]
max_mempool_mb = 300
max_mempool_txs = 100000
min_relay_fee_rate = 1  # 1 sat/vB
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 25
max_descendant_count = 25
mempool_expiry_hours = 336
```

## Eviction Strategies Explained

### Lowest Fee Rate
Evicts transactions with the lowest fee rate first. This maximizes the average fee rate of remaining transactions.

**Best for:**
- Mining pools
- Nodes prioritizing fee revenue
- Bitcoin Core compatibility

### Oldest First (FIFO)
Evicts the oldest transactions first, regardless of fee rate.

**Best for:**
- Nodes with strict time-based policies
- Preventing transaction aging issues

### Largest First
Evicts the largest transactions first to free the most space quickly.

**Best for:**
- Nodes with limited memory
- Quick space recovery

### No Descendants First
Evicts transactions with no descendants first. This prevents orphaning dependent transactions.

**Best for:**
- Nodes prioritizing transaction package integrity
- Preventing cascading evictions

### Hybrid
Combines fee rate and age with configurable weights.

**Best for:**
- Custom eviction policies
- Balancing multiple factors

## Ancestor/Descendant Limits

Ancestor/descendant limits prevent transaction package spam and ensure mempool stability.

### Ancestors
Transactions that a given transaction depends on (parent transactions).

**Example:**
- Transaction B spends an output from Transaction A
- Transaction A is an ancestor of Transaction B

### Descendants
Transactions that depend on a given transaction (child transactions).

**Example:**
- Transaction B spends an output from Transaction A
- Transaction B is a descendant of Transaction A

### Limits
When a transaction would exceed ancestor or descendant limits, it is rejected from the mempool.

## Best Practices

1. **Exchanges**: Use conservative limits and higher fee thresholds
2. **Miners**: Use larger mempool sizes and lower fee thresholds
3. **General Users**: Use default settings for Bitcoin Core compatibility
4. **High-Throughput Nodes**: Increase size limits and use aggressive eviction

## Bitcoin Core Compatibility

Default values match Bitcoin Core defaults:
- `max_mempool_mb`: 300 MB
- `min_relay_fee_rate`: 1 sat/vB
- `max_ancestor_count`: 25
- `max_ancestor_size`: 101 kB
- `max_descendant_count`: 25
- `max_descendant_size`: 101 kB
- `eviction_strategy`: `lowest_fee_rate`


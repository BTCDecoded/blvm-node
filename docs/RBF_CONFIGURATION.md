# RBF (Replace-By-Fee) Configuration Guide

## Overview

Supports configurable RBF (Replace-By-Fee) modes to accommodate different use cases. RBF allows transactions to be replaced by new transactions that spend the same inputs but pay higher fees.

## RBF Modes

### Disabled
No RBF replacements are allowed. All transactions are final once added to the mempool.

**Use Cases:**
- Enterprise/compliance requirements
- Nodes that prioritize transaction finality
- Exchanges with strict security policies

**Configuration:**
```toml
[rbf]
mode = "disabled"
```

### Conservative
Strict RBF rules with higher fee requirements and additional safety checks.

**Features:**
- 2x fee rate multiplier (100% increase required)
- 5000 sat minimum absolute fee bump
- 1 confirmation minimum before allowing replacement
- Maximum 3 replacements per transaction
- 300 second cooldown period

**Use Cases:**
- Exchanges
- Wallets prioritizing user safety
- Nodes that want to prevent RBF spam

**Configuration:**
```toml
[rbf]
mode = "conservative"
min_fee_rate_multiplier = 2.0
min_fee_bump_satoshis = 5000
min_confirmations = 1
max_replacements_per_tx = 3
cooldown_seconds = 300
```

### Standard (Default)
BIP125-compliant RBF with standard fee requirements.

**Features:**
- 1.1x fee rate multiplier (10% increase, BIP125 minimum)
- 1000 sat minimum absolute fee bump (BIP125 MIN_RELAY_FEE)
- No confirmation requirement
- Maximum 10 replacements per transaction
- 60 second cooldown period

**Use Cases:**
- General purpose nodes
- Default configuration
- Bitcoin Core compatibility

**Configuration:**
```toml
[rbf]
mode = "standard"
min_fee_rate_multiplier = 1.1
min_fee_bump_satoshis = 1000
```

### Aggressive
Relaxed RBF rules for miners and high-throughput nodes.

**Features:**
- 1.05x fee rate multiplier (5% increase)
- 500 sat minimum absolute fee bump
- Package replacement support
- Maximum 10 replacements per transaction
- 60 second cooldown period

**Use Cases:**
- Mining pools
- High-throughput nodes
- Nodes prioritizing fee revenue

**Configuration:**
```toml
[rbf]
mode = "aggressive"
min_fee_rate_multiplier = 1.05
min_fee_bump_satoshis = 500
allow_package_replacements = true
max_replacements_per_tx = 10
cooldown_seconds = 60
```

## Configuration Parameters

### `mode`
RBF mode: `disabled`, `conservative`, `standard`, or `aggressive`

### `min_fee_rate_multiplier`
Minimum fee rate multiplier for replacement (e.g., 1.1 = 10% increase)

### `min_fee_bump_satoshis`
Minimum absolute fee bump in satoshis

### `min_confirmations`
Minimum confirmations before allowing replacement (conservative mode only)

### `allow_package_replacements`
Allow package replacements (aggressive mode only). Package = parent + child transactions replaced together.

### `max_replacements_per_tx`
Maximum number of replacements per transaction. Prevents replacement spam.

### `cooldown_seconds`
Replacement cooldown period in seconds. Prevents rapid-fire replacements.

## Examples

### Exchange Node (Conservative)
```toml
[rbf]
mode = "conservative"
min_fee_rate_multiplier = 2.0
min_fee_bump_satoshis = 5000
min_confirmations = 1
max_replacements_per_tx = 3
cooldown_seconds = 300
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
```

### Standard Node (Default)
```toml
[rbf]
mode = "standard"
```

## Best Practices

1. **Exchanges**: Use conservative mode to protect users from unexpected replacements
2. **Miners**: Use aggressive mode to maximize fee revenue
3. **General Users**: Use standard mode for Bitcoin Core compatibility
4. **Enterprise**: Use disabled mode if RBF is not allowed by policy

## BIP125 Compliance

All modes enforce BIP125 rules:
- Existing transaction must signal RBF (sequence < 0xffffffff)
- New transaction must have higher fee rate
- New transaction must have higher absolute fee
- New transaction must conflict with existing transaction
- No new unconfirmed dependencies

Mode-specific requirements are applied in addition to BIP125 rules.


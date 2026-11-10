# Testing RBF and Mempool Policies

## Test Coverage Summary

### RBF Mode Tests (`tests/rbf_mode_tests.rs`)

Comprehensive tests for all RBF modes:

1. **`test_rbf_mode_disabled`** - Verifies disabled mode rejects all replacements
2. **`test_rbf_mode_conservative`** - Tests conservative mode with strict fee requirements
3. **`test_rbf_mode_standard`** - Tests standard BIP125-compliant mode
4. **`test_rbf_mode_aggressive`** - Tests aggressive mode with relaxed rules
5. **`test_rbf_replacement_count_limit`** - Verifies replacement count limits
6. **`test_rbf_cooldown_period`** - Tests cooldown period enforcement
7. **`test_rbf_fee_rate_multiplier`** - Tests fee rate multiplier requirements
8. **`test_rbf_absolute_fee_bump`** - Tests absolute fee bump requirements
9. **`test_rbf_config_with_mode`** - Verifies mode-specific defaults

### Mempool Policy Tests (`tests/mempool_policy_tests.rs`)

Comprehensive tests for mempool policies:

1. **`test_eviction_strategy_lowest_fee_rate`** - Tests lowest fee rate eviction
2. **`test_eviction_strategy_oldest_first`** - Tests oldest-first eviction
3. **`test_ancestor_count_limit`** - Tests ancestor count limits
4. **`test_ancestor_size_limit`** - Tests ancestor size limits
5. **`test_descendant_count_limit`** - Tests descendant count limits
6. **`test_descendant_size_limit`** - Tests descendant size limits
7. **`test_mempool_size_limit`** - Tests mempool size limits
8. **`test_mempool_transaction_count_limit`** - Tests transaction count limits
9. **`test_mempool_expiry`** - Tests transaction expiry
10. **`test_policy_config_defaults`** - Verifies default configuration values
11. **`test_eviction_strategy_variants`** - Tests all eviction strategy variants

### Integration Tests (`tests/integration/rbf_mempool_integration_tests.rs`)

Integration tests for RBF and mempool policies with the node system:

1. **`test_node_config_with_rbf`** - Tests RBF configuration in NodeConfig
2. **`test_node_config_with_mempool_policy`** - Tests mempool policy configuration in NodeConfig
3. **`test_mempool_manager_config_application`** - Tests config application to MempoolManager
4. **`test_rbf_mode_switching`** - Tests dynamic RBF mode switching
5. **`test_mempool_policy_eviction_strategies`** - Tests all eviction strategies
6. **`test_rbf_and_mempool_policy_together`** - Tests RBF and mempool policies working together

## Running Tests

### Run All RBF and Mempool Tests

```bash
# Run all RBF mode tests
cargo test --test rbf_mode_tests

# Run all mempool policy tests
cargo test --test mempool_policy_tests

# Run all integration tests
cargo test --lib integration::rbf_mempool_integration_tests
```

### Run Specific Tests

```bash
# Run a specific test
cargo test --test rbf_mode_tests test_rbf_mode_disabled

# Run tests matching a pattern
cargo test --test mempool_policy_tests eviction
```

### Run with Output

```bash
# Show test output
cargo test --test rbf_mode_tests -- --nocapture

# Show only failures
cargo test --test rbf_mode_tests -- --quiet
```

## Test Coverage

### RBF Configuration
- ✅ All 4 modes (Disabled, Conservative, Standard, Aggressive)
- ✅ Mode-specific defaults
- ✅ Fee rate multipliers
- ✅ Absolute fee bumps
- ✅ Replacement count limits
- ✅ Cooldown periods
- ✅ Configuration application

### Mempool Policies
- ✅ All 5 eviction strategies
- ✅ Size limits (MB and transaction count)
- ✅ Ancestor/descendant limits (count and size)
- ✅ Fee thresholds
- ✅ Transaction expiry
- ✅ Default configuration values

### Integration
- ✅ NodeConfig integration
- ✅ MempoolManager configuration
- ✅ Dynamic configuration changes
- ✅ Combined RBF and mempool policies

## Adding New Tests

When adding new RBF or mempool policy features:

1. Add unit tests to `tests/rbf_mode_tests.rs` or `tests/mempool_policy_tests.rs`
2. Add integration tests to `tests/integration/rbf_mempool_integration_tests.rs`
3. Update this documentation
4. Ensure tests compile and pass

## Test Utilities

The test files include helper functions:

- `create_rbf_tx()` - Creates a test transaction with RBF signaling
- `create_test_tx()` - Creates a test transaction with configurable size
- `create_test_utxo_set()` - Creates a test UTXO set

These can be reused across tests for consistency.


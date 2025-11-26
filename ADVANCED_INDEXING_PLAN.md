# Advanced Indexing Implementation Plan

## Executive Summary

This document outlines the plan to implement advanced transaction indexing features (address indexing and value range indexing) and identifies other high-value TODOs in the codebase.

## 1. Advanced Indexing Implementation

### 1.1 Address Indexing (High Priority)

**Current Status:** Placeholder that returns empty vector

**Use Cases:**
- RPC `getaddresshistory` / `getaddresstxids` equivalent
- Wallet balance queries
- Address-based transaction filtering
- Block explorer functionality

**Implementation Plan:**

#### Phase 1: Core Index Structure
1. **Add new database trees:**
   - `address_tx_index`: `script_pubkey_hash → Vec<tx_hash>` (all transactions touching address)
   - `address_output_index`: `script_pubkey_hash → Vec<(tx_hash, output_index)>` (outputs sent to address)
   - `address_input_index`: `script_pubkey_hash → Vec<(tx_hash, input_index, prev_tx_hash, prev_output_index)>` (inputs spending from address)

2. **Index key design:**
   - Use SHA256 hash of script_pubkey as key (32 bytes) for efficient lookups
   - Store full script_pubkey in value if needed for verification
   - Consider using sorted lists or B-trees for efficient range queries

3. **Update `TxIndex` struct:**
   ```rust
   pub struct TxIndex {
       // ... existing fields ...
       address_tx_index: Arc<dyn Tree>,        // script_pubkey_hash → Vec<tx_hash>
       address_output_index: Arc<dyn Tree>,   // script_pubkey_hash → Vec<(tx_hash, output_index)>
       address_input_index: Arc<dyn Tree>,    // script_pubkey_hash → Vec<(tx_hash, input_index, ...)>
   }
   ```

#### Phase 2: Indexing Logic
1. **Update `index_transaction()` method:**
   - Extract all script_pubkeys from transaction outputs
   - Extract all script_pubkeys from transaction inputs (via UTXO lookup)
   - Hash each script_pubkey
   - Append tx_hash to each address's transaction list
   - Store output/input metadata

2. **Handle reorgs:**
   - Update `remove_transaction()` to remove from address indices
   - Consider using append-only log with compaction for better reorg handling

3. **Performance optimizations:**
   - Batch writes for multiple addresses in same transaction
   - Use sorted lists for efficient range queries
   - Consider bloom filters for fast "does address exist?" checks

#### Phase 3: Query Implementation
1. **Implement `get_transactions_by_address()`:**
   - Hash the script_pubkey
   - Lookup in `address_tx_index`
   - Deserialize and return transaction list
   - Add pagination support (start/limit parameters)

2. **Add helper methods:**
   - `get_address_outputs()`: Get all outputs sent to address
   - `get_address_inputs()`: Get all inputs spending from address
   - `get_address_balance()`: Calculate balance (sum outputs - sum inputs)

#### Phase 4: Configuration & Performance
1. **Make address indexing optional:**
   - Add `enable_address_index` config flag
   - Only build index if enabled (saves disk space)
   - Add migration path for existing nodes

2. **Performance considerations:**
   - Address indexing significantly increases disk usage (~20-30% more)
   - Indexing time: ~2-3x slower block processing
   - Consider background indexing for initial sync

**Estimated Effort:** 3-5 days
**Priority:** High (enables wallet functionality)

### 1.2 Value Range Indexing (Medium Priority)

**Current Status:** Placeholder that returns empty vector

**Use Cases:**
- Find large transactions (e.g., > 1 BTC)
- Statistical analysis (average transaction size)
- Fee estimation improvements
- Whale watching / large transaction monitoring

**Implementation Plan:**

#### Phase 1: Index Structure
1. **Add database tree:**
   - `value_index`: `value_bucket → Vec<(tx_hash, output_index)>`
   - Use logarithmic bucketing: 0-1000, 1000-10000, 10000-100000, etc.

2. **Alternative: Sorted value index:**
   - Store `(value, tx_hash, output_index)` tuples sorted by value
   - Enables efficient range queries

#### Phase 2: Indexing Logic
1. **Update `index_transaction()`:**
   - Extract all output values
   - Bucket each value
   - Store (tx_hash, output_index) in appropriate bucket

2. **Handle reorgs:**
   - Remove from value index in `remove_transaction()`

#### Phase 3: Query Implementation
1. **Implement `get_transactions_by_value_range()`:**
   - Determine which buckets overlap with range
   - Query each bucket
   - Filter results to exact range
   - Return sorted by value (descending)

**Estimated Effort:** 2-3 days
**Priority:** Medium (nice-to-have, less critical than address indexing)

## 2. High-Value TODOs Identified

### 2.1 Critical (P0 - Blocks Production)

#### RPC Method Completion (`src/rpc/blockchain.rs`)
**Status:** Multiple TODOs in RPC methods
**Issues:**
- `getblock` missing: confirmations, size, height, transactions, difficulty
- `getblockheader` incomplete formatting
- Various RPC methods return placeholder values

**Impact:** High - RPC API is primary interface for users
**Effort:** 2-3 days
**Priority:** P0

**Implementation:**
1. Calculate confirmations: `chain_height - block_height + 1`
2. Get block size from metadata or calculate
3. Get height from chainstate
4. Include transactions in verbose mode
5. Calculate difficulty from chainstate

#### Fibre Statistics Tracking (`src/network/fibre.rs`)
**Status:** All statistics are placeholders (TODO)
**Issues:**
- `blocks_sent`, `blocks_received` not tracked
- `chunks_sent`, `chunks_received` not tracked
- `fec_recoveries`, `udp_errors` not tracked
- `average_latency_ms`, `success_rate` not tracked

**Impact:** Medium - Monitoring/debugging feature
**Effort:** 1-2 days
**Priority:** P1

### 2.2 High Priority (P1 - Important Features)

#### Database Range Iteration (`src/storage/database.rs`)
**Status:** `clear()` method has TODO for proper Range iteration
**Issue:** Line 437: `// TODO: Implement proper Range iteration for clear()`

**Impact:** Medium - Performance optimization
**Effort:** 1 day
**Priority:** P1

#### Stratum V2 ContributionTracker Integration (`src/network/stratum_v2/merge_mining.rs`)
**Status:** Placeholder integration
**Issue:** Line 177: `// TODO: Integrate with bllvm-commons ContributionTracker`

**Impact:** Medium - Merge mining functionality incomplete
**Effort:** 2-3 days
**Priority:** P1

### 2.3 Medium Priority (P2 - Enhancements)

#### RPC Notification Infrastructure (`src/rpc/blockchain.rs`)
**Status:** Multiple "Full implementation requires async notification infrastructure"
**Issues:**
- `getblockchaininfo` notifications incomplete
- `getnetworkinfo` notifications incomplete
- Various RPC methods need async notification support

**Impact:** Low-Medium - Advanced RPC features
**Effort:** 3-5 days
**Priority:** P2

#### Module System Resource Limits (`src/module/`)
**Status:** Phase 2+ feature (intentional deferral)
**Impact:** Low - Only needed for production module deployment
**Effort:** 5-7 days
**Priority:** P2

### 2.4 Low Priority (P3 - Nice to Have)

#### BIP70 Payment Verification (`src/network/`)
**Status:** Incomplete implementation
**Impact:** Low - Optional feature
**Effort:** 3-5 days
**Priority:** P3

#### BIP158 GCS Decoder (`src/network/`)
**Status:** Incomplete implementation
**Impact:** Low - Optional feature
**Effort:** 2-3 days
**Priority:** P3

## 3. Implementation Roadmap

### Phase 1: Critical RPC Completion (Week 1)
- [ ] Complete `getblock` RPC method
- [ ] Complete `getblockheader` RPC method
- [ ] Add missing confirmations/size/height calculations
- [ ] Test all RPC methods

### Phase 2: Address Indexing (Week 2-3)
- [ ] Design index structure
- [ ] Add database trees
- [ ] Implement indexing logic
- [ ] Implement query methods
- [ ] Add configuration flag
- [ ] Add tests
- [ ] Performance testing

### Phase 3: Value Range Indexing (Week 4)
- [ ] Design index structure
- [ ] Implement indexing logic
- [ ] Implement query methods
- [ ] Add tests

### Phase 4: Monitoring & Statistics (Week 5)
- [ ] Implement Fibre statistics tracking
- [ ] Add RPC endpoints for statistics
- [ ] Add monitoring dashboard support

### Phase 5: Polish & Optimization (Week 6)
- [ ] Database range iteration optimization
- [ ] Performance profiling
- [ ] Documentation updates

## 4. Technical Considerations

### 4.1 Disk Space Impact
- **Address Indexing:** ~20-30% increase in disk usage
- **Value Indexing:** ~5-10% increase in disk usage
- **Total:** ~25-40% increase

### 4.2 Performance Impact
- **Address Indexing:** 2-3x slower block processing
- **Value Indexing:** ~1.5x slower block processing
- **Query Performance:** O(1) lookups once indexed

### 4.3 Migration Strategy
1. Add feature flags for each index type
2. Support enabling/disabling at runtime (requires reindex)
3. Provide migration script for existing nodes
4. Document disk space requirements

### 4.4 Testing Strategy
1. Unit tests for indexing logic
2. Integration tests with real blocks
3. Performance benchmarks
4. Reorg handling tests
5. Stress tests with large address sets

## 5. Success Metrics

### Address Indexing
- [ ] Can query all transactions for a given address in < 100ms
- [ ] Index build time < 2x normal sync time
- [ ] Disk usage increase < 30%
- [ ] Reorg handling works correctly

### Value Range Indexing
- [ ] Can query transactions by value range in < 200ms
- [ ] Index build time < 1.5x normal sync time
- [ ] Disk usage increase < 10%

### RPC Completion
- [ ] All RPC methods return complete data
- [ ] No placeholder values in responses
- [ ] All methods have tests

## 6. Dependencies

### Required
- Existing transaction index infrastructure
- Database abstraction layer (already implemented)
- UTXO store (for input script_pubkey lookup)

### Optional
- Background indexing worker (for initial sync)
- Bloom filter library (for fast existence checks)
- Statistics aggregation library

## 7. Risks & Mitigations

### Risk 1: Disk Space Exhaustion
**Mitigation:** Make indexing optional, document requirements, add disk space monitoring

### Risk 2: Performance Degradation
**Mitigation:** Background indexing, batch writes, performance profiling

### Risk 3: Reorg Handling Complexity
**Mitigation:** Comprehensive testing, append-only log design, incremental updates

### Risk 4: Index Corruption
**Mitigation:** Validation checks, index verification on startup, rebuild capability

## 8. Future Enhancements

1. **Multi-address queries:** Query transactions touching multiple addresses
2. **Time-based indexing:** Index by block time for temporal queries
3. **Script type indexing:** Index by script type (P2PKH, P2SH, P2WPKH, etc.)
4. **Fee rate indexing:** Index by fee rate for mempool analysis
5. **Compressed indices:** Use compression to reduce disk usage


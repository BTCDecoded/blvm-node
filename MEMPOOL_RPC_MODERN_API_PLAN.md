# Mempool Policies, RBF Options, RPC Improvements, and Modern API Plan

**Last Updated**: 2025-01-XX  
**Status**: Planning Phase

## Executive Summary

This document outlines a comprehensive plan for:
1. **Configurable RBF (Replace-By-Fee) modes** - Conservative, Standard, Aggressive, Disabled
2. **Enhanced Mempool Policies** - Fee thresholds, size limits, eviction strategies, ancestor/descendant limits
3. **RPC Improvements** - Better error messages, common queries, batch optimization
4. **Modern API Module** - A new, developer-friendly API alongside backwards-compatible JSON-RPC

## 1. RBF (Replace-By-Fee) Configuration

### Current State
- ✅ BIP125 RBF rules implemented in `bllvm-consensus`
- ✅ Basic RBF checks in `replacement_checks()` function
- ❌ No configurable RBF modes
- ❌ Single RBF policy for all use cases

### Problem Statement
Different node operators have different RBF requirements:
- **Miners**: Want aggressive RBF to maximize fee revenue
- **Users/Wallets**: Want conservative RBF to prevent unexpected replacements
- **Enterprise**: May want to disable RBF entirely for compliance
- **Exchanges**: Need fine-grained control over replacement policies

### Proposed Solution: RBF Modes

#### 1.1 RBF Mode Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RbfMode {
    /// RBF disabled - no replacements allowed
    Disabled,
    
    /// Conservative RBF - strict BIP125 rules with additional safety checks
    /// - Requires higher fee rate increase (e.g., 2x instead of 1.1x)
    /// - Longer confirmation wait before allowing replacement
    /// - Additional validation checks
    Conservative,
    
    /// Standard RBF - strict BIP125 compliance (default)
    /// - Standard fee rate increase requirement
    /// - Standard absolute fee bump
    /// - All BIP125 rules enforced
    Standard,
    
    /// Aggressive RBF - relaxed rules for miners
    /// - Lower fee rate increase threshold
    /// - Faster replacement processing
    /// - May allow package replacements
    Aggressive,
}
```

#### 1.2 RBF Configuration Structure

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RbfConfig {
    /// RBF mode (disabled, conservative, standard, aggressive)
    #[serde(default = "default_rbf_mode")]
    pub mode: RbfMode,
    
    /// Minimum fee rate multiplier for replacement (e.g., 1.1 = 10% increase)
    /// Conservative: 2.0 (100% increase)
    /// Standard: 1.1 (10% increase, BIP125 minimum)
    /// Aggressive: 1.05 (5% increase)
    #[serde(default = "default_rbf_fee_rate_multiplier")]
    pub min_fee_rate_multiplier: f64,
    
    /// Minimum absolute fee bump in satoshis
    /// Conservative: 5000 sat
    /// Standard: 1000 sat (BIP125 MIN_RELAY_FEE)
    /// Aggressive: 500 sat
    #[serde(default = "default_rbf_min_fee_bump")]
    pub min_fee_bump_satoshis: u64,
    
    /// Minimum confirmations before allowing replacement (conservative only)
    /// 0 = allow immediately
    #[serde(default = "default_rbf_min_confirmations")]
    pub min_confirmations: u32,
    
    /// Allow package replacements (aggressive only)
    /// Package = parent + child transactions replaced together
    #[serde(default = "default_rbf_allow_packages")]
    pub allow_package_replacements: bool,
    
    /// Maximum number of replacements per transaction
    /// Prevents replacement spam
    #[serde(default = "default_rbf_max_replacements")]
    pub max_replacements_per_tx: u32,
    
    /// Replacement cooldown period (seconds)
    /// Prevents rapid-fire replacements
    #[serde(default = "default_rbf_cooldown_seconds")]
    pub cooldown_seconds: u64,
}
```

#### 1.3 Implementation Plan

**Phase 1: Configuration**
- Add `RbfConfig` to `NodeConfig`
- Add RBF mode selection to mempool config
- Wire up configuration to mempool manager

**Phase 2: Mode Implementation**
- Extend `replacement_checks()` to accept `RbfConfig`
- Implement mode-specific fee rate calculations
- Add cooldown tracking per transaction
- Add replacement count tracking

**Phase 3: Advanced Features**
- Package replacement support (aggressive mode)
- Conservative mode confirmation checks
- Replacement rate limiting

**Priority**: High  
**Estimated Effort**: 2-3 days  
**Dependencies**: None

---

## 2. Enhanced Mempool Policies

### Current State
- ✅ Basic `MempoolConfig` in `bllvm-consensus`
- ✅ Basic fee rate checks
- ✅ Simple size limit (10000 transactions)
- ❌ No configurable eviction strategies
- ❌ No ancestor/descendant limits
- ❌ No minimum fee thresholds beyond basic relay fee

### Problem Statement
Bitcoin Core has sophisticated mempool management that we need to match:
- **Fee thresholds**: Different minimum fees for different scenarios
- **Size limits**: Both transaction count and memory size
- **Eviction strategies**: How to remove transactions when mempool is full
- **Ancestor/descendant limits**: Prevent transaction package spam

### Proposed Solution: Comprehensive Mempool Policies

#### 2.1 Enhanced Mempool Configuration

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolPolicyConfig {
    /// Maximum mempool size in megabytes
    #[serde(default = "default_max_mempool_mb")]
    pub max_mempool_mb: u64,
    
    /// Maximum number of transactions in mempool
    #[serde(default = "default_max_mempool_txs")]
    pub max_mempool_txs: usize,
    
    /// Minimum relay fee rate (satoshis per vbyte)
    #[serde(default = "default_min_relay_fee_rate")]
    pub min_relay_fee_rate: u64,
    
    /// Minimum transaction fee (absolute, in satoshis)
    #[serde(default = "default_min_tx_fee")]
    pub min_tx_fee: u64,
    
    /// Incremental relay fee (for fee bumping)
    #[serde(default = "default_incremental_relay_fee")]
    pub incremental_relay_fee: u64,
    
    /// Maximum ancestor count (transaction + all ancestors)
    #[serde(default = "default_max_ancestor_count")]
    pub max_ancestor_count: u32,
    
    /// Maximum ancestor size in vbytes
    #[serde(default = "default_max_ancestor_size")]
    pub max_ancestor_size: u64,
    
    /// Maximum descendant count (transaction + all descendants)
    #[serde(default = "default_max_descendant_count")]
    pub max_descendant_count: u32,
    
    /// Maximum descendant size in vbytes
    #[serde(default = "default_max_descendant_size")]
    pub max_descendant_size: u64,
    
    /// Transaction eviction strategy
    #[serde(default = "default_eviction_strategy")]
    pub eviction_strategy: EvictionStrategy,
    
    /// Mempool expiry time in hours
    #[serde(default = "default_mempool_expiry_hours")]
    pub mempool_expiry_hours: u64,
    
    /// Enable mempool persistence (survives restarts)
    #[serde(default = "default_false")]
    pub persist_mempool: bool,
    
    /// Mempool persistence file path
    #[serde(default = "default_mempool_persistence_path")]
    pub mempool_persistence_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvictionStrategy {
    /// Evict lowest fee rate transactions first (Bitcoin Core default)
    LowestFeeRate,
    
    /// Evict oldest transactions first (FIFO)
    OldestFirst,
    
    /// Evict largest transactions first (to free most space)
    LargestFirst,
    
    /// Evict transactions with no descendants first (safest)
    NoDescendantsFirst,
    
    /// Hybrid: Combine fee rate and age
    Hybrid {
        fee_rate_weight: f64,  // 0.0 to 1.0
        age_weight: f64,        // 0.0 to 1.0
    },
}
```

#### 2.2 Implementation Plan

**Phase 1: Configuration**
- Extend `MempoolConfig` with new fields
- Add eviction strategy enum
- Wire up configuration

**Phase 2: Ancestor/Descendant Tracking**
- Implement ancestor/descendant calculation
- Add limits checking in mempool validation
- Track package sizes and counts

**Phase 3: Eviction Implementation**
- Implement each eviction strategy
- Add mempool size monitoring
- Implement eviction triggers

**Phase 4: Advanced Features**
- Mempool persistence
- Fee rate estimation improvements
- Package relay optimization

**Priority**: High  
**Estimated Effort**: 4-5 days  
**Dependencies**: None

---

## 3. RPC Improvements

### Current State
- ✅ Basic JSON-RPC 2.0 implementation
- ✅ Bitcoin Core-compatible error codes
- ✅ Basic error messages
- ❌ No batch request support
- ❌ Error messages could be more descriptive
- ❌ Missing common query methods

### Problem Statement
Bitcoin Core's RPC has known issues:
- **Poor error messages**: "Transaction rejected" without context
- **No batch optimization**: Multiple requests require multiple round trips
- **Missing common queries**: Some obvious operations require multiple calls
- **Inconsistent parameter validation**: Errors only surface after processing

### Proposed Solution: Enhanced RPC

#### 3.1 Improved Error Messages

**Current**:
```json
{
  "code": -25,
  "message": "Transaction rejected"
}
```

**Proposed**:
```json
{
  "code": -25,
  "message": "Transaction rejected: insufficient fee",
  "data": {
    "reason": "insufficient_fee",
    "required_fee_rate": 1.5,
    "provided_fee_rate": 0.8,
    "transaction_id": "abc123...",
    "suggestions": [
      "Increase fee rate to at least 1.5 sat/vB",
      "Use estimatefee to get current fee recommendations"
    ]
  }
}
```

**Implementation**:
- Add structured error data to `RpcError`
- Create error context builders for common scenarios
- Add suggestion system for common errors
- Include transaction/block context in errors

#### 3.2 Batch Request Support

**Current**: Single request per HTTP call

**Proposed**: JSON-RPC 2.0 batch requests
```json
[
  {"jsonrpc": "2.0", "method": "getblockhash", "params": [100], "id": 1},
  {"jsonrpc": "2.0", "method": "getblock", "params": ["abc..."], "id": 2},
  {"jsonrpc": "2.0", "method": "getmempoolinfo", "params": [], "id": 3}
]
```

**Response**:
```json
[
  {"jsonrpc": "2.0", "result": "abc...", "id": 1},
  {"jsonrpc": "2.0", "result": {...}, "id": 2},
  {"jsonrpc": "2.0", "result": {...}, "id": 3}
]
```

**Optimizations**:
- Parallel execution of independent requests
- Shared context (e.g., same block queried multiple times)
- Transaction batching (single DB query for multiple blocks)

#### 3.3 Common Query Methods

**Missing Methods to Add**:

1. **`getblockchainstate`** - Single call for chain tip, height, difficulty, etc.
2. **`gettransactiondetails`** - Comprehensive transaction info (block, confirmations, inputs, outputs, fees)
3. **`getaddressinfo`** - Address balance, transaction count, UTXO count, etc.
4. **`getmempoolentry`** - Detailed mempool entry (ancestors, descendants, fees, age)
5. **`estimatesmartfee`** - Enhanced fee estimation with confidence intervals
6. **`getnetworkinfo`** - Network stats, peer count, sync status
7. **`validateaddress`** - Address validation with detailed feedback
8. **`getrawmempool`** - Enhanced with filtering options (fee rate, age, etc.)

#### 3.4 Implementation Plan

**Phase 1: Error Improvements**
- Extend `RpcError` with structured data
- Add error context builders
- Update all RPC methods to use enhanced errors

**Phase 2: Batch Support**
- Parse batch requests
- Implement parallel execution
- Add request deduplication

**Phase 3: Common Queries**
- Implement missing methods
- Add comprehensive documentation
- Create query optimization helpers

**Priority**: Medium  
**Estimated Effort**: 3-4 days  
**Dependencies**: None

---

## 4. Modern API Module

### Problem Statement
JSON-RPC 2.0 is:
- ✅ Backwards compatible with Bitcoin Core
- ✅ Widely supported
- ❌ Verbose and error-prone
- ❌ No type safety
- ❌ Poor developer experience
- ❌ Inefficient for modern applications

### Proposed Solution: Modern REST/GraphQL API

#### 4.1 Architecture Decision

**Option A: REST API** (Recommended)
- ✅ Simple, familiar, HTTP-native
- ✅ Easy to cache
- ✅ Good tooling support
- ✅ Can coexist with JSON-RPC

**Option B: GraphQL**
- ✅ Flexible queries
- ✅ Type-safe
- ❌ More complex
- ❌ Less familiar to Bitcoin developers

**Option C: gRPC**
- ✅ Type-safe, efficient
- ✅ Streaming support
- ❌ Requires code generation
- ❌ Less web-friendly

**Recommendation**: REST API with optional GraphQL endpoint

#### 4.2 REST API Design

**Base Path**: `/api/v1/`

**Endpoints**:

```
GET  /api/v1/chain/tip
GET  /api/v1/chain/height
GET  /api/v1/chain/info
GET  /api/v1/blocks/{hash}
GET  /api/v1/blocks/{hash}/transactions
GET  /api/v1/blocks/height/{height}
GET  /api/v1/transactions/{txid}
GET  /api/v1/transactions/{txid}/confirmations
GET  /api/v1/addresses/{address}/balance
GET  /api/v1/addresses/{address}/transactions
GET  /api/v1/addresses/{address}/utxos
GET  /api/v1/mempool
GET  /api/v1/mempool/transactions/{txid}
GET  /api/v1/mempool/stats
POST /api/v1/transactions (submit transaction)
GET  /api/v1/fees/estimate
GET  /api/v1/network/info
GET  /api/v1/network/peers
```

**Response Format**:
```json
{
  "data": {...},
  "meta": {
    "timestamp": "2025-01-XX...",
    "version": "1.0"
  },
  "links": {
    "self": "/api/v1/transactions/abc...",
    "block": "/api/v1/blocks/def..."
  }
}
```

**Error Format**:
```json
{
  "error": {
    "code": "INSUFFICIENT_FEE",
    "message": "Transaction fee is too low",
    "details": {
      "required": 1500,
      "provided": 800
    },
    "suggestions": [
      "Increase fee to at least 1500 satoshis"
    ]
  },
  "meta": {
    "timestamp": "...",
    "request_id": "uuid"
  }
}
```

#### 4.3 Type Safety

**Rust Types**:
```rust
// Auto-generated from OpenAPI spec
#[derive(Serialize, Deserialize)]
pub struct TransactionResponse {
    pub txid: String,
    pub block_hash: Option<String>,
    pub confirmations: u64,
    pub inputs: Vec<TxInput>,
    pub outputs: Vec<TxOutput>,
    pub fee: u64,
    pub fee_rate: f64,
}

// Type-safe client library
let tx: TransactionResponse = client
    .get_transaction("abc...")
    .await?;
```

#### 4.4 Features

1. **OpenAPI/Swagger Spec** - Auto-generated API documentation
2. **Type-Safe Client Libraries** - Rust, TypeScript, Python
3. **WebSocket Support** - Real-time updates (new blocks, mempool changes)
4. **Pagination** - Cursor-based pagination for large lists
5. **Filtering** - Query parameters for filtering results
6. **Field Selection** - `?fields=txid,confirmations` to reduce payload
7. **Rate Limiting** - Per-endpoint rate limits
8. **Caching** - HTTP cache headers for immutable data

#### 4.5 Implementation Plan

**Phase 1: Core REST API**
- Implement REST router (using `axum` or `warp`)
- Create endpoint handlers
- Add request/response serialization
- Basic error handling

**Phase 2: OpenAPI Integration**
- Generate OpenAPI spec from code
- Add Swagger UI endpoint
- Type-safe request/response validation

**Phase 3: Advanced Features**
- WebSocket support
- Pagination
- Field selection
- Caching

**Phase 4: Client Libraries**
- Rust client (native)
- TypeScript/JavaScript client
- Python client

**Priority**: Medium (can be done in parallel)  
**Estimated Effort**: 5-7 days  
**Dependencies**: None (can run alongside JSON-RPC)

---

## 5. Implementation Priority

### High Priority (Before Release)
1. ✅ **RBF Configuration** - Critical for different use cases
2. ✅ **Mempool Policies** - Essential for production use
3. ⚠️ **RPC Error Improvements** - Better UX, relatively easy

### Medium Priority (Post-Release v1)
4. ⚠️ **RPC Batch Support** - Nice to have, improves efficiency
5. ⚠️ **Common Query Methods** - Fills gaps in API
6. ⚠️ **Modern API (Phase 1)** - Foundation for future

### Low Priority (Future Enhancements)
7. **Modern API (Advanced Features)** - WebSocket, pagination, etc.
8. **Client Libraries** - Can be community-driven
9. **GraphQL Endpoint** - If REST proves insufficient

---

## 6. Configuration Examples

### Example 1: Conservative Node (Exchange)
```toml
[mempool]
max_mempool_mb = 500
max_mempool_txs = 200000
min_relay_fee_rate = 2  # 2 sat/vB
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 25
max_descendant_count = 25

[rbf]
mode = "conservative"
min_fee_rate_multiplier = 2.0
min_fee_bump_satoshis = 5000
min_confirmations = 1
max_replacements_per_tx = 3
cooldown_seconds = 300
```

### Example 2: Aggressive Node (Miner)
```toml
[mempool]
max_mempool_mb = 1000
max_mempool_txs = 500000
min_relay_fee_rate = 1  # 1 sat/vB
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 50
max_descendant_count = 50

[rbf]
mode = "aggressive"
min_fee_rate_multiplier = 1.05
min_fee_bump_satoshis = 500
allow_package_replacements = true
max_replacements_per_tx = 10
cooldown_seconds = 60
```

### Example 3: Standard Node (Default)
```toml
[mempool]
max_mempool_mb = 300
max_mempool_txs = 100000
min_relay_fee_rate = 1  # 1 sat/vB
eviction_strategy = "lowest_fee_rate"
max_ancestor_count = 25
max_descendant_count = 25

[rbf]
mode = "standard"
min_fee_rate_multiplier = 1.1
min_fee_bump_satoshis = 1000
```

---

## 7. Testing Strategy

### Unit Tests
- RBF mode validation
- Eviction strategy correctness
- Error message generation
- Batch request parsing

### Integration Tests
- End-to-end RBF replacement flows
- Mempool eviction under load
- RPC batch request handling
- REST API endpoint validation

### Performance Tests
- Mempool eviction performance
- Batch request throughput
- REST API vs JSON-RPC latency

---

## 8. Documentation Requirements

1. **Configuration Guide** - How to configure RBF and mempool policies
2. **RPC Method Reference** - Enhanced with better examples
3. **REST API Documentation** - OpenAPI/Swagger spec
4. **Migration Guide** - From Bitcoin Core to bllvm-node
5. **Best Practices** - Recommended configurations for different use cases

---

## 9. Open Questions

1. **RBF Package Replacements**: Should we support BIP330 package relay with RBF?
2. **Mempool Persistence Format**: Use Bitcoin Core's format for compatibility?
3. **REST API Versioning**: How to handle API versioning long-term?
4. **GraphQL**: Should we add GraphQL as an alternative to REST?
5. **WebSocket Protocol**: Custom protocol or use existing standards?

---

## 10. Success Metrics

- ✅ Configurable RBF modes working for all use cases
- ✅ Mempool policies match Bitcoin Core behavior
- ✅ RPC errors provide actionable feedback
- ✅ Batch requests reduce round trips by 50%+
- ✅ Modern API has 90%+ feature parity with JSON-RPC
- ✅ Zero breaking changes to existing JSON-RPC API

---

## Appendix: Bitcoin Core Compatibility

All changes maintain backwards compatibility:
- JSON-RPC 2.0 API unchanged
- Default configurations match Bitcoin Core defaults
- New features are opt-in via configuration
- Existing clients continue to work without modification


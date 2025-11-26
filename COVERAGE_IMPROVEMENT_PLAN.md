# Test Coverage Improvement Plan

## Current Status
- **Coverage**: 23%
- **Test Files**: 63
- **Test Functions**: ~354
- **Source Files**: 135

## Priority Framework

Systems are prioritized based on:
1. **Security Impact** - Can bugs cause security vulnerabilities?
2. **Consensus Impact** - Can bugs affect consensus correctness?
3. **Data Integrity** - Can bugs corrupt or lose data?
4. **User Impact** - Can bugs affect user funds or experience?
5. **Complexity** - How complex is the code (more complex = more bugs)?

## Priority Tiers

### 🔴 Tier 1: Critical Security & Consensus (Target: 90%+ coverage)
**Must have comprehensive coverage - bugs here are catastrophic**

#### 1.1 RPC Authentication & Authorization
**Files**: `src/rpc/auth.rs`
**Current Coverage**: Low (only basic tests in `tests/integration/rpc_auth_tests.rs`)
**Priority**: **CRITICAL** - Security boundary for all RPC access

**Missing Coverage**:
- [ ] Token validation edge cases (expired, malformed, brute force)
- [ ] Certificate-based authentication
- [ ] Rate limiting per user/IP
- [ ] Token revocation
- [ ] Concurrent authentication attempts
- [ ] Authentication bypass attempts
- [ ] Rate limiter token bucket refill logic
- [ ] Per-user rate limit enforcement

**Test Files Needed**:
- `tests/rpc/auth_comprehensive_tests.rs` - Full auth system tests
- `tests/rpc/auth_security_tests.rs` - Security boundary tests
- `tests/rpc/rate_limiting_tests.rs` - Rate limiting edge cases

**Estimated Effort**: 2-3 days
**Target Coverage**: 90%

**Note**: Create `tests/rpc/` directory before starting

#### 1.2 Module Security & Sandboxing
**Files**: 
- `src/module/security/permissions.rs`
- `src/module/security/validator.rs`
- `src/module/sandbox/process.rs`
- `src/module/sandbox/filesystem.rs`
- `src/module/sandbox/network.rs`

**Current Coverage**: Low (basic tests in `tests/module/security_tests.rs`)
**Priority**: **CRITICAL** - Prevents modules from compromising node security

**Missing Coverage**:
- [ ] Permission enforcement (deny unauthorized operations)
- [ ] Request validation (consensus modification attempts)
- [ ] Process sandboxing (filesystem isolation)
- [ ] Network sandboxing (restricted network access)
- [ ] Permission escalation attempts
- [ ] Module crash isolation
- [ ] Resource limit enforcement
- [ ] IPC security (malformed messages, injection)

**Test Files Needed**:
- `tests/module/security/permissions_tests.rs` - Permission model
- `tests/module/security/validator_tests.rs` - Request validation
- `tests/module/security/sandbox_tests.rs` - Sandbox isolation
- `tests/module/security/ipc_security_tests.rs` - IPC security

**Estimated Effort**: 3-4 days
**Target Coverage**: 90%

**Note**: Create `tests/module/security/` subdirectory before starting

#### 1.3 Storage Integrity (Blockstore, Chainstate, UTXO Store)
**Files**:
- `src/storage/blockstore.rs`
- `src/storage/chainstate.rs`
- `src/storage/utxostore.rs`
- `src/storage/txindex.rs`

**Current Coverage**: Medium (tests in `tests/storage_tests.rs`)
**Priority**: **CRITICAL** - Data corruption = consensus failure

**Missing Coverage**:
- [ ] Concurrent write protection
- [ ] Database corruption recovery
- [ ] Transaction atomicity
- [ ] UTXO set consistency checks
- [ ] Chain reorganization edge cases
- [ ] Block validation before storage
- [ ] Index consistency
- [ ] Pruning edge cases

**Test Files Needed**:
- `tests/storage/integrity_tests.rs` - Data integrity
- `tests/storage/concurrency_tests.rs` - Concurrent access
- `tests/storage/corruption_tests.rs` - Corruption handling
- `tests/storage/reorg_tests.rs` - Chain reorganization

**Estimated Effort**: 3-4 days
**Target Coverage**: 85%

**Note**: Storage is large (1,100+ lines) - allow extra time for comprehensive testing

#### 1.4 DoS Protection & Rate Limiting
**Files**:
- `src/network/dos_protection.rs`
- `src/network/rate_limiter_proofs.rs`
- `src/rpc/auth.rs` (rate limiting)

**Current Coverage**: Low (basic tests in `tests/integration/dos_protection_tests.rs`)
**Priority**: **CRITICAL** - Prevents node from being overwhelmed

**Missing Coverage**:
- [ ] Connection flood protection
- [ ] Message flood protection
- [ ] Rate limiter accuracy
- [ ] Ban list effectiveness
- [ ] Resource exhaustion scenarios
- [ ] Distributed attack scenarios
- [ ] Rate limit bypass attempts

**Test Files Needed**:
- `tests/security/dos_protection_comprehensive_tests.rs`
- `tests/security/rate_limiting_tests.rs`
- `tests/security/ban_list_tests.rs`

**Estimated Effort**: 2 days
**Target Coverage**: 85%

### 🟠 Tier 2: Core Functionality (Target: 80%+ coverage)
**High priority - bugs affect node operation**

#### 2.1 Network Layer (Peer Management, Message Handling)
**Files**:
- `src/network/peer.rs`
- `src/network/protocol.rs`
- `src/network/relay.rs`
- `src/network/message_bridge.rs`

**Current Coverage**: Medium (tests in `tests/network_tests.rs`)
**Priority**: **HIGH** - Network is critical for node operation

**Missing Coverage**:
- [ ] Peer connection lifecycle
- [ ] Message serialization/deserialization edge cases
- [ ] Protocol version negotiation
- [ ] Message ordering guarantees
- [ ] Peer disconnection handling
- [ ] Network partition recovery
- [ ] Message replay protection

**Test Files Needed**:
- `tests/network/peer_lifecycle_tests.rs`
- `tests/network/message_handling_tests.rs`
- `tests/network/protocol_compatibility_tests.rs`

**Estimated Effort**: 3-4 days
**Target Coverage**: 80%

**Note**: Network layer is very large (3,000+ lines) - allow extra time

#### 2.2 Mempool Management
**Files**: `src/node/mempool.rs`
**Current Coverage**: Medium (tests in `tests/mempool_tests.rs`)
**Priority**: **HIGH** - Affects transaction handling

**Missing Coverage**:
- [ ] Transaction replacement (RBF)
- [ ] Fee estimation accuracy
- [ ] Mempool eviction policies
- [ ] Transaction dependency handling
- [ ] Concurrent transaction addition
- [ ] Memory limits
- [ ] Transaction validation

**Test Files Needed**:
- `tests/mempool/replacement_tests.rs`
- `tests/mempool/fee_estimation_tests.rs`
- `tests/mempool/eviction_tests.rs`

**Estimated Effort**: 2 days
**Target Coverage**: 80%

#### 2.3 Block Processing & Validation
**Files**: `src/node/block_processor.rs`
**Current Coverage**: Low
**Priority**: **HIGH** - Affects consensus

**Missing Coverage**:
- [ ] Block validation pipeline
- [ ] Invalid block rejection
- [ ] Block ordering
- [ ] Orphan block handling
- [ ] Block propagation
- [ ] Chain tip updates

**Test Files Needed**:
- `tests/node/block_processing_tests.rs`
- `tests/node/block_validation_tests.rs`

**Estimated Effort**: 2 days
**Target Coverage**: 80%

#### 2.4 RPC Server & Methods
**Files**:
- `src/rpc/server.rs`
- `src/rpc/blockchain.rs`
- `src/rpc/mining.rs`
- `src/rpc/control.rs`

**Current Coverage**: Medium (tests in `tests/rpc_tests.rs`)
**Priority**: **HIGH** - User-facing API

**Missing Coverage**:
- [ ] RPC method parameter validation
- [ ] Error handling and reporting
- [ ] Concurrent RPC requests
- [ ] RPC method authorization
- [ ] Response serialization
- [ ] Timeout handling

**Test Files Needed**:
- `tests/rpc/server_tests.rs`
- `tests/rpc/method_validation_tests.rs`
- `tests/rpc/concurrency_tests.rs`

**Estimated Effort**: 2 days
**Target Coverage**: 75%

### 🟡 Tier 3: Infrastructure (Target: 70%+ coverage)
**Medium priority - bugs affect features but not core operation**

#### 3.1 Module System (IPC, Process Isolation)
**Files**:
- `src/module/ipc/client.rs`
- `src/module/ipc/server.rs`
- `src/module/process/spawner.rs`
- `src/module/process/monitor.rs`

**Current Coverage**: Low (basic tests in `tests/module/`)
**Priority**: **MEDIUM** - Affects module functionality

**Missing Coverage**:
- [ ] IPC message serialization
- [ ] Process lifecycle management
- [ ] Process crash recovery
- [ ] IPC connection handling
- [ ] Module loading/unloading

**Test Files Needed**:
- `tests/module/ipc_tests.rs`
- `tests/module/process_lifecycle_tests.rs`

**Estimated Effort**: 2 days
**Target Coverage**: 70%

#### 3.2 Governance & Webhooks
**Files**:
- `src/governance/webhook.rs`
- `src/governance/user_signaling.rs`

**Current Coverage**: Low
**Priority**: **MEDIUM** - Optional feature

**Missing Coverage**:
- [ ] Webhook delivery
- [ ] Webhook retry logic
- [ ] User signaling
- [ ] Error handling

**Test Files Needed**:
- `tests/governance/webhook_tests.rs`
- `tests/governance/signaling_tests.rs`

**Estimated Effort**: 1 day
**Target Coverage**: 70%

#### 3.3 Utilities (Circuit Breaker, Retry, etc.)
**Files**:
- `src/utils/circuit_breaker.rs`
- `src/utils/retry.rs`
- `src/utils/timeout.rs`
- `src/utils/validation.rs`

**Current Coverage**: Low
**Priority**: **MEDIUM** - Used throughout codebase

**Missing Coverage**:
- [ ] Circuit breaker state transitions
- [ ] Retry logic with backoff
- [ ] Timeout handling
- [ ] Validation edge cases

**Test Files Needed**:
- `tests/utils/circuit_breaker_tests.rs`
- `tests/utils/retry_tests.rs`
- `tests/utils/timeout_tests.rs`

**Estimated Effort**: 1-2 days
**Target Coverage**: 70%

### 🟢 Tier 4: Nice to Have (Target: 60%+ coverage)
**Lower priority - bugs are annoying but not critical**

#### 4.1 BIP21, ZMQ, etc.
**Files**:
- `src/bip21.rs`
- `src/zmq/publisher.rs`

**Current Coverage**: Low
**Priority**: **LOW** - Optional features

**Estimated Effort**: 1 day
**Target Coverage**: 60%

## Implementation Strategy

### Phase 1: Critical Security (Weeks 1-3)
**Goal**: Get Tier 1 systems to 85%+ coverage
1. RPC Authentication & Authorization (2-3 days)
2. Module Security & Sandboxing (3-4 days)
3. Storage Integrity (3-4 days)
4. DoS Protection (2 days)

**Total**: 10-13 days (3 weeks)
**Expected Coverage After Phase 1**: ~40-45%

**Prerequisites**: Create test directories (`tests/rpc/`, `tests/module/security/`)

### Phase 2: Core Functionality (Weeks 4-6)
**Goal**: Get Tier 2 systems to 75%+ coverage
1. Network Layer (3-4 days)
2. Mempool Management (2 days)
3. Block Processing (2 days)
4. RPC Server (2 days)

**Total**: 9-10 days (2.5 weeks)
**Expected Coverage After Phase 2**: ~55-60%

### Phase 3: Infrastructure (Weeks 5-6)
**Goal**: Get Tier 3 systems to 70%+ coverage
1. Module System (2 days)
2. Governance (1 day)
3. Utilities (2 days)

**Expected Coverage After Phase 3**: ~65-70%

### Phase 4: Polish (Week 7+)
**Goal**: Fill remaining gaps, reach 80%+ overall
1. Tier 4 systems (1 day)
2. Edge cases in Tier 1-3 (ongoing)
3. Integration tests (ongoing)

**Expected Coverage After Phase 4**: ~80%+

## Test Writing Guidelines

### For Each System:
1. **Happy Path**: Test normal operation
2. **Error Cases**: Test error handling
3. **Edge Cases**: Test boundary conditions
4. **Security**: Test security boundaries
5. **Concurrency**: Test concurrent access
6. **Integration**: Test with other systems

### Test Structure:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_happy_path() { ... }
    
    #[test]
    fn test_error_case() { ... }
    
    #[test]
    fn test_edge_case() { ... }
    
    #[tokio::test]
    async fn test_concurrent_access() { ... }
    
    #[test]
    fn test_security_boundary() { ... }
}
```

## Metrics & Tracking

### Coverage Goals by System:
- Tier 1 (Critical): 85-90%
- Tier 2 (Core): 75-80%
- Tier 3 (Infrastructure): 70%
- Tier 4 (Nice to have): 60%

### Overall Goals:
- **Current**: 23%
- **Phase 1 End**: 40-45%
- **Phase 2 End**: 55-60%
- **Phase 3 End**: 65-70%
- **Phase 4 End**: 80%+

## Tools & Resources

### Coverage Tools:
- `cargo tarpaulin` (current)
- `cargo llvm-cov` (future - faster)

### Test Utilities:
- `tempfile` - Temporary directories
- `tokio-test` - Async testing
- `proptest` - Property-based testing
- `serial_test` - Sequential test execution

### Best Practices:
- Use `#[ignore]` for slow tests
- Use `serial_test` for database tests
- Use property-based tests for complex logic
- Test security boundaries explicitly

## Success Criteria

### Phase 1 Complete When:
- [ ] RPC auth: 90% coverage
- [ ] Module security: 90% coverage
- [ ] Storage: 85% coverage
- [ ] DoS protection: 85% coverage
- [ ] Overall: 40%+ coverage

### Phase 2 Complete When:
- [ ] Network: 80% coverage
- [ ] Mempool: 80% coverage
- [ ] Block processing: 80% coverage
- [ ] RPC server: 75% coverage
- [ ] Overall: 60%+ coverage

### Phase 3 Complete When:
- [ ] Module system: 70% coverage
- [ ] Governance: 70% coverage
- [ ] Utilities: 70% coverage
- [ ] Overall: 70%+ coverage

### Final Goal:
- [ ] Overall: 80%+ coverage
- [ ] All Tier 1 systems: 85%+ coverage
- [ ] All Tier 2 systems: 75%+ coverage

## Notes

- Focus on **quality over quantity** - comprehensive tests are better than many shallow tests
- **Security tests are critical** - don't skip them
- **Integration tests** complement unit tests
- **Property-based tests** catch edge cases
- **Coverage is a guide**, not a goal - aim for meaningful tests


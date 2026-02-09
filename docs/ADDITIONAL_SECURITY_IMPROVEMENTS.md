# Additional Security Improvements - Attack Vector Analysis

## Overview

This document analyzes additional attack vectors similar to IBD bandwidth exhaustion and recommends improvements. These are **less critical** than IBD protection but still important for production hardening.

---

## Attack Vector Analysis

### 1. Transaction Relay Exhaustion ⚠️ **PARTIALLY PROTECTED**

**Attack**: Malicious peer sends many large transactions, exhausting bandwidth on relay.

**Current Protection**:
- ✅ Transaction rate limiting (per-peer, configurable)
- ✅ Transaction byte rate limiting (100 KB/s default)
- ✅ Spam filtering (detects Ordinals, Inscriptions, BRC-20)
- ✅ Spam ban system (auto-ban after threshold)

**Gaps**:
- ⚠️ No per-IP/subnet bandwidth limits for transaction relay
- ⚠️ No cumulative bandwidth tracking for transaction serving
- ⚠️ Rate limits are per-peer, can be bypassed with multiple peers from same IP

**Recommendation**: **MEDIUM PRIORITY**
- Extend IBD protection pattern to transaction relay
- Add per-IP/subnet bandwidth limits for transaction serving
- Track cumulative transaction bandwidth per IP/subnet

**Estimated Complexity**: Low (reuse IBD protection infrastructure)
**Estimated Time**: 2-3 hours

---

### 2. Address Relay Exhaustion ⚠️ **PARTIALLY PROTECTED**

**Attack**: Malicious peer sends many addr messages, exhausting bandwidth.

**Current Protection**:
- ✅ Rate limiting: `addr_relay_min_interval_seconds` (2.4 hours default)
- ✅ Max addresses per message: 1000 (Bitcoin Core limit)
- ✅ Address database size limits

**Gaps**:
- ⚠️ No per-peer bandwidth limits for addr messages
- ⚠️ No per-IP limits (can spam from multiple peers)
- ⚠️ Rate limit is global, not per-peer

**Recommendation**: **LOW PRIORITY**
- Add per-peer addr message rate limiting
- Track addr message bandwidth per IP/subnet
- Consider per-peer addr message count limits

**Estimated Complexity**: Low
**Estimated Time**: 1-2 hours

---

### 3. Compact Block Relay Exhaustion ⚠️ **VULNERABLE**

**Attack**: Malicious peer requests many compact blocks (BIP152), exhausting bandwidth.

**Current Protection**:
- ✅ Compact block protocol exists
- ❌ **NO bandwidth limits for compact block serving**
- ❌ **NO rate limiting for GetBlockTxn requests**

**Gaps**:
- ❌ No per-peer bandwidth limits
- ❌ No per-IP limits
- ❌ No concurrent serving limits
- ❌ Can request many compact blocks rapidly

**Recommendation**: **MEDIUM PRIORITY**
- Add bandwidth limits for compact block serving
- Rate limit GetBlockTxn requests
- Track compact block bandwidth per peer/IP

**Estimated Complexity**: Medium
**Estimated Time**: 3-4 hours

---

### 4. Filter Service Exhaustion (BIP157/158) ⚠️ **VULNERABLE**

**Attack**: Malicious peer requests many block filters, exhausting bandwidth/CPU.

**Current Protection**:
- ✅ Filter service exists
- ❌ **NO bandwidth limits for filter serving**
- ❌ **NO rate limiting for GetCfilters requests**
- ❌ **NO CPU limits for filter generation**

**Gaps**:
- ❌ No per-peer bandwidth limits
- ❌ No per-IP limits
- ❌ No concurrent serving limits
- ❌ Filter generation is CPU-intensive (no limits)

**Recommendation**: **MEDIUM PRIORITY**
- Add bandwidth limits for filter serving
- Rate limit GetCfilters/GetCfheaders requests
- Add CPU time limits for filter generation
- Track filter bandwidth per peer/IP

**Estimated Complexity**: Medium
**Estimated Time**: 3-4 hours

---

### 5. UTXO Commitment Exhaustion ⚠️ **VULNERABLE**

**Attack**: Malicious peer requests many UTXO commitments, exhausting bandwidth.

**Current Protection**:
- ✅ UTXO commitment protocol exists
- ✅ Replay protection (request ID)
- ❌ **NO bandwidth limits for UTXO commitment serving**
- ❌ **NO rate limiting**

**Gaps**:
- ❌ No per-peer bandwidth limits
- ❌ No per-IP limits
- ❌ UTXO commitments can be large (several MB)
- ❌ Can request many commitments rapidly

**Recommendation**: **MEDIUM PRIORITY**
- Add bandwidth limits for UTXO commitment serving
- Rate limit GetUTXOSet requests
- Track UTXO commitment bandwidth per peer/IP

**Estimated Complexity**: Medium
**Estimated Time**: 2-3 hours

---

### 6. Module Serving Exhaustion ⚠️ **VULNERABLE**

**Attack**: Malicious peer requests many modules, exhausting bandwidth.

**Current Protection**:
- ✅ Module registry exists
- ✅ Payment system (can require payment)
- ❌ **NO bandwidth limits if payment disabled**
- ❌ **NO rate limiting for GetModule requests**

**Gaps**:
- ❌ No per-peer bandwidth limits
- ❌ No per-IP limits
- ❌ Modules can be large (MB to GB)
- ❌ Can request many modules rapidly

**Recommendation**: **LOW PRIORITY** (payment system provides some protection)
- Add bandwidth limits for module serving
- Rate limit GetModule requests
- Consider free tier limits (e.g., 100 MB/day free)

**Estimated Complexity**: Low-Medium
**Estimated Time**: 2-3 hours

---

### 7. Header-Only Sync Exhaustion ⚠️ **PARTIALLY PROTECTED**

**Attack**: Malicious peer requests many headers (without blocks), exhausting bandwidth.

**Current Protection**:
- ✅ IBD protection covers full chain requests
- ⚠️ **Headers-only requests not explicitly protected**
- ⚠️ Headers are small (~80 bytes each) but can add up

**Gaps**:
- ⚠️ No specific limits for header-only requests
- ⚠️ Can request many headers rapidly
- ⚠️ Headers are served through normal GetHeaders flow

**Recommendation**: **LOW PRIORITY**
- Extend IBD protection to detect large header requests
- Add per-peer header count limits
- Consider header request cooldown

**Estimated Complexity**: Low
**Estimated Time**: 1-2 hours

---

### 8. Mempool Flooding (Memory Exhaustion) ✅ **PROTECTED**

**Attack**: Malicious peer sends many transactions, exhausting memory.

**Current Protection**:
- ✅ Mempool size limits (300 MB default)
- ✅ Transaction count limits (100,000 default)
- ✅ Eviction strategies (lowest fee rate, etc.)
- ✅ Transaction rate limiting
- ✅ Spam filtering

**Status**: **WELL PROTECTED** - No additional work needed

---

### 9. Inventory Message Spam ⚠️ **PARTIALLY PROTECTED**

**Attack**: Malicious peer sends many inv messages, exhausting bandwidth/CPU.

**Current Protection**:
- ✅ Message rate limiting (per-peer)
- ⚠️ **No specific inv message limits**
- ⚠️ **No bandwidth tracking for inv messages**

**Gaps**:
- ⚠️ Inv messages can be large (many inventory items)
- ⚠️ No per-IP limits for inv messages
- ⚠️ Can spam inv messages from multiple peers

**Recommendation**: **LOW PRIORITY**
- Add inv message size limits
- Rate limit inv messages per peer/IP
- Track inv message bandwidth

**Estimated Complexity**: Low
**Estimated Time**: 1-2 hours

---

### 10. Package Relay Exhaustion (BIP331) ⚠️ **VULNERABLE**

**Attack**: Malicious peer sends many package relay requests, exhausting bandwidth.

**Current Protection**:
- ✅ Package relay protocol exists
- ❌ **NO bandwidth limits for package serving**
- ❌ **NO rate limiting for PkgTxn requests**

**Gaps**:
- ❌ No per-peer bandwidth limits
- ❌ No per-IP limits
- ❌ Packages can be large (multiple transactions)
- ❌ Can request many packages rapidly

**Recommendation**: **MEDIUM PRIORITY**
- Add bandwidth limits for package serving
- Rate limit PkgTxn requests
- Track package bandwidth per peer/IP

**Estimated Complexity**: Medium
**Estimated Time**: 2-3 hours

---

## Recommended Implementation Priority

### HIGH PRIORITY (Post-Release)
1. **Compact Block Relay Protection** - Medium complexity, significant impact
2. **Filter Service Protection** - Medium complexity, CPU-intensive
3. **UTXO Commitment Protection** - Medium complexity, large responses

### MEDIUM PRIORITY (Future Enhancement)
4. **Transaction Relay Enhancement** - Low complexity, reuse IBD infrastructure
5. **Package Relay Protection** - Medium complexity, newer protocol

### LOW PRIORITY (Nice to Have)
6. **Address Relay Enhancement** - Low complexity, already partially protected
7. **Header-Only Sync Protection** - Low complexity, headers are small
8. **Inventory Message Limits** - Low complexity, minor impact
9. **Module Serving Limits** - Low-Medium complexity, payment system helps

---

## Unified Bandwidth Protection System

### Proposed Architecture

Instead of implementing separate protection for each service, we could create a **unified bandwidth protection system** that:

1. **Tracks bandwidth per service type**:
   - IBD (blocks/headers)
   - Transaction relay
   - Compact blocks
   - Filter service
   - UTXO commitments
   - Module serving
   - Package relay

2. **Applies limits per peer/IP/subnet**:
   - Per-service limits
   - Combined limits (total bandwidth across all services)
   - Priority-based throttling (IBD > filters > transactions)

3. **Configuration**:
   ```toml
   [bandwidth_protection]
   # Per-service limits
   [bandwidth_protection.ibd]
   max_bandwidth_per_peer_per_day_gb = 50
   
   [bandwidth_protection.compact_blocks]
   max_bandwidth_per_peer_per_day_gb = 10
   
   [bandwidth_protection.filters]
   max_bandwidth_per_peer_per_day_gb = 5
   
   # Combined limits
   max_total_bandwidth_per_peer_per_day_gb = 100
   ```

**Benefits**:
- Reuse existing IBD protection infrastructure
- Consistent protection across all services
- Easier to configure and maintain
- Better visibility (unified metrics)

**Estimated Complexity**: Medium-High
**Estimated Time**: 8-12 hours

---

## Other Security Improvements

### 1. Enhanced Pattern Detection

**Current**: Basic cooldown and reputation
**Enhancement**: Advanced pattern detection

- Detect coordinated attacks (multiple IPs requesting same data)
- Detect rapid reconnection patterns (disconnect → reconnect → request)
- Detect suspicious request patterns (requesting same blocks repeatedly)
- Machine learning-based anomaly detection (future)

**Estimated Complexity**: High
**Estimated Time**: 10-15 hours

### 2. Proof-of-Work for High-Bandwidth Requests

**Idea**: Require proof-of-work for large requests (similar to Hashcash)

- Lightweight PoW for IBD requests
- PoW difficulty scales with request size
- Legitimate nodes can easily compute, attackers need significant resources

**Estimated Complexity**: High
**Estimated Time**: 8-10 hours

### 3. Authentication for High-Bandwidth Operations

**Idea**: Optional authentication for trusted nodes

- Whitelist trusted nodes (by IP or public key)
- Higher bandwidth limits for authenticated nodes
- Useful for node operators with multiple nodes

**Estimated Complexity**: Medium
**Estimated Time**: 4-6 hours

### 4. Time-Delayed Serving

**Idea**: Add artificial delay for suspicious requests

- Detect suspicious patterns
- Add increasing delay (exponential backoff)
- Legitimate nodes: no delay
- Suspicious nodes: increasing delay

**Estimated Complexity**: Medium
**Estimated Time**: 3-4 hours

### 5. Bandwidth Monitoring & Metrics

**Enhancement**: Better visibility into bandwidth usage

- RPC endpoints for bandwidth statistics
- Per-peer/IP/subnet bandwidth reports
- Historical bandwidth usage tracking
- Alerting for unusual patterns

**Estimated Complexity**: Low-Medium
**Estimated Time**: 4-5 hours

### 6. Adaptive Rate Limiting

**Idea**: Dynamically adjust limits based on network conditions

- Increase limits when network is quiet
- Decrease limits when under attack
- Learn from legitimate vs. malicious patterns

**Estimated Complexity**: High
**Estimated Time**: 12-15 hours

---

## Quick Wins (Low Effort, High Value)

### 1. Extend IBD Protection to Other Services ⭐ **RECOMMENDED**

**Approach**: Reuse `IbdProtectionManager` for other services

**Changes**:
- Rename to `BandwidthProtectionManager`
- Add service type parameter
- Apply same limits to compact blocks, filters, etc.

**Estimated Time**: 4-6 hours
**Impact**: High (protects multiple services)

### 2. Add Per-IP Limits to Transaction Relay ⭐ **RECOMMENDED**

**Approach**: Extend existing transaction rate limiting

**Changes**:
- Add per-IP transaction bandwidth tracking
- Add per-subnet limits
- Reuse IBD protection infrastructure

**Estimated Time**: 2-3 hours
**Impact**: Medium (closes bypass vector)

### 3. Add Compact Block Rate Limiting ⭐ **RECOMMENDED**

**Approach**: Similar to IBD protection

**Changes**:
- Rate limit GetBlockTxn requests
- Track compact block bandwidth
- Apply per-peer/IP limits

**Estimated Time**: 3-4 hours
**Impact**: Medium (BIP152 is commonly used)

---

## Implementation Plan

### Phase 1: Quick Wins (1-2 days)
1. Extend IBD protection to compact blocks
2. Add per-IP limits to transaction relay
3. Add filter service rate limiting

### Phase 2: Unified System (3-5 days)
1. Create unified bandwidth protection system
2. Migrate all services to unified system
3. Add comprehensive metrics

### Phase 3: Advanced Features (1-2 weeks)
1. Enhanced pattern detection
2. Proof-of-work for requests
3. Adaptive rate limiting

---

## Testing Recommendations

For each new protection:
1. **Unit Tests**: Test limit enforcement
2. **Integration Tests**: Simulate attack scenarios
3. **Performance Tests**: Verify overhead is acceptable
4. **Legitimate Use Tests**: Ensure legitimate nodes aren't blocked

---

## Configuration Examples

### Comprehensive Protection
```toml
[bandwidth_protection]
# Unified limits (applies to all services)
max_total_bandwidth_per_peer_per_day_gb = 100
max_total_bandwidth_per_ip_per_day_gb = 200

# Per-service limits (optional, overrides unified)
[bandwidth_protection.ibd]
max_bandwidth_per_peer_per_day_gb = 50

[bandwidth_protection.compact_blocks]
max_bandwidth_per_peer_per_day_gb = 10
max_requests_per_hour = 100

[bandwidth_protection.filters]
max_bandwidth_per_peer_per_day_gb = 5
max_requests_per_hour = 50
cpu_time_limit_ms = 100  # Max CPU time per filter generation
```

---

## Conclusion

While IBD protection addresses the most critical attack vector, several other services could benefit from similar protection. The recommended approach is to:

1. **Quick Wins**: Extend IBD protection pattern to other high-bandwidth services
2. **Unified System**: Create a unified bandwidth protection system for consistency
3. **Advanced Features**: Add pattern detection and adaptive limits over time

**Total Estimated Time for Comprehensive Protection**: 2-3 weeks
**Priority**: Medium (post-release enhancement)















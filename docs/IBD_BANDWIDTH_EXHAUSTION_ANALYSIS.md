# IBD Bandwidth Exhaustion Attack - Vulnerability Analysis

## Executive Summary

**Status: VULNERABLE** ⚠️

Bitcoin Commons node is currently vulnerable to IBD bandwidth exhaustion attacks. An attacker can force a node to upload the entire blockchain (~700GB) multiple times by cycling through fake nodes, potentially causing:
- Residential ISP data cap overages (hundreds of dollars)
- ISP throttling or service suspension
- Economic DoS making node operation prohibitively expensive

**Priority: CRITICAL - Release Blocking**

---

## Attack Mechanism

1. Attacker runs automated fake nodes that connect to target
2. Each fake node claims to be "new" and requests full blockchain sync (GetHeaders + GetData)
3. Target node serves full chain (~700GB upload per request)
4. Attacker disconnects and reconnects with new peer ID from same IP/subnet
5. Process repeats, forcing multiple full uploads
6. Cost to attacker: Minimal (automated connections)
7. Cost to defender: Prohibitive (bandwidth overages)

---

## Current Implementation Analysis

### 1. IBD Request Handling

**Location**: `blvm-node/src/network/mod.rs` (lines 4172-4273)

**Current Behavior**:
- GetHeaders/GetData messages are processed through protocol layer
- No bandwidth tracking for IBD serving
- No limits on how much data can be served to a single peer
- No detection of repeated full sync requests

**Vulnerability**: ✅ **EXPLOITABLE**
- Any peer can request full blockchain without limits
- No tracking of cumulative bandwidth per peer/IP
- No detection of suspicious patterns (reconnect + new peer ID)

### 2. Existing Protections

**DoS Protection** (`blvm-node/src/network/dos_protection.rs`):
- ✅ Connection rate limiting per IP (10 connections per 60s window)
- ✅ Message queue size limits
- ✅ Active connection limits
- ❌ **NO bandwidth limits for IBD serving**
- ❌ **NO per-peer bandwidth tracking**

**Peer Tracking** (`blvm-node/src/network/peer.rs`):
- ✅ Tracks `bytes_sent` and `bytes_recv` per peer
- ❌ **NOT used for bandwidth limiting**
- ❌ **NOT checked before serving IBD**

**Connection Rate Limiting**:
- ✅ Per-IP connection limits exist
- ❌ **Can be bypassed by cycling through IPs in same subnet**
- ❌ **No subnet-level tracking**

### 3. Missing Protections

1. **Per-Peer Bandwidth Limits**: No daily/hourly bandwidth caps per peer
2. **Per-IP/Subnet Bandwidth Limits**: No cumulative bandwidth tracking per IP or subnet
3. **IBD Request Tracking**: No detection of repeated full sync requests
4. **Peer Reputation**: No scoring system to penalize abusive peers
5. **Suspicious Pattern Detection**: No detection of reconnect + new peer ID pattern
6. **Concurrent IBD Limits**: No limit on how many peers can request IBD simultaneously

---

## Vulnerability Assessment

### Attack Scenarios

#### Scenario 1: Single IP Attack
- **Feasibility**: ⚠️ **PARTIALLY BLOCKED**
- **Mitigation**: Connection rate limiting (10 connections/60s) provides some protection
- **Bypass**: Attacker can still request full sync on each connection before hitting rate limit

#### Scenario 2: Subnet Attack
- **Feasibility**: ✅ **FULLY EXPLOITABLE**
- **Attack**: Use multiple IPs in same subnet (e.g., 192.168.1.0/24)
- **Impact**: Can bypass per-IP connection limits
- **Current Protection**: None

#### Scenario 3: Rapid Reconnection
- **Feasibility**: ✅ **FULLY EXPLOITABLE**
- **Attack**: Connect → Request full sync → Disconnect → Reconnect with new peer ID
- **Impact**: Can request full sync multiple times from same IP
- **Current Protection**: None (connection rate limit resets after window)

#### Scenario 4: Distributed Attack
- **Feasibility**: ✅ **FULLY EXPLOITABLE**
- **Attack**: Multiple IPs/subnets coordinate to request IBD simultaneously
- **Impact**: Can exhaust bandwidth from multiple sources
- **Current Protection**: None (no concurrent IBD limits)

---

## Code Locations

### IBD Request Processing
- **GetHeaders/GetData Handling**: `blvm-node/src/network/mod.rs:4172-4273`
- **Protocol Message Routing**: `blvm-node/src/network/mod.rs:4000-4273`
- **Chain Access**: `blvm-node/src/network/chain_access.rs:67-94`

### Existing Rate Limiting
- **DoS Protection**: `blvm-node/src/network/dos_protection.rs`
- **Connection Rate Limiting**: `blvm-node/src/network/dos_protection.rs:14-77`
- **Peer Tracking**: `blvm-node/src/network/peer.rs:29-32` (bytes_sent/bytes_recv)

### Configuration
- **Network Config**: `blvm-node/src/config/mod.rs:75-129`
- **DoS Config**: Currently hardcoded in `dos_protection.rs`

---

## Mitigation Implementation Plan

### Phase 1: Per-Peer Bandwidth Limits (HIGH PRIORITY)

**Complexity**: Medium
**Estimated Time**: 4-6 hours

1. Add per-peer bandwidth tracking with time windows
2. Implement token bucket rate limiter for IBD serving
3. Track cumulative bandwidth per peer (daily/hourly)
4. Reject IBD requests if peer exceeds limit

**Files to Modify**:
- `blvm-node/src/network/mod.rs` - Add bandwidth tracking
- `blvm-node/src/network/peer.rs` - Add bandwidth limit checking
- `blvm-node/src/config/mod.rs` - Add configuration options

### Phase 2: Per-IP/Subnet Tracking (HIGH PRIORITY)

**Complexity**: Medium
**Estimated Time**: 3-4 hours

1. Track bandwidth per IP address
2. Track bandwidth per subnet (/24 for IPv4, /64 for IPv6)
3. Implement subnet-level rate limiting
4. Block suspicious IP ranges

**Files to Modify**:
- `blvm-node/src/network/dos_protection.rs` - Add IP/subnet tracking
- `blvm-node/src/network/mod.rs` - Check IP/subnet limits before serving

### Phase 3: Suspicious Pattern Detection (MEDIUM PRIORITY)

**Complexity**: Medium-High
**Estimated Time**: 4-5 hours

1. Track IBD requests per IP/subnet
2. Detect rapid reconnection pattern (disconnect → reconnect with new peer ID)
3. Implement peer reputation scoring
4. Penalize peers that repeatedly request full sync

**Files to Modify**:
- `blvm-node/src/network/mod.rs` - Add pattern detection
- New file: `blvm-node/src/network/ibd_protection.rs` - IBD-specific protection

### Phase 4: Configuration & Testing (MEDIUM PRIORITY)

**Complexity**: Low-Medium
**Estimated Time**: 2-3 hours

1. Add configuration options for all limits
2. Create comprehensive test suite
3. Document recommended defaults
4. Add monitoring/metrics

**Files to Create/Modify**:
- `blvm-node/src/config/mod.rs` - Add IBD protection config
- `blvm-node/tests/integration/ibd_protection_tests.rs` - Test suite

---

## Recommended Default Configurations

```toml
[ibd_protection]
# Per-peer bandwidth limits
max_bandwidth_per_peer_per_day_gb = 50  # 50 GB/day per peer
max_bandwidth_per_peer_per_hour_gb = 10  # 10 GB/hour per peer

# Per-IP bandwidth limits
max_bandwidth_per_ip_per_day_gb = 100    # 100 GB/day per IP
max_bandwidth_per_ip_per_hour_gb = 20    # 20 GB/hour per IP

# Per-subnet bandwidth limits
max_bandwidth_per_subnet_per_day_gb = 500  # 500 GB/day per /24 subnet
max_bandwidth_per_subnet_per_hour_gb = 100 # 100 GB/hour per /24 subnet

# Concurrent IBD limits
max_concurrent_ibd_serving = 3  # Max 3 peers can request IBD simultaneously

# Suspicious pattern detection
suspicious_reconnection_threshold = 3  # 3 reconnections in 1 hour = suspicious
ibd_request_cooldown_seconds = 3600    # 1 hour cooldown after full sync request

# Peer reputation
reputation_penalty_per_ibd_request = 10  # Penalty points per IBD request
reputation_ban_threshold = 100           # Ban peer if reputation < -100
reputation_decay_per_hour = 1            # Reputation improves by 1 point/hour

# Emergency throttle mode
enable_emergency_throttle = false  # Enable to throttle all IBD serving
emergency_throttle_percent = 50    # Reduce bandwidth by 50% when enabled
```

---

## Testing Approach

### Unit Tests
1. Test per-peer bandwidth tracking
2. Test IP/subnet bandwidth limits
3. Test suspicious pattern detection
4. Test peer reputation scoring

### Integration Tests
1. Simulate single IP attack (should be blocked)
2. Simulate subnet attack (should be blocked)
3. Simulate rapid reconnection (should be detected)
4. Simulate legitimate new node (should be allowed)
5. Test emergency throttle mode

### Performance Tests
1. Measure overhead of bandwidth tracking
2. Test with 100+ concurrent peers
3. Verify limits don't impact legitimate sync

---

## Implementation Priority

1. **CRITICAL** (Release Blocking):
   - Per-peer bandwidth limits
   - Per-IP bandwidth limits
   - Basic suspicious pattern detection

2. **HIGH** (Post-Release):
   - Subnet-level tracking
   - Advanced pattern detection
   - Peer reputation system

3. **MEDIUM** (Future Enhancement):
   - Proof-of-work for IBD requests
   - Authentication for high-bandwidth operations
   - Time-delayed serving

---

## Estimated Total Implementation Time

- **Phase 1**: 4-6 hours
- **Phase 2**: 3-4 hours
- **Phase 3**: 4-5 hours
- **Phase 4**: 2-3 hours
- **Total**: 13-18 hours

---

## References

- Bitcoin Core: Uses similar protections but implementation details vary
- BIP152: Compact Block Relay reduces bandwidth but doesn't prevent attack
- Bitcoin Core IBD limits: ~125 peers max, but no per-peer bandwidth limits documented


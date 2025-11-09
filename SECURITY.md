# Security Boundaries and Threat Model

This document covers repo-specific security boundaries. See the [BTCDecoded Security Policy](https://github.com/BTCDecoded/.github/blob/main/SECURITY.md) for organization-wide policy.

## Overview

This document defines the security boundaries, threat model, and limitations of the BTCDecoded reference-node implementation. This is critical for understanding what this node can and cannot do safely.

## Security Boundaries

### ✅ IN SCOPE - What This Node Handles

1. **Consensus Validation**
   - Block validation using consensus-proof
   - Transaction validation and script execution
   - Proof of work verification
   - Economic rule enforcement (supply limits, fees)

2. **Network Protocol**
   - Bitcoin P2P protocol message parsing
   - Peer connection management
   - Block and transaction relay
   - Network message validation

3. **Storage Layer**
   - Block storage and indexing
   - UTXO set management
   - Chain state tracking
   - Transaction indexing

4. **RPC Interface**
   - JSON-RPC 2.0 compliant API
   - Blockchain data queries
   - Network status reporting
   - Mining coordination

### ❌ OUT OF SCOPE - What This Node NEVER Handles

1. **Private Key Management**
   - NO private key storage
   - NO private key generation
   - NO wallet functionality
   - NO signing operations

2. **Wallet Operations**
   - NO address generation
   - NO transaction creation
   - NO UTXO selection
   - NO change calculation

3. **Mining Operations**
   - NO actual mining (proof of work)
   - NO block template generation
   - NO nonce searching
   - NO mining pool coordination

## Threat Model

### Current Deployment (Pre-Production Testing)

**Environment**: Trusted network only
**Timeline**: 6-12 months testing phase
**Threats**: Limited to development and testing scenarios

#### Threats NOT Applicable (Trusted Network)
- Eclipse attacks
- Sybil attacks  
- Eclipse attacks
- Network partitioning attacks
- Malicious peer injection

#### Threats That Apply
- **Code vulnerabilities** in consensus validation
- **Memory corruption** in parsing
- **Integer overflow** in calculations
- **Resource exhaustion** (DoS)
- **Supply chain attacks** on dependencies

### Future Mainnet Deployment

**Environment**: Public Bitcoin network
**Timeline**: After security audit and hardening
**Threats**: Full Bitcoin network threat model

#### Additional Threats for Mainnet
- **Eclipse attacks** - malicious peers isolate node
- **Sybil attacks** - fake peer identities
- **Network partitioning** - routing attacks
- **Eclipse attacks** - peer selection manipulation
- **Resource exhaustion** - memory/CPU DoS
- **Protocol manipulation** - malformed messages

## Security Limitations

### Current Implementation Limitations

1. **Storage Layer**
   - ✅ `redb` is now default database (pure Rust, ACID, production-ready)
   - ⚠️ `sled` available as fallback (beta quality, not recommended for production)
   - ✅ Database abstraction allows switching backends
   - ⚠️ No advanced indexing (sufficient for current use case)

2. **Network Layer**
   - ✅ Peer management implemented
   - ✅ Rate limiting implemented (token bucket, per-IP connection limits)
   - ✅ DoS protection implemented (connection rate limiting, message queue limits, auto-ban, resource monitoring)

3. **RPC Interface**
   - ✅ Authentication implemented (token-based and certificate-based, configurable)
   - ✅ Rate limiting implemented (per-user rate limiting)
   - ✅ Input validation and sanitization
   - ✅ Access control via authentication

4. **Consensus Layer**
   - Signature verification now uses real transaction hashes ✅
   - All consensus-critical dependencies pinned ✅
   - Proper Bitcoin hashing implemented ✅
   - Network protocol validation added ✅

### Security Hardening Roadmap

#### Phase 1: Pre-Production (Current)
- [x] Fix signature verification with real transaction hashes
- [x] Implement proper Bitcoin double SHA256 hashing
- [x] Pin all dependencies to exact versions
- [x] Add network protocol input validation
- [ ] Add storage bounds checking
- [ ] Add comprehensive test vectors

#### Phase 2: Production Readiness
- [x] Replace sled with redb (redb is now default, sled available as fallback)
- [x] Add DoS protection mechanisms (connection rate limiting, auto-ban, resource monitoring)
- [x] Add RPC authentication (token-based and certificate-based)
- [x] Implement rate limiting (per-user RPC rate limiting, network message rate limiting)
- [x] Add comprehensive fuzzing (protocol parsing, compact blocks, enhanced edge cases)
- [x] Add eclipse attack prevention (IP diversity tracking, limits connections from same IP range)
- [x] Add storage bounds checking (prevents overflow, warns when approaching limits)

#### Phase 3: Mainnet Hardening
- [ ] Professional security audit (external, requires security firm)
- [x] Formal verification of critical paths (property tests for node invariants added)
- [x] Advanced peer management (connection quality tracking, peer selection, reliability scoring)
- [x] Performance optimization (performance profiler infrastructure added)
- [x] Monitoring and alerting (metrics collection, health checks, RPC endpoints)

## Usage Guidelines

### Safe Usage Patterns

1. **Validation Only**
   - Use for block and transaction validation
   - Query blockchain state
   - Monitor network health
   - Educational purposes

2. **Development and Testing**
   - Integration testing
   - Consensus rule validation
   - Protocol development
   - Research and analysis

### Unsafe Usage Patterns

1. **Never Use For**
   - Storing private keys
   - Creating transactions
   - Mining operations
   - Production mainnet without hardening

2. **Security Warnings**
   - Do not expose RPC to untrusted networks
   - Do not use for financial operations
   - Do not rely on for consensus without audit
   - Do not use sled storage for production

## Dependencies Security

### Consensus-Critical Dependencies (Exact Versions)
- `secp256k1 = "=0.28.2"` - ECDSA cryptography
- `sha2 = "=0.10.9"` - SHA256 hashing
- `ripemd = "=0.1.3"` - RIPEMD160 hashing
- `bitcoin_hashes = "=0.11.0"` - Bitcoin-specific hashing

### Non-Consensus Dependencies (Exact Versions)
- `serde = "=1.0.193"` - Serialization
- `anyhow = "=1.0.93"` - Error handling
- `thiserror = "=1.0.69"` - Error types

## Reporting Security Issues

### Responsible Disclosure

If you discover a security vulnerability:

1. **DO NOT** create public issues
2. **DO NOT** discuss publicly
3. **DO** report privately to: security@btcdecoded.org
4. **DO** provide detailed reproduction steps
5. **DO** allow reasonable time for fixes

### Security Response Process

1. **Acknowledgment** within 48 hours
2. **Assessment** within 7 days
3. **Fix development** within 30 days
4. **Public disclosure** after fix deployment

## Compliance and Standards

### Bitcoin Protocol Compliance
- Implements Bitcoin consensus rules
- Compatible with Bitcoin Core
- Follows BIP specifications
- Maintains protocol compatibility

### Security Standards
- Follows Rust security best practices
- Implements defense in depth
- Uses secure coding patterns
- Maintains audit trail

## Conclusion

This reference-node implementation provides a solid foundation for Bitcoin consensus validation but is **NOT suitable for production mainnet use** without significant hardening. It is designed for:

- **Educational purposes**
- **Development and testing**
- **Consensus rule validation**
- **Research and analysis**

For production use, additional security hardening, professional audit, and mainnet-specific protections are required.

---

**Last Updated**: December 2024  
**Version**: 0.1.0  
**Status**: Pre-Production Testing

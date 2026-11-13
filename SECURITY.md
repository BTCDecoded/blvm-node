# Security policy

BLVM validates Bitcoin. A consensus bug, a remote crash, and a memory-safety bug are the reports we want first.

## Report in private

Do not open a public GitHub issue for a vulnerability.

1. GitHub private vulnerability reporting on the repository that contains the bug: `https://github.com/BTCDecoded/<repo>/security/advisories/new`.
2. Email `security@thebitcoincommons.org`.

The encrypted channel is that mailbox. A published key is **not on file yet**. Until a maintainer publishes a fingerprint in this file, email is unencrypted and GitHub private reporting is the channel that keeps the report off the public issue tracker. Do not send a live mainnet exploit to a public list while the key is missing.

Include what you can: the crate, the commit, the network, whether you think it splits the chain or only crashes one process, and a minimal reproduction. A patch is welcome and not required.

## Scope

In scope:

- Consensus and validation in `blvm-consensus`, `blvm-spec`, `blvm-spec-lock`, and the frozen paths in `blvm-protocol` and `blvm-node`.
- Remote crash, denial of service, and memory safety in the node, the RPC server, and the P2P stack.
- Authentication bypass on RPC.
- Chainstate corruption after a crash or a reorg.

Out of scope:

- A third-party module you installed yourself, unless the node failed to confine it.
- Social engineering of operators.
- Issues that require a local account and do not cross into chainstate or keys, unless they are memory safety.

The node does not custody keys in the default build. Wallet-key bugs in a separate module are in scope for that module's repository and out of scope for consensus.

## Response targets

These are targets, not a claim about past performance. The advisory record is the evidence.

| Step | Target |
| --- | --- |
| Acknowledgement | 2 days |
| Triage, with a severity label | 7 days |
| Fix for consensus, remote crash, or memory safety | 14 days after triage |
| Fix for resource exhaustion | 30 days after triage |
| Fix for local-only and informational | 90 days after triage |
| Disclosure | When the fix is in a release, or sooner if the bug is being exploited. The default embargo after a fix is released is none: the release is the disclosure. A reporter can ask for a coordinated date up to 90 days after the fix. |

Severity labels, tied to node impact: chain split or consensus divergence, remote crash, resource exhaustion, local-only, and informational.

## Embargo and credit

We keep the report private until the fix is released, unless the bug is already public or actively exploited. We credit the reporter in the advisory when they agree in writing. Silence is not consent. The reconstructed November entries leave the reporter blank and set `reporter_consent` to `pending`.

## Advisories

The append-only record is one OSV JSON file per advisory id, plus a generated index. A correction is a new id that references the old one. Do not edit a published file. Public counts are generated from that record.

Advisory files are signed with the same key as release tags, once a signing key exists. They are unsigned until that key is configured.

Dependency advisories that affect a shipped build get an entry in the same record. `cargo audit` and `cargo deny` run in CI. Ignore entries belong in `.cargo/audit.toml` with a reason, not only in a workflow flag.

## Supported versions

Security fixes target the current beta line once `0.2.0-beta.1` is published. Until that tag exists, the supported line is the highest published `0.1.x` tag of the affected crate. Older `0.1` tags are not maintained as a second line.

The reporting policy above is the one in force. Older timelines, bounty language, and an "all 0.1.x supported" table are not restored. The notes below are this repository's existing boundary text.

# Security Boundaries and Threat Model

This document covers repo-specific security boundaries. See the [BTCDecoded Security Policy](https://github.com/BTCDecoded/.github/blob/main/SECURITY.md) for organization-wide policy.

## Overview

This document defines the security boundaries, threat model, and limitations of the BTCDecoded blvm-node implementation. This is critical for understanding what this node can and cannot do safely.

**Operators:** deployment exposure classes (RPC / P2P / modules) and **required vs recommended vs unsupported** combinations are summarized in the BLVM book — **[Deployment posture](https://docs.btcdecoded.org/blvm/security/deployment-posture.html)** — and in **[RPC transport × authentication](https://docs.btcdecoded.org/blvm/security/rpc-transport-auth-matrix.html)**.

## Security Boundaries

### What blvm-node Handles

- Consensus validation (delegated to blvm-consensus via blvm-protocol)
- Network protocol (P2P message parsing, peer management)
- Storage layer (block storage, UTXO set, chain state)
- RPC interface (JSON-RPC 2.0 API)
- Module orchestration (loading, IPC, lifecycle management)
- Mempool management
- Mining coordination

### What blvm-node NEVER Handles

- Consensus rule validation (delegated to blvm-consensus)
- Protocol variant selection (delegated to blvm-protocol)
- Private key management (no wallet functionality)
- Cryptographic key generation (delegated to blvm-sdk or modules)
- Governance enforcement (delegated to blvm-commons)

## Security Boundaries (Detailed)

### ✅ IN SCOPE - What This Node Handles

1. **Consensus Validation**
   - Block validation using blvm-consensus
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

#### Peer attacks

Eclipse, Sybil, network partition, and a peer that sends malformed messages are in scope whenever P2P listens beyond loopback. The released `blvm` binary binds P2P to `127.0.0.1` when no config file is loaded. A loopback bind keeps those attacks off the host's public interfaces. A config that sets `listen` to a public address brings them back into scope. The node still bans an oversized `addr`, `inv`, `getdata`, compact-block, `getheaders` locator, or version user agent on any bind.

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
- **Eclipse attacks** — malicious peers isolate node
- **Sybil attacks** — fake peer identities
- **Network partitioning** — routing attacks
- **Resource exhaustion** — memory/CPU DoS
- **Protocol manipulation** — malformed messages

## Security Limitations

### Current Implementation Limitations

1. **Storage Layer**
   - ✅ **RocksDB** is included in default features (performance path for mainnet-scale IBD)
   - ✅ **redb** available as `--features redb` (pure Rust; no default C++ `rocksdb`)
   - ⚠️ `sled` available behind `--features sled` (not recommended for production mainnet)
   - ✅ Database abstraction allows switching backends (`database_backend` config)
   - ⚠️ No advanced indexing (sufficient for current use case)

2. **Network Layer**
   - ✅ Peer management implemented
   - ✅ Rate limiting implemented (token bucket, per-IP connection limits)
   - ✅ DoS protection implemented (connection rate limiting, message queue limits, auto-ban, resource monitoring)

3. **RPC Interface**
   - ✅ Authentication implemented (token-based and certificate-based, configurable)
   - ✅ Rate limiting implemented (per-user, per-IP when auth disabled, batch-aware)
   - ✅ IP rate limiting when auth disabled (rate-limit-only mode)
   - ✅ Quinn RPC auth and rate limiting (same model as HTTP RPC)
   - ✅ Batch RPC rate limiting (consumes min(batch_len, 10) tokens)
   - ✅ Connection rate limiting (per-IP per minute for RPC and REST)
   - ✅ REST vault/pool/congestion: oversized body returns 413
   - ✅ Input validation and sanitization
   - ✅ Access control via authentication
   - ⚠️ **Certificate auth (S-001)**: When using certificate-based RPC auth, the RPC endpoint **must** be behind TLS termination (e.g. nginx, Caddy) that sets the `x-client-cert-fingerprint` header from the validated client certificate. Without this, the header is spoofable—any client can forge it. If RPC is not behind such TLS, use token-based auth only or disable cert auth.

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
- [x] Add redb as optional pure-Rust storage backend; RocksDB remains default feature set for performance
- [x] Add DoS protection mechanisms (connection rate limiting, auto-ban, resource monitoring)
- [x] Add RPC authentication (token-based and certificate-based)
- [x] Implement rate limiting (per-user RPC rate limiting, network message rate limiting)
- [x] Add comprehensive fuzzing (protocol parsing, compact blocks, enhanced edge cases)
- [x] Add eclipse attack prevention (IP diversity tracking, limits connections from same IP range)
- [x] Add storage bounds checking (prevents overflow, warns when approaching limits)

#### Phase 2.5: RPC Security Hardening (Complete)
- [x] Rate limiting when auth disabled
- [x] Quinn RPC auth and rate limiting
- [x] Batch RPC rate limiting
- [x] RPC connection rate limiting
- [x] REST vault/pool/congestion error handling (413 for oversized body)

#### Phase 3: Mainnet Hardening
- [ ] Professional security audit (external, requires security firm)
- [x] Property tests for node invariants on critical paths (testing, not formal verification)
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
   - **Never expose RPC to untrusted/public networks** without authentication. The node warns at startup when bound to a non-loopback address without auth (`rpc_auth.required = false`); heed the warning.
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


## Compliance and Standards

### Bitcoin Protocol Compliance
- Implements Bitcoin consensus rules
- Interoperates with common chain data and RPC conventions where implemented
- Follows BIP specifications
- Maintains protocol compatibility

### Security Standards
- Follows Rust security best practices
- Implements defense in depth
- Uses secure coding patterns
- Maintains audit trail

## Conclusion

This blvm-node implementation provides a solid foundation for Bitcoin consensus validation but is **NOT suitable for production mainnet use** without significant hardening. It is designed for:

- **Educational purposes**
- **Development and testing**
- **Consensus rule validation**
- **Research and analysis**

For production use, additional security hardening, professional audit, and mainnet-specific protections are required.

---

**Last Updated**: April 2026  
**Version**: 0.1.0  
**Status**: Pre-Production Testing

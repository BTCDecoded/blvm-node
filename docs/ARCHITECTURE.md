# blvm-node Architecture

## Overview

blvm-node implements a minimal Bitcoin node that adds only non-consensus infrastructure to the consensus and protocol layers. All consensus logic comes from `blvm-consensus`, and protocol abstraction comes from `blvm-protocol`.

## Design Principles

1. **Zero Consensus Re-implementation**: All consensus logic from blvm-consensus
2. **Protocol Abstraction**: Uses blvm-protocol for variant support
3. **Pure Infrastructure**: Only adds storage, networking, RPC, orchestration
4. **Production Ready**: Full Bitcoin node functionality

## Architectural Decisions

### Decision 1: Module IPC Protocol Location

**Question**: Should module IPC protocol be in `blvm-protocol`?

**Decision**: ❌ **NO - Module IPC stays in `blvm-node`**

**Rationale**:

1. **Architectural Boundary**: `blvm-protocol` is for Bitcoin P2P protocol abstraction. Module IPC is node-internal communication, not Bitcoin protocol.

2. **Dependency Direction**: Protocol layer should not depend on node concepts. Current architecture has `blvm-node` depending on `blvm-protocol` (correct). If IPC were in protocol, it would create incorrect dependency direction.

3. **Scope Mismatch**: `blvm-protocol` handles Bitcoin network messages (Version, Block, Tx). Module IPC is local process communication via Unix domain sockets.

4. **Reusability**: Module IPC is specific to this node's architecture. Other nodes may have different module systems.

**Conclusion**: Module IPC protocol is a node implementation detail, not a Bitcoin protocol abstraction. It correctly resides in `blvm-node`.

### Decision 2: SDK Crypto Usage for Module Signing

**Question**: Can we use `blvm-sdk` for cryptographic primitives in module signing?

**Decision**: ✅ **YES - Conditional use with feature gating**

**Rationale**:

- `blvm-sdk` has two parts: governance crypto (no node dependency) and composition framework (depends on node)
- Crypto modules (`governance/signatures.rs`, `governance/multisig.rs`) don't depend on node
- Composition framework depends on node, creating circular dependency if used

**Solution**: Use `blvm-sdk` with feature gating to include only crypto, not composition:

```toml
[dependencies]
blvm-sdk = { path = "../blvm-sdk", optional = true, default-features = false }

[features]
module-signing = ["blvm-sdk/governance-crypto"]  # Only crypto, not composition
```

**Implementation**:
- Use only crypto modules from SDK
- Do not import composition framework
- Feature-gate the dependency to avoid circular dependencies

**Fallback**: If SDK doesn't support feature-gated crypto, use direct implementation with `secp256k1` and `sha2`.

## Module System Architecture

See [MODULE_SYSTEM.md](MODULE_SYSTEM.md) for complete module system documentation.

## Security Boundaries

### What blvm-node Handles

- Storage (UTXO set, chain state)
- Networking (P2P protocol, peer management)
- RPC interface (JSON-RPC 2.0)
- Module orchestration (loading, IPC, lifecycle)
- Mempool management
- Mining coordination

### What blvm-node NEVER Handles

- Consensus rule validation (delegated to `blvm-consensus`)
- Protocol variant selection (delegated to `blvm-protocol`)
- Cryptographic key management (delegated to `blvm-sdk` or modules)
- Governance enforcement (delegated to `blvm-commons`)

## Dependencies

- **blvm-consensus**: All consensus logic (git dependency)
- **blvm-protocol**: Protocol abstraction and variant support
- **tokio**: Async runtime for networking
- **serde**: Serialization
- **anyhow/thiserror**: Error handling
- **tracing**: Logging
- **clap**: CLI interface

## Integration Points

### Consensus Integration

All consensus validation flows through `blvm-protocol` to `blvm-consensus`:

```
blvm-node → blvm-protocol → blvm-consensus
```

Node never calls consensus directly. Protocol layer selects appropriate variant and delegates to consensus.

### Protocol Integration

Node uses protocol layer for:
- Network variant selection (mainnet, testnet, regtest)
- Protocol-specific validation rules
- Message serialization
- Feature flag management

### Module Integration

Modules integrate through:
- IPC protocol (Unix domain sockets)
- API hub (blockchain, governance, communication APIs)
- Security layer (signature verification, permission checking, sandboxing)

See [MODULE_SYSTEM.md](MODULE_SYSTEM.md) for detailed module integration documentation.


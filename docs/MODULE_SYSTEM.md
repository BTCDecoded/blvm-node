# Module System

## Overview

The module system enables optional features (Lightning, merge mining, privacy enhancements) without affecting consensus or base node stability. Modules run in separate processes with IPC communication, providing security through isolation.

## Architecture

### System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Bitcoin Commons Infrastructure                │
│  ┌────────────────┐  ┌────────────────┐  ┌──────────────────┐ │
│  │ Governance App │  │ Module Registry│  │ Signature Service │ │
│  │  (GitHub App)  │  │   (REST API)   │  │  (Multisig)      │ │
│  └────────────────┘  └────────────────┘  └──────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ (REST API, Signatures, Governance)
                              │
┌─────────────────────────────────────────────────────────────────┐
│                         bllvm-node                              │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │                    Module Manager                         │ │
│  │  ┌──────────────┐  ┌──────────────┐  ┌────────────────┐ │ │
│  │  │   Loader     │  │  Discovery   │  │   Registry     │ │ │
│  │  │              │  │              │  │   Client       │ │ │
│  │  └──────────────┘  └──────────────┘  └────────────────┘ │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │                  Security Layer                           │ │
│  │  ┌──────────────┐  ┌──────────────┐  ┌────────────────┐ │ │
│  │  │   Signer     │  │  Validator   │  │   Sandbox      │ │ │
│  │  │  (Verify)    │  │  (Perms)     │  │  (Isolation)   │ │ │
│  │  └──────────────┘  └──────────────┘  └────────────────┘ │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │                    API Hub                                │ │
│  │  ┌──────────────┐  ┌──────────────┐  ┌────────────────┐ │ │
│  │  │ Blockchain   │  │  Governance  │  │  Communication │ │ │
│  │  │     API      │  │     API     │  │      API       │ │ │
│  │  └──────────────┘  └──────────────┘  └────────────────┘ │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                                 │
│  ┌──────────────────────────────────────────────────────────┐ │
│  │                  IPC Server                               │ │
│  │              (Unix Domain Sockets)                       │ │
│  └──────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ (IPC)
                              │
┌─────────────────────────────────────────────────────────────────┐
│                    Module Processes (Isolated)                  │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────┐  │
│  │   Module A     │  │   Module B    │  │    Module C        │  │
│  │  (Lightning)   │  │ (Merge Mine)  │  │  (Privacy)         │  │
│  └──────────────┘  └──────────────┘  └────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## Module Lifecycle

```
Discovery → Verification → Loading → Execution → Monitoring
    │            │            │           │            │
    │            │            │           │            │
    ▼            ▼            ▼           ▼            ▼
Registry    Signer      Loader      Process      Monitor
```

### Discovery

Modules discovered through:
- Local filesystem (`modules/` directory)
- Module registry (REST API)
- Manual installation

### Verification

Each module verified through:
- Hash verification (binary integrity)
- Signature verification (multisig maintainer signatures)
- Permission checking (capability validation)
- Compatibility checking (version requirements)

### Loading

Module loaded into isolated process:
- Sandbox creation (resource limits)
- IPC connection establishment
- API subscription setup

### Execution

Module runs in isolated process:
- Separate memory space
- Resource limits enforced
- IPC communication only
- Event subscription active

### Monitoring

Module health monitored:
- Process status
- Resource usage
- Error tracking
- Crash isolation

## Security Model

### Process Isolation

Modules run in separate processes with isolated memory. Node consensus state is protected and read-only to modules.

```
┌─────────────────────────────────────┐
│         bllvm-node Process          │
│  ┌───────────────────────────────┐ │
│  │    Consensus State             │ │
│  │    (Protected, Read-Only)      │ │
│  └───────────────────────────────┘ │
│  ┌───────────────────────────────┐ │
│  │    Module Manager             │ │
│  │    (Orchestration)            │ │
│  └───────────────────────────────┘ │
└─────────────────────────────────────┘
              │ IPC (Unix Sockets)
              │
┌─────────────┴─────────────────────┐
│      Module Process (Isolated)     │
│  ┌───────────────────────────────┐ │
│  │    Module State               │ │
│  │    (Separate Memory Space)    │ │
│  └───────────────────────────────┘ │
│  ┌───────────────────────────────┐ │
│  │    Sandbox                    │ │
│  │    (Resource Limits)          │ │
│  └───────────────────────────────┘ │
└─────────────────────────────────────┘
```

### Security Flow

```
Module Binary
    │
    ├─→ Hash Verification ──→ Integrity Check
    │
    ├─→ Signature Verification ──→ Multisig Check ──→ Maintainer Verification
    │
    ├─→ Permission Check ──→ Capability Validation
    │
    └─→ Sandbox Creation ──→ Resource Limits ──→ Isolation
```

### Permission Model

Modules request capabilities:
- `read_blockchain` - Read-only blockchain access
- `subscribe_events` - Subscribe to node events
- `governance_vote` - Cast governance votes (if authorized)
- `send_transactions` - Submit transactions to mempool (future)

## Module Manifest

Module manifests use TOML format:

```toml
# Module Identity
name = "lightning-network"
version = "1.2.3"
description = "Lightning Network implementation"
author = "Alice <alice@example.com>"

# Governance
[governance]
tier = "application"
maintainers = ["alice", "bob", "charlie"]
threshold = "2-of-3"
review_period_days = 14

# Signatures
[signatures]
maintainers = [
    { name = "alice", key = "02abc...", signature = "..." },
    { name = "bob", key = "03def...", signature = "..." }
]
threshold = "2-of-3"

# Binary
[binary]
hash = "sha256:abc123..."
size = 1234567
download_url = "https://registry.bitcoincommons.org/modules/lightning-network/1.2.3"

# Dependencies
[dependencies]
"bllvm-node" = ">=1.0.0"
"another-module" = ">=0.5.0"

# Compatibility
[compatibility]
min_consensus_version = "1.0.0"
min_protocol_version = "1.0.0"
min_node_version = "1.0.0"
tested_with = ["1.0.0", "1.1.0"]

# Capabilities
capabilities = [
    "read_blockchain",
    "subscribe_events",
    "governance_vote"
]
```

## Data Flow

### Module Installation Flow

```
1. User requests module installation
   │
   ├─→ Query Registry API
   │   │
   │   ├─→ Fetch module metadata
   │   ├─→ Verify maintainer signatures
   │   └─→ Check compatibility
   │
   ├─→ Download module binary
   │   │
   │   ├─→ Verify binary hash
   │   └─→ Verify binary signatures
   │
   ├─→ Resolve dependencies
   │   │
   │   ├─→ Check layer compatibility
   │   └─→ Verify dependency signatures
   │
   ├─→ Install to modules directory
   │
   └─→ Register with Module Manager
```

### Module Execution Flow

```
1. Module Manager loads module
   │
   ├─→ Verify signatures (if not cached)
   │
   ├─→ Check permissions
   │
   ├─→ Create sandbox environment
   │
   ├─→ Spawn module process
   │
   ├─→ Establish IPC connection
   │
   ├─→ Module subscribes to events
   │
   └─→ Module enters main loop
```

## Integration Points

### Registry Integration

Modules discovered and verified through module registry:

```
Module Registry Client
    │
    ├─→ REST API Client
    │   ├─→ Search modules
    │   ├─→ Get module metadata
    │   ├─→ Download binaries
    │   └─→ Verify signatures
    │
    └─→ Local Cache
        ├─→ Cached metadata
        └─→ Signature cache
```

### Governance Integration

Modules participate in governance through governance API:

```
Governance Client
    │
    ├─→ Proposal API
    │   ├─→ Get proposals
    │   ├─→ Cast votes
    │   └─→ Get results
    │
    └─→ Signature Verification
        ├─→ Verify maintainer signatures
        └─→ Multisig validation
```

### Security Integration

Module security handled through security layer:

```
Security Layer
    │
    ├─→ Module Signer
    │   ├─→ Verify manifest signatures
    │   ├─→ Verify binary signatures
    │   └─→ Multisig validation
    │
    ├─→ Permission Validator
    │   ├─→ Check capabilities
    │   ├─→ Tier validation
    │   └─→ Resource limits
    │
    └─→ Sandbox Manager
        ├─→ Process isolation
        ├─→ Resource limits
        └─→ Capability enforcement
```

## Usage

See [modules/README.md](../modules/README.md) for module installation and usage instructions.

## Security

Modules cannot:
- Modify consensus rules
- Modify UTXO set
- Access node private keys
- Bypass security boundaries
- Affect other modules

Module crashes are isolated and do not affect the base node.


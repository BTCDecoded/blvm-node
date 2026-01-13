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
│  ┌──────────────┐  ┌──────────────┐                            │
│  │   Module D     │  │   Module E    │                            │
│  │ (Marketplace)   │  │ (Stratum V2)  │                            │
│  └──────────────┘  └──────────────┘                            │
└─────────────────────────────────────────────────────────────────┘
```

## Available Modules

### Core Modules

- **blvm-stratum-v2**: Stratum V2 mining protocol support
- **blvm-merge-mining**: Merge mining for secondary chains (requires blvm-stratum-v2)
- **blvm-marketplace**: Module marketplace and registry (handles module payments)

### Module Dependencies

- **blvm-merge-mining** requires **blvm-stratum-v2** (for mining coordination)
- **blvm-marketplace** is standalone (handles module registry and payments)

### Payment Models

- **blvm-merge-mining**: One-time activation fee + hardcoded revenue share
- **blvm-marketplace**: Receives 15% of all module sales (75% author, 15% marketplace, 10% node)

## Module Lifecycle

```
Discovery → Verification → Loading → Execution → Monitoring
    │            │            │           │            │
    │            │            │           │            │
    ▼            ▼            ▼           ▼            ▼
Registry    Signer      Loader      Process      Monitor
```

### Lifecycle Events

Modules can subscribe to lifecycle events to react to dependency changes:

- **`ModuleLoaded`**: Published when a module is loaded
  - Payload: `{ module_id, module_name, version }`
  - Use case: Dependent modules can initialize connections when dependencies load

- **`ModuleUnloaded`**: Published when a module is unloaded
  - Payload: `{ module_id, module_name }`
  - Use case: Dependent modules can clean up when dependencies unload

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

Module manifests use TOML format with a clean, hierarchical structure:

```toml
# ============================================================================
# Module Manifest
# ============================================================================

# ----------------------------------------------------------------------------
# Core Identity (Required)
# ----------------------------------------------------------------------------
name = "lightning-network"
version = "1.2.3"
entry_point = "lightning-network"

# ----------------------------------------------------------------------------
# Metadata (Optional)
# ----------------------------------------------------------------------------
description = "Lightning Network payment processor implementation"
author = "Alice <alice@example.com>"

# ----------------------------------------------------------------------------
# Capabilities
# ----------------------------------------------------------------------------
# Permissions this module requires to function
capabilities = [
    "read_blockchain",    # Query blockchain data
    "subscribe_events",   # Receive node events
    "read_lightning",     # Access Lightning Network APIs
]

# ----------------------------------------------------------------------------
# Dependencies
# ----------------------------------------------------------------------------
# Required dependencies (module cannot load without these)
[dependencies]
"bllvm-node" = ">=1.0.0"

# Optional dependencies (module can work without these)
[optional_dependencies]
"bllvm-mesh" = ">=0.5.0"  # Optional mesh networking support

# ----------------------------------------------------------------------------
# Configuration Schema
# ----------------------------------------------------------------------------
# Descriptions of configuration keys this module accepts
[config_schema]
network = "Network: mainnet, testnet, regtest (default: mainnet)"
fee_rate = "Default fee rate in sat/vB (default: 1)"

# ----------------------------------------------------------------------------
# Advanced Features (Optional)
# ----------------------------------------------------------------------------

# Binary integrity verification
[binary]
hash = "sha256:abc123..."
size = 1234567

# Maintainer signatures (for verified modules)
[signatures]
threshold = "2-of-3"
maintainers = [
    { name = "alice", public_key = "02abc...", signature = "..." },
    { name = "bob", public_key = "03def...", signature = "..." },
]

# Payment configuration (for paid modules)
[payment]
required = true
price_sats = 100000
author_payment_code = "PM8TJ..."
commons_payment_code = "PM8TJ..."
payment_signature = "..."
```

### Manifest Structure

The manifest is organized into logical sections:

1. **Core Identity** (required): `name`, `version`, `entry_point`
2. **Metadata** (optional): `description`, `author`
3. **Capabilities**: List of permissions the module requires
4. **Dependencies**: Required and optional module dependencies
5. **Configuration Schema**: Descriptions of configurable options
6. **Advanced Features** (optional): Signatures, binary verification, payment config

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


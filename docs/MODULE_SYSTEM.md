# Module System Documentation

## Overview

The BTCDecoded reference-node includes a process-isolated module system that enables optional features (Lightning, merge mining, privacy enhancements) without affecting consensus or base node stability.

## Architecture

### Core Principles

1. **Process Isolation**: Each module runs in a separate process with isolated memory
2. **API Boundaries**: Modules communicate only through well-defined APIs
3. **Crash Containment**: Module failures don't propagate to the base node
4. **Consensus Isolation**: Modules cannot modify consensus rules, UTXO set, or block validation
5. **State Separation**: Module state is completely separate from consensus state

### Communication

Modules communicate with the node via **Inter-Process Communication (IPC)** using Unix domain sockets:

- **Protocol**: Length-delimited binary messages (bincode serialization)
- **Message Types**: Requests, Responses, Events
- **Connection**: Persistent connection for request/response pattern
- **Events**: Pub/sub pattern for real-time notifications

## Module Structure

### Directory Layout

Each module should be placed in a subdirectory within the `modules/` directory:

```
modules/
└── my-module/
    ├── Cargo.toml
    ├── src/
    │   └── main.rs
    └── module.toml          # Module manifest (required)
```

### Module Manifest (`module.toml`)

Every module must include a `module.toml` manifest file:

```toml
name = "my-module"
version = "1.0.0"
description = "Description of what this module does"
author = "Your Name <your.email@example.com>"

# Required capabilities/permissions
capabilities = [
    "read_blockchain",      # Required to query blockchain data
    "subscribe_events",     # Required to receive node events
]

# Optional dependencies on other modules
[dependencies]
# "another-module" = ">=0.1.0"

# Entry point (binary name)
entry_point = "my-module"
```

**Required Fields:**
- `name`: Module identifier (alphanumeric with dashes/underscores)
- `version`: Semantic version (e.g., "1.0.0")
- `entry_point`: Binary name or path

**Optional Fields:**
- `description`: Human-readable description
- `author`: Module author
- `capabilities`: List of required permissions
- `dependencies`: Other module dependencies

## Module Development

### Basic Module Structure

A minimal module implements the module lifecycle and connects to the node via IPC:

```rust
use reference_node::module::ipc::client::ModuleIpcClient;
use reference_node::module::traits::{ModuleContext, ModuleError};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Parse command-line arguments
    let args = Args::parse(); // Using clap
    
    // Connect to node IPC socket
    let mut client = ModuleIpcClient::connect(&args.socket_path).await?;
    
    // Main module loop
    loop {
        // Process events
        if let Some(event) = client.receive_event().await? {
            // Handle event
        }
        
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

### Command-Line Arguments

Modules receive the following command-line arguments:

- `--module-id <id>`: Unique module instance ID
- `--socket-path <path>`: Path to IPC socket
- `--data-dir <dir>`: Module data directory

Configuration is passed via environment variables:
- `MODULE_CONFIG_*`: Module-specific configuration

### Module Lifecycle

1. **Initialization**: Connect to node IPC socket
2. **Start**: Subscribe to events, initialize module state
3. **Running**: Process events and handle API requests
4. **Stop**: Clean shutdown, disconnect from node

### Querying Node Data

Modules can query blockchain data through the Node API:

```rust
// Get current chain tip
let request = RequestMessage {
    correlation_id: client.next_correlation_id(),
    payload: RequestPayload::GetChainTip,
};
let response = client.send_request(request).await?;

// Get a block
let request = RequestMessage {
    correlation_id: client.next_correlation_id(),
    payload: RequestPayload::GetBlock { hash },
};
let response = client.send_request(request).await?;
```

**Available API Operations:**
- `GetBlock`: Get block by hash
- `GetBlockHeader`: Get block header by hash
- `GetTransaction`: Get transaction by hash
- `HasTransaction`: Check if transaction exists
- `GetChainTip`: Get current chain tip hash
- `GetBlockHeight`: Get current block height
- `GetUtxo`: Get UTXO by outpoint (read-only)

### Subscribing to Events

Modules can subscribe to real-time node events:

```rust
// Subscribe to new block events
let request = RequestMessage {
    correlation_id: client.next_correlation_id(),
    payload: RequestPayload::SubscribeEvents {
        event_types: vec![EventType::NewBlock],
    },
};
let response = client.send_request(request).await?;

// Receive events in main loop
if let Some(event) = client.receive_event().await? {
    match event {
        ModuleMessage::Event(event_msg) => {
            // Handle event
        }
        _ => {}
    }
}
```

**Available Event Types:**
- `NewBlock`: New block connected to chain
- `NewTransaction`: New transaction in mempool
- `BlockDisconnected`: Block disconnected (chain reorg)
- `ChainReorg`: Chain reorganization occurred

## Configuration

### Node Configuration

Module system configuration in `NodeConfig`:

```json
{
  "modules": {
    "enabled": true,
    "modules_dir": "modules",
    "data_dir": "data/modules",
    "socket_dir": "data/modules/sockets",
    "enabled_modules": [],  // Empty = auto-discover all
    "module_configs": {
      "my-module": {
        "setting1": "value1"
      }
    }
  }
}
```

### Module Configuration

Modules can have their own configuration files:
- TOML format: `config.toml` in module directory
- Key-value format: `config` file in module directory

Configuration is passed to modules via environment variables.

## Security Model

### Permissions

Modules operate with **whitelist-only access control**. Each module declares required capabilities in its manifest:

- `read_blockchain`: Access to blockchain data
- `read_utxo`: Query UTXO set (read-only)
- `read_chain_state`: Query chain state (height, tip)
- `subscribe_events`: Receive node events
- `send_transactions`: Submit transactions to mempool (future)

### Sandboxing

Modules are sandboxed to ensure security:

1. **Process Isolation**: Separate process, isolated memory
2. **File System**: Access limited to module data directory
3. **Network**: No network access (modules can only communicate via IPC)
4. **Resource Limits**: CPU, memory, file descriptor limits (Phase 2+)

### Request Validation

All module API requests are validated:
- Permission checks (module has required permission)
- Consensus protection (no consensus-modifying operations)
- Resource limits (rate limiting, Phase 2+)

## Testing

### Test Utilities

Use the test utilities in `tests/module/test_utils.rs`:

```rust
use reference_node::tests::module::test_utils::*;

#[tokio::test]
async fn test_my_module() {
    let fixture = ModuleTestFixture::new().unwrap();
    // ... test module functionality
}
```

### Example Module

See `examples/simple-module/` for a complete working example.

## Troubleshooting

### Module Not Loading

- Check `module.toml` exists and is valid
- Verify module binary exists at expected path
- Check module logs for errors
- Verify module has required permissions

### IPC Connection Failures

- Ensure socket directory exists and is writable
- Check file permissions on socket directory
- Verify module process has access to socket

### Permission Denied Errors

- Check module manifest includes required capabilities
- Verify node configuration allows module permissions
- Review module API usage against permission model

## API Reference

See inline documentation in `reference-node/src/module/` for detailed API reference.

- **Module Traits**: `reference-node/src/module/traits.rs`
- **IPC Protocol**: `reference-node/src/module/ipc/protocol.rs`
- **Node API**: `reference-node/src/module/api/node_api.rs`
- **Security**: `reference-node/src/module/security/`


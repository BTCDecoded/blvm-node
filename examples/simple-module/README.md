# Simple Module Example

This is a minimal example module demonstrating how to create a module for the BTCDecoded reference-node.

## Features

- Module lifecycle implementation
- IPC communication with node
- Blockchain data queries
- Event subscription
- Error handling

## Building

```bash
cd examples/simple-module
cargo build --release
```

The binary will be at `target/release/simple-module`.

## Installation

1. Copy the module to the modules directory:
```bash
mkdir -p ../modules/simple-module
cp -r . ../modules/simple-module/
```

2. The module will be auto-discovered when the node starts.

## Running

The module is automatically started by the node when enabled. To test manually:

```bash
./target/release/simple-module \
  --module-id simple-module-1 \
  --socket-path /path/to/socket \
  --data-dir /path/to/data
```

## Configuration

Module-specific configuration can be set in:
- `config.toml` (TOML format)
- `config` (key=value format)

Configuration is passed via environment variables with prefix `MODULE_CONFIG_*`.

## Manifest

The `module.toml` file defines module metadata:

```toml
name = "simple-module"
version = "0.1.0"
description = "A simple example module"
author = "BTCDecoded Team"
capabilities = ["read_blockchain", "subscribe_events"]
entry_point = "simple-module"
```

## Code Structure

- `src/main.rs`: Main module implementation
  - Connects to node via IPC
  - Queries blockchain data
  - Subscribes to events
  - Handles module lifecycle

## See Also

- [Module System Documentation](../docs/MODULE_SYSTEM.md)
- Module development guide in main documentation


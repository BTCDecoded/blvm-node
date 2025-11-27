# Modules Directory

Contains runtime modules that can be loaded by bllvm-node.

## Module Structure

Each module should be in its own subdirectory with the following structure:

```
module-name/
├── module.toml          # Module manifest (required)
├── target/
│   └── release/
│       └── module-binary # Compiled module binary (required)
└── config.toml          # Module configuration (optional)
```

## Module Manifest (module.toml)

```toml
name = "module-name"
version = "0.1.0"
description = "Module description"
author = "Author name"
entry_point = "module-binary"

capabilities = ["capability1", "capability2"]

[dependencies]
other-module = "0.1.0"

[config_schema]
config_key = "Description"
```

## Installing Modules

1. Create a directory for your module: `mkdir modules/my-module`
2. Copy your module binary to: `modules/my-module/target/release/my-module`
3. Create `module.toml` manifest in the module directory
4. Restart bllvm-node or use runtime module loading

## Module Development

See `examples/simple-module/` for a complete example module implementation.

## Runtime Module Management

Modules can be loaded, unloaded, and reloaded at runtime:

```rust
// Load a module
node.module_manager_mut()
    .unwrap()
    .load_module("my-module", &binary_path, metadata, config)
    .await?;

// List loaded modules
let modules = node.module_manager()
    .unwrap()
    .list_modules()
    .await;

// Unload a module
node.module_manager_mut()
    .unwrap()
    .unload_module("my-module")
    .await?;

// Reload a module (hot reload)
node.module_manager_mut()
    .unwrap()
    .reload_module("my-module", &binary_path, metadata, config)
    .await?;
```

## Module Security

- Modules run in separate processes with isolated memory
- Modules cannot modify consensus rules or UTXO set
- Modules have read-only access to blockchain data
- Module crashes are isolated and don't affect the base node
- Modules communicate only through the IPC API


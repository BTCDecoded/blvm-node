# Modules Directory

Contains runtime modules that can be loaded by blvm-node.

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
# ============================================================================
# Module Manifest
# ============================================================================

# ----------------------------------------------------------------------------
# Core Identity (Required)
# ----------------------------------------------------------------------------
name = "module-name"
version = "0.1"
entry_point = "module-binary"

# ----------------------------------------------------------------------------
# Metadata (Optional)
# ----------------------------------------------------------------------------
description = "Module description"
author = "Author name"

# ----------------------------------------------------------------------------
# Capabilities
# ----------------------------------------------------------------------------
capabilities = ["read_blockchain", "subscribe_events"]

# ----------------------------------------------------------------------------
# Dependencies
# ----------------------------------------------------------------------------
[dependencies]
"other-module" = ">=0.1.0"

[optional_dependencies]
# "optional-module" = ">=0.5.0"

# ----------------------------------------------------------------------------
# Configuration Schema (Optional)
# ----------------------------------------------------------------------------
[config_schema]
config_key = "Description of this configuration option"
```

## Installing Modules

1. Create a directory for your module: `mkdir modules/my-module`
2. Copy your module binary to: `modules/my-module/target/release/my-module`
3. Create `module.toml` manifest in the module directory
4. Restart blvm-node or use runtime module loading

## Auto-install from registry (bootstrap)

Official modules (`blvm-miniscript`, `blvm-zmq`, …) publish `module.toml` on GitHub with a `[downloads]` table (URLs + SHA-256 per platform). The node can download and install those binaries on first boot when they are listed in **`enabled_modules`** but not yet present under **`modules_dir`**.

Requirements:

- **`[modules].enabled_modules`** must name the modules to pull in (non-empty allowlist).
- **`[modules].registry_url`** must point at a **`modules.json`** discovery index (array of `{ "name", "module_toml_url" }`).  
  Example (Bitcoin Commons monorepo):

  ```toml
  [modules]
  enabled_modules = ["blvm-miniscript", "blvm-zmq"]
  registry_url = "https://raw.githubusercontent.com/BTCDecoded/blvm/main/registry/modules.json"
  ```

  For backward compatibility, the same URL may instead be set as  
  **`[modules.blvm-marketplace] registry_url`** — the node uses `registry_url` on `[modules]` when set, otherwise that legacy key.

On startup you should see log lines like `Bootstrap: fetching manifest for 'blvm-miniscript'` and `Bootstrap: installed ...`. Then the normal auto-load path runs.

To sanity-check releases without starting the node, run  
`scripts/verify-published-modules.sh` (requires Python 3.11+, `curl`).

Configure **`blvm-zmq`** PUB endpoints via **`[modules.blvm-zmq]`** in the same config file (keys match `module.toml` `[config_schema]`, e.g. `hashblock`, `hashtx`, …).

## Module Development

See `examples/simple-module/` for a complete example module implementation.

## Runtime Module Management

Modules can be loaded, unloaded, and reloaded at runtime via **RPC**, **CLI**, or **programmatically**:

### RPC (JSON-RPC)

```bash
# Load a module
bitcoin-cli loadmodule "my-module"

# Unload a module
bitcoin-cli unloadmodule "my-module"

# Reload a module (hot reload)
bitcoin-cli reloadmodule "my-module"

# List loaded modules
bitcoin-cli listmodules
```

### CLI (blvm binary)

```bash
blvm module load my-module
blvm module unload my-module
blvm module reload my-module
blvm module list
```

Universal shorthand (same as above):
```bash
blvm unload my-module
blvm reload my-module
blvm config-path my-module   # Print module config file path (works offline)
```

### Programmatic (ModuleManager)

```rust
let manager = node.module_manager().unwrap();
let mut mgr = manager.lock().await;

// Load a module
mgr.load_module("my-module", &binary_path, metadata, config).await?;

// List loaded modules
let modules = mgr.list_modules().await;

// Unload a module
mgr.unload_module("my-module").await?;

// Reload a module (hot reload)
mgr.reload_module("my-module", &binary_path, metadata, config).await?;
```

### File Watcher (optional)

With the `module-watcher` feature, the node watches the modules directory for changes to `module.toml`, `config.toml`, or module binaries and automatically reloads loaded modules.

Config in `[modules]`:
- `watch_enabled` (default: true) — enable/disable the watcher
- `watch_auto_load` (default: false) — auto-load new modules when `module.toml` appears
- `watch_auto_unload` (default: false) — auto-unload when a module directory is removed

## Module Security

- Modules run in separate processes with isolated memory
- Modules cannot modify consensus rules or UTXO set
- Modules have read-only access to blockchain data
- Module crashes are isolated and don't affect the base node
- Modules communicate only through the IPC API


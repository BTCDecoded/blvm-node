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

Official modules (`blvm-miniscript`, `blvm-zmq`, …) publish **`module.toml`** on GitHub (identity + semver **`version`**) and attach **`sha256sums.txt`** plus per-platform binaries to each **GitHub Release** tag `v{version}`. The node downloads the checksum file and the binary for your platform (`{name}-x86_64-linux`, etc.), verifies SHA-256, and installs when a module is **pinned in config** but missing or the wrong version under **`modules_dir`**.

### Discovery vs allowlist vs opt-out

- **No pins in config (`enabled_modules` empty, `[modules]` omitted or no inline keys):** every module **discovered** under `modules_dir` is a candidate to auto-load; **no** HTTP bootstrap runs.
- **Non-empty pins:** only listed manifest names load from disk; missing names or versions that **do not match** the constraint may be **bootstrap-downloaded** when `registry_url` is set and the node was built with **`governance`** (on by default for Bitcoin Commons `blvm`).
- **`disabled_modules` (opt-out):** listed manifest names are **never** auto-loaded and are **skipped** for bootstrap. If a name is in both pins and `disabled_modules`, **disabled wins** (with a log warning).

Explicit **`loadmodule`** RPC still loads a module by name when invoked; `disabled_modules` applies to startup auto-load, watcher-driven `auto_load_modules`, and registry bootstrap.

Requirements for bootstrap (official binaries):

- **`[modules].registry_url`** should point at a **`modules.json`** discovery index (array of `{ "name", "repo" (owner/repo), optional "module_toml_url", optional "manifest_ref" }`). Default: **`https://raw.githubusercontent.com/BTCDecoded/blvm/main/registry/modules.json`**.
- **Version pins** name each module and optional constraint. Bootstrap selects the **highest GitHub Release** matching the constraint, fetches `module.toml` at tag `v{version}`, and installs the release artifact. Unpinned legacy entries (`enabled_modules = ["name"]` or constraint `"*"`) still fetch manifest from **`main`** (floating).

  Example (recommended — see `blvm/blvm.toml.example`):

  ```toml
  [modules]
  registry_url = "https://raw.githubusercontent.com/BTCDecoded/blvm/main/registry/modules.json"
  blvm-miniscript = "0.1.*"
  [modules.blvm-zmq]
  version = "0.3.*"
  hashblock = "tcp://127.0.0.1:28332"
  ```

  Legacy unpinned allowlist:

  ```toml
  [modules]
  enabled_modules = ["blvm-miniscript", "blvm-zmq"]
  registry_url = "https://raw.githubusercontent.com/BTCDecoded/blvm/main/registry/modules.json"
  ```

  For backward compatibility, `registry_url` may instead be set as **`[modules.blvm-marketplace] registry_url`** — the node prefers `[modules].registry_url` when set.

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


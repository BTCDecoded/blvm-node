//! WASM module loader trait.
//!
//! blvm-node defines this interface. Implementations (e.g. from blvm-sdk) are
//! injected by the binary/compositor. blvm-node does not depend on blvm-sdk.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::module::ipc::protocol::{CliSpec, InvocationResultPayload};
use crate::module::traits::ModuleError;

/// Opaque handle to a loaded WASM module instance.
/// blvm-node stores this; the concrete type comes from the loader implementation.
pub trait WasmModuleInstance: Send + Sync {
    /// Invoke a CLI subcommand (sync; host calls into module).
    fn invoke_cli(
        &self,
        subcommand: &str,
        args: Vec<String>,
    ) -> Result<InvocationResultPayload, ModuleError>;

    /// Invoke an RPC method (sync; host calls into module).
    fn invoke_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ModuleError>;

    /// Get CLI spec (for getmoduleclispecs). Returns None if not available.
    fn cli_spec(&self) -> Option<CliSpec>;
}

/// Loads WASM modules from path with storage and config.
/// Implemented by blvm-sdk; injected into blvm-node by the binary (e.g. blvm).
pub trait WasmModuleLoader: Send + Sync {
    fn load(
        &self,
        path: &Path,
        data_dir: &Path,
        config: HashMap<String, String>,
    ) -> Result<Arc<dyn WasmModuleInstance>, String>;
}

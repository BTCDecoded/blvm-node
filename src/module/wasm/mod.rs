//! WASM module loading.
//!
//! blvm-node defines the `WasmModuleLoader` trait. The implementation (from blvm-sdk)
//! is injected by the binary. blvm-node does not depend on blvm-sdk.

mod loader_trait;

pub use loader_trait::{WasmModuleInstance, WasmModuleLoader};

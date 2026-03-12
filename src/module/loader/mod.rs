//! Module loading system
//!
//! Handles dynamic module loading, initialization, and configuration.

#[allow(clippy::module_inception)]
pub mod loader;

pub use loader::ModuleLoader;

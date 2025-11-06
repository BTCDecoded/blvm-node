//! Module registry and discovery
//!
//! Handles module discovery, manifest parsing, and dependency resolution.

pub mod dependencies;
pub mod discovery;
pub mod manifest;

pub use dependencies::{DependencyResolution, ModuleDependencies};
pub use discovery::{DiscoveredModule, ModuleDiscovery};
pub use manifest::ModuleManifest;

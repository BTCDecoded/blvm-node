//! Module registry and discovery
//! 
//! Handles module discovery, manifest parsing, and dependency resolution.

pub mod discovery;
pub mod manifest;
pub mod dependencies;

pub use discovery::{ModuleDiscovery, DiscoveredModule};
pub use manifest::ModuleManifest;
pub use dependencies::{ModuleDependencies, DependencyResolution};


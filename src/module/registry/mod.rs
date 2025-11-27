//! Module registry and discovery
//!
//! Handles module discovery, manifest parsing, dependency resolution,
//! and decentralized module registry with content-addressable storage.

pub mod cache;
pub mod cas;
pub mod client;
pub mod dependencies;
pub mod discovery;
pub mod error;
pub mod manifest;

pub use cache::{CachedModule, LocalCache};
pub use cas::{ContentAddressableStorage, ModuleHash};
pub use client::{ModuleEntry, ModuleRegistry, RegistryMirror};
pub use dependencies::{DependencyResolution, ModuleDependencies};
pub use discovery::{DiscoveredModule, ModuleDiscovery};
pub use error::RegistryError;
pub use manifest::ModuleManifest;

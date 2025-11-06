//! Process sandboxing and resource limits for modules
//!
//! Provides OS-level sandboxing, file system access control, and network isolation
//! to ensure modules cannot access resources outside their allowed scope.

pub mod filesystem;
pub mod network;
pub mod process;

pub use filesystem::FileSystemSandbox;
pub use network::NetworkSandbox;
pub use process::{ProcessSandbox, ResourceLimits, SandboxConfig};

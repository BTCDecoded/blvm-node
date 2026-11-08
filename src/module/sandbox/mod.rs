//! Process sandboxing and resource limits for modules
//!
//! On Linux, module spawn enacts a Landlock domain (`landlock`) before exec.
//! Resource limits are applied with `prlimit` after the child starts.

pub mod filesystem;
pub mod landlock;
pub mod process;

pub use filesystem::FileSystemSandbox;
pub use process::{ProcessSandbox, ResourceLimits, SandboxConfig};

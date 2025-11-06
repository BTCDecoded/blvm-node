//! Process management for modules
//!
//! Handles spawning, monitoring, and lifecycle management of module processes.

pub mod monitor;
pub mod spawner;

pub use monitor::{ModuleHealth, ModuleProcessMonitor};
pub use spawner::{ModuleProcess, ModuleProcessSpawner};

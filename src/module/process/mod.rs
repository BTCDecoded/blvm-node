//! Process management for modules
//! 
//! Handles spawning, monitoring, and lifecycle management of module processes.

pub mod spawner;
pub mod monitor;

pub use spawner::{ModuleProcessSpawner, ModuleProcess};
pub use monitor::{ModuleProcessMonitor, ModuleHealth};


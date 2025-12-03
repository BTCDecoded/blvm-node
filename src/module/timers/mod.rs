//! Timer and scheduled task system for modules
//!
//! Allows modules to register periodic timers and one-time scheduled tasks.

pub mod manager;

pub use manager::TimerManager;

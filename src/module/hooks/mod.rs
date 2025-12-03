//! Module hooks system
//!
//! Allows modules to provide cached data for expensive operations.
//! All hooks are advisory and have timeouts to prevent hanging.

pub mod manager;

pub use manager::HookManager;

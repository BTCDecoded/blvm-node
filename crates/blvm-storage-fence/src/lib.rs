//! Storage engines live here. Callers depend on this crate, not on the engines.

#[cfg(feature = "heed3")]
pub use heed3;
#[cfg(feature = "redb")]
pub use redb;
#[cfg(feature = "rocksdb")]
pub use rocksdb;
#[cfg(feature = "sled")]
pub use sled;

/// Crate names this fence owns. A node manifest that lists one of these fails the dependency test.
pub const ENGINE_CRATES: &[&str] = &["heed3", "redb", "sled", "rocksdb"];

//! Time utilities for fault tolerance
//!
//! Provides safe time operations that handle errors gracefully.

use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Get current Unix timestamp (seconds since epoch)
///
/// Returns 0 if system time is before epoch (should never happen).
/// This prevents panics from system time issues.
pub fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| {
            warn!("System time is before UNIX epoch, using 0 as timestamp");
            std::time::Duration::from_secs(0)
        })
        .as_secs()
}

/// Get current Unix timestamp as Duration
///
/// Returns zero duration if system time is before epoch.
pub fn current_timestamp_duration() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| {
            warn!("System time is before UNIX epoch, using zero duration");
            std::time::Duration::from_secs(0)
        })
}

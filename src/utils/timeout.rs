//! Timeout utilities for fault tolerance
//!
//! Provides timeout wrappers for operations that might hang.
//!
//! **Default Timeouts**: These utilities use hardcoded default timeouts:
//! - Network: 30 seconds
//! - Storage: 10 seconds
//! - RPC: 60 seconds
//!
//! **Configurable Timeouts**: Use `storage_timeout_from_config()` etc. to get
//! `Duration` from `RequestTimeoutConfig`, then pass to `with_custom_timeout()`.
//! RPC handlers receive config via `with_request_timeouts()` and use these helpers.

use std::time::Duration;
use tokio::time::{Timeout, timeout};

/// Apply timeout to a future
pub fn with_timeout<F>(future: F, duration: Duration) -> Timeout<F>
where
    F: std::future::Future,
{
    timeout(duration, future)
}

/// Default timeout for network operations
///
/// Note: These are fallback defaults. Prefer using configurable timeouts
/// from RequestTimeoutConfig when available.
pub const DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for storage operations
///
/// Note: These are fallback defaults. Prefer using configurable timeouts
/// from RequestTimeoutConfig when available.
pub const DEFAULT_STORAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for RPC operations
///
/// Note: These are fallback defaults. Prefer using configurable timeouts
/// from RequestTimeoutConfig when available.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// Execute operation with default network timeout
pub async fn with_network_timeout<F, T>(operation: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    timeout(DEFAULT_NETWORK_TIMEOUT, operation).await
}

/// Execute operation with default storage timeout
pub async fn with_storage_timeout<F, T>(operation: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    timeout(DEFAULT_STORAGE_TIMEOUT, operation).await
}

/// Execute operation with default RPC timeout
pub async fn with_rpc_timeout<F, T>(operation: F) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    timeout(DEFAULT_RPC_TIMEOUT, operation).await
}

/// Execute operation with custom timeout
pub async fn with_custom_timeout<F, T>(
    operation: F,
    duration: Duration,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    timeout(duration, operation).await
}

/// Storage timeout duration from config, or default when config is None.
/// Use with `with_custom_timeout(operation, storage_timeout_from_config(config)).await`.
#[inline]
pub fn storage_timeout_from_config(
    config: Option<&crate::config::RequestTimeoutConfig>,
) -> Duration {
    config
        .map(|c| Duration::from_secs(c.storage_timeout_seconds))
        .unwrap_or(DEFAULT_STORAGE_TIMEOUT)
}

/// RPC timeout duration from config, or default when config is None.
#[inline]
pub fn rpc_timeout_from_config(config: Option<&crate::config::RequestTimeoutConfig>) -> Duration {
    config
        .map(|c| Duration::from_secs(c.rpc_timeout_seconds))
        .unwrap_or(DEFAULT_RPC_TIMEOUT)
}

/// Handshake timeout from config (default 10s).
#[inline]
pub fn handshake_timeout_from_config(
    config: Option<&crate::config::RequestTimeoutConfig>,
) -> Duration {
    config
        .map(|c| Duration::from_secs(c.handshake_timeout_secs))
        .unwrap_or(Duration::from_secs(10))
}

#[cfg(test)]
mod network_timeout_field_tests {
    use std::path::Path;

    fn needle() -> String {
        format!("{}{}", "network_timeout_", "seconds")
    }

    fn walk(dir: &Path, hits: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir).unwrap();
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, hits);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                if text.contains(&needle()) {
                    hits.push(path.display().to_string());
                }
            }
        }
    }

    #[test]
    fn network_timeout_field_is_not_in_src() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        walk(&src, &mut hits);
        assert!(
            hits.is_empty(),
            "src still names the removed field: {hits:?}"
        );

        let config = std::fs::read_to_string(src.join("config/mod.rs")).unwrap();
        assert!(config.contains("connect_timeout_secs"));
        assert!(config.contains("handshake_timeout_secs"));
        assert!(config.contains("async_request_timeout_seconds"));
        assert!(config.contains("rpc_timeout_seconds"));
        let timeouts = std::fs::read_to_string(src.join("utils/timeout.rs")).unwrap();
        assert!(timeouts.contains("fn rpc_timeout_from_config"));
        assert!(timeouts.contains("pub async fn with_network_timeout"));
        assert!(timeouts.contains("DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(30)"));
    }
}

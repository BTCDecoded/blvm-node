//! Timeout utilities for fault tolerance
//!
//! Provides timeout wrappers for operations that might hang.
//!
//! **Default Timeouts**: These utilities use hardcoded default timeouts:
//! - Network: 30 seconds
//! - Storage: 10 seconds  
//! - RPC: 60 seconds
//!
//! **Configurable Timeouts**: For configurable timeouts, use `RequestTimeoutConfig`
//! from `crate::config::RequestTimeoutConfig` and pass values to `with_custom_timeout()`:
//!
//! ```rust
//! use crate::utils::with_custom_timeout;
//! use std::time::Duration;
//!
//! let config_timeout = Duration::from_secs(config.storage_timeout_seconds);
//! with_custom_timeout(operation, config_timeout).await
//! ```

use std::time::Duration;
use tokio::time::{timeout, Timeout};

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
pub async fn with_network_timeout<F, T>(
    operation: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    timeout(DEFAULT_NETWORK_TIMEOUT, operation).await
}

/// Execute operation with default storage timeout
pub async fn with_storage_timeout<F, T>(
    operation: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    timeout(DEFAULT_STORAGE_TIMEOUT, operation).await
}

/// Execute operation with default RPC timeout
pub async fn with_rpc_timeout<F, T>(
    operation: F,
) -> Result<T, tokio::time::error::Elapsed>
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


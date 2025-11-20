//! Async operation helpers
//!
//! Provides utilities for common async patterns.

use std::time::Duration;
use tokio::time::sleep;

/// Execute an operation with a delay before retrying
///
/// Useful for retry logic with delays.
pub async fn delay_before<F, Fut, T>(delay: Duration, operation: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    sleep(delay).await;
    operation().await
}

/// Execute an operation and ignore errors (logging them)
///
/// Returns `Some(T)` on success, `None` on error (after logging).
pub async fn ignore_error<F, Fut, T, E>(operation: F, context: &str) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match operation().await {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::debug!("{}: {}", context, e);
            None
        }
    }
}

/// Execute multiple async operations and collect results
///
/// Returns a vector of results, with None for failed operations.
pub async fn collect_results<F, Fut, T, E>(
    operations: Vec<F>,
    context: &str,
) -> Vec<Option<T>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut results = Vec::new();
    for (i, op) in operations.into_iter().enumerate() {
        let ctx = format!("{} (operation {})", context, i);
        results.push(ignore_error(op, &ctx).await);
    }
    results
}

/// Execute an operation with a timeout, returning None on timeout
///
/// Returns `Some(T)` on success, `None` on timeout or error.
pub async fn with_timeout_opt<F, Fut, T>(
    operation: F,
    timeout_duration: Duration,
    context: &str,
) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    use tokio::time::timeout as tokio_timeout;
    match tokio_timeout(timeout_duration, operation()).await {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!("{}: Operation timed out after {:?}", context, timeout_duration);
            None
        }
    }
}


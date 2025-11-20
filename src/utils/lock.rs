//! Lock utilities for async contexts
//!
//! Provides helpers for common lock patterns with automatic release.

use tokio::sync::{Mutex, RwLock};
use tracing::warn;

/// Execute a closure with a Mutex lock, automatically releasing it
///
/// # Example
/// ```rust
/// use crate::utils::with_lock;
///
/// let result = with_lock(&mutex, |guard| {
///     // Use guard here
///     guard.do_something()
/// }).await;
/// ```
pub async fn with_lock<T, F, R>(mutex: &Mutex<T>, f: F) -> R
where
    F: FnOnce(&mut T) -> R,
{
    let mut guard = mutex.lock().await;
    f(&mut guard)
}

/// Execute a closure with a read lock, automatically releasing it
///
/// # Example
/// ```rust
/// use crate::utils::with_read_lock;
///
/// let value = with_read_lock(&rwlock, |guard| {
///     guard.get_value()
/// }).await;
/// ```
pub async fn with_read_lock<T, F, R>(rwlock: &RwLock<T>, f: F) -> R
where
    F: FnOnce(&T) -> R,
{
    let guard = rwlock.read().await;
    f(&guard)
}

/// Execute a closure with a write lock, automatically releasing it
///
/// # Example
/// ```rust
/// use crate::utils::with_write_lock;
///
/// with_write_lock(&rwlock, |guard| {
///     guard.set_value(new_value);
/// }).await;
/// ```
pub async fn with_write_lock<T, F, R>(rwlock: &RwLock<T>, f: F) -> R
where
    F: FnOnce(&mut T) -> R,
{
    let mut guard = rwlock.write().await;
    f(&mut guard)
}

/// Try to acquire a lock with a timeout
///
/// Returns `Some(R)` if lock acquired and operation succeeded, `None` on timeout.
pub async fn try_with_lock_timeout<T, F, R>(
    mutex: &Mutex<T>,
    timeout: std::time::Duration,
    f: F,
    context: &str,
) -> Option<R>
where
    F: FnOnce(&mut T) -> R,
{
    use tokio::time::timeout as tokio_timeout;
    match tokio_timeout(timeout, mutex.lock()).await {
        Ok(mut guard) => Some(f(&mut guard)),
        Err(_) => {
            warn!("{}: Failed to acquire lock within {:?}", context, timeout);
            None
        }
    }
}


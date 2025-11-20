//! Option utilities for common patterns
//!
//! Provides helpers for working with Option values.

/// Get value from Option or compute default
///
/// Similar to `unwrap_or_else`, but with a context message for logging.
///
/// # Example
/// ```rust
/// use crate::utils::unwrap_or_default_with;
///
/// let value = unwrap_or_default_with(opt, || {
///     tracing::debug!("Using default value");
///     0
/// });
/// ```
pub fn unwrap_or_default_with<T, F>(opt: Option<T>, f: F) -> T
where
    F: FnOnce() -> T,
{
    opt.unwrap_or_else(f)
}

/// Convert Option to Result with a context message
///
/// # Example
/// ```rust
/// use crate::utils::option_to_result;
///
/// let value = option_to_result(opt, "Value not found")?;
/// ```
pub fn option_to_result<T>(opt: Option<T>, context: &str) -> Result<T, String> {
    opt.ok_or_else(|| context.to_string())
}

/// Map Option and handle None case
///
/// Maps the Some value, or returns a default computed from the context.
///
/// # Example
/// ```rust
/// use crate::utils::map_or_default;
///
/// let result = map_or_default(opt, |v| v * 2, || 0);
/// ```
pub fn map_or_default<T, U, F, D>(opt: Option<T>, f: F, default: D) -> U
where
    F: FnOnce(T) -> U,
    D: FnOnce() -> U,
{
    opt.map(f).unwrap_or_else(default)
}

/// Chain two Option operations, returning first Some or None
///
/// # Example
/// ```rust
/// use crate::utils::or_else;
///
/// let result = or_else(opt1, || opt2);
/// ```
pub fn or_else<T, F>(opt: Option<T>, f: F) -> Option<T>
where
    F: FnOnce() -> Option<T>,
{
    opt.or_else(f)
}

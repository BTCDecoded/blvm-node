//! Validation utilities
//!
//! Provides helpers for common validation patterns.

/// Ensure a condition is true, or return an error
///
/// Similar to `assert!` but returns a Result instead of panicking.
///
/// # Example
/// ```rust
/// use crate::utils::ensure;
///
/// ensure(value > 0, "Value must be positive")?;
/// ```
pub fn ensure(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

/// Ensure a condition is true with a formatted error message
///
/// # Example
/// ```rust
/// use crate::utils::ensure_fmt;
///
/// ensure_fmt(value > 0, || format!("Value {} must be positive", value))?;
/// ```
pub fn ensure_fmt<F>(condition: bool, message: F) -> Result<(), String>
where
    F: FnOnce() -> String,
{
    if condition {
        Ok(())
    } else {
        Err(message())
    }
}

/// Validate a value is within a range
///
/// Returns `Ok(())` if `min <= value <= max`, error otherwise.
pub fn ensure_range<T>(value: T, min: T, max: T, name: &str) -> Result<(), String>
where
    T: PartialOrd + std::fmt::Display,
{
    if value < min || value > max {
        Err(format!("{} must be between {} and {}, got {}", name, min, max, value))
    } else {
        Ok(())
    }
}

/// Validate a value is not empty
///
/// Returns `Ok(())` if value is not empty, error otherwise.
pub fn ensure_not_empty<T>(value: &[T], name: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(format!("{} must not be empty", name))
    } else {
        Ok(())
    }
}

/// Validate a value is not None
///
/// Returns `Ok(value)` if Some, error if None.
pub fn ensure_some<T>(value: Option<T>, name: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("{} must be set", name))
}


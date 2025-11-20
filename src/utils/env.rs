//! Environment variable utilities
//!
//! Provides helpers for reading environment variables with defaults.

/// Get environment variable or return default value
///
/// # Example
/// ```rust
/// use crate::utils::env_or_default;
///
/// let data_dir = env_or_default("DATA_DIR", "data");
/// ```
pub fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Get environment variable or compute default
///
/// # Example
/// ```rust
/// use crate::utils::env_or_else;
///
/// let data_dir = env_or_else("DATA_DIR", || "data".to_string());
/// ```
pub fn env_or_else<F>(key: &str, f: F) -> String
where
    F: FnOnce() -> String,
{
    std::env::var(key).unwrap_or_else(|_| f())
}

/// Get environment variable as Option
///
/// Returns `Some(value)` if set, `None` if not set.
pub fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Get environment variable as boolean
///
/// Returns `true` if value is "true", "1", "yes", "on" (case-insensitive).
/// Returns `false` otherwise or if not set.
pub fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            let v_lower = v.to_lowercase();
            v_lower == "true" || v_lower == "1" || v_lower == "yes" || v_lower == "on"
        })
        .unwrap_or(false)
}

/// Get environment variable as integer
///
/// Returns `Some(value)` if set and parseable, `None` otherwise.
pub fn env_int<T>(key: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    std::env::var(key).ok()?.parse().ok()
}


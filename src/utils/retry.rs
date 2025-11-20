//! Retry utilities for fault tolerance
//!
//! Provides retry logic with exponential backoff for transient failures.

use std::time::Duration;
use tokio::time::sleep;

/// Retry configuration
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Initial delay between retries (exponential backoff)
    pub initial_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Multiplier for exponential backoff
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Create a new retry configuration
    pub fn new(max_attempts: u32, initial_delay: Duration) -> Self {
        Self {
            max_attempts,
            initial_delay,
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
        }
    }

    /// Create configuration for network operations
    pub fn network() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(60),
            backoff_multiplier: 2.0,
        }
    }

    /// Create configuration for storage operations
    pub fn storage() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            backoff_multiplier: 1.5,
        }
    }
}

/// Retry an operation with exponential backoff
pub async fn retry_with_backoff<F, T, E>(config: &RetryConfig, mut operation: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    E: std::fmt::Display,
{
    let mut delay = config.initial_delay;
    let mut last_error = None;

    for attempt in 0..config.max_attempts {
        match operation() {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_error = Some(e);
                if attempt < config.max_attempts - 1 {
                    tracing::debug!(
                        "Operation failed (attempt {}/{}): {}. Retrying in {:?}...",
                        attempt + 1,
                        config.max_attempts,
                        last_error.as_ref().unwrap(),
                        delay
                    );
                    sleep(delay).await;
                    delay = std::cmp::min(
                        Duration::from_secs_f64(delay.as_secs_f64() * config.backoff_multiplier),
                        config.max_delay,
                    );
                }
            }
        }
    }

    Err(last_error.expect("Should have at least one error"))
}

/// Retry an async operation with exponential backoff
pub async fn retry_async_with_backoff<F, Fut, T, E>(
    config: &RetryConfig,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = config.initial_delay;
    let mut last_error = None;

    for attempt in 0..config.max_attempts {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_error = Some(e);
                if attempt < config.max_attempts - 1 {
                    tracing::debug!(
                        "Async operation failed (attempt {}/{}): {}. Retrying in {:?}...",
                        attempt + 1,
                        config.max_attempts,
                        last_error.as_ref().unwrap(),
                        delay
                    );
                    sleep(delay).await;
                    delay = std::cmp::min(
                        Duration::from_secs_f64(delay.as_secs_f64() * config.backoff_multiplier),
                        config.max_delay,
                    );
                }
            }
        }
    }

    Err(last_error.expect("Should have at least one error"))
}

/// Check if an error is retryable (transient failure)
pub trait IsRetryable {
    fn is_retryable(&self) -> bool;
}

/// Retry only if error is retryable
pub async fn retry_if_retryable<F, Fut, T, E>(
    config: &RetryConfig,
    mut operation: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: IsRetryable + std::fmt::Display,
{
    let mut delay = config.initial_delay;
    let mut last_error = None;

    for attempt in 0..config.max_attempts {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if !e.is_retryable() {
                    // Non-retryable error - return immediately
                    return Err(e);
                }

                last_error = Some(e);
                if attempt < config.max_attempts - 1 {
                    tracing::debug!(
                        "Retryable error (attempt {}/{}): {}. Retrying in {:?}...",
                        attempt + 1,
                        config.max_attempts,
                        last_error.as_ref().unwrap(),
                        delay
                    );
                    sleep(delay).await;
                    delay = std::cmp::min(
                        Duration::from_secs_f64(delay.as_secs_f64() * config.backoff_multiplier),
                        config.max_delay,
                    );
                }
            }
        }
    }

    Err(last_error.expect("Should have at least one error"))
}

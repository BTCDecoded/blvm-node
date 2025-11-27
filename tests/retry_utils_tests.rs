//! Tests for retry utilities

use bllvm_node::utils::retry::{retry_with_backoff, RetryConfig};
use std::time::Duration;

#[test]
fn test_retry_config_default() {
    let config = RetryConfig::default();
    assert_eq!(config.max_attempts, 3);
    assert_eq!(config.initial_delay, Duration::from_millis(100));
    assert_eq!(config.max_delay, Duration::from_secs(30));
    assert_eq!(config.backoff_multiplier, 2.0);
}

#[test]
fn test_retry_config_new() {
    let config = RetryConfig::new(5, Duration::from_millis(200));
    assert_eq!(config.max_attempts, 5);
    assert_eq!(config.initial_delay, Duration::from_millis(200));
    assert_eq!(config.max_delay, Duration::from_secs(30));
    assert_eq!(config.backoff_multiplier, 2.0);
}

#[test]
fn test_retry_config_network() {
    let config = RetryConfig::network();
    assert_eq!(config.max_attempts, 5);
    assert_eq!(config.initial_delay, Duration::from_millis(500));
    assert_eq!(config.max_delay, Duration::from_secs(60));
}

#[test]
fn test_retry_config_storage() {
    let config = RetryConfig::storage();
    assert_eq!(config.max_attempts, 3);
    assert_eq!(config.initial_delay, Duration::from_millis(100));
    assert_eq!(config.max_delay, Duration::from_secs(10));
    assert_eq!(config.backoff_multiplier, 1.5);
}

#[tokio::test]
async fn test_retry_success_first_attempt() {
    let config = RetryConfig::new(3, Duration::from_millis(10));
    let attempt = std::sync::Arc::new(std::sync::Mutex::new(0));
    let attempt_clone = attempt.clone();

    let result: Result<i32, String> = retry_with_backoff(&config, move || {
        let mut count = attempt_clone.lock().unwrap();
        *count += 1;
        Ok(*count)
    })
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 1);
    assert_eq!(*attempt.lock().unwrap(), 1);
}

#[tokio::test]
async fn test_retry_success_after_failures() {
    let config = RetryConfig::new(3, Duration::from_millis(10));
    let attempt = std::sync::Arc::new(std::sync::Mutex::new(0));
    let attempt_clone = attempt.clone();

    let result = retry_with_backoff(&config, move || {
        let mut count = attempt_clone.lock().unwrap();
        *count += 1;
        if *count < 3 {
            Err("Failed".to_string())
        } else {
            Ok(*count)
        }
    })
    .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 3);
    assert_eq!(*attempt.lock().unwrap(), 3);
}

#[tokio::test]
async fn test_retry_exhausts_attempts() {
    let config = RetryConfig::new(3, Duration::from_millis(10));
    let attempt = std::sync::Arc::new(std::sync::Mutex::new(0));
    let attempt_clone = attempt.clone();

    let result: Result<i32, String> = retry_with_backoff(&config, move || {
        let mut count = attempt_clone.lock().unwrap();
        *count += 1;
        Err(format!("Error {}", *count))
    })
    .await;

    assert!(result.is_err());
    assert_eq!(*attempt.lock().unwrap(), 3);
    assert!(result.unwrap_err().contains("Error 3"));
}

// Note: Async retry tests are complex due to closure capture requirements
// The sync retry tests above provide good coverage of the retry logic
// Async functionality is tested through integration tests

#[test]
fn test_retry_config_clone() {
    let config = RetryConfig::default();
    let config2 = config.clone();

    assert_eq!(config.max_attempts, config2.max_attempts);
    assert_eq!(config.initial_delay, config2.initial_delay);
    assert_eq!(config.max_delay, config2.max_delay);
    assert_eq!(config.backoff_multiplier, config2.backoff_multiplier);
}

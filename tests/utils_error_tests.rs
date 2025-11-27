//! Error handling utility tests
//!
//! Tests for error handling helpers.

use bllvm_node::utils::error::{
    err_option_to_result, log_error, log_error_async, result_to_option, with_default,
    with_default_async, with_fallback, with_fallback_async,
};

#[test]
fn test_log_error_success() {
    let result = log_error(|| Ok::<i32, String>(42), "test operation");

    assert_eq!(result, Some(42));
}

#[test]
fn test_log_error_failure() {
    let result = log_error(
        || Err::<i32, String>("error message".to_string()),
        "test operation",
    );

    assert_eq!(result, None);
}

#[tokio::test]
async fn test_log_error_async_success() {
    let result = log_error_async(|| async { Ok::<i32, String>(42) }, "test operation").await;

    assert_eq!(result, Some(42));
}

#[tokio::test]
async fn test_log_error_async_failure() {
    let result = log_error_async(
        || async { Err::<i32, String>("error message".to_string()) },
        "test operation",
    )
    .await;

    assert_eq!(result, None);
}

#[test]
fn test_result_to_option_ok() {
    let result: Result<i32, String> = Ok(42);

    let opt = result_to_option(result, "test operation");

    assert_eq!(opt, Some(42));
}

#[test]
fn test_result_to_option_err() {
    let result: Result<i32, String> = Err("error".to_string());

    let opt = result_to_option(result, "test operation");

    assert_eq!(opt, None);
}

#[test]
fn test_err_option_to_result_some() {
    let opt = Some(42);

    let result: Result<i32, String> = err_option_to_result(opt, || "not found".to_string());

    assert_eq!(result.unwrap(), 42);
}

#[test]
fn test_err_option_to_result_none() {
    let opt: Option<i32> = None;

    let result: Result<i32, String> = err_option_to_result(opt, || "not found".to_string());

    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "not found");
}

#[test]
fn test_with_default_some() {
    let value = with_default(|| Ok::<i32, String>(42), 100, "test operation");

    assert_eq!(value, 42);
}

#[test]
fn test_with_default_none() {
    let value = with_default(
        || Err::<i32, String>("error".to_string()),
        100,
        "test operation",
    );

    assert_eq!(value, 100);
}

#[tokio::test]
async fn test_with_default_async_some() {
    let value = with_default_async(|| async { Ok::<i32, String>(42) }, 100, "test operation").await;

    assert_eq!(value, 42);
}

#[tokio::test]
async fn test_with_default_async_none() {
    let value = with_default_async(
        || async { Err::<i32, String>("error".to_string()) },
        100,
        "test operation",
    )
    .await;

    assert_eq!(value, 100);
}

#[test]
fn test_with_fallback_success() {
    let value = with_fallback(|| Ok::<i32, String>(42), || 100, "primary failed");

    assert_eq!(value, 42);
}

#[test]
fn test_with_fallback_fallback() {
    let value = with_fallback(
        || Err::<i32, String>("primary error".to_string()),
        || 100,
        "primary failed",
    );

    assert_eq!(value, 100);
}

#[tokio::test]
async fn test_with_fallback_async_success() {
    let value = with_fallback_async(
        || async { Ok::<i32, String>(42) },
        || async { 100 },
        "primary failed",
    )
    .await;

    assert_eq!(value, 42);
}

#[tokio::test]
async fn test_with_fallback_async_fallback() {
    let value = with_fallback_async(
        || async { Err::<i32, String>("primary error".to_string()) },
        || async { 100 },
        "primary failed",
    )
    .await;

    assert_eq!(value, 100);
}

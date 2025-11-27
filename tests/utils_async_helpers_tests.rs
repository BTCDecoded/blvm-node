//! Async helpers tests
//!
//! Tests for async operation utilities.

use bllvm_node::utils::async_helpers::{
    collect_results, delay_before, ignore_error, with_timeout_opt,
};
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test]
async fn test_delay_before() {
    let start = Instant::now();

    let result = delay_before(Duration::from_millis(10), || async { 42 }).await;

    let elapsed = start.elapsed();

    assert_eq!(result, 42);
    assert!(elapsed >= Duration::from_millis(10));
}

#[tokio::test]
async fn test_ignore_error_success() {
    let result = ignore_error(|| async { Ok::<i32, String>(100) }, "test operation").await;

    assert_eq!(result, Some(100));
}

#[tokio::test]
async fn test_ignore_error_failure() {
    let result = ignore_error(
        || async { Err::<i32, String>("error message".to_string()) },
        "test operation",
    )
    .await;

    assert_eq!(result, None);
}

#[tokio::test]
async fn test_collect_results() {
    let operations = vec![
        || async { Ok::<i32, String>(1) },
        || async { Ok::<i32, String>(2) },
        || async { Err::<i32, String>("error".to_string()) },
        || async { Ok::<i32, String>(4) },
    ];

    let results = collect_results(operations, "test").await;

    assert_eq!(results.len(), 4);
    assert_eq!(results[0], Some(1));
    assert_eq!(results[1], Some(2));
    assert_eq!(results[2], None);
    assert_eq!(results[3], Some(4));
}

#[tokio::test]
async fn test_with_timeout_opt_success() {
    let result = with_timeout_opt(
        || async { 42 },
        Duration::from_millis(100),
        "test operation",
    )
    .await;

    assert_eq!(result, Some(42));
}

#[tokio::test]
async fn test_with_timeout_opt_timeout() {
    let result = with_timeout_opt(
        || async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            42
        },
        Duration::from_millis(10),
        "test operation",
    )
    .await;

    assert_eq!(result, None);
}

#[tokio::test]
async fn test_collect_results_empty() {
    let operations: Vec<
        fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<i32, String>> + Send>>,
    > = vec![];

    let results = collect_results(operations, "test").await;

    assert_eq!(results.len(), 0);
}

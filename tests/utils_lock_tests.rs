//! Lock utility tests
//!
//! Tests for async lock helpers.

use bllvm_node::utils::lock::{try_with_lock_timeout, with_lock, with_read_lock, with_write_lock};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

#[tokio::test]
async fn test_with_lock() {
    let mutex = Mutex::new(42);

    let result = with_lock(&mutex, |value| {
        *value = 100;
        *value
    })
    .await;

    assert_eq!(result, 100);
    assert_eq!(*mutex.lock().await, 100);
}

#[tokio::test]
async fn test_with_read_lock() {
    let rwlock = RwLock::new(42);

    let result = with_read_lock(&rwlock, |value| *value).await;

    assert_eq!(result, 42);
    assert_eq!(*rwlock.read().await, 42);
}

#[tokio::test]
async fn test_with_write_lock() {
    let rwlock = RwLock::new(42);

    with_write_lock(&rwlock, |value| {
        *value = 100;
    })
    .await;

    assert_eq!(*rwlock.read().await, 100);
}

#[tokio::test]
async fn test_try_with_lock_timeout_success() {
    let mutex = Mutex::new(42);

    let result = try_with_lock_timeout(
        &mutex,
        Duration::from_millis(100),
        |value| {
            *value = 100;
            *value
        },
        "test operation",
    )
    .await;

    assert_eq!(result, Some(100));
}

#[tokio::test]
async fn test_try_with_lock_timeout_timeout() {
    let mutex = Arc::new(Mutex::new(42));

    // Hold the lock in another task
    let mutex_clone = mutex.clone();
    tokio::spawn(async move {
        let _guard = mutex_clone.lock().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    // Wait a bit for the lock to be held
    tokio::time::sleep(Duration::from_millis(10)).await;

    let result = try_with_lock_timeout(
        &mutex,
        Duration::from_millis(10),
        |value| {
            *value = 100;
            *value
        },
        "test operation",
    )
    .await;

    assert_eq!(result, None);
}

#[tokio::test]
async fn test_with_lock_concurrent() {
    let mutex = Arc::new(Mutex::new(0));

    let mut handles = vec![];
    for _ in 0..10 {
        let mutex_clone = mutex.clone();
        handles.push(tokio::spawn(async move {
            with_lock(&mutex_clone, |value| {
                *value += 1;
            })
            .await;
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    assert_eq!(*mutex.lock().await, 10);
}

#[tokio::test]
async fn test_with_read_lock_multiple_readers() {
    let rwlock = Arc::new(RwLock::new(42));

    let mut handles = vec![];
    for _ in 0..5 {
        let rwlock_clone = rwlock.clone();
        handles.push(tokio::spawn(async move {
            with_read_lock(&rwlock_clone, |value| *value).await
        }));
    }

    let results: Vec<i32> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    for result in results {
        assert_eq!(result, 42);
    }
}

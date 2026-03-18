//! Thread-local timed caches for RPC handlers.
//!
//! Reduces repeated computation (e.g. tip height, difficulty) with a simple
//! TTL. Use when the same value is read many times per request or across
//! nearby requests on the same thread.

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// Thread-local cache that holds a value and refresh timestamp (TTL-only invalidation).
/// Call `get_or_refresh(max_age, compute)` to use the cached value or recompute and store it.
#[derive(Default)]
pub struct ThreadLocalTimedCache<T> {
    inner: RefCell<Option<(T, Instant)>>,
}

/// Keyed variant: stores value + key so cache is invalid when key changes.
#[derive(Default)]
pub struct ThreadLocalKeyedCache<T, K> {
    inner: RefCell<Option<(T, Instant, K)>>,
}

impl<T> ThreadLocalTimedCache<T> {
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(None),
        }
    }
}

impl<T: Clone> ThreadLocalTimedCache<T> {
    /// Return the cached value, or run `compute`, store it, and return it.
    /// Cache is used only if it is younger than `max_age`.
    pub fn get_or_refresh<F>(&self, max_age: Duration, compute: F) -> T
    where
        F: FnOnce() -> T,
    {
        let mut guard = self.inner.borrow_mut();
        let now = Instant::now();
        if let Some((ref v, ref at)) = *guard {
            if at.elapsed() < max_age {
                return v.clone();
            }
        }
        let v = compute();
        *guard = Some((v.clone(), now));
        v
    }
}

impl<T, K> ThreadLocalKeyedCache<T, K> {
    pub fn new() -> Self {
        Self {
            inner: RefCell::new(None),
        }
    }
}

impl<T: Clone, K: Clone + PartialEq> ThreadLocalKeyedCache<T, K> {
    /// Return the cached value, or run `compute`, store it with `key`, and return it.
    /// Cache is used only if it is younger than `max_age` and `key` equals the stored key.
    pub fn get_or_refresh<F>(&self, max_age: Duration, key: &K, compute: F) -> T
    where
        F: FnOnce() -> T,
    {
        let mut guard = self.inner.borrow_mut();
        let now = Instant::now();
        if let Some((ref v, ref at, ref k)) = *guard {
            if k == key && at.elapsed() < max_age {
                return v.clone();
            }
        }
        let v = compute();
        *guard = Some((v.clone(), now, key.clone()));
        v
    }
}

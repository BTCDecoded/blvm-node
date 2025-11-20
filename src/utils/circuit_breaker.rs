//! Circuit breaker pattern for fault tolerance
//!
//! Prevents cascading failures by stopping requests to failing services.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Circuit breaker state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Circuit is closed - normal operation
    Closed,
    /// Circuit is open - failing, reject requests
    Open,
    /// Circuit is half-open - testing if service recovered
    HalfOpen,
}

/// Circuit breaker for fault tolerance
pub struct CircuitBreaker {
    /// Current state
    state: std::sync::atomic::AtomicU8, // 0=Closed, 1=Open, 2=HalfOpen
    /// Failure count
    failure_count: AtomicU32,
    /// Success count (for half-open state)
    success_count: AtomicU32,
    /// Last failure time
    last_failure_time: AtomicU64,
    /// Threshold for opening circuit
    failure_threshold: u32,
    /// Threshold for closing circuit (half-open -> closed)
    success_threshold: u32,
    /// Timeout before attempting half-open
    timeout: Duration,
}

impl CircuitBreaker {
    /// Create a new circuit breaker
    pub fn new(failure_threshold: u32, timeout: Duration) -> Self {
        Self {
            state: std::sync::atomic::AtomicU8::new(0), // Closed
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            last_failure_time: AtomicU64::new(0),
            failure_threshold,
            success_threshold: 1, // Default: 1 success to close
            timeout,
        }
    }

    /// Create with custom success threshold
    pub fn with_success_threshold(
        failure_threshold: u32,
        success_threshold: u32,
        timeout: Duration,
    ) -> Self {
        Self {
            state: std::sync::atomic::AtomicU8::new(0), // Closed
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            last_failure_time: AtomicU64::new(0),
            failure_threshold,
            success_threshold,
            timeout,
        }
    }

    /// Get current state
    pub fn state(&self) -> CircuitState {
        match self.state.load(Ordering::Acquire) {
            0 => CircuitState::Closed,
            1 => CircuitState::Open,
            2 => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }

    /// Check if request should be allowed
    pub fn allow_request(&self) -> bool {
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if timeout has elapsed
                let last_failure = self.last_failure_time.load(Ordering::Acquire);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                if now.saturating_sub(last_failure) >= self.timeout.as_secs() {
                    // Timeout elapsed - transition to half-open
                    self.state.store(2, Ordering::Release); // HalfOpen
                    self.success_count.store(0, Ordering::Release);
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => true, // Allow one request to test
        }
    }

    /// Record success
    pub fn record_success(&self) {
        match self.state() {
            CircuitState::Closed => {
                // Reset failure count on success
                self.failure_count.store(0, Ordering::Release);
            }
            CircuitState::HalfOpen => {
                let successes = self.success_count.fetch_add(1, Ordering::AcqRel) + 1;
                if successes >= self.success_threshold {
                    // Enough successes - close circuit
                    self.state.store(0, Ordering::Release); // Closed
                    self.failure_count.store(0, Ordering::Release);
                    self.success_count.store(0, Ordering::Release);
                }
            }
            CircuitState::Open => {
                // Shouldn't happen, but handle gracefully
            }
        }
    }

    /// Record failure
    pub fn record_failure(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| {
                // Fallback to 0 if system time is before epoch (should never happen)
                std::time::Duration::from_secs(0)
            })
            .as_secs();

        match self.state() {
            CircuitState::Closed => {
                let failures = self.failure_count.fetch_add(1, Ordering::AcqRel) + 1;
                self.last_failure_time.store(now, Ordering::Release);

                if failures >= self.failure_threshold {
                    // Too many failures - open circuit
                    self.state.store(1, Ordering::Release); // Open
                }
            }
            CircuitState::HalfOpen => {
                // Failure in half-open - immediately open
                self.state.store(1, Ordering::Release); // Open
                self.last_failure_time.store(now, Ordering::Release);
                self.success_count.store(0, Ordering::Release);
            }
            CircuitState::Open => {
                // Already open - just update time
                self.last_failure_time.store(now, Ordering::Release);
            }
        }
    }

    /// Reset circuit breaker (for testing/recovery)
    pub fn reset(&self) {
        self.state.store(0, Ordering::Release); // Closed
        self.failure_count.store(0, Ordering::Release);
        self.success_count.store(0, Ordering::Release);
        self.last_failure_time.store(0, Ordering::Release);
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(60))
    }
}

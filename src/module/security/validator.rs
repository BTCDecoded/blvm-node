//! Request validation for module API calls
//!
//! Validates that modules cannot request consensus-modifying operations.

use crate::module::ipc::protocol::RequestPayload;
use crate::module::traits::ModuleError;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Result of request validation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Request is valid and allowed
    Allowed,
    /// Request is invalid and denied
    Denied(String),
}

/// Request validator that ensures modules cannot modify consensus
pub struct RequestValidator {
    /// Rate limiters per module (module_id -> RateLimiter)
    rate_limiters: Mutex<HashMap<String, RateLimiter>>,
    /// Maximum requests per second per module
    max_requests_per_second: u64,
    /// Time window for rate limiting (seconds)
    time_window_seconds: u64,
}

/// Rate limiter using sliding window approach
struct RateLimiter {
    /// Timestamps of recent requests (circular buffer)
    request_timestamps: Vec<u64>,
    /// Current index in circular buffer
    current_index: usize,
    /// Buffer size (number of timestamps to track)
    buffer_size: usize,
}

impl RateLimiter {
    /// Create a new rate limiter
    fn new(max_requests: u64, window_seconds: u64) -> Self {
        // Buffer size: track at least 2x the max requests to handle bursts
        let buffer_size = (max_requests * 2).max(100) as usize;
        Self {
            request_timestamps: Vec::with_capacity(buffer_size),
            current_index: 0,
            buffer_size,
        }
    }

    /// Check if a request is allowed (rate limit not exceeded)
    fn check_rate_limit(&mut self, max_requests: u64, window_seconds: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Remove timestamps outside the time window
        let cutoff = now.saturating_sub(window_seconds);
        self.request_timestamps.retain(|&ts| ts > cutoff);

        // Check if we're under the limit
        if self.request_timestamps.len() < max_requests as usize {
            // Add current request timestamp
            if self.request_timestamps.len() < self.buffer_size {
                self.request_timestamps.push(now);
            } else {
                // Circular buffer: overwrite oldest entry
                self.request_timestamps[self.current_index] = now;
                self.current_index = (self.current_index + 1) % self.buffer_size;
            }
            true
        } else {
            false
        }
    }
}

impl RequestValidator {
    /// Create a new request validator with default rate limits
    pub fn new() -> Self {
        Self::with_rate_limit(100, 1) // Default: 100 requests per second
    }

    /// Create a new request validator with custom rate limits
    pub fn with_rate_limit(max_requests_per_second: u64, time_window_seconds: u64) -> Self {
        Self {
            rate_limiters: Mutex::new(HashMap::new()),
            max_requests_per_second,
            time_window_seconds,
        }
    }

    /// Validate a module request to ensure it doesn't modify consensus
    ///
    /// All current RequestPayload variants are read-only, so validation always passes.
    /// When write operations are added, they will be rejected here.
    #[inline]
    pub fn validate_request(
        &self,
        _module_id: &str,
        payload: &RequestPayload,
    ) -> Result<ValidationResult, ModuleError> {
        // Fast path: all current operations are read-only
        // Using match ensures exhaustiveness when new variants are added
        match payload {
            // Handshake is always allowed (first message)
            RequestPayload::Handshake { .. } => Ok(ValidationResult::Allowed),
            // Read-only operations - all allowed
            RequestPayload::GetBlock { .. }
            | RequestPayload::GetBlockHeader { .. }
            | RequestPayload::GetTransaction { .. }
            | RequestPayload::HasTransaction { .. }
            | RequestPayload::GetChainTip
            | RequestPayload::GetBlockHeight
            | RequestPayload::GetUtxo { .. }
            | RequestPayload::SubscribeEvents { .. } => Ok(ValidationResult::Allowed),
        }
    }

    /// Validate that a module cannot modify consensus state
    ///
    /// This is a safeguard - modules should never have write access,
    /// but we validate explicitly to be defensive.
    pub fn validate_no_consensus_modification(
        &self,
        module_id: &str,
        operation: &str,
    ) -> Result<(), ModuleError> {
        // In Phase 2+, we would reject any operations that could modify:
        // - UTXO set
        // - Block validation rules
        // - Chain state
        // - Mempool state (unless explicitly allowed)

        // For now, all operations are read-only, so this always passes
        debug!(
            "Validated no consensus modification for module {} operation: {}",
            module_id, operation
        );
        Ok(())
    }

    /// Validate resource limits (rate limiting, etc.)
    ///
    /// Enforces rate limiting per module using a sliding window approach.
    /// Default limit: 100 requests per second per module.
    pub fn validate_resource_limits(
        &self,
        module_id: &str,
        _operation: &str,
    ) -> Result<(), ModuleError> {
        let mut limiters = self.rate_limiters.lock().unwrap();

        // Get or create rate limiter for this module
        let limiter = limiters
            .entry(module_id.to_string())
            .or_insert_with(|| {
                RateLimiter::new(self.max_requests_per_second, self.time_window_seconds)
            });

        // Check rate limit
        if !limiter.check_rate_limit(self.max_requests_per_second, self.time_window_seconds) {
            warn!(
                "Rate limit exceeded for module {}: {} requests per {} seconds",
                module_id, self.max_requests_per_second, self.time_window_seconds
            );
            return Err(ModuleError::RateLimitExceeded(format!(
                "Module {} exceeded rate limit: {} requests per {} seconds",
                module_id, self.max_requests_per_second, self.time_window_seconds
            )));
        }

        debug!(
            "Rate limit check passed for module {} operation: {}",
            module_id, _operation
        );
        Ok(())
    }
}

impl Default for RequestValidator {
    fn default() -> Self {
        Self::new()
    }
}

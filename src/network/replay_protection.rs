//! Replay protection for custom protocol messages
//!
//! Provides message ID deduplication, timestamp validation, and request ID tracking
//! to prevent replay attacks on custom protocol messages.

use crate::utils::current_timestamp;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{interval, Instant};
use tracing::{debug, warn};

/// Replay protection errors
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("Duplicate message ID: {0}")]
    DuplicateMessageId(String),
    #[error("Timestamp too old: {0} seconds old (max: {1} seconds)")]
    TimestampTooOld(i64, u64),
    #[error("Timestamp too far in future: {0} seconds ahead (max: {1} seconds)")]
    TimestampTooFuture(i64, u64),
    #[error("Duplicate request ID: {0}")]
    DuplicateRequestId(u64),
}

/// Replay protection manager
///
/// Tracks seen message IDs and request IDs to prevent replay attacks.
/// Automatically cleans up old entries to prevent memory exhaustion.
pub struct ReplayProtection {
    /// Tracked message IDs (message_id -> first_seen_timestamp)
    message_ids: Arc<Mutex<HashMap<String, u64>>>,
    /// Tracked request IDs (request_id -> first_seen_timestamp)
    request_ids: Arc<Mutex<HashMap<u64, u64>>>,
    /// Cleanup interval (default: 5 minutes)
    cleanup_interval: Duration,
    /// Message ID expiration (default: 1 hour)
    message_id_expiration: Duration,
    /// Request ID expiration (default: 5 minutes)
    request_id_expiration: Duration,
    /// Future timestamp tolerance (default: 5 minutes)
    future_tolerance: u64,
}

impl ReplayProtection {
    /// Create a new replay protection manager with default settings
    pub fn new() -> Self {
        Self::with_config(
            Duration::from_secs(300),  // cleanup every 5 minutes
            Duration::from_secs(3600), // message IDs expire after 1 hour
            Duration::from_secs(300),  // request IDs expire after 5 minutes
            300,                       // 5 minute future tolerance
        )
    }

    /// Create a new replay protection manager with custom configuration
    pub fn with_config(
        cleanup_interval: Duration,
        message_id_expiration: Duration,
        request_id_expiration: Duration,
        future_tolerance: u64,
    ) -> Self {
        let protection = Self {
            message_ids: Arc::new(Mutex::new(HashMap::new())),
            request_ids: Arc::new(Mutex::new(HashMap::new())),
            cleanup_interval,
            message_id_expiration,
            request_id_expiration,
            future_tolerance,
        };

        // Start cleanup task
        protection.start_cleanup_task();

        protection
    }

    /// Check if a message ID has been seen before
    ///
    /// Returns Ok(()) if the message ID is new, or Err(ReplayError) if it's a duplicate.
    pub async fn check_message_id(&self, message_id: &str, timestamp: i64) -> Result<()> {
        let mut message_ids = self.message_ids.lock().await;

        // Check if we've seen this message ID before
        if let Some(first_seen) = message_ids.get(message_id) {
            return Err(ReplayError::DuplicateMessageId(message_id.to_string()).into());
        }

        // Store the message ID with current timestamp
        let now = current_timestamp();
        message_ids.insert(message_id.to_string(), now);

        Ok(())
    }

    /// Check if a request ID has been seen before
    ///
    /// Returns Ok(()) if the request ID is new, or Err(ReplayError) if it's a duplicate.
    pub async fn check_request_id(&self, request_id: u64) -> Result<()> {
        let mut request_ids = self.request_ids.lock().await;

        // Check if we've seen this request ID before
        if let Some(_first_seen) = request_ids.get(&request_id) {
            return Err(ReplayError::DuplicateRequestId(request_id).into());
        }

        // Store the request ID with current timestamp
        let now = current_timestamp();
        request_ids.insert(request_id, now);

        Ok(())
    }

    /// Validate a timestamp
    ///
    /// Checks that the timestamp is:
    /// - Not too far in the future (within future_tolerance)
    /// - Not too old (within max_age_seconds)
    ///
    /// Returns Ok(()) if valid, or Err(ReplayError) if invalid.
    pub fn validate_timestamp(timestamp: i64, max_age_seconds: u64) -> Result<()> {
        let now = current_timestamp() as i64;
        let future_tolerance = 300; // 5 minutes default

        // Check future tolerance
        if timestamp > (now + future_tolerance as i64) {
            return Err(ReplayError::TimestampTooFuture(timestamp - now, future_tolerance).into());
        }

        // Check max age
        if timestamp < (now - max_age_seconds as i64) {
            return Err(ReplayError::TimestampTooOld(now - timestamp, max_age_seconds).into());
        }

        Ok(())
    }

    /// Validate timestamp with custom future tolerance
    pub fn validate_timestamp_with_tolerance(
        timestamp: i64,
        max_age_seconds: u64,
        future_tolerance: u64,
    ) -> Result<()> {
        let now = current_timestamp() as i64;

        // Check future tolerance
        if timestamp > (now + future_tolerance as i64) {
            return Err(ReplayError::TimestampTooFuture(timestamp - now, future_tolerance).into());
        }

        // Check max age
        if timestamp < (now - max_age_seconds as i64) {
            return Err(ReplayError::TimestampTooOld(now - timestamp, max_age_seconds).into());
        }

        Ok(())
    }

    /// Start the cleanup task
    ///
    /// Removes old message IDs and request IDs periodically to prevent memory exhaustion.
    fn start_cleanup_task(&self) {
        let message_ids = Arc::clone(&self.message_ids);
        let request_ids = Arc::clone(&self.request_ids);
        let cleanup_interval = self.cleanup_interval;
        let message_id_expiration = self.message_id_expiration;
        let request_id_expiration = self.request_id_expiration;

        tokio::spawn(async move {
            let mut interval = interval(cleanup_interval);
            loop {
                interval.tick().await;

                let now = current_timestamp();
                let message_id_cutoff = now.saturating_sub(message_id_expiration.as_secs());
                let request_id_cutoff = now.saturating_sub(request_id_expiration.as_secs());

                // Cleanup message IDs
                let mut msg_ids = message_ids.lock().await;
                let before = msg_ids.len();
                msg_ids.retain(|_, &mut timestamp| timestamp > message_id_cutoff);
                let after = msg_ids.len();
                if before > after {
                    debug!(
                        "Replay protection cleanup: removed {} old message IDs ({} remaining)",
                        before - after,
                        after
                    );
                }
                drop(msg_ids);

                // Cleanup request IDs
                let mut req_ids = request_ids.lock().await;
                let before = req_ids.len();
                req_ids.retain(|_, &mut timestamp| timestamp > request_id_cutoff);
                let after = req_ids.len();
                if before > after {
                    debug!(
                        "Replay protection cleanup: removed {} old request IDs ({} remaining)",
                        before - after,
                        after
                    );
                }
            }
        });
    }

    /// Get statistics about tracked entries
    pub async fn stats(&self) -> (usize, usize) {
        let message_ids = self.message_ids.lock().await;
        let request_ids = self.request_ids.lock().await;
        (message_ids.len(), request_ids.len())
    }
}

impl Default for ReplayProtection {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_message_id_deduplication() {
        let protection = ReplayProtection::new();
        let message_id = "test-message-id-123";

        // First check should succeed
        assert!(protection
            .check_message_id(message_id, current_timestamp() as i64)
            .await
            .is_ok());

        // Second check should fail (duplicate)
        assert!(protection
            .check_message_id(message_id, current_timestamp() as i64)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_request_id_deduplication() {
        let protection = ReplayProtection::new();
        let request_id = 12345u64;

        // First check should succeed
        assert!(protection.check_request_id(request_id).await.is_ok());

        // Second check should fail (duplicate)
        assert!(protection.check_request_id(request_id).await.is_err());
    }

    #[test]
    fn test_timestamp_validation() {
        let now = current_timestamp() as i64;

        // Valid timestamp (current time)
        assert!(ReplayProtection::validate_timestamp(now, 3600).is_ok());

        // Valid timestamp (1 minute ago)
        assert!(ReplayProtection::validate_timestamp(now - 60, 3600).is_ok());

        // Invalid timestamp (too old)
        assert!(ReplayProtection::validate_timestamp(now - 4000, 3600).is_err());

        // Invalid timestamp (too far in future)
        assert!(ReplayProtection::validate_timestamp(now + 400, 3600).is_err());
    }

    #[tokio::test]
    async fn test_cleanup() {
        let protection = ReplayProtection::with_config(
            Duration::from_millis(100), // cleanup every 100ms
            Duration::from_millis(200), // message IDs expire after 200ms
            Duration::from_millis(200), // request IDs expire after 200ms
            300,
        );

        // Add some entries
        protection
            .check_message_id("msg1", current_timestamp() as i64)
            .await
            .unwrap();
        protection.check_request_id(1).await.unwrap();

        // Wait for cleanup
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Check that entries are cleaned up
        let (msg_count, req_count) = protection.stats().await;
        assert_eq!(msg_count, 0);
        assert_eq!(req_count, 0);
    }
}

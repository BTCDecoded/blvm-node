//! Time utility tests
//!
//! Tests for time-related utilities.

use bllvm_node::utils::time::{current_timestamp, current_timestamp_duration};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn test_current_timestamp() {
    let timestamp1 = current_timestamp();
    let timestamp2 = current_timestamp();

    // Timestamps should be non-zero
    assert!(timestamp1 > 0);
    assert!(timestamp2 > 0);

    // Second timestamp should be >= first (allowing for small time differences)
    assert!(timestamp2 >= timestamp1);
}

#[test]
fn test_current_timestamp_duration() {
    let duration1 = current_timestamp_duration();
    let duration2 = current_timestamp_duration();

    // Duration should be non-zero
    assert!(duration1.as_secs() > 0);
    assert!(duration2.as_secs() > 0);

    // Second duration should be >= first
    assert!(duration2 >= duration1);
}

#[test]
fn test_current_timestamp_consistency() {
    let timestamp = current_timestamp();
    let duration = current_timestamp_duration();

    // They should be approximately equal (within 1 second)
    let diff = (timestamp as i64) - (duration.as_secs() as i64);
    assert!(diff.abs() <= 1);
}

#[test]
fn test_current_timestamp_monotonic() {
    let timestamps: Vec<u64> = (0..10).map(|_| current_timestamp()).collect();

    // Timestamps should be non-decreasing (allowing for clock adjustments)
    for i in 1..timestamps.len() {
        // Allow small decreases due to clock adjustments, but not large ones
        if timestamps[i] < timestamps[i - 1] {
            assert!(
                timestamps[i - 1] - timestamps[i] < 10,
                "Large timestamp decrease detected"
            );
        }
    }
}

#[test]
fn test_current_timestamp_reasonable() {
    let timestamp = current_timestamp();

    // Timestamp should be reasonable (after 2000-01-01, before 2100-01-01)
    let min_timestamp = 946_684_800; // 2000-01-01 00:00:00 UTC
    let max_timestamp = 4_102_444_800; // 2100-01-01 00:00:00 UTC

    assert!(
        timestamp >= min_timestamp,
        "Timestamp too old: {}",
        timestamp
    );
    assert!(
        timestamp <= max_timestamp,
        "Timestamp too far in future: {}",
        timestamp
    );
}

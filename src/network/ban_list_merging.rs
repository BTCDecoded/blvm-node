//! Ban list merging utilities
//!
//! Provides functions to merge ban lists from multiple peers,
//! with conflict resolution and validation.

use crate::network::protocol::{BanEntry, BanListMessage, NetworkAddress};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Merge multiple ban lists into a single list
///
/// Conflict resolution:
/// - If same address appears in multiple lists, use longest ban duration
/// - If permanent ban (u64::MAX) exists, keep it
/// - Combine reasons from all sources
pub fn merge_ban_lists(ban_lists: Vec<&BanListMessage>) -> Vec<BanEntry> {
    let mut merged: HashMap<NetworkAddress, BanEntry> = HashMap::new();

    for ban_list in ban_lists {
        if !ban_list.is_full {
            continue; // Skip hash-only responses
        }

        for entry in &ban_list.ban_entries {
            let addr = entry.addr.clone();

            match merged.get_mut(&addr) {
                Some(existing) => {
                    // Conflict resolution: use longest ban
                    if entry.unban_timestamp == u64::MAX {
                        // Permanent ban always wins
                        existing.unban_timestamp = u64::MAX;
                    } else if existing.unban_timestamp != u64::MAX {
                        // Both temporary - use longer one
                        if entry.unban_timestamp > existing.unban_timestamp {
                            existing.unban_timestamp = entry.unban_timestamp;
                        }
                    }

                    // Merge reasons
                    if let Some(ref reason) = entry.reason {
                        existing.reason = existing
                            .reason
                            .as_ref()
                            .map(|r| format!("{}; {}", r, reason))
                            .or_else(|| Some(reason.clone()));
                    }
                }
                None => {
                    merged.insert(addr, entry.clone());
                }
            }
        }
    }

    // Convert to sorted vector
    let mut result: Vec<BanEntry> = merged.into_values().collect();
    result.sort_by(|a, b| {
        // Sort by address for deterministic output
        a.addr
            .ip
            .cmp(&b.addr.ip)
            .then_with(|| a.addr.port.cmp(&b.addr.port))
    });

    result
}

/// Validate a ban list entry
///
/// Returns true if entry is valid and should be applied
pub fn validate_ban_entry(entry: &BanEntry) -> bool {
    // Check if ban is expired
    if entry.unban_timestamp != u64::MAX {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if now >= entry.unban_timestamp {
            return false; // Ban already expired
        }
    }

    // Additional validation could include:
    // - IP address format validation
    // - Reason length limits
    // - etc.

    true
}

/// Calculate hash of ban list (for verification)
pub fn calculate_ban_list_hash(entries: &[BanEntry]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    // Sort entries for deterministic hashing
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        a.addr
            .ip
            .cmp(&b.addr.ip)
            .then_with(|| a.addr.port.cmp(&b.addr.port))
    });

    // Serialize and hash
    let serialized = bincode::serialize(&sorted).unwrap_or_default();
    let hash = Sha256::digest(&serialized);

    let mut result = [0u8; 32];
    result.copy_from_slice(&hash);
    result
}

/// Verify ban list hash matches entries
pub fn verify_ban_list_hash(entries: &[BanEntry], expected_hash: &[u8; 32]) -> bool {
    let calculated = calculate_ban_list_hash(entries);
    calculated == *expected_hash
}

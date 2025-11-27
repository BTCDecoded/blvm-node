//! Tests for ban list merging

use bllvm_node::network::ban_list_merging::{
    calculate_ban_list_hash, merge_ban_lists, validate_ban_entry,
};
use bllvm_node::network::protocol::{BanEntry, BanListMessage, NetworkAddress};
use std::net::Ipv4Addr;

fn create_ban_entry(ip: u32, port: u16, unban_timestamp: u64, reason: Option<String>) -> BanEntry {
    let mut ip_bytes = [0u8; 16];
    // Convert IPv4 to IPv6-mapped format
    ip_bytes[10] = 0xff;
    ip_bytes[11] = 0xff;
    ip_bytes[12..16].copy_from_slice(&ip.to_be_bytes());

    BanEntry {
        addr: NetworkAddress {
            services: 0,
            ip: ip_bytes,
            port,
        },
        unban_timestamp,
        reason,
    }
}

fn create_ban_list_message(entries: Vec<BanEntry>, is_full: bool) -> BanListMessage {
    use sha2::{Digest, Sha256};
    let hash = if is_full && !entries.is_empty() {
        let mut hasher = Sha256::new();
        // Simplified hash calculation
        hasher.update(b"ban_list");
        hasher.finalize().into()
    } else {
        [0u8; 32]
    };

    BanListMessage {
        is_full,
        ban_list_hash: hash,
        ban_entries: entries,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    }
}

#[test]
fn test_merge_ban_lists_empty() {
    let ban_lists = vec![];
    let merged = merge_ban_lists(ban_lists);
    assert!(merged.is_empty());
}

#[test]
fn test_merge_ban_lists_single_list() {
    let entries = vec![
        create_ban_entry(0x7f000001, 8333, 1000, Some("spam".to_string())),
        create_ban_entry(0x7f000002, 8333, 2000, Some("abuse".to_string())),
    ];
    let ban_list = create_ban_list_message(entries.clone(), true);

    let merged = merge_ban_lists(vec![&ban_list]);
    assert_eq!(merged.len(), 2);
}

#[test]
fn test_merge_ban_lists_multiple_lists() {
    let entries1 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("spam".to_string()),
    )];
    let entries2 = vec![create_ban_entry(
        0x7f000002,
        8333,
        2000,
        Some("abuse".to_string()),
    )];

    let ban_list1 = create_ban_list_message(entries1, true);
    let ban_list2 = create_ban_list_message(entries2, true);

    let merged = merge_ban_lists(vec![&ban_list1, &ban_list2]);
    assert_eq!(merged.len(), 2);
}

#[test]
fn test_merge_ban_lists_conflict_longer_duration() {
    let entries1 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("spam".to_string()),
    )];
    let entries2 = vec![create_ban_entry(
        0x7f000001,
        8333,
        2000,
        Some("abuse".to_string()),
    )];

    let ban_list1 = create_ban_list_message(entries1, true);
    let ban_list2 = create_ban_list_message(entries2, true);

    let merged = merge_ban_lists(vec![&ban_list1, &ban_list2]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].unban_timestamp, 2000); // Longer duration wins
}

#[test]
fn test_merge_ban_lists_permanent_ban_wins() {
    let entries1 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("spam".to_string()),
    )];
    let entries2 = vec![create_ban_entry(
        0x7f000001,
        8333,
        u64::MAX,
        Some("permanent".to_string()),
    )];

    let ban_list1 = create_ban_list_message(entries1, true);
    let ban_list2 = create_ban_list_message(entries2, true);

    let merged = merge_ban_lists(vec![&ban_list1, &ban_list2]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].unban_timestamp, u64::MAX); // Permanent ban wins
}

#[test]
fn test_merge_ban_lists_merge_reasons() {
    let entries1 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("spam".to_string()),
    )];
    let entries2 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("abuse".to_string()),
    )];

    let ban_list1 = create_ban_list_message(entries1, true);
    let ban_list2 = create_ban_list_message(entries2, true);

    let merged = merge_ban_lists(vec![&ban_list1, &ban_list2]);
    assert_eq!(merged.len(), 1);
    assert!(merged[0].reason.as_ref().unwrap().contains("spam"));
    assert!(merged[0].reason.as_ref().unwrap().contains("abuse"));
}

#[test]
fn test_merge_ban_lists_skip_hash_only() {
    let entries = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("spam".to_string()),
    )];
    let ban_list_full = create_ban_list_message(entries.clone(), true);
    let ban_list_hash = create_ban_list_message(entries, false); // hash-only

    let merged = merge_ban_lists(vec![&ban_list_full, &ban_list_hash]);
    assert_eq!(merged.len(), 1); // Only full list is used
}

#[test]
fn test_validate_ban_entry_permanent() {
    let entry = create_ban_entry(0x7f000001, 8333, u64::MAX, Some("permanent".to_string()));
    assert!(validate_ban_entry(&entry));
}

#[test]
fn test_validate_ban_entry_future() {
    let future_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600; // 1 hour in future

    let entry = create_ban_entry(
        0x7f000001,
        8333,
        future_timestamp,
        Some("future".to_string()),
    );
    assert!(validate_ban_entry(&entry));
}

#[test]
fn test_validate_ban_entry_expired() {
    let past_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 3600; // 1 hour in past

    let entry = create_ban_entry(
        0x7f000001,
        8333,
        past_timestamp,
        Some("expired".to_string()),
    );
    assert!(!validate_ban_entry(&entry)); // Expired bans are invalid
}

#[test]
fn test_calculate_ban_list_hash() {
    let entries = vec![
        create_ban_entry(0x7f000001, 8333, 1000, Some("spam".to_string())),
        create_ban_entry(0x7f000002, 8333, 2000, Some("abuse".to_string())),
    ];

    let hash1 = calculate_ban_list_hash(&entries);
    let hash2 = calculate_ban_list_hash(&entries);

    // Same entries should produce same hash
    assert_eq!(hash1, hash2);
}

#[test]
fn test_calculate_ban_list_hash_different_entries() {
    let entries1 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("spam".to_string()),
    )];
    let entries2 = vec![create_ban_entry(
        0x7f000002,
        8333,
        2000,
        Some("abuse".to_string()),
    )];

    let hash1 = calculate_ban_list_hash(&entries1);
    let hash2 = calculate_ban_list_hash(&entries2);

    // Different entries should produce different hashes
    assert_ne!(hash1, hash2);
}

#[test]
fn test_merge_ban_lists_sorted_output() {
    let entries1 = vec![create_ban_entry(
        0x7f000002,
        8333,
        2000,
        Some("second".to_string()),
    )];
    let entries2 = vec![create_ban_entry(
        0x7f000001,
        8333,
        1000,
        Some("first".to_string()),
    )];

    let ban_list1 = create_ban_list_message(entries1, true);
    let ban_list2 = create_ban_list_message(entries2, true);

    let merged = merge_ban_lists(vec![&ban_list1, &ban_list2]);
    assert_eq!(merged.len(), 2);
    // Should be sorted by IP address (compare as arrays)
    let ip1 = &merged[0].addr.ip;
    let ip2 = &merged[1].addr.ip;
    assert!(ip1 < ip2 || (ip1 == ip2 && merged[0].addr.port <= merged[1].addr.port));
}

//! Tests for ban list sharing and merging

use bllvm_node::network::ban_list_merging::*;
use bllvm_node::network::protocol::{BanEntry, BanListMessage, NetworkAddress};
use std::time::{SystemTime, UNIX_EPOCH};

fn create_test_ban_entry(ip: [u8; 16], port: u16, unban_timestamp: u64) -> BanEntry {
    BanEntry {
        addr: NetworkAddress {
            services: 0,
            ip,
            port,
        },
        unban_timestamp,
        reason: Some("Test ban".to_string()),
    }
}

#[test]
fn test_merge_ban_lists() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    
    // Create two ban lists with overlapping entries
    let list1 = BanListMessage {
        is_full: true,
        ban_list_hash: [0; 32],
        ban_entries: vec![
            create_test_ban_entry([127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 3600),
            create_test_ban_entry([192, 168, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 7200),
        ],
        timestamp: now,
    };
    
    let list2 = BanListMessage {
        is_full: true,
        ban_list_hash: [0; 32],
        ban_entries: vec![
            // Same address, longer ban
            create_test_ban_entry([127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 7200),
            // New address
            create_test_ban_entry([10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 1800),
        ],
        timestamp: now,
    };
    
    let merged = merge_ban_lists(vec![&list1, &list2]);
    
    // Should have 3 unique addresses
    assert_eq!(merged.len(), 3);
    
    // 127.0.0.1 should have longer ban (7200)
    let entry_127 = merged.iter()
        .find(|e| e.addr.ip[0] == 127)
        .unwrap();
    assert_eq!(entry_127.unban_timestamp, now + 7200);
}

#[test]
fn test_validate_ban_entry() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    
    // Valid future ban
    let future_ban = create_test_ban_entry([127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 3600);
    assert!(validate_ban_entry(&future_ban));
    
    // Valid permanent ban
    let permanent_ban = create_test_ban_entry([192, 168, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, u64::MAX);
    assert!(validate_ban_entry(&permanent_ban));
    
    // Expired ban (should be invalid)
    let expired_ban = create_test_ban_entry([10, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now.saturating_sub(3600));
    assert!(!validate_ban_entry(&expired_ban));
}

#[test]
fn test_calculate_ban_list_hash() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    
    let entries = vec![
        create_test_ban_entry([127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 3600),
        create_test_ban_entry([192, 168, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8333, now + 7200),
    ];
    
    let hash1 = calculate_ban_list_hash(&entries);
    let hash2 = calculate_ban_list_hash(&entries);
    
    // Same entries should produce same hash
    assert_eq!(hash1, hash2);
    
    // Verify hash
    assert!(verify_ban_list_hash(&entries, &hash1));
}


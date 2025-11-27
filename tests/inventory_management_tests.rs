//! Tests for inventory management

use bllvm_node::network::inventory::{InventoryManager, MSG_BLOCK, MSG_TX};
use bllvm_node::network::protocol::InventoryItem;
use bllvm_node::Hash;

fn create_test_hash(byte: u8) -> Hash {
    let mut hash = [0u8; 32];
    hash[0] = byte;
    hash
}

#[test]
fn test_inventory_manager_creation() {
    let manager = InventoryManager::new();
    assert_eq!(manager.inventory_count(), 0);
    assert_eq!(manager.pending_request_count(), 0);
}

#[test]
fn test_inventory_manager_default() {
    let manager = InventoryManager::default();
    assert_eq!(manager.inventory_count(), 0);
}

#[test]
fn test_add_inventory() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);
    let inventory = vec![InventoryItem {
        inv_type: MSG_BLOCK,
        hash,
    }];

    let result = manager.add_inventory("peer1", &inventory);
    assert!(result.is_ok());
    assert_eq!(manager.inventory_count(), 1);
    assert!(manager.has_inventory(&hash));
}

#[test]
fn test_add_multiple_inventory_items() {
    let mut manager = InventoryManager::new();
    let hash1 = create_test_hash(1);
    let hash2 = create_test_hash(2);
    let inventory = vec![
        InventoryItem {
            inv_type: MSG_BLOCK,
            hash: hash1,
        },
        InventoryItem {
            inv_type: MSG_TX,
            hash: hash2,
        },
    ];

    manager.add_inventory("peer1", &inventory).unwrap();
    assert_eq!(manager.inventory_count(), 2);
    assert!(manager.has_inventory(&hash1));
    assert!(manager.has_inventory(&hash2));
}

#[test]
fn test_add_inventory_from_multiple_peers() {
    let mut manager = InventoryManager::new();
    let hash1 = create_test_hash(1);
    let hash2 = create_test_hash(2);

    manager
        .add_inventory(
            "peer1",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash: hash1,
            }],
        )
        .unwrap();

    manager
        .add_inventory(
            "peer2",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash: hash2,
            }],
        )
        .unwrap();

    assert_eq!(manager.inventory_count(), 2);
    assert!(manager.has_inventory(&hash1));
    assert!(manager.has_inventory(&hash2));
}

#[test]
fn test_has_inventory() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);

    assert!(!manager.has_inventory(&hash));

    manager
        .add_inventory(
            "peer1",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash,
            }],
        )
        .unwrap();

    assert!(manager.has_inventory(&hash));
}

#[test]
fn test_request_data() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);

    let get_data = manager.request_data(hash, MSG_BLOCK, "peer1").unwrap();

    assert_eq!(get_data.inventory.len(), 1);
    assert_eq!(get_data.inventory[0].hash, hash);
    assert_eq!(get_data.inventory[0].inv_type, MSG_BLOCK);
    assert_eq!(manager.pending_request_count(), 1);
}

#[test]
fn test_mark_fulfilled() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);

    manager.request_data(hash, MSG_BLOCK, "peer1").unwrap();
    assert_eq!(manager.pending_request_count(), 1);

    manager.mark_fulfilled(&hash);
    assert_eq!(manager.pending_request_count(), 0);
}

#[test]
fn test_get_pending_requests() {
    let mut manager = InventoryManager::new();
    let hash1 = create_test_hash(1);
    let hash2 = create_test_hash(2);

    manager.request_data(hash1, MSG_BLOCK, "peer1").unwrap();
    manager.request_data(hash2, MSG_TX, "peer2").unwrap();

    let pending = manager.get_pending_requests();
    assert_eq!(pending.len(), 2);
}

#[test]
fn test_cleanup_old_requests() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);

    manager.request_data(hash, MSG_BLOCK, "peer1").unwrap();
    assert_eq!(manager.pending_request_count(), 1);

    // Cleanup requests older than 0 seconds (should remove all)
    manager.cleanup_old_requests(0);
    assert_eq!(manager.pending_request_count(), 0);
}

#[test]
fn test_get_peer_inventory() {
    let mut manager = InventoryManager::new();
    let hash1 = create_test_hash(1);
    let hash2 = create_test_hash(2);

    manager
        .add_inventory(
            "peer1",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash: hash1,
            }],
        )
        .unwrap();

    manager
        .add_inventory(
            "peer2",
            &[InventoryItem {
                inv_type: MSG_TX,
                hash: hash2,
            }],
        )
        .unwrap();

    let peer1_inv = manager.get_peer_inventory("peer1").unwrap();
    assert_eq!(peer1_inv.len(), 1);
    assert!(peer1_inv.contains(&hash1));

    let peer2_inv = manager.get_peer_inventory("peer2").unwrap();
    assert_eq!(peer2_inv.len(), 1);
    assert!(peer2_inv.contains(&hash2));

    assert!(manager.get_peer_inventory("peer3").is_none());
}

#[test]
fn test_remove_peer() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);

    manager
        .add_inventory(
            "peer1",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash,
            }],
        )
        .unwrap();

    assert!(manager.get_peer_inventory("peer1").is_some());
    manager.remove_peer("peer1");
    assert!(manager.get_peer_inventory("peer1").is_none());
}

#[test]
fn test_inventory_count() {
    let mut manager = InventoryManager::new();

    assert_eq!(manager.inventory_count(), 0);

    for i in 0..5 {
        let hash = create_test_hash(i);
        manager
            .add_inventory(
                "peer1",
                &[InventoryItem {
                    inv_type: MSG_BLOCK,
                    hash,
                }],
            )
            .unwrap();
    }

    assert_eq!(manager.inventory_count(), 5);
}

#[test]
fn test_duplicate_inventory() {
    let mut manager = InventoryManager::new();
    let hash = create_test_hash(1);

    // Add same inventory twice
    manager
        .add_inventory(
            "peer1",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash,
            }],
        )
        .unwrap();

    manager
        .add_inventory(
            "peer1",
            &[InventoryItem {
                inv_type: MSG_BLOCK,
                hash,
            }],
        )
        .unwrap();

    // Should still only count as one
    assert_eq!(manager.inventory_count(), 1);
}

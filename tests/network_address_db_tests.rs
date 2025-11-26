//! Tests for Address Database

use bllvm_node::network::address_db::{AddressDatabase, AddressEntry};
use bllvm_node::network::protocol::NetworkAddress;
use std::collections::HashMap;
use std::net::SocketAddr;

fn create_test_network_address(ip: [u8; 16], port: u16) -> NetworkAddress {
    NetworkAddress {
        services: 1,
        ip,
        port,
    }
}

fn create_ipv4_mapped_address(ipv4: [u8; 4], port: u16) -> NetworkAddress {
    let mut ip = [0u8; 16];
    ip[10] = 0xff;
    ip[11] = 0xff;
    ip[12..16].copy_from_slice(&ipv4);
    NetworkAddress {
        services: 1,
        ip,
        port,
    }
}

#[test]
fn test_address_entry_new() {
    let addr = create_test_network_address([0u8; 16], 8333);
    let entry = AddressEntry::new(addr.clone(), 1);
    
    assert_eq!(entry.addr.port, addr.port);
    assert_eq!(entry.services, 1);
    assert_eq!(entry.seen_count, 1);
    assert!(entry.first_seen > 0);
    assert_eq!(entry.first_seen, entry.last_seen);
}

#[test]
fn test_address_entry_update_seen() {
    let addr = create_test_network_address([0u8; 16], 8333);
    let mut entry = AddressEntry::new(addr, 1);
    let first_seen = entry.first_seen;
    
    // Wait a bit (or simulate time passing)
    std::thread::sleep(std::time::Duration::from_millis(10));
    entry.update_seen();
    
    assert_eq!(entry.first_seen, first_seen); // First seen doesn't change
    assert!(entry.last_seen >= first_seen); // Last seen updated
    assert_eq!(entry.seen_count, 2);
}

#[test]
fn test_address_entry_is_fresh() {
    let addr = create_test_network_address([0u8; 16], 8333);
    let entry = AddressEntry::new(addr, 1);
    
    // Should be fresh with 24 hour expiration
    assert!(entry.is_fresh(24 * 60 * 60));
    
    // Should not be fresh with 0 expiration
    assert!(!entry.is_fresh(0));
}

#[test]
fn test_address_database_new() {
    let db = AddressDatabase::new(100);
    assert!(db.is_empty());
    assert_eq!(db.len(), 0);
}

#[test]
fn test_address_database_with_expiration() {
    let db = AddressDatabase::with_expiration(50, 3600);
    assert!(db.is_empty());
    assert_eq!(db.len(), 0);
}

#[test]
fn test_address_database_add_address() {
    let mut db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    
    db.add_address(addr.clone(), 1);
    assert_eq!(db.len(), 1);
    assert!(!db.is_empty());
}

#[test]
fn test_address_database_add_duplicate_address() {
    let mut db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    
    db.add_address(addr.clone(), 1);
    db.add_address(addr.clone(), 2);
    
    // Should still be one entry, but services merged
    assert_eq!(db.len(), 1);
}

#[test]
fn test_address_database_add_addresses() {
    let mut db = AddressDatabase::new(100);
    let addrs = vec![
        create_test_network_address([1u8; 16], 8333),
        create_test_network_address([2u8; 16], 8334),
        create_test_network_address([3u8; 16], 8335),
    ];
    
    db.add_addresses(addrs, 1);
    assert_eq!(db.len(), 3);
}

#[test]
fn test_address_database_get_fresh_addresses() {
    let mut db = AddressDatabase::new(100);
    let addrs = vec![
        create_test_network_address([1u8; 16], 8333),
        create_test_network_address([2u8; 16], 8334),
        create_test_network_address([3u8; 16], 8335),
    ];
    
    db.add_addresses(addrs.clone(), 1);
    
    let fresh = db.get_fresh_addresses(2);
    assert_eq!(fresh.len(), 2);
    
    let all_fresh = db.get_all_fresh_addresses();
    assert_eq!(all_fresh.len(), 3);
}

#[test]
fn test_address_database_remove_expired() {
    let mut db = AddressDatabase::with_expiration(100, 1); // 1 second expiration
    let addr = create_test_network_address([0u8; 16], 8333);
    
    db.add_address(addr, 1);
    assert_eq!(db.len(), 1);
    
    // Wait for expiration
    std::thread::sleep(std::time::Duration::from_secs(2));
    
    let removed = db.remove_expired();
    assert_eq!(removed, 1);
    assert_eq!(db.len(), 0);
}

#[test]
fn test_address_database_remove_address() {
    let mut db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    
    db.add_address(addr.clone(), 1);
    assert_eq!(db.len(), 1);
    
    db.remove_address(&addr);
    assert_eq!(db.len(), 0);
    assert!(db.is_empty());
}

#[test]
fn test_address_database_is_banned() {
    let db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    let socket_addr = db.network_addr_to_socket(&addr);
    
    let mut ban_list = HashMap::new();
    ban_list.insert(socket_addr, u64::MAX); // Permanent ban
    
    assert!(db.is_banned(&addr, &ban_list));
}

#[test]
fn test_address_database_is_banned_expired() {
    let db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    let socket_addr = db.network_addr_to_socket(&addr);
    
    let mut ban_list = HashMap::new();
    ban_list.insert(socket_addr, 0); // Expired ban
    
    assert!(!db.is_banned(&addr, &ban_list));
}

#[test]
fn test_address_database_is_local_ipv4() {
    let db = AddressDatabase::new(100);
    
    // Test localhost
    let localhost = create_ipv4_mapped_address([127, 0, 0, 1], 8333);
    assert!(db.is_local(&localhost));
    
    // Test private IP
    let private = create_ipv4_mapped_address([192, 168, 1, 1], 8333);
    assert!(db.is_local(&private));
    
    // Test public IP
    let public = create_ipv4_mapped_address([8, 8, 8, 8], 8333);
    assert!(!db.is_local(&public));
}

#[test]
fn test_address_database_filter_addresses() {
    let db = AddressDatabase::new(100);
    let localhost = create_ipv4_mapped_address([127, 0, 0, 1], 8333);
    let public = create_ipv4_mapped_address([8, 8, 8, 8], 8333);
    
    let addrs = vec![localhost.clone(), public.clone()];
    let ban_list = HashMap::new();
    let connected = vec![];
    
    let filtered = db.filter_addresses(addrs, &ban_list, &connected);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].port, public.port);
}

#[test]
fn test_address_database_filter_banned() {
    let db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    let socket_addr = db.network_addr_to_socket(&addr);
    
    let mut ban_list = HashMap::new();
    ban_list.insert(socket_addr, u64::MAX);
    
    let addrs = vec![addr.clone()];
    let connected = vec![];
    
    let filtered = db.filter_addresses(addrs, &ban_list, &connected);
    assert!(filtered.is_empty());
}

#[test]
fn test_address_database_filter_connected() {
    let db = AddressDatabase::new(100);
    let addr = create_test_network_address([0u8; 16], 8333);
    let socket_addr = db.network_addr_to_socket(&addr);
    
    let addrs = vec![addr.clone()];
    let ban_list = HashMap::new();
    let connected = vec![socket_addr];
    
    let filtered = db.filter_addresses(addrs, &ban_list, &connected);
    assert!(filtered.is_empty());
}

#[test]
fn test_address_database_max_addresses() {
    let mut db = AddressDatabase::new(3);
    
    // Add more than max
    for i in 0..5 {
        let mut ip = [0u8; 16];
        ip[15] = i;
        let addr = create_test_network_address(ip, 8333);
        db.add_address(addr, 1);
    }
    
    // Should be limited to max_addresses
    assert!(db.len() <= 3);
}

#[test]
fn test_address_database_total_count() {
    let mut db = AddressDatabase::new(100);
    
    let addr1 = create_test_network_address([1u8; 16], 8333);
    let addr2 = create_test_network_address([2u8; 16], 8334);
    
    db.add_address(addr1, 1);
    db.add_address(addr2, 1);
    
    assert_eq!(db.total_count(), 2);
    assert_eq!(db.len(), 2);
}

#[test]
fn test_address_database_network_addr_to_socket_ipv4() {
    let db = AddressDatabase::new(100);
    let addr = create_ipv4_mapped_address([192, 168, 1, 1], 8333);
    
    let socket = db.network_addr_to_socket(&addr);
    assert_eq!(socket.port(), 8333);
    
    if let std::net::IpAddr::V4(ipv4) = socket.ip() {
        assert_eq!(ipv4.octets(), [192, 168, 1, 1]);
    } else {
        panic!("Expected IPv4 address");
    }
}

#[test]
fn test_address_database_network_addr_to_socket_ipv6() {
    let db = AddressDatabase::new(100);
    let mut ip = [0u8; 16];
    ip[0] = 0x20;
    ip[1] = 0x01;
    ip[2] = 0x0d;
    ip[3] = 0xb8;
    let addr = create_test_network_address(ip, 8333);
    
    let socket = db.network_addr_to_socket(&addr);
    assert_eq!(socket.port(), 8333);
}


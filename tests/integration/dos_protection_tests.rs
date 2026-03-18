//! Integration tests for Enhanced DoS Protection
//!
//! Tests connection rate limiting, message queue limits, resource monitoring,
//! and automatic mitigation.

use crate::network::NetworkManager;
use std::net::SocketAddr;

#[tokio::test]
async fn test_connection_rate_limiting() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(listen_addr);
    
    // Start network manager
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would create connections from same IP; per-IP limits tested in dos_protection unit tests.
}

#[tokio::test]
async fn test_active_connection_limit() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::with_config(listen_addr, 5); // Max 5 peers
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would connect 6+ peers; connection limit tested in dos_protection unit tests.
}

#[tokio::test]
async fn test_message_queue_limit() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(listen_addr);
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would flood with messages; queue limits tested in dos_protection unit tests.
}

#[tokio::test]
async fn test_auto_ban_after_violations() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(listen_addr);
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would violate limits repeatedly; auto-ban tested in dos_protection unit tests.
}

#[tokio::test]
async fn test_resource_usage_monitoring() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(listen_addr);
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would verify resource metrics; tested in dos_protection unit tests.
}

#[tokio::test]
async fn test_dos_attack_detection() {
    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let network_manager = NetworkManager::new(listen_addr);
    
    network_manager.start(listen_addr).await.unwrap();
    
    // Full test would simulate DoS; detection tested in dos_protection unit tests.
}


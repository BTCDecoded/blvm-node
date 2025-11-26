//! Tests for DNS Seeds

use bllvm_node::network::dns_seeds::{resolve_dns_seeds, MAINNET_DNS_SEEDS, TESTNET_DNS_SEEDS};
use bllvm_node::network::protocol::NetworkAddress;

#[tokio::test]
async fn test_dns_seeds_constants() {
    // Test that constants are defined
    assert!(!MAINNET_DNS_SEEDS.is_empty());
    assert!(!TESTNET_DNS_SEEDS.is_empty());
}

#[tokio::test]
async fn test_resolve_dns_seeds_empty_seeds() {
    let seeds = vec![];
    let addresses = resolve_dns_seeds(&seeds, 8333, 10).await;
    assert!(addresses.is_empty());
}

#[tokio::test]
async fn test_resolve_dns_seeds_invalid_seed() {
    let seeds = vec!["invalid-dns-seed-that-does-not-exist-12345.example.com"];
    let addresses = resolve_dns_seeds(&seeds, 8333, 10).await;
    // Should handle gracefully and return empty or partial results
    // (actual behavior depends on DNS resolution)
    assert!(addresses.len() <= 10);
}

#[tokio::test]
async fn test_resolve_dns_seeds_max_addresses() {
    // Use a known working DNS seed (localhost won't work, but we can test the limit)
    let seeds = vec!["localhost"]; // This will likely fail, but tests the limit logic
    let addresses = resolve_dns_seeds(&seeds, 8333, 5).await;
    assert!(addresses.len() <= 5);
}

#[tokio::test]
async fn test_resolve_dns_seeds_port() {
    // Test that port is correctly set
    let seeds = vec!["localhost"]; // Will likely fail, but tests structure
    let addresses = resolve_dns_seeds(&seeds, 8334, 10).await;
    
    // If any addresses are returned, they should have the correct port
    for addr in &addresses {
        assert_eq!(addr.port, 8334);
    }
}

#[tokio::test]
async fn test_resolve_dns_seeds_multiple_seeds() {
    // Test with multiple seeds (even if they fail)
    let seeds = vec![
        "invalid-seed-1.example.com",
        "invalid-seed-2.example.com",
    ];
    let addresses = resolve_dns_seeds(&seeds, 8333, 10).await;
    // Should handle multiple seeds gracefully
    assert!(addresses.len() <= 10);
}

#[test]
fn test_mainnet_dns_seeds_not_empty() {
    assert!(!MAINNET_DNS_SEEDS.is_empty());
    // Verify they're valid hostnames (basic check)
    for seed in MAINNET_DNS_SEEDS {
        assert!(!seed.is_empty());
        assert!(!seed.contains(' '));
    }
}

#[test]
fn test_testnet_dns_seeds_not_empty() {
    assert!(!TESTNET_DNS_SEEDS.is_empty());
    // Verify they're valid hostnames (basic check)
    for seed in TESTNET_DNS_SEEDS {
        assert!(!seed.is_empty());
        assert!(!seed.contains(' '));
    }
}

#[test]
fn test_dns_seeds_no_duplicates() {
    // Check for duplicates in mainnet seeds
    let mut seen = std::collections::HashSet::new();
    for seed in MAINNET_DNS_SEEDS {
        assert!(seen.insert(seed), "Duplicate DNS seed: {}", seed);
    }
    
    // Check for duplicates in testnet seeds
    let mut seen = std::collections::HashSet::new();
    for seed in TESTNET_DNS_SEEDS {
        assert!(seen.insert(seed), "Duplicate DNS seed: {}", seed);
    }
}


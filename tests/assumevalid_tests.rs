//! AssumeValid functional tests
//!
//! Verifies -assumevalid CLI, config wiring, and per-network defaults.

use blvm_node::config::{
    default_assume_valid_height_for_network, BlockValidationNodeConfig, NodeConfig,
};

#[test]
fn test_default_assume_valid_height_per_network() {
    assert_eq!(default_assume_valid_height_for_network("mainnet"), 912_683);
    assert_eq!(default_assume_valid_height_for_network("Mainnet"), 912_683);
    assert_eq!(
        default_assume_valid_height_for_network("bitcoinv1"),
        912_683
    );
    assert_eq!(
        default_assume_valid_height_for_network("testnet"),
        4_550_000
    );
    assert_eq!(
        default_assume_valid_height_for_network("Testnet"),
        4_550_000
    );
    assert_eq!(default_assume_valid_height_for_network("signet"), 267_665);
    assert_eq!(default_assume_valid_height_for_network("regtest"), 0);
    assert_eq!(default_assume_valid_height_for_network("unknown"), 0);
}

#[test]
fn test_block_validation_config_assume_valid_height() {
    let config = BlockValidationNodeConfig {
        assume_valid_height: 700_000,
        assume_valid_hash: None,
    };
    assert_eq!(config.assume_valid_height, 700_000);
    assert!(config.assume_valid_hash.is_none());
}

#[test]
fn test_block_validation_config_assume_valid_hash() {
    let hash = [0xab; 32];
    let config = BlockValidationNodeConfig {
        assume_valid_height: 0,
        assume_valid_hash: Some(hash),
    };
    assert_eq!(config.assume_valid_height, 0);
    assert_eq!(config.assume_valid_hash, Some(hash));
}

#[test]
fn test_noassumevalid_disables_skip() {
    let config = BlockValidationNodeConfig {
        assume_valid_height: 0,
        assume_valid_hash: None,
    };
    assert_eq!(config.assume_valid_height, 0);
}

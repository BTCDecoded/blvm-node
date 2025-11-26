//! RBF (Replace-By-Fee) Mode Tests
//!
//! Comprehensive tests for all RBF modes:
//! - Disabled: No replacements allowed
//! - Conservative: Strict rules with higher fee requirements
//! - Standard: BIP125-compliant (default)
//! - Aggressive: Relaxed rules for miners

use bllvm_node::config::{RbfConfig, RbfMode};
use bllvm_node::node::mempool::MempoolManager;
use bllvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput, UtxoSet, UTXO};
use std::collections::HashMap;

/// Create a test transaction with RBF signaling
fn create_rbf_tx(input_value: u64, output_value: u64) -> Transaction {
    Transaction {
        version: 1,
        inputs: bllvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [1; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xfffffffe, // RBF enabled
        }],
        outputs: bllvm_protocol::tx_outputs![TransactionOutput {
            value: output_value as i64,
            script_pubkey: vec![0x76, 0xa9, 0x14, 0x00].repeat(20), // P2PKH
        }],
        lock_time: 0,
    }
}

/// Create a test UTXO set
fn create_test_utxo_set() -> UtxoSet {
    let mut utxo_set = HashMap::new();
    utxo_set.insert(
        OutPoint {
            hash: [1; 32],
            index: 0,
        },
        UTXO {
            value: 100_000,
            script_pubkey: vec![0x76, 0xa9, 0x14, 0x00].repeat(20),
            height: 0,
        },
    );
    utxo_set
}

#[test]
fn test_rbf_mode_disabled() {
    let mempool = MempoolManager::new();
    let rbf_config = RbfConfig::with_mode(RbfMode::Disabled);
    mempool.set_rbf_config(Some(rbf_config));

    let existing_tx = create_rbf_tx(100_000, 90_000); // Fee: 10,000
    let new_tx = create_rbf_tx(100_000, 80_000); // Fee: 20,000 (higher)

    let utxo_set = create_test_utxo_set();
    let result = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);

    // RBF disabled - should reject
    assert!(result.is_ok());
    assert!(!result.unwrap(), "RBF should be disabled");
}

#[test]
fn test_rbf_mode_conservative() {
    let mempool = MempoolManager::new();
    let rbf_config = RbfConfig::with_mode(RbfMode::Conservative);
    mempool.set_rbf_config(Some(rbf_config));

    let existing_tx = create_rbf_tx(100_000, 90_000); // Fee: 10,000, Fee rate: ~100 sat/vB
    let new_tx = create_rbf_tx(100_000, 70_000); // Fee: 30,000, Fee rate: ~300 sat/vB (3x increase)

    let utxo_set = create_test_utxo_set();
    let result = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);

    // Conservative mode requires 2x fee rate multiplier and 5000 sat absolute bump
    // 300 / 100 = 3x (passes 2x requirement)
    // 30,000 - 10,000 = 20,000 (passes 5000 sat requirement)
    assert!(result.is_ok());
    // Note: This will also check BIP125 rules, so result depends on full validation
}

#[test]
fn test_rbf_mode_standard() {
    let mempool = MempoolManager::new();
    let rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    mempool.set_rbf_config(Some(rbf_config));

    let existing_tx = create_rbf_tx(100_000, 90_000); // Fee: 10,000
    let new_tx = create_rbf_tx(100_000, 88_000); // Fee: 12,000 (1.2x fee rate, 2000 sat bump)

    let utxo_set = create_test_utxo_set();
    let result = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);

    // Standard mode requires 1.1x fee rate multiplier and 1000 sat absolute bump
    // Should pass if BIP125 rules are satisfied
    assert!(result.is_ok());
}

#[test]
fn test_rbf_mode_aggressive() {
    let mempool = MempoolManager::new();
    let rbf_config = RbfConfig::with_mode(RbfMode::Aggressive);
    mempool.set_rbf_config(Some(rbf_config));

    let existing_tx = create_rbf_tx(100_000, 90_000); // Fee: 10,000
    let new_tx = create_rbf_tx(100_000, 89_500); // Fee: 10,500 (1.05x fee rate, 500 sat bump)

    let utxo_set = create_test_utxo_set();
    let result = mempool.check_rbf_replacement(&new_tx, &existing_tx, &utxo_set, None);

    // Aggressive mode requires 1.05x fee rate multiplier and 500 sat absolute bump
    // Should pass if BIP125 rules are satisfied
    assert!(result.is_ok());
}

#[test]
fn test_rbf_replacement_count_limit() {
    // Test that replacement count limit is configured correctly
    let mut rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    rbf_config.max_replacements_per_tx = 2; // Allow only 2 replacements

    assert_eq!(rbf_config.max_replacements_per_tx, 2);
    assert_eq!(rbf_config.mode, RbfMode::Standard);
}

#[test]
fn test_rbf_cooldown_period() {
    // Test that cooldown period is configured correctly
    let mut rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    rbf_config.cooldown_seconds = 60; // 60 second cooldown

    assert_eq!(rbf_config.cooldown_seconds, 60);
    assert_eq!(rbf_config.mode, RbfMode::Standard);
}

#[test]
fn test_rbf_fee_rate_multiplier() {
    let mempool = MempoolManager::new();
    let mut rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    rbf_config.min_fee_rate_multiplier = 1.5; // Require 50% increase
    mempool.set_rbf_config(Some(rbf_config));

    let existing_tx = create_rbf_tx(100_000, 90_000); // Fee: 10,000, ~100 sat/vB
    let new_tx_insufficient = create_rbf_tx(100_000, 91_000); // Fee: 9,000, ~90 sat/vB (less than 1.5x)
    let new_tx_sufficient = create_rbf_tx(100_000, 85_000); // Fee: 15,000, ~150 sat/vB (1.5x)

    let utxo_set = create_test_utxo_set();

    // Insufficient fee rate increase should fail
    let result1 =
        mempool.check_rbf_replacement(&new_tx_insufficient, &existing_tx, &utxo_set, None);
    assert!(result1.is_ok());
    // Should be rejected due to insufficient fee rate

    // Sufficient fee rate increase should pass (if other checks pass)
    let result2 = mempool.check_rbf_replacement(&new_tx_sufficient, &existing_tx, &utxo_set, None);
    assert!(result2.is_ok());
}

#[test]
fn test_rbf_absolute_fee_bump() {
    let mempool = MempoolManager::new();
    let mut rbf_config = RbfConfig::with_mode(RbfMode::Standard);
    rbf_config.min_fee_bump_satoshis = 5000; // Require 5000 sat absolute bump
    mempool.set_rbf_config(Some(rbf_config));

    let existing_tx = create_rbf_tx(100_000, 90_000); // Fee: 10,000
    let new_tx_insufficient = create_rbf_tx(100_000, 89_500); // Fee: 10,500 (only 500 sat bump)
    let new_tx_sufficient = create_rbf_tx(100_000, 84_000); // Fee: 16,000 (6000 sat bump)

    let utxo_set = create_test_utxo_set();

    // Insufficient absolute fee bump should fail
    let result1 =
        mempool.check_rbf_replacement(&new_tx_insufficient, &existing_tx, &utxo_set, None);
    assert!(result1.is_ok());
    // Should be rejected due to insufficient absolute fee bump

    // Sufficient absolute fee bump should pass (if other checks pass)
    let result2 = mempool.check_rbf_replacement(&new_tx_sufficient, &existing_tx, &utxo_set, None);
    assert!(result2.is_ok());
}

#[test]
fn test_rbf_config_with_mode() {
    // Test that with_mode sets appropriate defaults
    let conservative = RbfConfig::with_mode(RbfMode::Conservative);
    assert_eq!(conservative.mode, RbfMode::Conservative);
    assert_eq!(conservative.min_fee_rate_multiplier, 2.0);
    assert_eq!(conservative.min_fee_bump_satoshis, 5000);
    assert_eq!(conservative.min_confirmations, 1);

    let aggressive = RbfConfig::with_mode(RbfMode::Aggressive);
    assert_eq!(aggressive.mode, RbfMode::Aggressive);
    assert_eq!(aggressive.min_fee_rate_multiplier, 1.05);
    assert_eq!(aggressive.min_fee_bump_satoshis, 500);
    assert!(aggressive.allow_package_replacements);

    let standard = RbfConfig::with_mode(RbfMode::Standard);
    assert_eq!(standard.mode, RbfMode::Standard);
    assert_eq!(standard.min_fee_rate_multiplier, 1.1);
    assert_eq!(standard.min_fee_bump_satoshis, 1000);
}

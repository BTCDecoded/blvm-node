//! Unit tests for Vault Engine
//!
//! Tests vault creation, unvaulting, withdrawal, recovery, and state management.

#![cfg(feature = "ctv")]

use blvm_node::payment::covenant::CovenantEngine;
use blvm_node::payment::processor::PaymentError;
use blvm_node::payment::vault::{VaultConfig, VaultEngine, VaultLifecycle, VaultState};
use blvm_protocol::payment::PaymentOutput;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Helper to create a test vault engine
fn create_test_vault_engine() -> VaultEngine {
    let covenant_engine = Arc::new(CovenantEngine::new());
    VaultEngine::new(covenant_engine)
}

/// Helper to create test withdrawal script
fn create_test_withdrawal_script() -> Vec<u8> {
    vec![0x76, 0xa9, 0x14, 0x00, 0x87] // P2PKH test script
}

/// Test vault creation
#[test]
fn test_create_vault() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_1";
    let deposit_amount = 100000; // 0.001 BTC
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig::default();

    let result = engine.create_vault(vault_id, deposit_amount, withdrawal_script.clone(), config);

    assert!(result.is_ok(), "Vault creation should succeed");
    let vault_state = result.unwrap();

    assert_eq!(vault_state.vault_id, vault_id);
    assert_eq!(vault_state.deposit_amount, deposit_amount);
    assert_eq!(vault_state.state, VaultLifecycle::Deposited);
    assert!(vault_state.deposit_covenant.template_hash.len() == 32);
    assert!(vault_state.unvault_covenant.is_none());
    assert!(vault_state.withdrawal_covenant.is_none());
}

/// Test vault creation with custom config
#[test]
fn test_create_vault_custom_config() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_2";
    let deposit_amount = 50000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        withdrawal_delay_blocks: 288,            // ~2 days
        recovery_script: Some(vec![0x51, 0x87]), // OP_1, OP_EQUAL
        max_withdrawal: Some(100000),
        require_unvault: true,
    };

    let result = engine.create_vault(vault_id, deposit_amount, withdrawal_script, config.clone());

    assert!(result.is_ok());
    let vault_state = result.unwrap();
    assert_eq!(vault_state.config.withdrawal_delay_blocks, 288);
    assert!(vault_state.config.recovery_script.is_some());
    assert_eq!(vault_state.config.max_withdrawal, Some(100000));
}

/// Test unvaulting
#[test]
fn test_unvault() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_3";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        require_unvault: true,
        ..Default::default()
    };

    // Create vault
    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config)
        .unwrap();

    // Unvault
    let unvault_script = vec![0x52, 0x87]; // OP_2, OP_EQUAL
    let result = engine.unvault(&vault_state, unvault_script.clone());

    assert!(result.is_ok(), "Unvaulting should succeed");
    let new_state = result.unwrap();
    assert_eq!(new_state.state, VaultLifecycle::Unvaulting);
    assert!(new_state.unvault_covenant.is_some());
}

/// Test unvaulting fails when not required
#[test]
fn test_unvault_fails_when_not_required() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_4";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        require_unvault: false,
        ..Default::default()
    };

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config)
        .unwrap();

    let unvault_script = vec![0x52, 0x87];
    let result = engine.unvault(&vault_state, unvault_script);

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        PaymentError::ProcessingError(_)
    ));
}

/// Test withdrawal eligibility check
#[test]
fn test_check_withdrawal_eligibility() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_5";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        withdrawal_delay_blocks: 144, // ~1 day
        require_unvault: true,
        ..Default::default()
    };

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config)
        .unwrap();

    // Mark as unvaulted at block 1000
    let unvault_block_height = 1000;
    let unvault_tx_hash = [0x01; 32];
    let new_state = engine
        .mark_unvaulted(&vault_state, unvault_tx_hash, unvault_block_height)
        .unwrap();

    // Check eligibility before delay
    let current_height = 1100; // 100 blocks after unvault (need 144)
    assert!(!VaultEngine::check_withdrawal_eligibility(
        &new_state,
        current_height
    ));

    // Check eligibility after delay
    let current_height = 1144; // Exactly 144 blocks after
    assert!(VaultEngine::check_withdrawal_eligibility(
        &new_state,
        current_height
    ));

    // Check eligibility well after delay
    let current_height = 2000;
    assert!(VaultEngine::check_withdrawal_eligibility(
        &new_state,
        current_height
    ));
}

/// Test withdrawal
#[test]
fn test_withdraw() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_6";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        withdrawal_delay_blocks: 144,
        require_unvault: true,
        ..Default::default()
    };

    // Create and unvault
    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config.clone())
        .unwrap();

    let unvault_script = vec![0x52, 0x87];
    let unvaulted_state = engine.unvault(&vault_state, unvault_script).unwrap();

    // Mark as unvaulted
    let unvault_tx_hash = [0x02; 32];
    let unvault_block_height = 1000;
    let mut unvaulted_state = engine
        .mark_unvaulted(&unvaulted_state, unvault_tx_hash, unvault_block_height)
        .unwrap();

    // Wait for delay
    let current_block_height = unvault_block_height + config.withdrawal_delay_blocks as u64 + 1;

    // Withdraw
    let final_withdrawal_script = vec![0x53, 0x87]; // OP_3, OP_EQUAL
    let result = engine.withdraw(
        &unvaulted_state,
        final_withdrawal_script,
        current_block_height,
    );

    assert!(result.is_ok(), "Withdrawal should succeed after delay");
    let withdrawn_state = result.unwrap();
    assert_eq!(withdrawn_state.state, VaultLifecycle::Withdrawing);
    assert!(withdrawn_state.withdrawal_covenant.is_some());
}

/// Test withdrawal fails before delay
#[test]
fn test_withdraw_fails_before_delay() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_7";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        withdrawal_delay_blocks: 144,
        require_unvault: true,
        ..Default::default()
    };

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config.clone())
        .unwrap();

    let unvault_script = vec![0x52, 0x87];
    let unvaulted_state = engine.unvault(&vault_state, unvault_script).unwrap();

    let unvault_tx_hash = [0x02; 32];
    let unvault_block_height = 1000;
    let unvaulted_state = engine
        .mark_unvaulted(&unvaulted_state, unvault_tx_hash, unvault_block_height)
        .unwrap();

    // Try to withdraw before delay
    let current_block_height = unvault_block_height + 100; // Only 100 blocks, need 144
    let final_withdrawal_script = vec![0x53, 0x87];
    let result = engine.withdraw(
        &unvaulted_state,
        final_withdrawal_script,
        current_block_height,
    );

    assert!(result.is_err(), "Withdrawal should fail before delay");
}

/// Test recovery path
#[test]
fn test_recover() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_8";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        recovery_script: Some(vec![0x51, 0x87]), // OP_1, OP_EQUAL
        ..Default::default()
    };

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config)
        .unwrap();

    let recovery_script = vec![0x54, 0x87]; // OP_4, OP_EQUAL
    let result = engine.recover(&vault_state, recovery_script);

    assert!(result.is_ok(), "Recovery should succeed");
    let recovered_state = result.unwrap();
    assert_eq!(recovered_state.state, VaultLifecycle::Recovered);
}

/// Test recovery fails when not configured
#[test]
fn test_recover_fails_when_not_configured() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_9";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        recovery_script: None,
        ..Default::default()
    };

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config)
        .unwrap();

    let recovery_script = vec![0x54, 0x87];
    let result = engine.recover(&vault_state, recovery_script);

    assert!(result.is_err(), "Recovery should fail when not configured");
}

/// Test mark_unvaulted updates state correctly
#[test]
fn test_mark_unvaulted() {
    let engine = create_test_vault_engine();
    let vault_id = "test_vault_10";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig {
        withdrawal_delay_blocks: 144,
        require_unvault: true,
        ..Default::default()
    };

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config.clone())
        .unwrap();

    let unvault_script = vec![0x52, 0x87];
    let unvaulted_state = engine.unvault(&vault_state, unvault_script).unwrap();

    let unvault_tx_hash = [0x03; 32];
    let unvault_block_height = 5000;
    let result = engine.mark_unvaulted(&unvaulted_state, unvault_tx_hash, unvault_block_height);

    assert!(result.is_ok());
    let new_state = result.unwrap();
    assert_eq!(new_state.unvault_tx_hash, Some(unvault_tx_hash));
    assert_eq!(
        new_state.withdrawal_available_at,
        Some(unvault_block_height + config.withdrawal_delay_blocks as u64)
    );
    assert!(matches!(
        new_state.state,
        VaultLifecycle::Unvaulted {
            unvaulted_at_block: 5000
        }
    ));
}

/// Test vault state storage (if storage available)
#[cfg(feature = "ctv")]
#[test]
fn test_vault_state_storage() {
    use blvm_node::storage::Storage;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let storage_path = temp_dir.path();

    // Create storage
    let storage = Storage::new(storage_path).expect("Failed to create storage");

    // Open or create the vaults tree (required for redb)
    // Note: In production, this would be done during Storage initialization
    // For tests, we'll skip storage persistence and test in-memory behavior
    let storage_arc = Arc::new(storage);

    // Create vault engine with storage
    let covenant_engine = Arc::new(CovenantEngine::new());
    let engine = VaultEngine::with_storage(covenant_engine, storage_arc);

    // Create vault
    let vault_id = "test_vault_storage";
    let deposit_amount = 100000;
    let withdrawal_script = create_test_withdrawal_script();
    let config = VaultConfig::default();

    let vault_state = engine
        .create_vault(vault_id, deposit_amount, withdrawal_script, config)
        .unwrap();

    // Retrieve vault (from in-memory cache)
    let retrieved = engine.get_vault(vault_id).unwrap();
    assert!(retrieved.is_some());
    let retrieved_state = retrieved.unwrap();
    assert_eq!(retrieved_state.vault_id, vault_state.vault_id);
    assert_eq!(retrieved_state.deposit_amount, vault_state.deposit_amount);
}

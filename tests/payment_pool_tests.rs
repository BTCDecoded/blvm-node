//! Unit tests for Payment Pool Engine
//!
//! Tests pool creation, joining, distribution, exit, and state management.

#![cfg(feature = "ctv")]

use bllvm_node::payment::covenant::CovenantEngine;
use bllvm_node::payment::pool::{PoolConfig, PoolEngine};
use bllvm_node::payment::processor::PaymentError;
use std::sync::Arc;

/// Helper to create a test pool engine
fn create_test_pool_engine() -> PoolEngine {
    let covenant_engine = Arc::new(CovenantEngine::new());
    PoolEngine::new(covenant_engine)
}

/// Helper to create test participants
fn create_test_participants() -> Vec<(String, u64, Vec<u8>)> {
    vec![
        ("participant_1".to_string(), 10000, vec![0x51, 0x87]),
        ("participant_2".to_string(), 20000, vec![0x52, 0x87]),
        ("participant_3".to_string(), 15000, vec![0x53, 0x87]),
    ]
}

/// Test pool creation
#[test]
fn test_create_pool() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_1";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let result = engine.create_pool(pool_id, initial_participants.clone(), config);

    assert!(result.is_ok(), "Pool creation should succeed");
    let pool_state = result.unwrap();

    assert_eq!(pool_state.pool_id, pool_id);
    assert_eq!(pool_state.participants.len(), 3);
    assert_eq!(pool_state.total_balance, 45000); // 10000 + 20000 + 15000
    assert!(pool_state.covenant_template.is_some());
    assert_eq!(pool_state.pool_utxo, None); // Not yet on-chain
}

/// Test pool creation with custom config
#[test]
fn test_create_pool_custom_config() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_2";
    let initial_participants = create_test_participants();
    let config = PoolConfig {
        min_contribution: 5000,
        max_participants: 10,
        pool_fee_percent: 2,
        min_balance: 100,
    };

    let result = engine.create_pool(pool_id, initial_participants, config.clone());

    assert!(result.is_ok());
    let pool_state = result.unwrap();
    assert_eq!(pool_state.config.min_contribution, 5000);
    assert_eq!(pool_state.config.max_participants, 10);
    assert_eq!(pool_state.config.pool_fee_percent, 2);
}

/// Test pool creation fails with too many participants
#[test]
fn test_create_pool_too_many_participants() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_3";
    let config = PoolConfig {
        max_participants: 2,
        ..Default::default()
    };

    let mut participants = create_test_participants();
    participants.push(("participant_4".to_string(), 10000, vec![0x54, 0x87]));

    let result = engine.create_pool(pool_id, participants, config);

    assert!(
        result.is_err(),
        "Pool creation should fail with too many participants"
    );
}

/// Test pool creation fails with insufficient contribution
#[test]
fn test_create_pool_insufficient_contribution() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_4";
    let config = PoolConfig {
        min_contribution: 50000,
        ..Default::default()
    };

    let participants = create_test_participants(); // All below 50000

    let result = engine.create_pool(pool_id, participants, config);

    assert!(
        result.is_err(),
        "Pool creation should fail with insufficient contribution"
    );
}

/// Test join pool
#[test]
fn test_join_pool() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_5";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    let new_participant_id = "participant_4";
    let contribution = 25000;
    let script_pubkey = vec![0x54, 0x87];

    let result = engine.join_pool(
        &pool_state,
        new_participant_id,
        contribution,
        script_pubkey.clone(),
    );

    assert!(result.is_ok(), "Join pool should succeed");
    let new_state = result.unwrap();
    assert_eq!(new_state.participants.len(), 4);
    assert_eq!(new_state.total_balance, 70000); // 45000 + 25000
}

/// Test join pool fails when participant already exists
#[test]
fn test_join_pool_duplicate_participant() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_6";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    let contribution = 25000;
    let script_pubkey = vec![0x54, 0x87];

    // Try to join with existing participant ID
    let result = engine.join_pool(&pool_state, "participant_1", contribution, script_pubkey);

    assert!(
        result.is_err(),
        "Join pool should fail with duplicate participant"
    );
}

/// Test join pool fails when pool is full
#[test]
fn test_join_pool_pool_full() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_7";
    let config = PoolConfig {
        max_participants: 2,
        ..Default::default()
    };

    let participants = vec![
        ("participant_1".to_string(), 10000, vec![0x51, 0x87]),
        ("participant_2".to_string(), 20000, vec![0x52, 0x87]),
    ];

    let pool_state = engine.create_pool(pool_id, participants, config).unwrap();

    let result = engine.join_pool(&pool_state, "participant_3", 15000, vec![0x53, 0x87]);

    assert!(result.is_err(), "Join pool should fail when pool is full");
}

/// Test distribute from pool
#[test]
fn test_distribute_pool() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_8";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    let distribution = vec![
        ("participant_1".to_string(), 5000),
        ("participant_2".to_string(), 10000),
    ];

    let result = engine.distribute(&pool_state, distribution.clone());

    assert!(result.is_ok(), "Distribution should succeed");
    let (new_state, covenant_proof) = result.unwrap();
    assert_eq!(new_state.total_balance, 30000); // 45000 - 15000
    assert!(covenant_proof.template_hash.len() == 32);

    // Check participant balances updated
    let p1 = new_state
        .participants
        .iter()
        .find(|p| p.participant_id == "participant_1")
        .unwrap();
    assert_eq!(p1.balance, 5000); // 10000 - 5000

    let p2 = new_state
        .participants
        .iter()
        .find(|p| p.participant_id == "participant_2")
        .unwrap();
    assert_eq!(p2.balance, 10000); // 20000 - 10000
}

/// Test distribute fails when amount exceeds balance
#[test]
fn test_distribute_exceeds_balance() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_9";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    let distribution = vec![
        ("participant_1".to_string(), 50000), // Exceeds total balance of 45000
    ];

    let result = engine.distribute(&pool_state, distribution);

    assert!(
        result.is_err(),
        "Distribution should fail when amount exceeds balance"
    );
}

/// Test exit pool
#[test]
fn test_exit_pool() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_10";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    let exit_amount = Some(5000);
    let result = engine.exit_pool(&pool_state, "participant_1", exit_amount);

    assert!(result.is_ok(), "Exit pool should succeed");
    let (new_state, exit_covenant) = result.unwrap();
    assert_eq!(new_state.total_balance, 40000); // 45000 - 5000
    assert!(exit_covenant.template_hash.len() == 32);

    // Check participant balance updated
    let p1 = new_state
        .participants
        .iter()
        .find(|p| p.participant_id == "participant_1");
    if let Some(p) = p1 {
        assert_eq!(p.balance, 5000); // 10000 - 5000
    }
}

/// Test exit pool with full balance
#[test]
fn test_exit_pool_full_balance() {
    let engine = create_test_pool_engine();
    let pool_id = "test_pool_11";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    let result = engine.exit_pool(&pool_state, "participant_1", None); // Exit full balance

    assert!(result.is_ok(), "Exit pool with full balance should succeed");
    let (new_state, _) = result.unwrap();

    // Participant should be removed if balance below min_balance
    let p1 = new_state
        .participants
        .iter()
        .find(|p| p.participant_id == "participant_1");
    // Participant might be removed if balance is zero and below min_balance
    assert!(p1.is_none() || p1.unwrap().balance == 0);
}

/// Test pool state storage (if storage available)
#[cfg(feature = "ctv")]
#[test]
fn test_pool_state_storage() {
    use bllvm_node::storage::Storage;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let storage_path = temp_dir.path();

    let storage = Storage::new(storage_path).expect("Failed to create storage");
    let storage_arc = Arc::new(storage);

    let covenant_engine = Arc::new(CovenantEngine::new());
    let engine = PoolEngine::with_storage(covenant_engine, storage_arc);

    let pool_id = "test_pool_storage";
    let initial_participants = create_test_participants();
    let config = PoolConfig::default();

    let pool_state = engine
        .create_pool(pool_id, initial_participants, config)
        .unwrap();

    // Retrieve pool (from in-memory cache)
    let retrieved = engine.get_pool(pool_id).unwrap();
    assert!(retrieved.is_some());
    let retrieved_state = retrieved.unwrap();
    assert_eq!(retrieved_state.pool_id, pool_state.pool_id);
    assert_eq!(retrieved_state.total_balance, pool_state.total_balance);
}

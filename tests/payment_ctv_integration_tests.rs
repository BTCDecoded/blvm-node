//! CTV + BIP70 Payment Integration Tests
//!
//! Tests the complete payment flow including:
//! - Payment request creation
//! - CTV covenant proof creation
//! - Payment state machine transitions
//! - Settlement monitoring
//! - Instant proof (not instant settlement) functionality

use bllvm_node::config::PaymentConfig;
#[cfg(feature = "ctv")]
use bllvm_node::payment::covenant::{CovenantEngine, CovenantProof};
use bllvm_node::payment::processor::PaymentProcessor;
use bllvm_node::payment::settlement::SettlementMonitor;
use bllvm_node::payment::state_machine::{PaymentState, PaymentStateMachine};
use bllvm_protocol::payment::PaymentOutput;
use std::sync::Arc;
use tempfile::TempDir;

/// Helper to create test payment outputs
fn create_test_outputs() -> Vec<PaymentOutput> {
    vec![
        PaymentOutput {
            script: vec![0x51, 0x87], // OP_1, OP_EQUAL (test script)
            amount: Some(100000),     // 0.001 BTC
        },
        PaymentOutput {
            script: vec![0x52, 0x87], // OP_2, OP_EQUAL
            amount: Some(50000),      // 0.0005 BTC
        },
    ]
}

/// Test payment request creation
#[tokio::test]
async fn test_payment_request_creation() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();
    let merchant_data = Some(b"test_merchant_data".to_vec());

    // Create payment request without covenant
    let (payment_id, covenant_proof) = state_machine
        .create_payment_request(outputs.clone(), merchant_data.clone(), false)
        .await
        .expect("Failed to create payment request");

    assert!(!payment_id.is_empty(), "Payment ID should not be empty");
    assert_eq!(
        covenant_proof, None,
        "Covenant proof should be None when create_covenant=false"
    );

    // Verify state
    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");

    match state {
        PaymentState::RequestCreated { request_id } => {
            assert_eq!(request_id, payment_id);
        }
        _ => panic!("Expected RequestCreated state, got {:?}", state),
    }
}

/// Test CTV covenant proof creation
#[tokio::test]
#[cfg(feature = "ctv")]
async fn test_covenant_proof_creation() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();

    // Create payment request with covenant
    let (payment_id, covenant_proof) = state_machine
        .create_payment_request(outputs.clone(), None, true)
        .await
        .expect("Failed to create payment request with covenant");

    assert!(!payment_id.is_empty(), "Payment ID should not be empty");
    assert!(
        covenant_proof.is_some(),
        "Covenant proof should be created when create_covenant=true"
    );

    let proof = covenant_proof.unwrap();
    assert_eq!(
        proof.payment_request_id, payment_id,
        "Covenant proof should reference correct payment ID"
    );
    assert!(
        !proof.template_hash.is_empty(),
        "Template hash should not be empty"
    );

    // Verify state
    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");

    match state {
        PaymentState::ProofCreated { request_id, .. } => {
            assert_eq!(request_id, payment_id);
        }
        _ => panic!("Expected ProofCreated state, got {:?}", state),
    }
}

/// Test payment state transitions
#[tokio::test]
async fn test_payment_state_transitions() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();

    // Create payment request
    let (payment_id, _) = state_machine
        .create_payment_request(outputs, None, false)
        .await
        .expect("Failed to create payment request");

    // Verify initial state
    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");
    assert!(matches!(state, PaymentState::RequestCreated { .. }));

    // Mark as in mempool
    let tx_hash = [0x42u8; 32];
    state_machine
        .mark_in_mempool(&payment_id, tx_hash)
        .await
        .expect("Failed to mark in mempool");

    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");
    match state {
        PaymentState::InMempool {
            request_id,
            tx_hash: tx,
        } => {
            assert_eq!(request_id, payment_id);
            assert_eq!(tx, tx_hash);
        }
        _ => panic!("Expected InMempool state, got {:?}", state),
    }

    // Mark as settled
    let block_hash = [0x43u8; 32];
    state_machine
        .mark_settled(&payment_id, tx_hash, block_hash, 1)
        .await
        .expect("Failed to mark settled");

    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");
    match state {
        PaymentState::Settled {
            request_id,
            tx_hash: tx,
            block_hash: block,
            confirmation_count,
        } => {
            assert_eq!(request_id, payment_id);
            assert_eq!(tx, tx_hash);
            assert_eq!(block, block_hash);
            assert_eq!(confirmation_count, 1);
        }
        _ => panic!("Expected Settled state, got {:?}", state),
    }
}

/// Test payment failure state
#[tokio::test]
async fn test_payment_failure() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();

    // Create payment request
    let (payment_id, _) = state_machine
        .create_payment_request(outputs, None, false)
        .await
        .expect("Failed to create payment request");

    // Mark as failed
    let reason = "Transaction rejected".to_string();
    state_machine
        .mark_failed(&payment_id, reason.clone())
        .await
        .expect("Failed to mark failed");

    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");
    match state {
        PaymentState::Failed {
            request_id,
            reason: r,
        } => {
            assert_eq!(request_id, payment_id);
            assert_eq!(r, reason);
        }
        _ => panic!("Expected Failed state, got {:?}", state),
    }
}

/// Test listing all payments
#[tokio::test]
async fn test_list_payments() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();

    // Create multiple payment requests
    let mut payment_ids = Vec::new();
    for _ in 0..3 {
        let outputs = create_test_outputs();
        let (payment_id, _) = state_machine
            .create_payment_request(outputs, None, false)
            .await
            .expect("Failed to create payment request");
        payment_ids.push(payment_id);
    }

    // List all payments
    let states = state_machine.list_payment_states();
    assert!(
        states.len() >= 3,
        "Should have at least 3 payments, got {}",
        states.len()
    );

    // Verify all created payments are in the list
    for payment_id in &payment_ids {
        assert!(
            states.contains_key(payment_id),
            "Payment {} should be in list",
            payment_id
        );
    }
}

/// Test instant proof functionality (CTV covenant)
#[tokio::test]
#[cfg(feature = "ctv")]
async fn test_instant_proof_functionality() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();

    // Create payment request with instant proof (CTV covenant)
    let (payment_id, covenant_proof) = state_machine
        .create_payment_request(outputs.clone(), None, true)
        .await
        .expect("Failed to create payment request with instant proof");

    // Verify instant proof was created
    assert!(
        covenant_proof.is_some(),
        "Instant proof (covenant) should be created immediately"
    );

    let proof = covenant_proof.unwrap();

    // Verify proof structure
    assert_eq!(
        proof.payment_request_id, payment_id,
        "Proof should reference correct payment ID"
    );
    assert!(
        !proof.template_hash.is_empty(),
        "Template hash should be present"
    );
    assert!(
        proof.transaction_template.outputs.len() == outputs.len(),
        "Template should have same number of outputs"
    );

    // Verify state is ProofCreated (instant proof ready)
    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");

    match &state {
        PaymentState::ProofCreated { request_id, .. } => {
            assert_eq!(request_id, &payment_id);
            // This is the key: instant proof is available, but settlement hasn't happened yet
            // Merchant can verify payment commitment immediately via CTV proof
        }
        _ => panic!(
            "Expected ProofCreated state for instant proof, got {:?}",
            state
        ),
    }

    // Verify that settlement hasn't happened yet (this is the point of instant proof)
    // The payment is NOT in mempool or settled - proof is instant, settlement is delayed
    assert!(
        !matches!(
            &state,
            PaymentState::InMempool { .. } | PaymentState::Settled { .. }
        ),
        "Settlement should not have happened yet - instant proof is separate from settlement"
    );
}

/// Test covenant proof creation for existing payment
#[tokio::test]
#[cfg(feature = "ctv")]
async fn test_create_covenant_proof_for_existing_payment() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    let outputs = create_test_outputs();

    // Create payment request without covenant
    let (payment_id, _) = state_machine
        .create_payment_request(outputs, None, false)
        .await
        .expect("Failed to create payment request");

    // Create covenant proof for existing payment
    let covenant_proof = state_machine
        .create_covenant_proof(&payment_id)
        .await
        .expect("Failed to create covenant proof");

    assert_eq!(
        covenant_proof.payment_request_id, payment_id,
        "Covenant proof should reference correct payment ID"
    );

    // Verify state transitioned to ProofCreated
    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("Failed to get payment state");

    match state {
        PaymentState::ProofCreated { request_id, .. } => {
            assert_eq!(request_id, payment_id);
        }
        _ => panic!("Expected ProofCreated state, got {:?}", state),
    }
}

/// Test settlement monitor initialization
#[tokio::test]
async fn test_settlement_monitor_initialization() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    // Create settlement monitor
    let monitor = SettlementMonitor::new(state_machine);

    // Monitor should be created successfully
    // (No assertions needed - just verify it compiles and initializes)
    assert!(true, "Settlement monitor created successfully");
}

/// Test payment state machine with multiple concurrent payments
#[tokio::test]
async fn test_concurrent_payment_requests() {
    let temp_dir = TempDir::new().unwrap();
    let config = PaymentConfig::default();
    let processor =
        Arc::new(PaymentProcessor::new(config).expect("Failed to create payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    // Create multiple payment requests concurrently
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let state_machine = Arc::clone(&state_machine);
            let outputs = create_test_outputs();
            tokio::spawn(async move {
                state_machine
                    .create_payment_request(outputs, None, false)
                    .await
            })
        })
        .collect();

    // Wait for all requests to complete
    let mut payment_ids = Vec::new();
    for handle in handles {
        let result = handle.await.expect("Task should complete successfully");
        let (payment_id, _) = result.expect("Payment request should succeed");
        payment_ids.push(payment_id);
    }

    // Verify all payments were created
    assert_eq!(payment_ids.len(), 5, "Should have 5 payment requests");

    // Verify all payment IDs are unique
    let unique_ids: std::collections::HashSet<String> = payment_ids.iter().cloned().collect();
    assert_eq!(unique_ids.len(), 5, "All payment IDs should be unique");

    // Verify all payments are in the state machine
    let states = state_machine.list_payment_states();
    for payment_id in &payment_ids {
        assert!(
            states.contains_key(payment_id),
            "Payment {} should be in state machine",
            payment_id
        );
    }
}

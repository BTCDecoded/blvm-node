//! verify_on_chain_payment RPC round-trip (path 3, no CTV required).

#![cfg(feature = "bip70-http")]

use blvm_node::config::PaymentConfig;
use blvm_node::payment::processor::PaymentProcessor;
use blvm_node::payment::state_machine::{PaymentState, PaymentStateMachine};
use blvm_node::rpc::payment::PaymentRpc;
use blvm_protocol::payment::PaymentOutput;
use serde_json::json;
use std::sync::Arc;

fn create_test_outputs() -> Vec<PaymentOutput> {
    vec![
        PaymentOutput {
            script: vec![0x51, 0x87],
            amount: Some(100_000),
        },
        PaymentOutput {
            script: vec![0x52, 0x87],
            amount: Some(50_000),
        },
    ]
}

fn create_rpc() -> (PaymentRpc, Arc<PaymentStateMachine>) {
    let processor =
        Arc::new(PaymentProcessor::new(PaymentConfig::default()).expect("payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));
    (
        PaymentRpc::with_state_machine(state_machine.clone()),
        state_machine,
    )
}

#[tokio::test]
async fn verify_on_chain_payment_in_mempool() {
    let (rpc, state_machine) = create_rpc();
    let (payment_id, _) = state_machine
        .create_payment_request(create_test_outputs(), None, false)
        .await
        .expect("create payment request");

    let tx_hash = [0x42u8; 32];
    state_machine
        .mark_in_mempool(&payment_id, tx_hash)
        .await
        .expect("mark in mempool");

    let result = rpc
        .verify_on_chain_payment(&json!([payment_id, hex::encode(tx_hash)]))
        .await
        .expect("verify_on_chain_payment");

    assert_eq!(result["verified"], true);
    assert_eq!(result["state"], "in_mempool");
    assert_eq!(result["amount_sats"], 150_000);
    assert_eq!(result["tx_hash"], hex::encode(tx_hash));
}

#[tokio::test]
async fn verify_on_chain_payment_settled() {
    let (rpc, state_machine) = create_rpc();
    let (payment_id, _) = state_machine
        .create_payment_request(create_test_outputs(), None, false)
        .await
        .expect("create payment request");

    let tx_hash = [0x43u8; 32];
    let block_hash = [0x44u8; 32];
    state_machine
        .mark_in_mempool(&payment_id, tx_hash)
        .await
        .expect("mark in mempool");
    state_machine
        .mark_settled(&payment_id, tx_hash, block_hash, 6, None)
        .await
        .expect("mark settled");

    let result = rpc
        .verify_on_chain_payment(&json!([payment_id, hex::encode(tx_hash)]))
        .await
        .expect("verify_on_chain_payment");

    assert_eq!(result["verified"], true);
    assert_eq!(result["state"], "settled");
    assert_eq!(result["amount_sats"], 150_000);
}

#[tokio::test]
async fn verify_on_chain_payment_tx_mismatch() {
    let (rpc, state_machine) = create_rpc();
    let (payment_id, _) = state_machine
        .create_payment_request(create_test_outputs(), None, false)
        .await
        .expect("create payment request");

    let tx_hash = [0x45u8; 32];
    state_machine
        .mark_in_mempool(&payment_id, tx_hash)
        .await
        .expect("mark in mempool");

    let wrong_hash = [0x99u8; 32];
    let result = rpc
        .verify_on_chain_payment(&json!([payment_id, hex::encode(wrong_hash)]))
        .await
        .expect("verify_on_chain_payment");

    assert_eq!(result["verified"], false);
    assert_eq!(result["state"], "tx_mismatch");
}

#[tokio::test]
async fn verify_on_chain_payment_pending() {
    let (rpc, state_machine) = create_rpc();
    let (payment_id, _) = state_machine
        .create_payment_request(create_test_outputs(), None, false)
        .await
        .expect("create payment request");

    let state = state_machine
        .get_payment_state(&payment_id)
        .await
        .expect("get state");
    assert!(matches!(state, PaymentState::RequestCreated { .. }));

    let tx_hash = [0x46u8; 32];
    let result = rpc
        .verify_on_chain_payment(&json!([payment_id, hex::encode(tx_hash)]))
        .await
        .expect("verify_on_chain_payment");

    assert_eq!(result["verified"], false);
    assert_eq!(result["state"], "pending");
}

#[tokio::test]
async fn verify_on_chain_payment_not_found() {
    let (rpc, _) = create_rpc();

    let result = rpc
        .verify_on_chain_payment(&json!(["missing-id", hex::encode([0u8; 32])]))
        .await
        .expect("verify_on_chain_payment");

    assert_eq!(result["verified"], false);
    assert_eq!(result["state"], "not_found");
}

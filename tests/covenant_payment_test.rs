//! verify_covenant_proof RPC (path 2, requires CTV).

#![cfg(all(feature = "bip70-http", feature = "ctv"))]

use blvm_node::config::PaymentConfig;
use blvm_node::payment::processor::PaymentProcessor;
use blvm_node::payment::state_machine::PaymentStateMachine;
use blvm_node::rpc::payment::PaymentRpc;
use blvm_protocol::payment::PaymentOutput;
use serde_json::json;
use std::sync::Arc;

fn create_test_outputs() -> Vec<PaymentOutput> {
    vec![PaymentOutput {
        script: vec![0x51, 0x87],
        amount: Some(100_000),
    }]
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
async fn verify_covenant_proof_accepts_valid_proof() {
    let (rpc, state_machine) = create_rpc();
    let (payment_id, covenant_proof) = state_machine
        .create_payment_request(create_test_outputs(), None, true)
        .await
        .expect("create payment with covenant");

    let proof = covenant_proof.expect("covenant proof created");
    let proof_bytes = bincode::serialize(&proof).expect("serialize covenant proof");

    let result = rpc
        .verify_covenant_proof(&json!([hex::encode(&proof_bytes), 0u32, 100_000u64,]))
        .await
        .expect("verify_covenant_proof");

    assert_eq!(result["verified"], true, "payment_id={payment_id}");
}

#[tokio::test]
async fn verify_covenant_proof_rejects_amount_mismatch() {
    let (rpc, state_machine) = create_rpc();
    let (_payment_id, covenant_proof) = state_machine
        .create_payment_request(create_test_outputs(), None, true)
        .await
        .expect("create payment with covenant");

    let proof_bytes = bincode::serialize(&covenant_proof.expect("proof")).unwrap();

    let result = rpc
        .verify_covenant_proof(&json!([hex::encode(&proof_bytes), 0u32, 1u64,]))
        .await
        .expect("verify_covenant_proof");

    assert_eq!(result["verified"], false);
}

#[tokio::test]
async fn verify_covenant_proof_rejects_empty_bytes() {
    let (rpc, _state_machine) = create_rpc();

    let result = rpc
        .verify_covenant_proof(&json!(["", 0u32, 100_000u64]))
        .await
        .expect("verify_covenant_proof");

    assert_eq!(result["verified"], false);
}

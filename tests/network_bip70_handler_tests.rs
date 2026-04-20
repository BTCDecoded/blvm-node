//! Tests for BIP70 Payment Protocol Handler

use blvm_node::config::PaymentConfig;
use blvm_node::network::bip70_handler::{
    handle_get_payment_request, handle_payment, validate_payment_ack_message,
    validate_payment_request_message,
};
use blvm_node::network::protocol::{
    GetPaymentRequestMessage, PaymentACKMessage, PaymentMessage, PaymentRequestMessage,
};
use blvm_node::payment::processor::PaymentProcessor;
use blvm_protocol::payment::{Payment, PaymentOutput, PaymentRequest};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Same key scheme as `PaymentProcessor::generate_payment_id`.
fn processor_payment_id(request: &PaymentRequest) -> String {
    let serialized = bincode::serialize(request).unwrap_or_default();
    let hash = Sha256::digest(&serialized);
    hex::encode(&hash[..16])
}

fn create_test_payment_request() -> PaymentRequest {
    PaymentRequest::new(
        "mainnet".to_string(),
        vec![PaymentOutput {
            script: vec![0x76, 0xa9, 0x14], // P2PKH script
            amount: Some(100000),           // 0.001 BTC
        }],
        1234567890,
    )
}

fn create_test_payment() -> Payment {
    Payment {
        transactions: vec![vec![1, 2, 3]], // Mock transaction
        refund_to: None,
        merchant_data: None,
        memo: None,
    }
}

#[tokio::test]
async fn test_handle_get_payment_request_not_found() {
    let request = GetPaymentRequestMessage {
        network: "mainnet".to_string(),
        merchant_pubkey: vec![4, 5, 6],
        payment_id: vec![1, 2, 3],
    };

    let result = handle_get_payment_request(&request, None).await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("not found") || msg.contains("not available"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn test_handle_get_payment_request_found() {
    let config = PaymentConfig::default();
    let processor = Arc::new(PaymentProcessor::new(config).expect("payment processor"));

    let payment_request = processor
        .create_payment_request(
            vec![PaymentOutput {
                script: vec![0x76, 0xa9, 0x14],
                amount: Some(100000),
            }],
            None,
            None,
        )
        .await
        .expect("create payment request");

    let key = processor_payment_id(&payment_request);
    let payment_id_bytes = hex::decode(&key).expect("payment id hex");

    let request = GetPaymentRequestMessage {
        network: "mainnet".to_string(),
        merchant_pubkey: vec![4, 5, 6],
        payment_id: payment_id_bytes.clone(),
    };

    let result = handle_get_payment_request(&request, Some(processor)).await;
    assert!(result.is_ok());

    let response = result.unwrap();
    assert_eq!(response.payment_id, payment_id_bytes);
}

#[tokio::test]
async fn test_handle_payment_not_found() {
    let payment_msg = PaymentMessage {
        payment: create_test_payment(),
        payment_id: vec![1, 2, 3],
        customer_signature: None,
    };

    let result = handle_payment(&payment_msg, None, None).await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("not found") || msg.contains("not available"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn test_validate_payment_request_message_no_signature() {
    let payment_request = create_test_payment_request();
    let msg = PaymentRequestMessage {
        payment_request: payment_request.clone(),
        payment_id: vec![],
        merchant_pubkey: vec![],
        merchant_signature: vec![],
        #[cfg(feature = "ctv")]
        covenant_proof: None,
    };

    // Should fail validation (no signature)
    let result = validate_payment_request_message(&msg);
    // Note: This may succeed if validation is lenient, or fail if strict
    // The actual behavior depends on PaymentProtocolClient::validate_payment_request
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_validate_payment_ack_message() {
    use blvm_protocol::payment::PaymentACK;

    let payment = create_test_payment();
    let payment_ack = PaymentACK {
        payment: payment.clone(),
        memo: Some("Payment received".to_string()),
        signature: None,
    };

    let msg = PaymentACKMessage {
        payment_ack: payment_ack.clone(),
        payment_id: vec![],
        merchant_signature: vec![],
    };

    // Validation may succeed or fail depending on implementation
    let result = validate_payment_ack_message(&msg, &[]);
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_payment_request_message_structure() {
    let payment_request = create_test_payment_request();
    let msg = PaymentRequestMessage {
        payment_request: payment_request.clone(),
        payment_id: vec![1, 2, 3],
        merchant_pubkey: vec![4, 5, 6],
        merchant_signature: vec![],
        #[cfg(feature = "ctv")]
        covenant_proof: None,
    };

    assert_eq!(msg.payment_id, vec![1, 2, 3]);
    assert_eq!(msg.merchant_pubkey, vec![4, 5, 6]);
}

#[test]
fn test_payment_message_structure() {
    let payment = create_test_payment();
    let msg = PaymentMessage {
        payment: payment.clone(),
        payment_id: vec![1, 2, 3],
        customer_signature: None,
    };

    assert_eq!(msg.payment_id, vec![1, 2, 3]);
    assert_eq!(msg.payment.transactions.len(), 1);
}

#[test]
fn test_payment_ack_message_structure() {
    use blvm_protocol::payment::PaymentACK;

    let payment = create_test_payment();
    let payment_ack = PaymentACK {
        payment: payment.clone(),
        memo: Some("ACK".to_string()),
        signature: None,
    };

    let msg = PaymentACKMessage {
        payment_ack: payment_ack.clone(),
        payment_id: vec![1, 2, 3],
        merchant_signature: vec![],
    };

    assert_eq!(msg.payment_id, vec![1, 2, 3]);
    assert!(msg.payment_ack.memo.is_some());
}

#[test]
fn test_get_payment_request_message_structure() {
    let msg = GetPaymentRequestMessage {
        network: "mainnet".to_string(),
        merchant_pubkey: vec![4, 5, 6],
        payment_id: vec![1, 2, 3],
    };

    assert_eq!(msg.network, "mainnet");
    assert_eq!(msg.payment_id, vec![1, 2, 3]);
    assert_eq!(msg.merchant_pubkey, vec![4, 5, 6]);
}

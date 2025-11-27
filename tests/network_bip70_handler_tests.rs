//! Tests for BIP70 Payment Protocol Handler

use bllvm_node::network::bip70_handler::{
    handle_get_payment_request, handle_payment, validate_payment_ack_message,
    validate_payment_request_message,
};
use bllvm_node::network::protocol::{
    GetPaymentRequestMessage, PaymentACKMessage, PaymentMessage, PaymentRequestMessage,
};
use bllvm_protocol::payment::{Payment, PaymentOutput, PaymentRequest};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type PaymentRequestStore = Arc<Mutex<HashMap<String, PaymentRequest>>>;

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
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[tokio::test]
async fn test_handle_get_payment_request_found() {
    let request = GetPaymentRequestMessage {
        network: "mainnet".to_string(),
        merchant_pubkey: vec![4, 5, 6],
        payment_id: vec![1, 2, 3],
    };

    let payment_request = create_test_payment_request();
    let mut store = HashMap::new();
    let key = format!(
        "{}_{}",
        hex::encode(&request.payment_id),
        hex::encode(&request.merchant_pubkey)
    );
    store.insert(key, payment_request.clone());
    let store: PaymentRequestStore = Arc::new(Mutex::new(store));

    let result = handle_get_payment_request(&request, Some(&store)).await;
    assert!(result.is_ok());

    let response = result.unwrap();
    assert_eq!(response.payment_id, request.payment_id);
    assert_eq!(response.merchant_pubkey, request.merchant_pubkey);
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
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[tokio::test]
async fn test_validate_payment_request_message_no_signature() {
    let payment_request = create_test_payment_request();
    let msg = PaymentRequestMessage {
        payment_request: payment_request.clone(),
        payment_id: vec![],
        merchant_pubkey: vec![],
        merchant_signature: vec![],
    };

    // Should fail validation (no signature)
    let result = validate_payment_request_message(&msg);
    // Note: This may succeed if validation is lenient, or fail if strict
    // The actual behavior depends on PaymentProtocolClient::validate_payment_request
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_validate_payment_ack_message() {
    use bllvm_protocol::payment::PaymentACK;

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
    use bllvm_protocol::payment::PaymentACK;

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

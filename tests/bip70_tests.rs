//! Tests for BIP70 payment verification and signing.
//!
//! Payment transaction bytes use **bincode** to match `verify_payment_transactions`
//! in blvm-protocol (not Bitcoin wire `serialize_transaction`).

use blvm_node::network::protocol::PaymentMessage;
use blvm_protocol::payment::{Payment, PaymentOutput, PaymentProtocolServer, PaymentRequest};
use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

#[test]
fn test_payment_verification() {
    // Create a payment request
    let output = PaymentOutput {
        script: vec![0x51], // OP_1
        amount: Some(1000),
    };

    let payment_request = PaymentRequest::new("main".to_string(), vec![output.clone()], 1234567890);

    // Create a payment transaction
    let tx = Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51], // Matches payment request
        }],
        lock_time: 0,
    };

    let tx_bytes = bincode::serialize(&tx)
        .expect("bincode tx for BIP70 must match deserialize in verify_payment_transactions");

    let payment = Payment::new(vec![tx_bytes]);

    // Create payment message
    let payment_msg = PaymentMessage {
        payment,
        payment_id: vec![1, 2, 3, 4],
        customer_signature: None,
    };

    // Process payment (without merchant key for now)
    // Note: process_payment expects &Payment, not &PaymentMessage
    let result = PaymentProtocolServer::process_payment(
        &payment_msg.payment,
        &payment_request,
        None, // No merchant key
    );

    // Should succeed (validation passes)
    assert!(result.is_ok());
}

#[test]
fn test_payment_ack_signing() {
    // Scalar 1 — valid secret; pubkey derivation + ECDSA live in blvm-protocol via blvm-secp256k1.
    let mut merchant_key = [0u8; 32];
    merchant_key[31] = 1;

    let output = PaymentOutput {
        script: vec![0x51],
        amount: Some(1000),
    };

    let mut payment_request = PaymentRequest::new("main".to_string(), vec![output], 1_234_567_890);
    payment_request
        .sign(&merchant_key)
        .expect("sign payment request (sets merchant_pubkey + blvm-secp256k1 signature)");
    payment_request
        .verify_signature()
        .expect("payment request signature must verify");

    let merchant_pubkey = payment_request
        .merchant_pubkey
        .as_ref()
        .expect("sign() sets merchant_pubkey")
        .as_slice();

    let tx = Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    let tx_bytes = bincode::serialize(&tx)
        .expect("bincode tx for BIP70 must match deserialize in verify_payment_transactions");
    let payment = Payment::new(vec![tx_bytes]);

    let payment_msg = PaymentMessage {
        payment,
        payment_id: vec![1, 2, 3, 4],
        customer_signature: None,
    };

    let result = PaymentProtocolServer::process_payment(
        &payment_msg.payment,
        &payment_request,
        Some(&merchant_key),
    );
    assert!(result.is_ok(), "{:?}", result.err());
    let ack = result.unwrap();

    ack.verify_signature(merchant_pubkey)
        .expect("PaymentACK merchant signature must verify");

    assert!(ack.memo.is_some());
}

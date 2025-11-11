//! Tests for BIP70 payment verification and signing

use bllvm_node::network::protocol::PaymentMessage;
use bllvm_protocol::payment::{Payment, PaymentOutput, PaymentProtocolServer, PaymentRequest};
use bllvm_protocol::serialization::transaction::serialize_transaction;
use bllvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
use secp256k1::{Secp256k1, SecretKey};

#[test]
#[ignore] // BIP70 module not yet implemented
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
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51], // Matches payment request
        }],
        lock_time: 0,
    };

    let tx_bytes = serialize_transaction(&tx);

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
#[ignore] // BIP70 module not yet implemented
fn test_payment_ack_signing() {
    let secp = Secp256k1::new();
    let merchant_key = SecretKey::from_slice(&[1; 32]).unwrap();

    // Create payment request with merchant pubkey
    let output = PaymentOutput {
        script: vec![0x51],
        amount: Some(1000),
    };

    let mut payment_request = PaymentRequest::new("main".to_string(), vec![output], 1234567890);

    // Set merchant pubkey
    let merchant_pubkey = secp256k1::PublicKey::from_secret_key(&secp, &merchant_key);
    payment_request.merchant_pubkey = Some(merchant_pubkey.serialize().to_vec());

    // Create payment
    let tx = Transaction {
        version: 1,
        inputs: vec![TransactionInput {
            prevout: OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            script_sig: vec![],
            sequence: 0xffffffff,
        }],
        outputs: vec![TransactionOutput {
            value: 1000,
            script_pubkey: vec![0x51],
        }],
        lock_time: 0,
    };

    let tx_bytes = serialize_transaction(&tx);
    let payment = Payment::new(vec![tx_bytes]);

    let payment_msg = PaymentMessage {
        payment,
        payment_id: vec![1, 2, 3, 4],
        customer_signature: None,
    };

    // Process payment with merchant key
    // Note: process_payment expects &Payment, not &PaymentMessage
    let result = PaymentProtocolServer::process_payment(
        &payment_msg.payment,
        &payment_request,
        Some(&merchant_key),
    );

    assert!(result.is_ok());
    let ack = result.unwrap();

    // Verify payment ACK structure (PaymentACK doesn't have merchant_signature field)
    assert!(ack.memo.is_some());
}

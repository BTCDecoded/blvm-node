//! Integration tests for module payment system
//!
//! Tests the full module payment flow including payment request creation,
//! payment verification, and the 75/15/10 split.

use bllvm_node::config::PaymentConfig;
use bllvm_node::module::registry::manifest::{ModuleManifest, PaymentSection};
use bllvm_node::module::security::signing::ModuleSigner;
use bllvm_node::payment::processor::{PaymentError, PaymentProcessor};
use bllvm_protocol::address::{BitcoinAddress, Network};
use bllvm_protocol::payment::PaymentOutput;
use secp256k1::{Secp256k1, SecretKey};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Helper to create valid test addresses
fn create_test_addresses() -> (String, String) {
    // Create valid testnet P2WPKH addresses (20 bytes witness program)
    let author_program = vec![0x75; 20];
    let commons_program = vec![0x76; 20]; // Different program for different address

    let author_addr = BitcoinAddress::new(Network::Testnet, 0, author_program)
        .unwrap()
        .encode()
        .unwrap();
    let commons_addr = BitcoinAddress::new(Network::Testnet, 0, commons_program)
        .unwrap()
        .encode()
        .unwrap();

    (author_addr, commons_addr)
}

/// Helper to create a test manifest with payment section
fn create_manifest_with_payment(
    name: &str,
    price_sats: u64,
    author_address: &str,
    commons_address: &str,
) -> ModuleManifest {
    // Create a payment signature matching the actual implementation
    // The message format is: author_address||commons_address||price_sats
    let secp = Secp256k1::new();
    let test_key = SecretKey::from_slice(&[1; 32]).unwrap();

    // Create message in the format used by verify_payment_addresses
    let message_data = format!("{}||{}||{}", author_address, commons_address, price_sats);
    let message_hash = Sha256::digest(message_data.as_bytes());
    let message = secp256k1::Message::from_digest_slice(&message_hash).unwrap();
    let signature = secp.sign_ecdsa(&message, &test_key);
    let signature_hex = hex::encode(signature.serialize_compact());

    // Get public key for signature section
    let pubkey = secp256k1::PublicKey::from_secret_key(&secp, &test_key);
    let pubkey_hex = hex::encode(pubkey.serialize());

    // Create signature section first (needed for payment verification)
    use bllvm_node::module::registry::manifest::{MaintainerSignature, SignatureSection};
    let signature_section = SignatureSection {
        maintainers: vec![MaintainerSignature {
            name: "test-maintainer".to_string(),
            public_key: pubkey_hex.clone(),
            signature: "dummy".to_string(), // Not used for payment verification
        }],
        threshold: Some("1-of-1".to_string()),
    };

    let payment_section = PaymentSection {
        required: true,
        price_sats: Some(price_sats),
        author_payment_code: None,
        author_address: Some(author_address.to_string()),
        commons_payment_code: None,
        commons_address: Some(commons_address.to_string()),
        payment_signature: Some(signature_hex),
    };

    ModuleManifest {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        description: Some(format!("Test module: {}", name)),
        author: Some("Test Author".to_string()),
        capabilities: Vec::new(),
        dependencies: HashMap::new(),
        entry_point: format!("{}.so", name),
        config_schema: HashMap::new(),
        binary: None,
        signatures: Some(signature_section),
        payment: Some(payment_section),
    }
}

#[tokio::test]
async fn test_module_payment_request_creation() {
    let config = PaymentConfig::default();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Create a manifest with payment info
    let (author_addr, commons_addr) = create_test_addresses();
    let manifest = create_manifest_with_payment(
        "test-module",
        100000, // 100k sats
        &author_addr,
        &commons_addr,
    );

    // Node's payment script (10% of payment)
    let node_script = vec![0x51, 0x00]; // Dummy script

    // Create module hash for encryption
    let module_hash = {
        let data = b"test_module_binary";
        let hash = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Create payment request
    let payment_request = processor
        .create_module_payment_request(&manifest, &module_hash, node_script.clone(), None)
        .await
        .expect("Failed to create module payment request");

    // Verify payment request structure
    assert_eq!(payment_request.payment_details.outputs.len(), 3);

    // Verify 75/15/10 split
    let outputs = &payment_request.payment_details.outputs;
    let author_amount = outputs[0].amount.unwrap();
    let commons_amount = outputs[1].amount.unwrap();
    let node_amount = outputs[2].amount.unwrap();

    assert_eq!(author_amount, 75000); // 75% of 100k
    assert_eq!(commons_amount, 15000); // 15% of 100k
    assert_eq!(node_amount, 10000); // 10% of 100k

    // Verify total equals price (accounting for rounding)
    let total = author_amount + commons_amount + node_amount;
    assert!(total <= 100000);
    assert!(total >= 99900); // Allow for small rounding differences

    // Verify merchant_data contains module info
    if let Some(ref merchant_data) = payment_request.payment_details.merchant_data {
        let merchant_json: serde_json::Value =
            serde_json::from_slice(merchant_data).expect("Failed to parse merchant_data");
        assert_eq!(merchant_json["module_name"], "test-module");
        assert_eq!(merchant_json["payment_type"], "module_payment");
        assert_eq!(merchant_json["price_sats"], 100000);
        // Verify module_hash is present
        assert!(merchant_json["module_hash"].is_string());
    } else {
        panic!("merchant_data should contain module information");
    }
}

#[tokio::test]
async fn test_module_payment_request_free_module() {
    let config = PaymentConfig::default();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Create a manifest without payment requirement
    let (author_addr, commons_addr) = create_test_addresses();
    let mut manifest = create_manifest_with_payment("free-module", 0, &author_addr, &commons_addr);
    manifest.payment.as_mut().unwrap().required = false;

    let node_script = vec![0x51, 0x00];

    // Create module hash
    let module_hash = {
        let data = b"test_module_binary";
        let hash = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Should fail because module doesn't require payment
    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script, None)
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        PaymentError::ProcessingError(msg) => {
            assert!(msg.contains("does not require payment"));
        }
        _ => panic!("Expected ProcessingError for free module"),
    }
}

#[tokio::test]
async fn test_module_payment_request_missing_payment_section() {
    let config = PaymentConfig::default();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Create a manifest without payment section
    let manifest = ModuleManifest {
        name: "no-payment-module".to_string(),
        version: "1.0.0".to_string(),
        description: None,
        author: None,
        capabilities: Vec::new(),
        dependencies: HashMap::new(),
        entry_point: "no-payment-module.so".to_string(),
        config_schema: HashMap::new(),
        binary: None,
        signatures: None,
        payment: None,
    };

    let node_script = vec![0x51, 0x00];

    // Create module hash
    let module_hash = {
        let data = b"test_module_binary";
        let hash = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Should fail because payment section is missing
    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script, None)
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        PaymentError::ProcessingError(msg) => {
            assert!(msg.contains("missing payment section"));
        }
        _ => panic!("Expected ProcessingError for missing payment section"),
    }
}

#[tokio::test]
async fn test_module_payment_request_missing_addresses() {
    let config = PaymentConfig::default();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Create a manifest with payment section but missing addresses
    let (author_addr, commons_addr) = create_test_addresses();
    let mut manifest =
        create_manifest_with_payment("missing-addr-module", 100000, &author_addr, &commons_addr);
    manifest.payment.as_mut().unwrap().author_address = None;
    manifest.payment.as_mut().unwrap().commons_address = None;

    let node_script = vec![0x51, 0x00];

    // Create module hash
    let module_hash = {
        let data = b"test_module_binary";
        let hash = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Should fail because addresses are missing
    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script, None)
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        PaymentError::ProcessingError(msg) => {
            assert!(
                msg.contains("payment address") || msg.contains("payment code"),
                "Error message should mention payment address or code"
            );
        }
        _ => panic!("Expected ProcessingError for missing addresses"),
    }
}

#[tokio::test]
async fn test_module_payment_request_invalid_signature() {
    let config = PaymentConfig::default();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Create a manifest with invalid payment signature
    let (author_addr, commons_addr) = create_test_addresses();
    let mut manifest =
        create_manifest_with_payment("invalid-sig-module", 100000, &author_addr, &commons_addr);
    manifest.payment.as_mut().unwrap().payment_signature = Some("invalid_signature".to_string());

    let node_script = vec![0x51, 0x00];

    // Create module hash
    let module_hash = {
        let data = b"test_module_binary";
        let hash = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Should fail because signature is invalid
    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script, None)
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        PaymentError::ValidationFailed(_) => {
            // Expected - signature verification should fail
        }
        PaymentError::ProcessingError(msg) if msg.contains("signature") => {
            // Also acceptable - signature parsing might fail first
        }
        e => panic!(
            "Expected ValidationFailed or ProcessingError for invalid signature, got: {:?}",
            e
        ),
    }
}

#[tokio::test]
async fn test_module_payment_request_different_prices() {
    let config = PaymentConfig::default();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Test with different price amounts
    let prices = vec![1000, 10000, 100000, 1000000];

    for price in prices {
        let (author_addr, commons_addr) = create_test_addresses();
        let manifest = create_manifest_with_payment(
            &format!("module-{}", price),
            price,
            &author_addr,
            &commons_addr,
        );

        let node_script = vec![0x51, 0x00];

        // Create module hash
        let module_hash = {
            let data = format!("module-{}", price).into_bytes();
            let hash = Sha256::digest(&data);
            let mut h = [0u8; 32];
            h.copy_from_slice(&hash);
            h
        };

        let payment_request = processor
            .create_module_payment_request(&manifest, &module_hash, node_script.clone(), None)
            .await
            .expect(&format!(
                "Failed to create payment request for price {}",
                price
            ));

        // Verify split proportions remain correct
        let outputs = &payment_request.payment_details.outputs;
        let author_amount = outputs[0].amount.unwrap();
        let commons_amount = outputs[1].amount.unwrap();
        let node_amount = outputs[2].amount.unwrap();

        // Calculate expected amounts (allowing for rounding)
        let expected_author = (price * 75) / 100;
        let expected_commons = (price * 15) / 100;
        let expected_node = (price * 10) / 100;

        assert_eq!(
            author_amount, expected_author,
            "Author amount incorrect for price {}",
            price
        );
        assert_eq!(
            commons_amount, expected_commons,
            "Commons amount incorrect for price {}",
            price
        );
        assert_eq!(
            node_amount, expected_node,
            "Node amount incorrect for price {}",
            price
        );
    }
}

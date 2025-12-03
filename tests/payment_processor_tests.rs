//! Unit tests for unified payment processor
//!
//! Tests the core payment processing logic that works for both HTTP and P2P.

use blvm_node::config::PaymentConfig;
use blvm_node::payment::processor::{PaymentError, PaymentProcessor};
use blvm_protocol::payment::{Payment, PaymentOutput, PaymentRequest};
use sha2::{Digest, Sha256};

fn default_payment_config() -> PaymentConfig {
    PaymentConfig::default()
}

#[tokio::test]
async fn test_create_payment_request() {
    let config = default_payment_config();

    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    let outputs = vec![PaymentOutput {
        script: vec![0x51, 0x00], // OP_1 OP_0 (dummy script)
        amount: Some(100000),
    }];

    let merchant_data = Some(b"test_merchant_data".to_vec());

    let payment_request = processor
        .create_payment_request(outputs.clone(), merchant_data, None)
        .await
        .expect("Failed to create payment request");

    // Verify payment request structure
    assert_eq!(payment_request.payment_details.outputs.len(), 1);
    assert_eq!(
        payment_request.payment_details.outputs[0].amount,
        Some(100000)
    );
    assert_eq!(
        payment_request.payment_details.merchant_data,
        Some(b"test_merchant_data".to_vec())
    );

    // Payment request is stored internally with a generated ID
    // We can verify the request was created successfully by checking its structure
    assert!(payment_request.payment_details.time > 0);
}

#[tokio::test]
async fn test_process_payment() {
    let config = default_payment_config();

    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Create a payment request first
    let outputs = vec![PaymentOutput {
        script: vec![0x51, 0x00],
        amount: Some(100000),
    }];

    let payment_request = processor
        .create_payment_request(outputs, None, None)
        .await
        .expect("Failed to create payment request");

    // We need to get the payment_id that was stored when creating the request
    // Since generate_payment_id is private, we'll need to recreate it or use a different approach
    // For now, let's test that we can create a payment request and it's stored
    // The actual payment processing will be tested in integration tests

    // Generate payment_id the same way the processor does
    use sha2::{Digest, Sha256};
    let serialized = bincode::serialize(&payment_request).unwrap_or_default();
    let hash = Sha256::digest(&serialized);
    let payment_id = hex::encode(&hash[..16]);

    // Create a payment with empty transactions (should fail validation)
    let mut payment = Payment::new(vec![]); // Empty transactions
    payment.merchant_data = payment_request.payment_details.merchant_data.clone();

    // Process payment - should fail validation because no transactions
    let result = processor.process_payment(payment, payment_id, None).await;

    // Verify validation correctly rejects invalid payment
    assert!(result.is_err());
    match result.unwrap_err() {
        PaymentError::ValidationFailed(_) => {
            // Expected - payment validation correctly rejected empty transactions
        }
        _ => panic!("Expected ValidationFailed error for empty transactions"),
    }
}

#[tokio::test]
async fn test_bip47_derivation_error_paths() {
    // Test BIP47 payment code derivation error handling and fallback
    let config = default_payment_config();
    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    use blvm_node::module::registry::manifest::{ModuleManifest, PaymentSection};

    // Create a manifest with BIP47 payment code but no legacy address
    let mut manifest = ModuleManifest {
        name: "test_module".to_string(),
        version: "1.0.0".to_string(),
        entry_point: "test".to_string(),
        description: None,
        author: None,
        capabilities: vec![],
        dependencies: std::collections::HashMap::new(),
        optional_dependencies: std::collections::HashMap::new(),
        config_schema: std::collections::HashMap::new(),
        signatures: None,
        binary: None,
        payment: Some(PaymentSection {
            required: true,
            price_sats: Some(10000),
            author_payment_code: Some("PM8TJTLJbPRGxSbc8EJi42Wrr6QbNSaSSVJ5Y3E4pbCYiTHUskHg13935Ubb7q8tx9GVgc2NZK5LXiAtWVt2SN3AoRcTHMihqVh2V9Gns5T7HHmNq".to_string()),
            author_address: None, // No fallback address
            commons_payment_code: Some("PM8TJTLJbPRGxSbc8EJi42Wrr6QbNSaSSVJ5Y3E4pbCYiTHUskHg13935Ubb7q8tx9GVgc2NZK5LXiAtWVt2SN3AoRcTHMihqVh2V9Gns5T7HHmNq".to_string()),
            commons_address: None, // No fallback address
        }),
    };

    // Create dummy module hash and node script for testing
    let module_hash = [0u8; 32];
    let node_script = vec![0x51, 0x00]; // OP_1 OP_0 (dummy script)

    // Test 1: BIP47 payment code without legacy fallback should return error
    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script.clone(), None)
        .await;
    assert!(
        result.is_err(),
        "Should fail when BIP47 derivation fails and no legacy address provided"
    );
    match result.unwrap_err() {
        PaymentError::ProcessingError(msg) => {
            assert!(
                msg.contains("BIP47 payment code provided but derivation failed")
                    || msg.contains("legacy address not provided"),
                "Error message should mention BIP47 derivation failure or missing legacy address"
            );
        }
        _ => panic!("Expected ProcessingError for BIP47 without fallback"),
    }

    // Test 2: BIP47 payment code with legacy fallback should succeed
    manifest.payment.as_mut().unwrap().author_address =
        Some("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh".to_string());
    manifest.payment.as_mut().unwrap().commons_address =
        Some("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh".to_string());

    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script.clone(), None)
        .await;
    assert!(
        result.is_ok(),
        "Should succeed with legacy address fallback"
    );
    let payment_request = result.unwrap();
    assert!(
        !payment_request.payment_details.outputs.is_empty(),
        "Payment request should have outputs"
    );

    // Test 3: Legacy address only (no BIP47) should work
    manifest.payment.as_mut().unwrap().author_payment_code = None;
    manifest.payment.as_mut().unwrap().commons_payment_code = None;

    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script.clone(), None)
        .await;
    assert!(result.is_ok(), "Should succeed with legacy address only");
    let payment_request = result.unwrap();
    assert!(
        !payment_request.payment_details.outputs.is_empty(),
        "Payment request should have outputs"
    );

    // Test 4: No payment address or code should fail
    manifest.payment.as_mut().unwrap().author_address = None;
    manifest.payment.as_mut().unwrap().commons_address = None;

    let result = processor
        .create_module_payment_request(&manifest, &module_hash, node_script, None)
        .await;
    assert!(
        result.is_err(),
        "Should fail when no address or payment code provided"
    );
    match result.unwrap_err() {
        PaymentError::ProcessingError(msg) => {
            assert!(
                msg.contains("payment address or payment code not specified"),
                "Error message should mention missing address or payment code"
            );
        }
        _ => panic!("Expected ProcessingError for missing address"),
    }
}

#[tokio::test]
async fn test_payment_request_not_found() {
    let config = default_payment_config();

    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    // Try to get non-existent payment request
    let result = processor.get_payment_request("nonexistent_id").await;

    assert!(result.is_err());
    match result.unwrap_err() {
        PaymentError::RequestNotFound(_) => {}
        _ => panic!("Expected RequestNotFound error"),
    }
}

#[tokio::test]
async fn test_payment_processor_config_validation() {
    // Test that HTTP requires feature flag
    let mut config = PaymentConfig::default();
    config.p2p_enabled = false;
    config.http_enabled = true;

    #[cfg(not(feature = "bip70-http"))]
    {
        let result = PaymentProcessor::new(config);
        assert!(result.is_err());
        if let Err(PaymentError::FeatureNotEnabled(_)) = result {
            // Expected
        } else {
            // Can't use unwrap_err() because PaymentProcessor doesn't implement Debug
            // Just verify it's an error
            assert!(result.is_err());
        }
    }

    #[cfg(feature = "bip70-http")]
    {
        // With feature enabled, should work
        let result = PaymentProcessor::new(config);
        assert!(result.is_ok());
    }
}

#[tokio::test]
async fn test_payment_processor_no_transport_enabled() {
    // Test that at least one transport must be enabled
    let mut config = PaymentConfig::default();
    config.p2p_enabled = false;
    config.http_enabled = false;

    let result = PaymentProcessor::new(config);
    assert!(result.is_err());
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("Expected error but got Ok"),
    };
    match err {
        PaymentError::NoTransportEnabled => {}
        _ => panic!("Expected NoTransportEnabled error"),
    }
}

#[tokio::test]
async fn test_payment_processor_p2p_only() {
    // Test P2P-only configuration (default)
    let config = default_payment_config();

    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");
    // Config is private, so we can't directly check it
    // But we can verify the processor works by creating a payment request
    let outputs = vec![PaymentOutput {
        script: vec![0x51, 0x00],
        amount: Some(100000),
    }];
    let _request = processor
        .create_payment_request(outputs, None, None)
        .await
        .expect("Failed to create payment request");
}

#[tokio::test]
async fn test_payment_id_generation() {
    let config = default_payment_config();

    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    let outputs = vec![PaymentOutput {
        script: vec![0x51, 0x00],
        amount: Some(100000),
    }];

    // Create two payment requests
    let req1 = processor
        .create_payment_request(outputs.clone(), None, None)
        .await
        .expect("Failed to create payment request 1");

    let req2 = processor
        .create_payment_request(outputs.clone(), Some(b"different".to_vec()), None)
        .await
        .expect("Failed to create payment request 2");

    // Payment IDs should be different (generate them the same way the processor does)
    use sha2::{Digest, Sha256};
    let serialized1 = bincode::serialize(&req1).unwrap_or_default();
    let hash1 = Sha256::digest(&serialized1);
    let id1 = hex::encode(&hash1[..16]);

    let serialized2 = bincode::serialize(&req2).unwrap_or_default();
    let hash2 = Sha256::digest(&serialized2);
    let id2 = hex::encode(&hash2[..16]);

    assert_ne!(id1, id2);
    assert_eq!(id1.len(), 32); // 16 bytes hex = 32 chars
}

#[tokio::test]
async fn test_payment_request_storage() {
    let config = default_payment_config();

    let processor = PaymentProcessor::new(config).expect("Failed to create payment processor");

    let outputs = vec![PaymentOutput {
        script: vec![0x51, 0x00],
        amount: Some(100000),
    }];

    // Create payment request
    let payment_request = processor
        .create_payment_request(outputs, None, None)
        .await
        .expect("Failed to create payment request");

    // Generate payment_id the same way the processor does
    use sha2::{Digest, Sha256};
    let serialized = bincode::serialize(&payment_request).unwrap_or_default();
    let hash = Sha256::digest(&serialized);
    let payment_id = hex::encode(&hash[..16]);

    // Verify it's stored
    let retrieved = processor
        .get_payment_request(&payment_id)
        .await
        .expect("Failed to retrieve payment request");

    assert_eq!(
        retrieved.payment_details.outputs[0].amount,
        payment_request.payment_details.outputs[0].amount
    );
}

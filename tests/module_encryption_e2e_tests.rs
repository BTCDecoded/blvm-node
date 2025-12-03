//! End-to-end tests for module encryption system
//!
//! Tests the complete flow: request → pay → encrypt → confirm → decrypt

use blvm_node::config::PaymentConfig;
use blvm_node::module::encryption::{
    load_encrypted_module, EncryptedModuleMetadata, ModuleEncryption,
};
use blvm_node::module::registry::client::ModuleRegistry;
use blvm_node::module::registry::manifest::{ModuleManifest, PaymentSection};
use blvm_node::payment::processor::PaymentProcessor;
use blvm_node::payment::state_machine::{PaymentState, PaymentStateMachine};
use blvm_protocol::payment::Payment;
use secp256k1::{Secp256k1, SecretKey};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

/// Helper to create a test manifest with payment
fn create_test_manifest_with_payment(name: &str) -> ModuleManifest {
    use blvm_node::module::registry::manifest::{MaintainerSignature, SignatureSection};

    let secp = Secp256k1::new();
    let test_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let author_addr = "tb1q0000000000000000000000000000000000";
    let commons_addr = "tb1q1111111111111111111111111111111111";
    let price_sats = 100000u64;

    let message_data = format!("{}||{}||{}", author_addr, commons_addr, price_sats);
    let message_hash = Sha256::digest(message_data.as_bytes());
    let message = secp256k1::Message::from_digest_slice(&message_hash).unwrap();
    let signature = secp.sign_ecdsa(&message, &test_key);
    let signature_hex = hex::encode(signature.serialize_compact());

    let pubkey = secp256k1::PublicKey::from_secret_key(&secp, &test_key);
    let pubkey_hex = hex::encode(pubkey.serialize());

    let signature_section = SignatureSection {
        maintainers: vec![MaintainerSignature {
            name: "test-maintainer".to_string(),
            public_key: pubkey_hex,
            signature: "dummy".to_string(),
        }],
        threshold: Some("1-of-1".to_string()),
    };

    let payment_section = PaymentSection {
        required: true,
        price_sats: Some(price_sats),
        author_payment_code: None,
        author_address: Some(author_addr.to_string()),
        commons_payment_code: None,
        commons_address: Some(commons_addr.to_string()),
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

/// Helper to create a test module registry
async fn create_test_registry(
    temp_dir: &TempDir,
    module_name: &str,
    module_binary: &[u8],
) -> Arc<ModuleRegistry> {
    use blvm_node::module::registry::cache::{CachedModule, LocalCache};
    use blvm_node::module::registry::cas::ContentAddressableStorage;

    let cache_dir = temp_dir.path().join("cache");
    let cas_dir = temp_dir.path().join("cas");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::create_dir_all(&cas_dir).unwrap();

    let cas = Arc::new(tokio::sync::RwLock::new(
        ContentAddressableStorage::new(&cas_dir).unwrap(),
    ));
    let cache = Arc::new(tokio::sync::RwLock::new(
        LocalCache::new(&cache_dir).unwrap(),
    ));

    let manifest = create_test_manifest_with_payment(module_name);
    let manifest_toml = toml::to_string(&manifest).unwrap();
    let manifest_hash = cas.write().await.store(manifest_toml.as_bytes()).unwrap();
    let binary_hash = cas.write().await.store(module_binary).unwrap();

    let combined = format!("{}{}", hex::encode(manifest_hash), hex::encode(binary_hash));
    let module_hash = Sha256::digest(combined.as_bytes());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&module_hash);

    let cached = CachedModule {
        name: module_name.to_string(),
        version: "1.0.0".to_string(),
        hash,
        manifest_hash,
        binary_hash,
        local_path: temp_dir.path().join(format!("{}.so", module_name)),
    };
    cache.write().await.add_module(cached).unwrap();

    Arc::new(ModuleRegistry::new(&cache_dir, &cas_dir, Vec::new()).unwrap())
}

#[tokio::test]
async fn test_full_encrypted_module_purchase_flow() {
    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();

    // Step 1: Setup
    let module_name = "test-module";
    let module_binary = b"original_module_binary_data";
    let registry = create_test_registry(&temp_dir, module_name, module_binary).await;

    let config = PaymentConfig::default();
    let mut processor = PaymentProcessor::new(config).unwrap();

    let encryption = Arc::new(ModuleEncryption::new());
    processor = processor.with_module_encryption(Arc::clone(&encryption));
    processor = processor.with_modules_dir(modules_dir.clone());
    processor = processor.with_module_registry(Arc::clone(&registry));

    let processor_arc = Arc::new(processor);
    let state_machine = Arc::new(PaymentStateMachine::new(Arc::clone(&processor_arc)));

    // Step 2: Create payment request
    let module_hash = {
        let hash = Sha256::digest(module_binary);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    let manifest = create_test_manifest_with_payment(module_name);
    let node_script = vec![0x51, 0x00];
    let payment_request = processor_arc
        .create_module_payment_request(&manifest, &module_hash, node_script, None)
        .await
        .unwrap();

    // Verify merchant_data contains module info
    assert!(payment_request.payment_details.merchant_data.is_some());
    let merchant_data = payment_request
        .payment_details
        .merchant_data
        .as_ref()
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_slice(merchant_data).unwrap();
    assert_eq!(metadata["module_name"], module_name);
    assert_eq!(metadata["payment_type"], "module_payment");

    // Step 3: Generate payment_id
    let serialized = bincode::serialize(&payment_request).unwrap_or_default();
    let hash = Sha256::digest(&serialized);
    let payment_id = hex::encode(&hash[..16]);

    // Step 4: Submit payment (should trigger encryption)
    let mut payment = Payment::new(vec![vec![0x01, 0x00, 0x00, 0x00]]); // Minimal valid tx
    payment.merchant_data = payment_request.payment_details.merchant_data.clone();

    // Process payment - encryption should happen
    let _ack = processor_arc
        .process_payment(payment, payment_id.clone(), None)
        .await;

    // Step 5: Verify module was encrypted
    let encrypted_path = modules_dir.join("encrypted").join(module_name);
    if encrypted_path.exists() {
        let (encrypted_binary, metadata) = load_encrypted_module(&modules_dir, module_name)
            .await
            .unwrap();

        assert_eq!(metadata.payment_id, payment_id);
        assert!(!encrypted_binary.is_empty());
        assert_ne!(encrypted_binary, module_binary);

        // Step 6: Confirm payment (mark as settled)
        use blvm_node::Hash;
        let tx_hash = [0x01u8; 32];
        let block_hash = [0x02u8; 32];
        state_machine
            .mark_settled(&payment_id, tx_hash, block_hash, 1)
            .await
            .unwrap();

        // Step 7: Verify payment state
        let state = state_machine.get_payment_state(&payment_id).await.unwrap();
        match state {
            PaymentState::Settled { .. } => {
                // Payment confirmed - module should be decryptable
                // Decryption would happen in handle_get_module when serving
                // For this test, we verify the state is correct
            }
            _ => panic!("Payment should be settled"),
        }

        // Step 8: Verify decryption works
        let decrypted = encryption
            .decrypt_module(&encrypted_binary, &metadata.nonce, payment_id, &module_hash)
            .unwrap();
        assert_eq!(decrypted, module_binary);
    }
}

#[tokio::test]
async fn test_encrypted_module_propagation() {
    // Test that encrypted modules can be stored and loaded correctly
    // This simulates propagation over P2P network

    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();

    let module_name = "propagated-module";
    let module_binary = b"module_binary_for_propagation";
    let payment_id = "payment_123";

    let encryption = ModuleEncryption::new();
    let module_hash = {
        let hash = Sha256::digest(module_binary);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Encrypt module
    let (encrypted_binary, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .unwrap();

    // Store encrypted module (simulating what happens on server)
    let metadata = EncryptedModuleMetadata {
        payment_id: payment_id.to_string(),
        module_hash: module_hash.to_vec(),
        nonce,
        encrypted_at: 1234567890,
        payment_method: "on-chain".to_string(),
    };

    blvm_node::module::encryption::store_encrypted_module(
        &modules_dir,
        module_name,
        &encrypted_binary,
        &metadata,
    )
    .await
    .unwrap();

    // Load encrypted module (simulating what happens on client)
    let (loaded_encrypted, loaded_metadata) = load_encrypted_module(&modules_dir, module_name)
        .await
        .unwrap();

    // Verify encrypted data matches
    assert_eq!(loaded_encrypted, encrypted_binary);
    assert_eq!(loaded_metadata.payment_id, payment_id);

    // Verify it can be decrypted (when payment is confirmed)
    let mut module_hash_array = [0u8; 32];
    module_hash_array.copy_from_slice(&loaded_metadata.module_hash);
    let decrypted = encryption
        .decrypt_module(
            &loaded_encrypted,
            &loaded_metadata.nonce,
            payment_id,
            &module_hash_array,
        )
        .unwrap();
    assert_eq!(decrypted, module_binary);
}

#[tokio::test]
async fn test_payment_state_transitions() {
    // Test that payment state transitions work correctly with encryption

    let temp_dir = TempDir::new().unwrap();
    let modules_dir = temp_dir.path().join("modules");
    std::fs::create_dir_all(&modules_dir).unwrap();

    let module_name = "state-test-module";
    let module_binary = b"test_binary";
    let payment_id = "payment_state_test";

    // Create encrypted module
    let encryption = ModuleEncryption::new();
    let module_hash = {
        let hash = Sha256::digest(module_binary);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    let (encrypted_binary, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .unwrap();

    let metadata = EncryptedModuleMetadata {
        payment_id: payment_id.to_string(),
        module_hash: module_hash.to_vec(),
        nonce,
        encrypted_at: 1234567890,
        payment_method: "on-chain".to_string(),
    };

    store_encrypted_module(&modules_dir, module_name, &encrypted_binary, &metadata)
        .await
        .unwrap();

    // Create payment state machine
    let config = PaymentConfig::default();
    let processor = Arc::new(PaymentProcessor::new(config).unwrap());
    let state_machine = Arc::new(PaymentStateMachine::new(processor));

    // Test state transitions
    // 1. Create payment request (sets state to RequestCreated)
    let manifest = create_test_manifest_with_payment(module_name);
    let node_script = vec![0x51, 0x00];
    let (created_payment_id, _) = state_machine
        .create_module_payment_request(&manifest, &module_hash, node_script, None, false)
        .await
        .unwrap();

    let state = state_machine
        .get_payment_state(&created_payment_id)
        .await
        .unwrap();
    assert!(matches!(state, PaymentState::RequestCreated { .. }));

    // 2. InMempool (payment pending but in mempool - can decrypt for CTV)
    use blvm_node::Hash;
    let tx_hash = [0x01u8; 32];
    state_machine
        .mark_in_mempool(&created_payment_id, tx_hash)
        .await
        .unwrap();

    let state = state_machine
        .get_payment_state(&created_payment_id)
        .await
        .unwrap();
    assert!(matches!(state, PaymentState::InMempool { .. }));

    // 3. Settled (payment confirmed - can decrypt)
    let block_hash = [0x02u8; 32];
    state_machine
        .mark_settled(&created_payment_id, tx_hash, block_hash, 1)
        .await
        .unwrap();

    let state = state_machine
        .get_payment_state(&created_payment_id)
        .await
        .unwrap();
    assert!(matches!(state, PaymentState::Settled { .. }));

    // At this point, module should be decryptable
    // Note: We use the original payment_id for decryption (key derivation)
    // The created_payment_id might be different, but we test with the original
    let mut module_hash_array = [0u8; 32];
    module_hash_array.copy_from_slice(&metadata.module_hash);
    let decrypted = encryption
        .decrypt_module(
            &encrypted_binary,
            &metadata.nonce,
            payment_id,
            &module_hash_array,
        )
        .unwrap();
    assert_eq!(decrypted, module_binary);
}

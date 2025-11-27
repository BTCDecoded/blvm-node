//! Tests for module encryption and decryption
//!
//! Tests the encryption/decryption functionality for paid modules,
//! including key derivation, encryption/decryption round-trips, and storage.

use bllvm_node::module::encryption::{
    load_encrypted_module, store_decrypted_module, store_encrypted_module, EncryptedModuleMetadata,
    ModuleEncryption,
};
use bllvm_node::module::traits::ModuleError;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tempfile::TempDir;

/// Helper to create a test module hash
fn create_test_module_hash() -> [u8; 32] {
    let data = b"test_module_binary_data";
    let hash = Sha256::digest(data);
    let mut module_hash = [0u8; 32];
    module_hash.copy_from_slice(&hash);
    module_hash
}

#[test]
fn test_encrypt_decrypt_round_trip() {
    let encryption = ModuleEncryption::new();
    let module_binary = b"test_module_binary_data";
    let payment_id = "test_payment_123";
    let module_hash = create_test_module_hash();

    // Encrypt
    let (encrypted, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    // Verify encrypted data is different from original
    assert_ne!(encrypted, module_binary);
    assert!(!encrypted.is_empty());
    assert_eq!(nonce.len(), 12); // GCM nonce is 12 bytes

    // Decrypt
    let decrypted = encryption
        .decrypt_module(&encrypted, &nonce, payment_id, &module_hash)
        .expect("Decryption should succeed");

    // Verify decrypted matches original
    assert_eq!(decrypted, module_binary);
}

#[test]
fn test_encrypt_decrypt_different_payment_ids() {
    let encryption = ModuleEncryption::new();
    let module_binary = b"test_module_binary_data";
    let module_hash = create_test_module_hash();

    // Encrypt with payment_id_1
    let (encrypted1, nonce1) = encryption
        .encrypt_module(module_binary, "payment_1", &module_hash)
        .expect("Encryption should succeed");

    // Encrypt with payment_id_2
    let (encrypted2, nonce2) = encryption
        .encrypt_module(module_binary, "payment_2", &module_hash)
        .expect("Encryption should succeed");

    // Encrypted data should be different (different keys)
    assert_ne!(encrypted1, encrypted2);

    // Each should decrypt with its own payment_id
    let decrypted1 = encryption
        .decrypt_module(&encrypted1, &nonce1, "payment_1", &module_hash)
        .expect("Decryption should succeed");
    assert_eq!(decrypted1, module_binary);

    let decrypted2 = encryption
        .decrypt_module(&encrypted2, &nonce2, "payment_2", &module_hash)
        .expect("Decryption should succeed");
    assert_eq!(decrypted2, module_binary);

    // Cross-decryption should fail (wrong payment_id)
    let result = encryption.decrypt_module(&encrypted1, &nonce1, "payment_2", &module_hash);
    assert!(result.is_err());
}

#[test]
fn test_encrypt_decrypt_different_module_hashes() {
    let encryption = ModuleEncryption::new();
    let module_binary = b"test_module_binary_data";
    let payment_id = "test_payment";

    let hash1 = create_test_module_hash();
    let hash2 = {
        let data = b"different_module_data";
        let hash = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&hash);
        h
    };

    // Encrypt with hash1
    let (encrypted1, nonce1) = encryption
        .encrypt_module(module_binary, payment_id, &hash1)
        .expect("Encryption should succeed");

    // Encrypt with hash2
    let (encrypted2, nonce2) = encryption
        .encrypt_module(module_binary, payment_id, &hash2)
        .expect("Encryption should succeed");

    // Encrypted data should be different (different keys)
    assert_ne!(encrypted1, encrypted2);

    // Each should decrypt with its own hash
    let decrypted1 = encryption
        .decrypt_module(&encrypted1, &nonce1, payment_id, &hash1)
        .expect("Decryption should succeed");
    assert_eq!(decrypted1, module_binary);

    let decrypted2 = encryption
        .decrypt_module(&encrypted2, &nonce2, payment_id, &hash2)
        .expect("Decryption should succeed");
    assert_eq!(decrypted2, module_binary);

    // Cross-decryption should fail (wrong hash)
    let result = encryption.decrypt_module(&encrypted1, &nonce1, payment_id, &hash2);
    assert!(result.is_err());
}

#[test]
fn test_decrypt_with_wrong_nonce() {
    let encryption = ModuleEncryption::new();
    let module_binary = b"test_module_binary_data";
    let payment_id = "test_payment";
    let module_hash = create_test_module_hash();

    // Encrypt
    let (encrypted, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    // Try to decrypt with wrong nonce
    let wrong_nonce = vec![0u8; 12];
    let result = encryption.decrypt_module(&encrypted, &wrong_nonce, payment_id, &module_hash);

    assert!(result.is_err());
    match result.unwrap_err() {
        ModuleError::CryptoError(_) => {
            // Expected - decryption should fail with wrong nonce
        }
        _ => panic!("Expected CryptoError for wrong nonce"),
    }
}

#[test]
fn test_decrypt_with_tampered_data() {
    let encryption = ModuleEncryption::new();
    let module_binary = b"test_module_binary_data";
    let payment_id = "test_payment";
    let module_hash = create_test_module_hash();

    // Encrypt
    let (mut encrypted, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    // Tamper with encrypted data
    if !encrypted.is_empty() {
        encrypted[0] ^= 0xFF; // Flip bits
    }

    // Decryption should fail (GCM authentication will fail)
    let result = encryption.decrypt_module(&encrypted, &nonce, payment_id, &module_hash);
    assert!(result.is_err());
}

#[tokio::test]
async fn test_store_and_load_encrypted_module() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let modules_dir = temp_dir.path();
    let module_name = "test-module";
    let payment_id = "test_payment_123";
    let module_hash = create_test_module_hash();

    // Create test binary data
    let module_binary = b"test_module_binary_data";
    let encryption = ModuleEncryption::new();

    // Encrypt
    let (encrypted_binary, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    // Create metadata
    let metadata = EncryptedModuleMetadata {
        payment_id: payment_id.to_string(),
        module_hash: module_hash.to_vec(),
        nonce,
        encrypted_at: 1234567890,
        payment_method: "on-chain".to_string(),
    };

    // Store encrypted module
    store_encrypted_module(modules_dir, module_name, &encrypted_binary, &metadata)
        .await
        .expect("Failed to store encrypted module");

    // Load encrypted module
    let (loaded_encrypted, loaded_metadata) = load_encrypted_module(modules_dir, module_name)
        .await
        .expect("Failed to load encrypted module");

    // Verify loaded data matches
    assert_eq!(loaded_encrypted, encrypted_binary);
    assert_eq!(loaded_metadata.payment_id, payment_id);
    assert_eq!(loaded_metadata.module_hash, module_hash.to_vec());
    assert_eq!(loaded_metadata.payment_method, "on-chain");
}

#[tokio::test]
async fn test_store_decrypted_module() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let modules_dir = temp_dir.path();
    let module_name = "test-module";

    let decrypted_binary = b"decrypted_module_binary";
    let manifest = b"name = \"test-module\"\nversion = \"1.0.0\"";

    // Store decrypted module
    store_decrypted_module(modules_dir, module_name, decrypted_binary, manifest)
        .await
        .expect("Failed to store decrypted module");

    // Verify files were created
    let module_dir = modules_dir.join("decrypted").join(module_name);
    assert!(module_dir.exists());

    let binary_path = module_dir.join(module_name);
    let manifest_path = module_dir.join("module.toml");

    assert!(binary_path.exists());
    assert!(manifest_path.exists());

    // Verify file contents
    let loaded_binary = std::fs::read(&binary_path).expect("Failed to read binary");
    assert_eq!(loaded_binary, decrypted_binary);

    let loaded_manifest = std::fs::read_to_string(&manifest_path).expect("Failed to read manifest");
    assert_eq!(loaded_manifest.as_bytes(), manifest);
}

#[test]
fn test_key_derivation_consistency() {
    let encryption = ModuleEncryption::new();
    let payment_id = "test_payment";
    let module_hash = create_test_module_hash();

    // Derive key multiple times - should be consistent
    // (We can't directly test derive_key since it's private, but we can test via encrypt/decrypt)
    let module_binary = b"test_data";

    // Encrypt twice with same parameters
    let (encrypted1, nonce1) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    let (encrypted2, nonce2) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    // Encrypted data will be different (random nonce), but both should decrypt correctly
    assert_ne!(encrypted1, encrypted2); // Different nonces = different ciphertext
    assert_ne!(nonce1, nonce2); // Nonces are random

    // Both should decrypt to the same plaintext
    let decrypted1 = encryption
        .decrypt_module(&encrypted1, &nonce1, payment_id, &module_hash)
        .expect("Decryption should succeed");

    let decrypted2 = encryption
        .decrypt_module(&encrypted2, &nonce2, payment_id, &module_hash)
        .expect("Decryption should succeed");

    assert_eq!(decrypted1, module_binary);
    assert_eq!(decrypted2, module_binary);
}

#[test]
fn test_encrypt_empty_binary() {
    let encryption = ModuleEncryption::new();
    let module_binary = b"";
    let payment_id = "test_payment";
    let module_hash = create_test_module_hash();

    // Should handle empty binary
    let (encrypted, nonce) = encryption
        .encrypt_module(module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    let decrypted = encryption
        .decrypt_module(&encrypted, &nonce, payment_id, &module_hash)
        .expect("Decryption should succeed");

    assert_eq!(decrypted, module_binary);
    assert!(decrypted.is_empty());
}

#[test]
fn test_encrypt_large_binary() {
    let encryption = ModuleEncryption::new();
    // Create a larger binary (1MB)
    let module_binary = vec![0x42u8; 1_000_000];
    let payment_id = "test_payment";
    let module_hash = create_test_module_hash();

    // Should handle large binary
    let (encrypted, nonce) = encryption
        .encrypt_module(&module_binary, payment_id, &module_hash)
        .expect("Encryption should succeed");

    let decrypted = encryption
        .decrypt_module(&encrypted, &nonce, payment_id, &module_hash)
        .expect("Decryption should succeed");

    assert_eq!(decrypted, module_binary);
    assert_eq!(decrypted.len(), 1_000_000);
}

//! Tests for Ban List Signing

use bllvm_node::network::ban_list_signing::{
    sign_ban_list, verify_ban_list_signature, SignedBanListMessage,
};
use bllvm_node::network::protocol::BanListMessage;
use secp256k1::{PublicKey, Secp256k1, SecretKey};

fn create_test_ban_list() -> BanListMessage {
    BanListMessage {
        is_full: true,
        ban_list_hash: [0u8; 32],
        ban_entries: vec![],
        timestamp: 0,
    }
}

#[test]
fn test_sign_ban_list() {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let ban_list = create_test_ban_list();

    let result = sign_ban_list(&ban_list, &secret_key);
    assert!(result.is_ok());

    let signature = result.unwrap();
    assert_eq!(signature.len(), 64); // secp256k1 signature is 64 bytes
}

#[test]
fn test_verify_ban_list_signature_valid() {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let ban_list = create_test_ban_list();

    // Sign
    let signature = sign_ban_list(&ban_list, &secret_key).unwrap();

    // Verify
    let result = verify_ban_list_signature(&ban_list, &signature, &public_key);
    assert!(result.is_ok());
    assert!(result.unwrap());
}

#[test]
fn test_verify_ban_list_signature_invalid() {
    let secp = Secp256k1::new();
    let secret_key1 = SecretKey::from_slice(&[1; 32]).unwrap();
    let secret_key2 = SecretKey::from_slice(&[2; 32]).unwrap();
    let public_key2 = PublicKey::from_secret_key(&secp, &secret_key2);
    let ban_list = create_test_ban_list();

    // Sign with key1
    let signature = sign_ban_list(&ban_list, &secret_key1).unwrap();

    // Try to verify with key2 (should fail)
    let result = verify_ban_list_signature(&ban_list, &signature, &public_key2);
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[test]
fn test_verify_ban_list_signature_wrong_length() {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let ban_list = create_test_ban_list();

    // Invalid signature length
    let invalid_signature = vec![0u8; 32]; // Too short

    let result = verify_ban_list_signature(&ban_list, &invalid_signature, &public_key);
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[test]
fn test_verify_ban_list_signature_modified_data() {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);
    let ban_list = create_test_ban_list();

    // Sign
    let signature = sign_ban_list(&ban_list, &secret_key).unwrap();

    // Modify ban list
    let mut modified_ban_list = ban_list.clone();
    modified_ban_list.ban_list_hash = [1u8; 32];

    // Verify with modified data (should fail)
    let result = verify_ban_list_signature(&modified_ban_list, &signature, &public_key);
    assert!(result.is_ok());
    assert!(!result.unwrap());
}

#[test]
fn test_signed_ban_list_message_new() {
    let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let ban_list = create_test_ban_list();

    let result = SignedBanListMessage::new(ban_list.clone(), &secret_key);
    assert!(result.is_ok());

    let signed = result.unwrap();
    assert_eq!(signed.ban_list.is_full, ban_list.is_full);
    assert_eq!(signed.signature.len(), 64);
}

#[test]
fn test_signed_ban_list_message_verify() {
    let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
    let ban_list = create_test_ban_list();

    let signed = SignedBanListMessage::new(ban_list, &secret_key).unwrap();

    // Verify
    let result = signed.verify();
    assert!(result.is_ok());
    assert!(result.unwrap());
}

#[test]
fn test_signed_ban_list_message_verify_invalid() {
    let secret_key1 = SecretKey::from_slice(&[1; 32]).unwrap();
    let secret_key2 = SecretKey::from_slice(&[2; 32]).unwrap();
    let ban_list = create_test_ban_list();

    // Create signed message with key1
    let mut signed = SignedBanListMessage::new(ban_list, &secret_key1).unwrap();

    // Modify signature to make it invalid
    signed.signature = vec![0u8; 64];

    // Verify should fail
    let result = signed.verify();
    assert!(result.is_ok());
    assert!(!result.unwrap());
}


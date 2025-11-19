//! Integration tests for User Signaling with Cryptographic Signing
//!
//! Tests the complete flow of user signaling including:
//! - Signal creation and signing
//! - Cross-node signal verification
//! - Weighted vote calculation with verified signals
//! - Tampering detection

use bllvm_node::governance::user_signaling::{UserSignalingManager, UserSignal, SignalType};
use bllvm_sdk::governance::GovernanceKeypair;
use std::collections::HashMap;

#[test]
fn test_user_signaling_end_to_end() {
    // Create two nodes with different keypairs
    let keypair1 = GovernanceKeypair::generate().unwrap();
    let keypair2 = GovernanceKeypair::generate().unwrap();
    
    let public_key1 = keypair1.public_key_bytes().to_vec();
    let private_key1 = keypair1.secret_key_bytes().to_vec();
    let public_key2 = keypair2.public_key_bytes().to_vec();
    let private_key2 = keypair2.secret_key_bytes().to_vec();
    
    let mut manager1 = UserSignalingManager::new(public_key1.clone(), private_key1);
    let mut manager2 = UserSignalingManager::new(public_key2.clone(), private_key2);
    
    // Node 1 creates a support signal
    let signal1 = manager1.create_signal("PR-123", SignalType::Support).unwrap();
    assert_eq!(signal1.change_id, "PR-123");
    assert_eq!(signal1.signal_type, SignalType::Support);
    assert!(!signal1.signature.is_empty());
    
    // Node 2 creates an oppose signal
    let signal2 = manager2.create_signal("PR-123", SignalType::Oppose).unwrap();
    assert_eq!(signal2.change_id, "PR-123");
    assert_eq!(signal2.signal_type, SignalType::Oppose);
    assert!(!signal2.signature.is_empty());
    
    // Node 1 verifies Node 2's signal
    let verified = manager1.verify_signal(&signal2, &public_key2);
    assert!(verified, "Node 2's signal should verify correctly");
    
    // Node 2 verifies Node 1's signal
    let verified = manager2.verify_signal(&signal1, &public_key1);
    assert!(verified, "Node 1's signal should verify correctly");
    
    // Test weighted vote calculation
    let signals = vec![signal1.clone(), signal2.clone()];
    let mut utxo_counts = HashMap::new();
    utxo_counts.insert(manager1.node_id(), 100); // sqrt(100) = 10
    utxo_counts.insert(manager2.node_id(), 400); // sqrt(400) = 20
    
    let (support_weight, oppose_weight) = 
        UserSignalingManager::calculate_weighted_vote(&signals, &utxo_counts);
    
    assert_eq!(support_weight, 10.0);
    assert_eq!(oppose_weight, 20.0);
}

#[test]
fn test_cross_node_signal_verification() {
    // Create three nodes
    let keypair1 = GovernanceKeypair::generate().unwrap();
    let keypair2 = GovernanceKeypair::generate().unwrap();
    let keypair3 = GovernanceKeypair::generate().unwrap();
    
    let public_key1 = keypair1.public_key_bytes().to_vec();
    let private_key1 = keypair1.secret_key_bytes().to_vec();
    let public_key2 = keypair2.public_key_bytes().to_vec();
    let private_key2 = keypair2.secret_key_bytes().to_vec();
    let public_key3 = keypair3.public_key_bytes().to_vec();
    let private_key3 = keypair3.secret_key_bytes().to_vec();
    
    let mut manager1 = UserSignalingManager::new(public_key1.clone(), private_key1);
    let mut manager2 = UserSignalingManager::new(public_key2.clone(), private_key2);
    let manager3 = UserSignalingManager::new(public_key3.clone(), private_key3);
    
    // All nodes create support signals
    let signal1 = manager1.create_signal("PR-456", SignalType::Support).unwrap();
    let signal2 = manager2.create_signal("PR-456", SignalType::Support).unwrap();
    let signal3 = manager3.create_signal("PR-456", SignalType::Support).unwrap();
    
    // Node 1 verifies all signals
    assert!(manager1.verify_signal(&signal1, &public_key1), "Own signal should verify");
    assert!(manager1.verify_signal(&signal2, &public_key2), "Node 2's signal should verify");
    assert!(manager1.verify_signal(&signal3, &public_key3), "Node 3's signal should verify");
    
    // Test override threshold
    let signals = vec![signal1, signal2, signal3];
    let mut utxo_counts = HashMap::new();
    utxo_counts.insert(manager1.node_id(), 100);
    utxo_counts.insert(manager2.node_id(), 100);
    utxo_counts.insert(manager3.node_id(), 100);
    
    let threshold_met = UserSignalingManager::check_override_threshold(
        &signals,
        &utxo_counts,
        3, // total_active_nodes
        0.75, // threshold
    );
    
    // With 3/3 nodes and equal weights, should meet threshold
    assert!(threshold_met);
}

#[test]
fn test_signal_tampering_detection() {
    let keypair1 = GovernanceKeypair::generate().unwrap();
    let keypair2 = GovernanceKeypair::generate().unwrap();
    
    let public_key1 = keypair1.public_key_bytes().to_vec();
    let private_key1 = keypair1.secret_key_bytes().to_vec();
    let public_key2 = keypair2.public_key_bytes().to_vec();
    
    let mut manager1 = UserSignalingManager::new(public_key1.clone(), private_key1);
    let manager2 = UserSignalingManager::new(public_key2.clone(), vec![]);
    
    // Create a signal
    let mut signal = manager1.create_signal("PR-789", SignalType::Support).unwrap();
    
    // Verify it's valid
    assert!(manager2.verify_signal(&signal, &public_key1));
    
    // Tamper with the signal
    signal.change_id = "PR-999".to_string();
    
    // Verification should fail
    assert!(!manager2.verify_signal(&signal, &public_key1), "Tampered signal should fail verification");
    
    // Tamper with signal type
    let mut signal2 = manager1.create_signal("PR-888", SignalType::Support).unwrap();
    assert!(manager2.verify_signal(&signal2, &public_key1));
    
    // Change signal type (this changes the message, so signature won't match)
    signal2.signal_type = SignalType::Oppose;
    assert!(!manager2.verify_signal(&signal2, &public_key1), "Changed signal type should fail verification");
}

#[test]
fn test_signal_with_wrong_public_key() {
    let keypair1 = GovernanceKeypair::generate().unwrap();
    let keypair2 = GovernanceKeypair::generate().unwrap();
    let keypair3 = GovernanceKeypair::generate().unwrap();
    
    let public_key1 = keypair1.public_key_bytes().to_vec();
    let private_key1 = keypair1.secret_key_bytes().to_vec();
    let public_key2 = keypair2.public_key_bytes().to_vec();
    let public_key3 = keypair3.public_key_bytes().to_vec();
    
    let mut manager1 = UserSignalingManager::new(public_key1.clone(), private_key1);
    let manager2 = UserSignalingManager::new(public_key2.clone(), vec![]);
    
    // Create signal with keypair1
    let signal = manager1.create_signal("PR-111", SignalType::Support).unwrap();
    
    // Verify with correct key - should pass
    assert!(manager2.verify_signal(&signal, &public_key1), "Correct key should verify");
    
    // Verify with wrong key - should fail
    assert!(!manager2.verify_signal(&signal, &public_key3), "Wrong key should fail verification");
}

#[test]
fn test_multiple_signals_same_change() {
    let keypair = GovernanceKeypair::generate().unwrap();
    let public_key = keypair.public_key_bytes().to_vec();
    let private_key = keypair.secret_key_bytes().to_vec();
    
    let mut manager = UserSignalingManager::new(public_key, private_key);
    
    // Create multiple signals for the same change (should update)
    let signal1 = manager.create_signal("PR-222", SignalType::Support).unwrap();
    let signal2 = manager.create_signal("PR-222", SignalType::Oppose).unwrap();
    
    // Both should have same node_id but different signal_type and signature
    assert_eq!(signal1.node_id, signal2.node_id);
    assert_ne!(signal1.signal_type, signal2.signal_type);
    assert_ne!(signal1.signature, signal2.signature);
    
    // Latest signal should be stored
    let stored = manager.get_signal("PR-222").unwrap();
    assert_eq!(stored.signal_type, SignalType::Oppose);
}


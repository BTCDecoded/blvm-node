//! User-Operated Node Signaling
//!
//! Allows user-operated nodes to signal support or opposition to governance changes.
//! Implements square-root weighting for Sybil resistance.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// User signaling message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSignal {
    /// Change identifier (PR number, issue number, or change ID)
    pub change_id: String,
    /// Signal type (support, oppose, override)
    pub signal_type: SignalType,
    /// Node identifier (derived from node's public key)
    pub node_id: String,
    /// Timestamp
    pub timestamp: u64,
    /// Signature (node signs the signal)
    pub signature: String,
}

/// Signal type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalType {
    /// Support the change
    Support,
    /// Oppose the change
    Oppose,
    /// Override time lock (requires threshold)
    Override,
}

/// User signaling manager
#[derive(ZeroizeOnDrop)]
pub struct UserSignalingManager {
    /// Node's public key (for identification)
    node_public_key: Vec<u8>,
    /// Node's private key (for signing)
    /// Note: This is zeroized on drop for security
    #[zeroize(on_drop)]
    node_private_key: Vec<u8>,
    /// Known signals (change_id -> signal)
    signals: HashMap<String, UserSignal>,
}

impl UserSignalingManager {
    /// Create a new user signaling manager
    pub fn new(node_public_key: Vec<u8>, node_private_key: Vec<u8>) -> Self {
        Self {
            node_public_key,
            node_private_key,
            signals: HashMap::new(),
        }
    }

    /// Generate node ID from public key
    pub fn node_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(&self.node_public_key);
        hex::encode(hasher.finalize())
    }

    /// Create a signal for a governance change
    pub fn create_signal(
        &mut self,
        change_id: &str,
        signal_type: SignalType,
    ) -> Result<UserSignal, String> {
        let node_id = self.node_id();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("Time error: {}", e))?
            .as_secs();

        // Create message to sign: change_id:signal_type:node_id:timestamp
        let message = format!("{}:{}:{}:{}", change_id, signal_type_str(signal_type), node_id, timestamp);
        
        // Sign message (simplified - in production, use proper secp256k1 signing)
        let signature = self.sign_message(&message)?;

        let signal = UserSignal {
            change_id: change_id.to_string(),
            signal_type,
            node_id,
            timestamp,
            signature,
        };

        // Store signal
        self.signals.insert(change_id.to_string(), signal.clone());

        info!("Created signal: {} for change {}", signal_type_str(signal_type), change_id);
        Ok(signal)
    }

    /// Sign a message with node's private key
    fn sign_message(&self, message: &str) -> Result<String, String> {
        // TODO: Use proper secp256k1 signing
        // For now, create a simple signature hash
        let mut hasher = Sha256::new();
        hasher.update(message.as_bytes());
        hasher.update(&self.node_private_key);
        Ok(hex::encode(hasher.finalize()))
    }

    /// Verify a signal from another node
    pub fn verify_signal(&self, signal: &UserSignal, node_public_key: &[u8]) -> bool {
        // Recreate message
        let message = format!(
            "{}:{}:{}:{}",
            signal.change_id,
            signal_type_str(signal.signal_type),
            signal.node_id,
            signal.timestamp
        );

        // Verify signature (simplified - in production, use proper secp256k1 verification)
        let mut hasher = Sha256::new();
        hasher.update(message.as_bytes());
        hasher.update(node_public_key);
        let expected_signature = hex::encode(hasher.finalize());

        expected_signature == signal.signature
    }

    /// Get signal for a change
    pub fn get_signal(&self, change_id: &str) -> Option<&UserSignal> {
        self.signals.get(change_id)
    }

    /// Calculate square-root weighted vote
    /// This provides Sybil resistance by weighting votes by sqrt(UTXO count)
    pub fn calculate_weighted_vote(
        signals: &[UserSignal],
        utxo_counts: &HashMap<String, u64>,
    ) -> (f64, f64) {
        let mut support_weight = 0.0;
        let mut oppose_weight = 0.0;

        for signal in signals {
            let utxo_count = utxo_counts.get(&signal.node_id).copied().unwrap_or(1);
            let weight = (utxo_count as f64).sqrt();

            match signal.signal_type {
                SignalType::Support => support_weight += weight,
                SignalType::Oppose => oppose_weight += weight,
                SignalType::Override => {
                    // Override signals count as support for override threshold
                    support_weight += weight;
                }
            }
        }

        (support_weight, oppose_weight)
    }

    /// Check if override threshold is met
    pub fn check_override_threshold(
        signals: &[UserSignal],
        utxo_counts: &HashMap<String, u64>,
        total_active_nodes: usize,
        threshold: f64,
    ) -> bool {
        let (support_weight, _) = Self::calculate_weighted_vote(signals, utxo_counts);
        let total_weight: f64 = utxo_counts.values().map(|&count| (count as f64).sqrt()).sum();
        
        if total_weight == 0.0 {
            return false;
        }

        let support_ratio = support_weight / total_weight;
        let node_ratio = signals.len() as f64 / total_active_nodes as f64;

        // Override threshold: 75% of weighted votes AND 75% of nodes
        support_ratio >= threshold && node_ratio >= threshold
    }
}

fn signal_type_str(signal_type: SignalType) -> &'static str {
    match signal_type {
        SignalType::Support => "support",
        SignalType::Oppose => "oppose",
        SignalType::Override => "override",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_id_generation() {
        let public_key = vec![0u8; 33];
        let private_key = vec![1u8; 32];
        let manager = UserSignalingManager::new(public_key, private_key);
        let node_id = manager.node_id();
        assert!(!node_id.is_empty());
    }

    #[test]
    fn test_signal_creation() {
        let public_key = vec![0u8; 33];
        let private_key = vec![1u8; 32];
        let mut manager = UserSignalingManager::new(public_key, private_key);
        
        let signal = manager.create_signal("PR-123", SignalType::Support).unwrap();
        assert_eq!(signal.change_id, "PR-123");
        assert_eq!(signal.signal_type, SignalType::Support);
    }

    #[test]
    fn test_weighted_vote_calculation() {
        let signals = vec![
            UserSignal {
                change_id: "PR-123".to_string(),
                signal_type: SignalType::Support,
                node_id: "node1".to_string(),
                timestamp: 1234567890,
                signature: "sig1".to_string(),
            },
            UserSignal {
                change_id: "PR-123".to_string(),
                signal_type: SignalType::Oppose,
                node_id: "node2".to_string(),
                timestamp: 1234567890,
                signature: "sig2".to_string(),
            },
        ];

        let mut utxo_counts = HashMap::new();
        utxo_counts.insert("node1".to_string(), 100); // sqrt(100) = 10
        utxo_counts.insert("node2".to_string(), 400); // sqrt(400) = 20

        let (support, oppose) = UserSignalingManager::calculate_weighted_vote(&signals, &utxo_counts);
        assert_eq!(support, 10.0);
        assert_eq!(oppose, 20.0);
    }
}


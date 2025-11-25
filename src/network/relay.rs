//! Block and transaction relay
//!
//! Handles relaying blocks and transactions to peers, managing relay policies,
//! and preventing duplicate relay.
//!
//! Includes Dandelion++ integration for privacy-preserving transaction relay.

#[cfg(feature = "dandelion")]
use super::dandelion::DandelionRelay;
use crate::utils::current_timestamp;
use bllvm_protocol::Hash;
#[cfg(feature = "fibre")]
use bllvm_protocol::Block;
use std::collections::HashMap;
use tracing::debug;
#[cfg(feature = "dandelion")]
use tracing::info;

/// Relay manager
pub struct RelayManager {
    /// Recently relayed blocks
    recently_relayed_blocks: HashMap<Hash, u64>,
    /// Recently relayed transactions
    recently_relayed_txs: HashMap<Hash, u64>,
    /// Relay policies
    policies: RelayPolicies,
    /// Dandelion++ privacy relay (only when compiled with feature)
    #[cfg(feature = "dandelion")]
    dandelion: Option<DandelionRelay>,
    /// Enable Dandelion++ (runtime toggle)
    enable_dandelion: bool,
}

/// Relay policies
#[derive(Debug, Clone)]
pub struct RelayPolicies {
    /// Maximum age for relayed items (seconds)
    pub max_relay_age: u64,
    /// Maximum number of items to track
    pub max_tracked_items: usize,
    /// Enable block relay
    pub enable_block_relay: bool,
    /// Enable transaction relay
    pub enable_tx_relay: bool,
    /// Enable Dandelion++ privacy relay
    pub enable_dandelion: bool,
}

impl Default for RelayPolicies {
    fn default() -> Self {
        Self {
            max_relay_age: 3600, // 1 hour
            max_tracked_items: 10000,
            enable_block_relay: true,
            enable_tx_relay: true,
            enable_dandelion: false, // Default OFF (requires feature flag)
        }
    }
}

impl Default for RelayManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayManager {
    /// Create a new relay manager
    pub fn new() -> Self {
        let policies = RelayPolicies::default();
        Self {
            recently_relayed_blocks: HashMap::new(),
            recently_relayed_txs: HashMap::new(),
            #[cfg(feature = "dandelion")]
            dandelion: if policies.enable_dandelion {
                Some(DandelionRelay::new())
            } else {
                None
            },
            enable_dandelion: policies.enable_dandelion,
            policies,
        }
    }

    /// Configure Dandelion stem timeout (testing/integration)
    #[cfg(feature = "dandelion")]
    pub fn set_dandelion_stem_timeout(&mut self, timeout: std::time::Duration) {
        if let Some(ref mut d) = self.dandelion {
            d.set_stem_timeout(timeout);
        }
    }
    #[cfg(not(feature = "dandelion"))]
    pub fn set_dandelion_stem_timeout(&mut self, _timeout: std::time::Duration) {}

    /// Configure Dandelion fluff probability (testing/integration)
    #[cfg(feature = "dandelion")]
    pub fn set_dandelion_fluff_probability(&mut self, p: f64) {
        if let Some(ref mut d) = self.dandelion {
            d.set_fluff_probability(p);
        }
    }
    #[cfg(not(feature = "dandelion"))]
    pub fn set_dandelion_fluff_probability(&mut self, _p: f64) {}

    /// Configure Dandelion max stem hops (testing/integration)
    #[cfg(feature = "dandelion")]
    pub fn set_dandelion_max_stem_hops(&mut self, hops: u8) {
        if let Some(ref mut d) = self.dandelion {
            d.set_max_stem_hops(hops);
        }
    }
    #[cfg(not(feature = "dandelion"))]
    pub fn set_dandelion_max_stem_hops(&mut self, _hops: u8) {}

    /// Create a relay manager with custom policies
    pub fn with_policies(policies: RelayPolicies) -> Self {
        Self {
            recently_relayed_blocks: HashMap::new(),
            recently_relayed_txs: HashMap::new(),
            #[cfg(feature = "dandelion")]
            dandelion: if policies.enable_dandelion {
                Some(DandelionRelay::new())
            } else {
                None
            },
            enable_dandelion: policies.enable_dandelion,
            policies,
        }
    }

    /// Check if a block should be relayed
    pub fn should_relay_block(&self, block_hash: &Hash) -> bool {
        if !self.policies.enable_block_relay {
            return false;
        }

        if self.recently_relayed_blocks.contains_key(block_hash) {
            return false;
        }

        true
    }

    /// Check if a transaction should be relayed
    pub fn should_relay_transaction(&self, tx_hash: &Hash) -> bool {
        if !self.policies.enable_tx_relay {
            return false;
        }

        if self.recently_relayed_txs.contains_key(tx_hash) {
            return false;
        }

        true
    }

    /// Mark a block as relayed
    pub fn mark_block_relayed(&mut self, block_hash: Hash) {
        let now = current_timestamp();

        self.recently_relayed_blocks.insert(block_hash, now);
        self.cleanup_old_items();

        debug!("Marked block {} as relayed", hex::encode(block_hash));
    }

    /// Mark a transaction as relayed
    pub fn mark_transaction_relayed(&mut self, tx_hash: Hash) {
        let now = current_timestamp();

        self.recently_relayed_txs.insert(tx_hash, now);

        // Clean up Dandelion state if transitioning to fluff
        #[cfg(feature = "dandelion")]
        {
            if let Some(ref mut dandelion) = self.dandelion {
                if dandelion.should_fluff(&tx_hash) {
                    dandelion.transition_to_fluff(tx_hash);
                }
            }
        }

        self.cleanup_old_items();

        debug!("Marked transaction {} as relayed", hex::encode(tx_hash));
    }

    /// Relay transaction with Dandelion++ privacy (if enabled)
    /// Returns the peer ID to relay to (if stem phase) or None (if fluff phase - broadcast to all)
    #[cfg(feature = "dandelion")]
    pub fn relay_transaction_dandelion(
        &mut self,
        tx_hash: Hash,
        current_peer: String,
        available_peers: &[String],
    ) -> Option<String> {
        if !self.enable_dandelion || self.dandelion.is_none() {
            return None; // Normal relay (broadcast to all)
        }

        let dandelion = self.dandelion.as_mut().unwrap();

        // Check if already in stem phase
        if let Some(_next_peer) = dandelion.get_stem_peer(&tx_hash) {
            // Already in stem phase, advance
            if dandelion.should_fluff(&tx_hash) {
                dandelion.transition_to_fluff(tx_hash);
                info!(
                    "Transaction {} transitioned to fluff phase",
                    hex::encode(tx_hash)
                );
                return None; // Broadcast to all
            } else {
                // Advance stem phase
                return dandelion.advance_stem(tx_hash, available_peers);
            }
        } else {
            // Start stem phase
            if available_peers.is_empty() {
                return None; // No peers available, skip Dandelion
            }

            let next_peer = dandelion.start_stem_phase(tx_hash, current_peer, available_peers);
            if next_peer.is_some() {
                info!(
                    "Transaction {} started Dandelion stem phase",
                    hex::encode(tx_hash)
                );
            }
            return next_peer;
        }
    }
    #[cfg(not(feature = "dandelion"))]
    pub fn relay_transaction_dandelion(
        &mut self,
        _tx_hash: Hash,
        _current_peer: String,
        _available_peers: &[String],
    ) -> Option<String> {
        None
    }

    /// Initialize Dandelion stem path for a peer
    #[cfg(feature = "dandelion")]
    pub fn initialize_dandelion_path(&mut self, peer_id: String, available_peers: &[String]) {
        if let Some(ref mut dandelion) = self.dandelion {
            dandelion.initialize_stem_path(peer_id, available_peers);
        }
    }
    #[cfg(not(feature = "dandelion"))]
    pub fn initialize_dandelion_path(&mut self, _peer_id: String, _available_peers: &[String]) {}

    /// Clean up expired Dandelion paths
    #[cfg(feature = "dandelion")]
    pub fn cleanup_dandelion(&mut self) {
        if let Some(ref mut dandelion) = self.dandelion {
            dandelion.cleanup_expired();
        }
    }
    #[cfg(not(feature = "dandelion"))]
    pub fn cleanup_dandelion(&mut self) {}

    /// Get relay statistics
    pub fn get_stats(&self) -> RelayStats {
        RelayStats {
            relayed_blocks: self.recently_relayed_blocks.len(),
            relayed_transactions: self.recently_relayed_txs.len(),
            policies: self.policies.clone(),
        }
    }

    /// Try to prioritize block relay via FIBRE (if available)
    /// Returns true if FIBRE encoding path executed (send is transport-dependent)
    #[cfg(feature = "fibre")]
    pub fn prioritize_block_via_fibre(
        &mut self,
        fibre: &mut crate::network::fibre::FibreRelay,
        block: &Block,
    ) -> bool {
        if !self.policies.enable_block_relay {
            return false;
        }
        // Encode and cache for FIBRE. Actual UDP send is out-of-scope here.
        match fibre.encode_block(block.clone()) {
            Ok(_encoded) => {
                debug!("Prepared FEC chunks for FIBRE relay");
                true
            }
            Err(_) => false,
        }
    }

    /// Clean up old relayed items
    fn cleanup_old_items(&mut self) {
        let now = current_timestamp();

        // Clean up old blocks
        let old_blocks: Vec<Hash> = self
            .recently_relayed_blocks
            .iter()
            .filter(|(_, &timestamp)| now - timestamp > self.policies.max_relay_age)
            .map(|(hash, _)| *hash)
            .collect();

        for hash in old_blocks {
            self.recently_relayed_blocks.remove(&hash);
        }

        // Clean up old transactions
        let old_txs: Vec<Hash> = self
            .recently_relayed_txs
            .iter()
            .filter(|(_, &timestamp)| now - timestamp > self.policies.max_relay_age)
            .map(|(hash, _)| *hash)
            .collect();

        for hash in old_txs {
            self.recently_relayed_txs.remove(&hash);
        }

        // Limit total items
        if self.recently_relayed_blocks.len() > self.policies.max_tracked_items {
            let excess = self.recently_relayed_blocks.len() - self.policies.max_tracked_items;
            let oldest_blocks: Vec<Hash> = self
                .recently_relayed_blocks
                .iter()
                .min_by_key(|(_, &timestamp)| timestamp)
                .map(|(hash, _)| *hash)
                .into_iter()
                .take(excess)
                .collect();

            for hash in oldest_blocks {
                self.recently_relayed_blocks.remove(&hash);
            }
        }

        if self.recently_relayed_txs.len() > self.policies.max_tracked_items {
            let excess = self.recently_relayed_txs.len() - self.policies.max_tracked_items;
            let oldest_txs: Vec<Hash> = self
                .recently_relayed_txs
                .iter()
                .min_by_key(|(_, &timestamp)| timestamp)
                .map(|(hash, _)| *hash)
                .into_iter()
                .take(excess)
                .collect();

            for hash in oldest_txs {
                self.recently_relayed_txs.remove(&hash);
            }
        }
    }
}

/// Relay statistics
#[derive(Debug, Clone)]
pub struct RelayStats {
    pub relayed_blocks: usize,
    pub relayed_transactions: usize,
    pub policies: RelayPolicies,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "fibre")]
    use crate::network::fibre::FibreRelay;

    #[test]
    #[cfg(feature = "fibre")]
    fn test_prioritize_block_via_fibre_encodes() {
        let mut relay = RelayManager::new();
        let mut fibre = FibreRelay::new();
        let block = Block {
            header: bllvm_protocol::BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        };
        let ok = relay.prioritize_block_via_fibre(&mut fibre, &block);
        assert!(ok);
    }
}

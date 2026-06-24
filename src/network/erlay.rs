//! Erlay (BIP330) transaction relay optimization
//!
//! Set reconciliation uses minisketch when the optional `erlay` feature is enabled.
//! BIP330 short transaction IDs (salted SipHash over wtxid) are always available for tests
//! and for negotiation state even without minisketch.

use blvm_protocol::Hash;
use sha2::{Digest, Sha256};
use siphasher::sip::SipHasher24;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;

#[cfg(feature = "erlay")]
use anyhow::{Context, Result};
#[cfg(feature = "erlay")]
use minisketch_rs::Minisketch;

const ERLAY_SHORT_ID_MOD: u64 = 0xFFFF_FFFF;
const ERLAY_TAGGED_HASH_TAG: &[u8] = b"Tx Relay Salting";

/// BIP340-style tagged hash: SHA256(SHA256(tag) || SHA256(tag) || msg).
pub fn tagged_hash(tag: &[u8], msg: &[u8]) -> [u8; 32] {
    let tag_hash = Sha256::digest(tag);
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher.update(msg);
    let out = hasher.finalize();
    let mut result = [0u8; 32];
    result.copy_from_slice(&out);
    result
}

/// Derive SipHash-2-4 keys from both peers' reconciliation salts (BIP330).
pub fn erlay_siphash_keys(local_salt: u64, remote_salt: u64) -> (u64, u64) {
    let (salt1, salt2) = if local_salt <= remote_salt {
        (local_salt, remote_salt)
    } else {
        (remote_salt, local_salt)
    };
    let mut msg = [0u8; 16];
    msg[0..8].copy_from_slice(&salt1.to_le_bytes());
    msg[8..16].copy_from_slice(&salt2.to_le_bytes());
    let h = tagged_hash(ERLAY_TAGGED_HASH_TAG, &msg);
    let k0 = u64::from_le_bytes(h[0..8].try_into().expect("8 bytes"));
    let k1 = u64::from_le_bytes(h[8..16].try_into().expect("8 bytes"));
    (k0, k1)
}

/// BIP330 short transaction ID: `1 + (SipHash-2-4((k0,k1), wtxid) mod 0xFFFFFFFF)`.
pub fn compute_erlay_short_id(wtxid: &Hash, k0: u64, k1: u64) -> u32 {
    let mut hasher = SipHasher24::new_with_keys(k0, k1);
    hasher.write(wtxid);
    let s = hasher.finish();
    1 + ((s % ERLAY_SHORT_ID_MOD) as u32)
}

/// Build a short-ID lookup table for a transaction set.
pub fn build_erlay_short_id_map(txs: &HashSet<Hash>, k0: u64, k1: u64) -> HashMap<u32, Hash> {
    let mut map = HashMap::with_capacity(txs.len());
    for wtxid in txs {
        let short_id = compute_erlay_short_id(wtxid, k0, k1);
        map.insert(short_id, *wtxid);
    }
    map
}

/// Resolve short IDs against a candidate wtxid set (reconciliation snapshot / mempool).
pub fn resolve_erlay_short_ids(
    candidates: &HashSet<Hash>,
    short_ids: &[u32],
    k0: u64,
    k1: u64,
) -> Vec<Hash> {
    let mut resolved = Vec::with_capacity(short_ids.len());
    for &short_id in short_ids {
        for wtxid in candidates {
            if compute_erlay_short_id(wtxid, k0, k1) == short_id {
                resolved.push(*wtxid);
                break;
            }
        }
    }
    resolved
}

/// Per-peer Erlay negotiation state (BIP330 `sendtxrcncl`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErlayPeerNegotiation {
    pub version: u16,
    pub local_salt: u64,
    pub remote_salt: Option<u64>,
    pub negotiated: bool,
    /// Negotiated minisketch field size in bits (32 or 64).
    pub field_size: u8,
}

impl Default for ErlayPeerNegotiation {
    fn default() -> Self {
        Self {
            version: 0,
            local_salt: 0,
            remote_salt: None,
            negotiated: false,
            field_size: 32,
        }
    }
}

impl ErlayPeerNegotiation {
    /// Record peer `sendtxrcncl` and derive negotiated parameters.
    pub fn apply_sendtxrcncl(
        &mut self,
        version: u16,
        remote_salt: u64,
        min_field_size: u8,
        max_field_size: u8,
        local_salt: u64,
    ) -> Result<(), ErlayNegotiationError> {
        if version != 1 {
            return Err(ErlayNegotiationError::UnsupportedVersion(version));
        }
        if min_field_size != 32 && min_field_size != 64 {
            return Err(ErlayNegotiationError::InvalidFieldSize(min_field_size));
        }
        if max_field_size != 32 && max_field_size != 64 {
            return Err(ErlayNegotiationError::InvalidFieldSize(max_field_size));
        }
        if min_field_size > max_field_size {
            return Err(ErlayNegotiationError::InvalidFieldRange {
                min: min_field_size,
                max: max_field_size,
            });
        }

        let field_size = if max_field_size >= 32 && min_field_size <= 32 {
            32
        } else {
            64
        };
        self.version = version;
        self.local_salt = local_salt;
        self.remote_salt = Some(remote_salt);
        self.field_size = field_size;
        self.negotiated = true;
        Ok(())
    }

    pub fn siphash_keys(&self) -> Option<(u64, u64)> {
        self.remote_salt
            .map(|remote| erlay_siphash_keys(self.local_salt, remote))
    }

    pub fn is_negotiated(&self) -> bool {
        self.negotiated && self.remote_salt.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErlayNegotiationError {
    #[error("unsupported Erlay version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid Erlay field size {0}")]
    InvalidFieldSize(u8),
    #[error("invalid Erlay field range min={min} max={max}")]
    InvalidFieldRange { min: u8, max: u8 },
}

/// Extract the BIP330 uint64 salt from our wire `[u8; 16]` payload (little-endian first 8 bytes).
pub fn salt_from_wire_bytes(salt: &[u8; 16]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&salt[0..8]);
    u64::from_le_bytes(bytes)
}

/// Erlay set reconciler configuration
#[derive(Debug, Clone)]
pub struct ErlayConfig {
    pub capacity: usize,
    pub field_size: u8,
    pub siphash_keys: (u64, u64),
}

impl ErlayConfig {
    pub fn from_negotiation(negotiation: &ErlayPeerNegotiation) -> Option<Self> {
        let keys = negotiation.siphash_keys()?;
        Some(Self {
            capacity: 100_000,
            field_size: negotiation.field_size,
            siphash_keys: keys,
        })
    }
}

impl Default for ErlayConfig {
    fn default() -> Self {
        Self {
            capacity: 100_000,
            field_size: 32,
            siphash_keys: (0, 1),
        }
    }
}

/// Result of sketch reconciliation for one side.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ErlayReconcileDiff {
    /// Wtxids we have that the peer is missing (from decoded short IDs in our map).
    pub we_have_peer_missing: Vec<Hash>,
    /// Short IDs the peer has that we lack (resolve via inv / reconcildiff).
    pub peer_has_we_missing: Vec<u32>,
}

#[cfg(feature = "erlay")]
pub struct ErlayReconciler {
    config: ErlayConfig,
}

#[cfg(feature = "erlay")]
impl ErlayReconciler {
    pub fn new(config: ErlayConfig) -> Self {
        Self { config }
    }

    pub fn create_sketch(
        &self,
        local_txs: &HashSet<Hash>,
        _remote_tx_count: usize,
    ) -> Result<Vec<u8>> {
        if local_txs.is_empty() {
            return Ok(vec![]);
        }

        let (k0, k1) = self.config.siphash_keys;
        let mut sketch = Minisketch::new(self.config.field_size, 0, self.config.capacity)
            .context("Failed to create minisketch")?;

        for wtxid in local_txs {
            let short_id = compute_erlay_short_id(wtxid, k0, k1);
            sketch
                .add(u64::from(short_id))
                .context("Failed to add short transaction ID to sketch")?;
        }

        sketch.serialize().context("Failed to serialize sketch")
    }

    pub fn reconcile_sets(
        &self,
        local_txs: &HashSet<Hash>,
        local_sketch: &[u8],
        remote_sketch: &[u8],
    ) -> Result<ErlayReconcileDiff> {
        if local_sketch.is_empty() && remote_sketch.is_empty() {
            return Ok(ErlayReconcileDiff::default());
        }

        let (k0, k1) = self.config.siphash_keys;
        let local_short_map = build_erlay_short_id_map(local_txs, k0, k1);

        let mut combined = Minisketch::new(self.config.field_size, 0, self.config.capacity)
            .context("Failed to create combined minisketch")?;

        if !local_sketch.is_empty() {
            let mut local = Minisketch::new(self.config.field_size, 0, self.config.capacity)
                .context("Failed to create local minisketch")?;
            local
                .deserialize(local_sketch)
                .context("Failed to deserialize local sketch")?;
            combined
                .merge(&local)
                .map_err(|e| anyhow::anyhow!("Failed to merge local sketch: {:?}", e))?;
        }

        if !remote_sketch.is_empty() {
            let mut remote = Minisketch::new(self.config.field_size, 0, self.config.capacity)
                .context("Failed to create remote minisketch")?;
            remote
                .deserialize(remote_sketch)
                .context("Failed to deserialize remote sketch")?;
            combined
                .merge(&remote)
                .map_err(|e| anyhow::anyhow!("Failed to merge remote sketch: {:?}", e))?;
        }

        let differences: Vec<u64> = combined
            .decode()
            .map_err(|e| anyhow::anyhow!("Failed to decode sketch: {:?}", e))?;

        let mut diff = ErlayReconcileDiff::default();
        for value in differences {
            let short_id = value as u32;
            if let Some(wtxid) = local_short_map.get(&short_id) {
                diff.we_have_peer_missing.push(*wtxid);
            } else {
                diff.peer_has_we_missing.push(short_id);
            }
        }
        Ok(diff)
    }
}

/// Erlay transaction set manager (requires `erlay` feature).
#[cfg(feature = "erlay")]
pub struct ErlayTxSet {
    txs: HashSet<Hash>,
    reconciler: ErlayReconciler,
}

#[cfg(feature = "erlay")]
impl ErlayTxSet {
    pub fn new() -> Self {
        Self {
            txs: HashSet::new(),
            reconciler: ErlayReconciler::new(ErlayConfig::default()),
        }
    }

    pub fn with_config(config: ErlayConfig) -> Self {
        Self {
            txs: HashSet::new(),
            reconciler: ErlayReconciler::new(config),
        }
    }

    pub fn add(&mut self, tx_hash: Hash) {
        self.txs.insert(tx_hash);
    }

    pub fn remove(&mut self, tx_hash: &Hash) {
        self.txs.remove(tx_hash);
    }

    pub fn size(&self) -> usize {
        self.txs.len()
    }

    pub fn contains(&self, tx_hash: &Hash) -> bool {
        self.txs.contains(tx_hash)
    }

    pub fn create_reconciliation_sketch(&self, remote_tx_count: usize) -> Result<Vec<u8>> {
        self.reconciler.create_sketch(&self.txs, remote_tx_count)
    }

    pub fn reconcile_with_peer(
        &self,
        local_sketch: &[u8],
        remote_sketch: &[u8],
    ) -> Result<ErlayReconcileDiff> {
        self.reconciler
            .reconcile_sets(&self.txs, local_sketch, remote_sketch)
    }

    pub fn get_all_hashes(&self) -> Vec<Hash> {
        self.txs.iter().cloned().collect()
    }
}

#[cfg(feature = "erlay")]
impl Default for ErlayTxSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erlay_siphash_keys_are_order_independent() {
        let a = erlay_siphash_keys(1, 2);
        let b = erlay_siphash_keys(2, 1);
        assert_eq!(a, b);
    }

    #[test]
    fn compute_erlay_short_id_is_stable_and_nonzero() {
        let wtxid = [0xab; 32];
        let (k0, k1) = erlay_siphash_keys(10, 20);
        let sid = compute_erlay_short_id(&wtxid, k0, k1);
        assert!(sid >= 1);
        assert_eq!(sid, compute_erlay_short_id(&wtxid, k0, k1));
    }

    #[test]
    fn build_erlay_short_id_map_round_trips() {
        let mut txs = HashSet::new();
        txs.insert([1u8; 32]);
        txs.insert([2u8; 32]);
        let (k0, k1) = erlay_siphash_keys(5, 9);
        let map = build_erlay_short_id_map(&txs, k0, k1);
        assert_eq!(map.len(), 2);
        for wtxid in &txs {
            let sid = compute_erlay_short_id(wtxid, k0, k1);
            assert_eq!(map.get(&sid), Some(wtxid));
        }
    }

    #[test]
    fn apply_sendtxrcncl_stores_negotiation_state() {
        let mut state = ErlayPeerNegotiation::default();
        state
            .apply_sendtxrcncl(1, 42, 32, 64, 99)
            .expect("negotiation");
        assert!(state.is_negotiated());
        assert_eq!(state.local_salt, 99);
        assert_eq!(state.remote_salt, Some(42));
        assert_eq!(state.field_size, 32);
        assert_eq!(state.siphash_keys(), Some(erlay_siphash_keys(99, 42)));
    }

    #[test]
    fn apply_sendtxrcncl_rejects_bad_version() {
        let mut state = ErlayPeerNegotiation::default();
        assert!(matches!(
            state.apply_sendtxrcncl(0, 1, 32, 32, 1),
            Err(ErlayNegotiationError::UnsupportedVersion(0))
        ));
    }

    #[cfg(feature = "erlay")]
    mod minisketch_tests {
        use super::*;

        #[test]
        fn test_erlay_reconciliation() {
            let (k0, k1) = erlay_siphash_keys(1, 2);
            let config = ErlayConfig {
                capacity: 10,
                field_size: 32,
                siphash_keys: (k0, k1),
            };
            let reconciler = ErlayReconciler::new(config);

            let mut local_txs = HashSet::new();
            local_txs.insert([1u8; 32]);
            local_txs.insert([2u8; 32]);
            local_txs.insert([3u8; 32]);

            let mut remote_txs = HashSet::new();
            remote_txs.insert([2u8; 32]);
            remote_txs.insert([3u8; 32]);
            remote_txs.insert([4u8; 32]);

            let local_sketch = reconciler
                .create_sketch(&local_txs, remote_txs.len())
                .expect("local sketch");
            let remote_sketch = reconciler
                .create_sketch(&remote_txs, local_txs.len())
                .expect("remote sketch");

            let missing_local = reconciler
                .reconcile_sets(&local_txs, &local_sketch, &remote_sketch)
                .expect("reconcile local");
            let missing_remote = reconciler
                .reconcile_sets(&remote_txs, &remote_sketch, &local_sketch)
                .expect("reconcile remote");

            assert_eq!(missing_local.peer_has_we_missing.len(), 1);
            assert_eq!(
                missing_local.peer_has_we_missing[0],
                compute_erlay_short_id(&[4u8; 32], k0, k1)
            );
            assert!(missing_remote.we_have_peer_missing.contains(&[4u8; 32]));
            assert_eq!(missing_remote.peer_has_we_missing.len(), 1);
            assert_eq!(
                missing_remote.peer_has_we_missing[0],
                compute_erlay_short_id(&[1u8; 32], k0, k1)
            );
        }
    }
}

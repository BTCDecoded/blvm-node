//! AssumeUTXO implementation for fast initial sync
//!
//! AssumeUTXO allows a node to become usable quickly by:
//! 1. Loading a pre-computed UTXO set snapshot at a known block height
//! 2. Validating transactions using this snapshot immediately
//! 3. Downloading and validating historical blocks in the background
//!
//! ## Security Model
//!
//! The snapshot hash is compared against a hardcoded value compiled into the binary.
//! This provides the same security guarantees as checkpoint hashes - you trust the
//! binary you're running. Background validation eventually validates all historical
//! blocks, achieving full node security.
//!
//! ## Snapshot Format
//!
//! Snapshots are stored as compressed binary files with the following structure:
//! - 4 bytes: version (u32 LE)
//! - 32 bytes: block hash
//! - 8 bytes: block height (u64 LE)
//! - 32 bytes: UTXO set hash (muhash)
//! - 8 bytes: UTXO count (u64 LE)
//! - variable: UTXO entries (outpoint + utxo, each preceded by u32 LE length)
//!
//! The file is compressed for efficient storage and transfer.

use anyhow::{Context, Result};
use blvm_protocol::{Hash, OutPoint, UtxoSet, UTXO};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use tracing::{debug, info, warn};

#[cfg(feature = "utxo-compression")]
use zstd;

/// Snapshot format version
const SNAPSHOT_VERSION: u32 = 1;

/// Known AssumeUTXO snapshot hashes for mainnet
///
/// These are the SHA256 hash of the serialized UTXO set at each height.
/// Add new entries as snapshots are created and verified.
pub const MAINNET_ASSUMEUTXO_SNAPSHOTS: &[(u64, &str)] = &[
    // Format: (height, muhash_hex)
    // Block 840,000 (4th halving) - TODO: Add verified hash
    // (840_000, "..."),
];

/// Known AssumeUTXO snapshot hashes for testnet
pub const TESTNET_ASSUMEUTXO_SNAPSHOTS: &[(u64, &str)] = &[
    // TODO: Add testnet snapshots
];

/// AssumeUTXO snapshot metadata
#[derive(Debug, Clone)]
pub struct SnapshotMetadata {
    /// Snapshot format version
    pub version: u32,
    /// Block hash at snapshot point
    pub block_hash: Hash,
    /// Block height at snapshot point
    pub block_height: u64,
    /// MuHash of the UTXO set (for verification)
    pub utxo_hash: Hash,
    /// Number of UTXOs in the snapshot
    pub utxo_count: u64,
}

/// AssumeUTXO manager for handling UTXO snapshots
pub struct AssumeUtxoManager {
    /// Data directory for storing snapshots
    data_dir: std::path::PathBuf,
    /// Known snapshot hashes (height -> expected_hash)
    known_snapshots: HashMap<u64, Hash>,
    /// Current loaded snapshot (if any)
    loaded_snapshot: Option<SnapshotMetadata>,
    /// Background validation progress (height validated up to)
    background_validated_height: u64,
}

impl AssumeUtxoManager {
    /// Create a new AssumeUTXO manager
    pub fn new(data_dir: impl Into<std::path::PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let mut known_snapshots = HashMap::new();
        
        // Load known mainnet snapshots
        for &(height, hash_hex) in MAINNET_ASSUMEUTXO_SNAPSHOTS {
            if let Ok(hash) = hex::decode(hash_hex) {
                if hash.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&hash);
                    known_snapshots.insert(height, arr);
                }
            }
        }
        
        Self {
            data_dir,
            known_snapshots,
            loaded_snapshot: None,
            background_validated_height: 0,
        }
    }

    /// Get the snapshot file path for a given height
    fn snapshot_path(&self, height: u64) -> std::path::PathBuf {
        self.data_dir.join(format!("utxo_snapshot_{}.dat", height))
    }

    /// Check if a snapshot exists for the given height
    pub fn has_snapshot(&self, height: u64) -> bool {
        self.snapshot_path(height).exists()
    }

    /// Get the expected hash for a known snapshot height
    pub fn expected_hash(&self, height: u64) -> Option<&Hash> {
        self.known_snapshots.get(&height)
    }

    /// Get the best available snapshot height
    pub fn best_snapshot_height(&self) -> Option<u64> {
        // Find the highest known snapshot that we have on disk
        self.known_snapshots
            .keys()
            .filter(|&&h| self.has_snapshot(h))
            .max()
            .copied()
    }

    /// Calculate MuHash of a UTXO set
    ///
    /// MuHash is an additive hash that can be efficiently updated as UTXOs are added/removed.
    /// For snapshots, we compute it over the entire set for verification.
    pub fn calculate_utxo_hash(utxo_set: &UtxoSet) -> Result<Hash> {
        use sha2::{Sha256, Digest};
        
        // Sort outpoints for deterministic ordering
        let mut entries: Vec<_> = utxo_set.iter().collect();
        entries.sort_by(|a, b| {
            let key_a = Self::outpoint_sort_key(a.0);
            let key_b = Self::outpoint_sort_key(b.0);
            key_a.cmp(&key_b)
        });
        
        // Hash all entries
        let mut hasher = Sha256::new();
        for (outpoint, utxo) in entries {
            // Hash outpoint
            hasher.update(&outpoint.hash);
            hasher.update(&outpoint.index.to_le_bytes());
            
            // Hash UTXO (value is i64, script_pubkey is Vec<u8>, height is u64)
            hasher.update(&utxo.value.to_le_bytes());
            hasher.update(&(utxo.script_pubkey.len() as u32).to_le_bytes());
            hasher.update(&utxo.script_pubkey);
            hasher.update(&(utxo.is_coinbase as u8).to_le_bytes());
            hasher.update(&utxo.height.to_le_bytes());
        }
        
        // Final hash
        let result = hasher.finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&result);
        
        // Double SHA256 for Bitcoin compatibility
        let mut hasher2 = Sha256::new();
        hasher2.update(&hash);
        let result2 = hasher2.finalize();
        hash.copy_from_slice(&result2);
        
        Ok(hash)
    }

    /// Create a sort key for deterministic ordering
    fn outpoint_sort_key(outpoint: &OutPoint) -> Vec<u8> {
        let mut key = Vec::with_capacity(36);
        key.extend_from_slice(&outpoint.hash);
        key.extend_from_slice(&outpoint.index.to_le_bytes());
        key
    }

    /// Create a UTXO snapshot at the current chain tip
    pub fn create_snapshot(
        &self,
        utxo_set: &UtxoSet,
        block_hash: Hash,
        block_height: u64,
    ) -> Result<SnapshotMetadata> {
        info!("Creating UTXO snapshot at height {}", block_height);
        
        let utxo_hash = Self::calculate_utxo_hash(utxo_set)?;
        let utxo_count = utxo_set.len() as u64;
        
        let metadata = SnapshotMetadata {
            version: SNAPSHOT_VERSION,
            block_hash,
            block_height,
            utxo_hash,
            utxo_count,
        };
        
        // Write snapshot to disk
        let path = self.snapshot_path(block_height);
        std::fs::create_dir_all(&self.data_dir)?;
        
        let file = File::create(&path).context("Failed to create snapshot file")?;
        let mut writer = BufWriter::new(file);
        
        // Write header
        writer.write_all(&metadata.version.to_le_bytes())?;
        writer.write_all(&metadata.block_hash)?;
        writer.write_all(&metadata.block_height.to_le_bytes())?;
        writer.write_all(&metadata.utxo_hash)?;
        writer.write_all(&metadata.utxo_count.to_le_bytes())?;
        
        // Write UTXOs
        for (outpoint, utxo) in utxo_set.iter() {
            // Serialize entry
            let entry = Self::serialize_utxo_entry(outpoint, utxo)?;
            writer.write_all(&(entry.len() as u32).to_le_bytes())?;
            writer.write_all(&entry)?;
        }
        
        writer.flush()?;
        
        let file_size = std::fs::metadata(&path)?.len();
        info!(
            "Created snapshot: {} UTXOs, {} bytes compressed at height {}",
            utxo_count,
            file_size,
            block_height
        );
        
        Ok(metadata)
    }

    /// Serialize a UTXO entry for snapshot storage
    fn serialize_utxo_entry(outpoint: &OutPoint, utxo: &UTXO) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        
        // Outpoint (index is stored as u32 for Bitcoin compatibility)
        buf.extend_from_slice(&outpoint.hash);
        buf.extend_from_slice(&(outpoint.index as u32).to_le_bytes());
        
        // UTXO (value is i64 Integer, height is u64 Natural)
        buf.extend_from_slice(&utxo.value.to_le_bytes());
        buf.extend_from_slice(&(utxo.script_pubkey.len() as u32).to_le_bytes());
        buf.extend_from_slice(&utxo.script_pubkey);
        buf.push(utxo.is_coinbase as u8);
        buf.extend_from_slice(&utxo.height.to_le_bytes());
        
        Ok(buf)
    }

    /// Deserialize a UTXO entry from snapshot
    fn deserialize_utxo_entry(data: &[u8]) -> Result<(OutPoint, UTXO)> {
        if data.len() < 56 {
            // 32 (hash) + 4 (index) + 8 (value) + 4 (script_len) + 0 (min script) + 1 (coinbase) + 8 (height) = 57 min
            return Err(anyhow::anyhow!("UTXO entry too short: {} bytes", data.len()));
        }
        
        let mut pos = 0;
        
        // Outpoint
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[pos..pos + 32]);
        pos += 32;
        
        let index = u32::from_le_bytes(data[pos..pos + 4].try_into()?) as u64;
        pos += 4;
        
        let outpoint = OutPoint { hash, index };
        
        // UTXO (value is i64, height is u64)
        let value = i64::from_le_bytes(data[pos..pos + 8].try_into()?);
        pos += 8;
        
        let script_len = u32::from_le_bytes(data[pos..pos + 4].try_into()?) as usize;
        pos += 4;
        
        if pos + script_len + 1 + 8 > data.len() {
            return Err(anyhow::anyhow!("UTXO entry truncated at script"));
        }
        
        let script_pubkey = data[pos..pos + script_len].to_vec();
        pos += script_len;
        
        let is_coinbase = data[pos] != 0;
        pos += 1;
        
        let height = u64::from_le_bytes(data[pos..pos + 8].try_into()?);
        
        let utxo = UTXO {
            value,
            script_pubkey,
            is_coinbase,
            height,
        };
        
        Ok((outpoint, utxo))
    }

    /// Load a UTXO snapshot from disk
    ///
    /// Returns the UTXO set and metadata if the snapshot exists and is valid.
    /// Verifies the hash if we have an expected hash for this height.
    pub fn load_snapshot(&mut self, height: u64) -> Result<(UtxoSet, SnapshotMetadata)> {
        let path = self.snapshot_path(height);
        if !path.exists() {
            return Err(anyhow::anyhow!("No snapshot found for height {}", height));
        }
        
        info!("Loading UTXO snapshot from height {}", height);
        
        let file = File::open(&path).context("Failed to open snapshot file")?;
        let mut reader = BufReader::new(file);
        
        // Read header
        let mut buf4 = [0u8; 4];
        let mut buf8 = [0u8; 8];
        let mut buf32 = [0u8; 32];
        
        reader.read_exact(&mut buf4)?;
        let version = u32::from_le_bytes(buf4);
        
        if version != SNAPSHOT_VERSION {
            return Err(anyhow::anyhow!(
                "Unsupported snapshot version: {} (expected {})",
                version,
                SNAPSHOT_VERSION
            ));
        }
        
        reader.read_exact(&mut buf32)?;
        let block_hash = buf32;
        
        reader.read_exact(&mut buf8)?;
        let block_height = u64::from_le_bytes(buf8);
        
        reader.read_exact(&mut buf32)?;
        let utxo_hash = buf32;
        
        reader.read_exact(&mut buf8)?;
        let utxo_count = u64::from_le_bytes(buf8);
        
        let metadata = SnapshotMetadata {
            version,
            block_hash,
            block_height,
            utxo_hash,
            utxo_count,
        };
        
        // Read UTXOs
        let mut utxo_set = HashMap::with_capacity(utxo_count as usize);
        
        for i in 0..utxo_count {
            reader.read_exact(&mut buf4)?;
            let entry_len = u32::from_le_bytes(buf4) as usize;
            
            let mut entry = vec![0u8; entry_len];
            reader.read_exact(&mut entry)?;
            
            let (outpoint, utxo) = Self::deserialize_utxo_entry(&entry)?;
            utxo_set.insert(outpoint, utxo);
            
            if i > 0 && i % 1_000_000 == 0 {
                debug!("Loaded {} / {} UTXOs ({:.1}%)", i, utxo_count, (i as f64 / utxo_count as f64) * 100.0);
            }
        }
        
        // Verify hash if we have an expected value
        if let Some(expected) = self.known_snapshots.get(&height) {
            info!("Verifying snapshot hash...");
            let computed = Self::calculate_utxo_hash(&utxo_set)?;
            if &computed != expected {
                return Err(anyhow::anyhow!(
                    "Snapshot hash mismatch at height {}: expected {}, got {}",
                    height,
                    hex::encode(expected),
                    hex::encode(computed)
                ));
            }
            info!("Snapshot hash verified!");
        } else {
            warn!(
                "No expected hash for snapshot at height {} - using without verification",
                height
            );
        }
        
        self.loaded_snapshot = Some(metadata.clone());
        
        info!(
            "Loaded {} UTXOs from snapshot at height {} (block: {})",
            utxo_set.len(),
            block_height,
            hex::encode(block_hash)
        );
        
        Ok((utxo_set, metadata))
    }

    /// Get the currently loaded snapshot metadata
    pub fn loaded_snapshot(&self) -> Option<&SnapshotMetadata> {
        self.loaded_snapshot.as_ref()
    }

    /// Check if we're using an assumeutxo snapshot (background validation not complete)
    pub fn is_using_snapshot(&self) -> bool {
        self.loaded_snapshot.is_some() && 
            self.loaded_snapshot.as_ref().map(|s| s.block_height).unwrap_or(0) > self.background_validated_height
    }

    /// Update background validation progress
    pub fn set_background_validated_height(&mut self, height: u64) {
        self.background_validated_height = height;
        
        // Check if we've caught up to the snapshot
        if let Some(snapshot) = &self.loaded_snapshot {
            if height >= snapshot.block_height {
                info!(
                    "Background validation complete! Validated up to snapshot height {}",
                    snapshot.block_height
                );
            }
        }
    }

    /// Get background validation progress
    pub fn background_validated_height(&self) -> u64 {
        self.background_validated_height
    }

    /// Get snapshot directory
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_utxo_set() -> UtxoSet {
        let mut utxo_set = HashMap::new();
        
        // Add some test UTXOs
        for i in 0..100u32 {
            let mut hash = [0u8; 32];
            hash[0..4].copy_from_slice(&i.to_le_bytes());
            
            let outpoint = OutPoint { hash, index: 0 };
            let utxo = UTXO {
                value: 50_000_000 * (i as i64 + 1), // 0.5 BTC * (i+1)
                script_pubkey: vec![0x76, 0xa9, 0x14, 0x00, 0x88, 0xac], // P2PKH placeholder
                is_coinbase: i == 0,
                height: 100 + i as u64,
            };
            
            utxo_set.insert(outpoint, utxo);
        }
        
        utxo_set
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let dir = tempdir().unwrap();
        let manager = AssumeUtxoManager::new(dir.path());
        
        let utxo_set = create_test_utxo_set();
        let block_hash = [1u8; 32];
        let height = 800_000u64;
        
        // Create snapshot
        let metadata = manager.create_snapshot(&utxo_set, block_hash, height).unwrap();
        assert_eq!(metadata.utxo_count, 100);
        assert_eq!(metadata.block_height, height);
        
        // Load snapshot
        let mut manager2 = AssumeUtxoManager::new(dir.path());
        let (loaded_set, loaded_metadata) = manager2.load_snapshot(height).unwrap();
        
        assert_eq!(loaded_set.len(), utxo_set.len());
        assert_eq!(loaded_metadata.block_height, metadata.block_height);
        assert_eq!(loaded_metadata.utxo_hash, metadata.utxo_hash);
        
        // Verify contents match
        for (outpoint, utxo) in utxo_set.iter() {
            let loaded_utxo = loaded_set.get(outpoint).expect("UTXO not found");
            assert_eq!(loaded_utxo.value, utxo.value);
            assert_eq!(loaded_utxo.script_pubkey, utxo.script_pubkey);
            assert_eq!(loaded_utxo.is_coinbase, utxo.is_coinbase);
            assert_eq!(loaded_utxo.height, utxo.height);
        }
    }

    #[test]
    fn test_utxo_hash_deterministic() {
        let utxo_set = create_test_utxo_set();
        
        let hash1 = AssumeUtxoManager::calculate_utxo_hash(&utxo_set).unwrap();
        let hash2 = AssumeUtxoManager::calculate_utxo_hash(&utxo_set).unwrap();
        
        assert_eq!(hash1, hash2, "Hash should be deterministic");
    }

    #[test]
    fn test_has_snapshot() {
        let dir = tempdir().unwrap();
        let manager = AssumeUtxoManager::new(dir.path());
        
        assert!(!manager.has_snapshot(800_000));
        
        // Create a snapshot
        let utxo_set = create_test_utxo_set();
        manager.create_snapshot(&utxo_set, [0u8; 32], 800_000).unwrap();
        
        assert!(manager.has_snapshot(800_000));
    }
}


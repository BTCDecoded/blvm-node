//! Bitcoin Core block file reader
//!
//! Reads blocks from Bitcoin Core's `blk*.dat` files.
//! Bitcoin Core stores blocks in a simple format:
//! - Magic bytes (4 bytes): 0xF9BEB4D9 for mainnet
//! - Block size (4 bytes, little-endian)
//! - Block data (variable length)

use crate::storage::bitcoin_core_detection::BitcoinCoreNetwork;
use anyhow::{Context, Result};
use blvm_consensus::serialization::block::deserialize_block;
use blvm_protocol::{Block, Hash};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Magic bytes for Bitcoin Core block files
const MAGIC_MAINNET: [u8; 4] = [0xF9, 0xBE, 0xB4, 0xD9];
const MAGIC_TESTNET: [u8; 4] = [0x0B, 0x11, 0x09, 0x07];
const MAGIC_REGTEST: [u8; 4] = [0xFA, 0xBF, 0xB5, 0xDA];
const MAGIC_SIGNET: [u8; 4] = [0x0A, 0x03, 0xCF, 0x40];

/// Location of a block within a block file
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlockLocation {
    file_path: PathBuf,
    offset: u64,
    size: u32,
}

/// Serialized block index for disk persistence
#[derive(Serialize, Deserialize)]
struct BlockIndexCache {
    version: u32,
    network: String,
    blocks_dir: PathBuf,
    entries: Vec<(Hash, BlockLocation)>,
}

/// Bitcoin Core block file reader
///
/// Reads blocks from Bitcoin Core's `blk*.dat` files.
/// Builds an index of block hashes to file locations for fast lookups.
pub struct BitcoinCoreBlockReader {
    blocks_dir: PathBuf,
    network: BitcoinCoreNetwork,
    /// Index mapping block hash to file location
    /// Lazy-loaded on first access, optionally persisted to disk
    block_index: Arc<Mutex<Option<HashMap<Hash, BlockLocation>>>>,
    /// Path to persisted index file (if using disk cache)
    index_cache_path: Option<PathBuf>,
}

impl BitcoinCoreBlockReader {
    /// Create a new block file reader
    ///
    /// `blocks_dir` should point to the `blocks/` directory containing `blk*.dat` files.
    pub fn new(blocks_dir: &Path, network: BitcoinCoreNetwork) -> Result<Self> {
        Self::new_with_cache(blocks_dir, network, None)
    }

    /// Create a new block file reader with optional index cache
    ///
    /// If `cache_dir` is provided, the block index will be persisted to disk
    /// for faster subsequent loads. The cache file is named `block_index_{network}.bin`.
    pub fn new_with_cache(
        blocks_dir: &Path,
        network: BitcoinCoreNetwork,
        cache_dir: Option<&Path>,
    ) -> Result<Self> {
        if !blocks_dir.exists() {
            return Err(anyhow::anyhow!("Blocks directory does not exist: {:?}", blocks_dir));
        }

        let index_cache_path = cache_dir.map(|dir| {
            let network_str = match network {
                BitcoinCoreNetwork::Mainnet => "mainnet",
                BitcoinCoreNetwork::Testnet => "testnet",
                BitcoinCoreNetwork::Regtest => "regtest",
                BitcoinCoreNetwork::Signet => "signet",
            };
            dir.join(format!("block_index_{}.bin", network_str))
        });

        Ok(Self {
            blocks_dir: blocks_dir.to_path_buf(),
            network,
            block_index: Arc::new(Mutex::new(None)),
            index_cache_path,
        })
    }

    /// Get the magic bytes for the network
    fn get_magic(&self) -> &[u8; 4] {
        match self.network {
            BitcoinCoreNetwork::Mainnet => &MAGIC_MAINNET,
            BitcoinCoreNetwork::Testnet => &MAGIC_TESTNET,
            BitcoinCoreNetwork::Regtest => &MAGIC_REGTEST,
            BitcoinCoreNetwork::Signet => &MAGIC_SIGNET,
        }
    }

    /// Build the block index by scanning all `blk*.dat` files
    ///
    /// This is expensive but only needs to be done once.
    /// The index is cached for subsequent lookups.
    fn build_index(&self) -> Result<HashMap<Hash, BlockLocation>> {
        let mut index = HashMap::new();

        // Find all blk*.dat files
        let entries = std::fs::read_dir(&self.blocks_dir)
            .with_context(|| format!("Failed to read blocks directory: {:?}", self.blocks_dir))?;

        let mut block_files: Vec<PathBuf> = entries
            .filter_map(|entry| {
                entry.ok().and_then(|e| {
                    let path = e.path();
                    if path.is_file() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if name.starts_with("blk") && name.ends_with(".dat") {
                                return Some(path);
                            }
                        }
                    }
                    None
                })
            })
            .collect();

        // Sort by filename to process in order
        block_files.sort();

        let magic = self.get_magic();

        // Scan each block file
        for file_path in block_files {
            if let Err(e) = self.scan_block_file(&file_path, magic, &mut index) {
                // Log error but continue with other files
                eprintln!("Warning: Failed to scan block file {:?}: {}", file_path, e);
            }
        }

        Ok(index)
    }

    /// Scan a single block file and add blocks to the index
    fn scan_block_file(
        &self,
        file_path: &Path,
        magic: &[u8; 4],
        index: &mut HashMap<Hash, BlockLocation>,
    ) -> Result<()> {
        let mut file = File::open(file_path)
            .with_context(|| format!("Failed to open block file: {:?}", file_path))?;

        let mut offset = 0u64;

        loop {
            // Read magic bytes
            let mut magic_buf = [0u8; 4];
            match file.read_exact(&mut magic_buf) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // End of file
                    break;
                }
                Err(e) => return Err(anyhow::anyhow!("Failed to read magic bytes: {}", e)),
            }

            // Verify magic bytes
            if magic_buf != *magic {
                // Not a valid block start, try to find next magic sequence
                // This handles corrupted files or partial writes
                offset += 1;
                file.seek(SeekFrom::Start(offset))?;
                continue;
            }

            offset += 4;

            // Read block size
            let mut size_buf = [0u8; 4];
            file.read_exact(&mut size_buf)
                .context("Failed to read block size")?;
            let block_size = u32::from_le_bytes(size_buf);
            offset += 4;

            // Validate block size (reasonable limit: 32MB)
            if block_size > 32 * 1024 * 1024 {
                return Err(anyhow::anyhow!(
                    "Block size too large: {} bytes at offset {}",
                    block_size,
                    offset - 4
                ));
            }

            // Read block header to compute hash
            let mut header_buf = [0u8; 80];
            file.read_exact(&mut header_buf)
                .context("Failed to read block header")?;

            // Compute block hash (double SHA256 of header)
            use sha2::{Digest, Sha256};
            let first_hash = Sha256::digest(header_buf);
            let second_hash = Sha256::digest(first_hash);
            let mut block_hash = [0u8; 32];
            block_hash.copy_from_slice(&second_hash);

            // Store location
            let location = BlockLocation {
                file_path: file_path.to_path_buf(),
                offset: offset - 88, // Start of block (magic + size + header)
                size: block_size,
            };

            index.insert(block_hash, location);

            // Skip to next block
            let remaining = block_size as u64 - 80; // Already read header
            file.seek(SeekFrom::Current(remaining as i64))
                .context("Failed to seek to next block")?;
            offset += remaining;
        }

        Ok(())
    }

    /// Get or build the block index (lazy initialization)
    ///
    /// First tries to load from disk cache if available, then builds from block files.
    fn get_index(&self) -> Result<Arc<HashMap<Hash, BlockLocation>>> {
        let mut index_guard = self.block_index.lock().unwrap();

        if index_guard.is_none() {
            // Try to load from cache first
            if let Some(cache_path) = &self.index_cache_path {
                if let Ok(index) = self.load_index_from_cache(cache_path) {
                    *index_guard = Some(index);
                } else {
                    // Cache load failed, build from scratch
                    let index = self.build_index()?;
                    // Save to cache for next time
                    if let Err(e) = self.save_index_to_cache(cache_path, &index) {
                        eprintln!("Warning: Failed to save block index cache: {}", e);
                    }
                    *index_guard = Some(index);
                }
            } else {
                // No cache, build from scratch
                *index_guard = Some(self.build_index()?);
            }
        }

        // Create a new Arc with the HashMap
        // Note: We can't return a reference to the HashMap inside the Mutex,
        // so we clone it. For large indexes, this could be expensive.
        // In practice, the index is built once and cached.
        Ok(Arc::new(index_guard.as_ref().unwrap().clone()))
    }

    /// Load block index from disk cache
    fn load_index_from_cache(&self, cache_path: &Path) -> Result<HashMap<Hash, BlockLocation>> {
        use std::fs::File;
        use std::io::Read;

        let mut file = File::open(cache_path)
            .with_context(|| format!("Failed to open index cache: {:?}", cache_path))?;

        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .context("Failed to read index cache")?;

        let cache: BlockIndexCache = bincode::deserialize(&data)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize index cache: {}", e))?;

        // Verify cache is for the correct network and blocks directory
        let network_str = match self.network {
            BitcoinCoreNetwork::Mainnet => "mainnet",
            BitcoinCoreNetwork::Testnet => "testnet",
            BitcoinCoreNetwork::Regtest => "regtest",
            BitcoinCoreNetwork::Signet => "signet",
        };

        if cache.network != network_str {
            return Err(anyhow::anyhow!(
                "Cache network mismatch: expected {}, got {}",
                network_str,
                cache.network
            ));
        }

        if cache.blocks_dir != self.blocks_dir {
            return Err(anyhow::anyhow!(
                "Cache blocks directory mismatch: expected {:?}, got {:?}",
                self.blocks_dir,
                cache.blocks_dir
            ));
        }

        // Convert Vec to HashMap
        Ok(cache.entries.into_iter().collect())
    }

    /// Save block index to disk cache
    fn save_index_to_cache(
        &self,
        cache_path: &Path,
        index: &HashMap<Hash, BlockLocation>,
    ) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        // Create parent directory if it doesn't exist
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create cache directory: {:?}", parent))?;
        }

        let network_str = match self.network {
            BitcoinCoreNetwork::Mainnet => "mainnet",
            BitcoinCoreNetwork::Testnet => "testnet",
            BitcoinCoreNetwork::Regtest => "regtest",
            BitcoinCoreNetwork::Signet => "signet",
        };

        let cache = BlockIndexCache {
            version: 1,
            network: network_str.to_string(),
            blocks_dir: self.blocks_dir.clone(),
            entries: index.iter().map(|(k, v)| (*k, v.clone())).collect(),
        };

        let data = bincode::serialize(&cache)
            .map_err(|e| anyhow::anyhow!("Failed to serialize index cache: {}", e))?;

        let mut file = File::create(cache_path)
            .with_context(|| format!("Failed to create index cache: {:?}", cache_path))?;

        file.write_all(&data)
            .context("Failed to write index cache")?;

        Ok(())
    }

    /// Read a block from the block files by hash
    pub fn read_block(&self, hash: &Hash) -> Result<Option<Block>> {
        let index = self.get_index()?;

        let location = match index.get(hash) {
            Some(loc) => loc,
            None => return Ok(None),
        };

        // Open the file and seek to block location
        let mut file = File::open(&location.file_path)
            .with_context(|| format!("Failed to open block file: {:?}", location.file_path))?;

        file.seek(SeekFrom::Start(location.offset))
            .with_context(|| {
                format!(
                    "Failed to seek to block offset {} in file {:?}",
                    location.offset, location.file_path
                )
            })?;

        // Read magic bytes (skip)
        let mut magic_buf = [0u8; 4];
        file.read_exact(&mut magic_buf)
            .context("Failed to read magic bytes")?;

        // Read block size (skip, we already know it)
        let mut size_buf = [0u8; 4];
        file.read_exact(&mut size_buf)
            .context("Failed to read block size")?;

        // Read block data
        let mut block_data = vec![0u8; location.size as usize];
        file.read_exact(&mut block_data)
            .with_context(|| {
                format!(
                    "Failed to read block data (size: {}) from file {:?}",
                    location.size, location.file_path
                )
            })?;

        // Deserialize block
        let block = deserialize_block(&block_data)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize block: {}", e))?;

        Ok(Some(block))
    }

    /// Check if a block exists in the block files
    pub fn has_block(&self, hash: &Hash) -> Result<bool> {
        let index = self.get_index()?;
        Ok(index.contains_key(hash))
    }

    /// Get the number of blocks indexed
    pub fn block_count(&self) -> Result<usize> {
        let index = self.get_index()?;
        Ok(index.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs::File;
    use std::io::Write;

    fn create_test_block_file(dir: &Path, filename: &str, blocks: &[&[u8]]) -> Result<()> {
        let file_path = dir.join(filename);
        let mut file = File::create(&file_path)?;

        for block_data in blocks {
            // Write magic bytes
            file.write_all(&MAGIC_MAINNET)?;
            // Write block size
            let size = block_data.len() as u32;
            file.write_all(&size.to_le_bytes())?;
            // Write block data
            file.write_all(block_data)?;
        }

        Ok(())
    }

    #[test]
    fn test_block_reader_creation() {
        let temp_dir = TempDir::new().unwrap();
        let blocks_dir = temp_dir.path().join("blocks");
        std::fs::create_dir_all(&blocks_dir).unwrap();

        // Test that reader can be created
        let reader = BitcoinCoreBlockReader::new(&blocks_dir, BitcoinCoreNetwork::Mainnet);
        assert!(reader.is_ok());
        
        let reader = reader.unwrap();
        // Test that block_count works (should return 0 for empty directory)
        assert_eq!(reader.block_count().unwrap(), 0);
    }

    #[test]
    fn test_block_reader_nonexistent_dir() {
        let temp_dir = TempDir::new().unwrap();
        let blocks_dir = temp_dir.path().join("nonexistent");
        
        // Test that reader fails gracefully for nonexistent directory
        let reader = BitcoinCoreBlockReader::new(&blocks_dir, BitcoinCoreNetwork::Mainnet);
        assert!(reader.is_err());
    }
}


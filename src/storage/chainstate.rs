//! Chain state storage implementation
//!
//! Stores chain metadata including tip, height, and chain parameters.

use crate::storage::database::{Database, Tree};
use anyhow::Result;
use bllvm_protocol::{BlockHeader, Hash};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// UTXO set statistics (cached for fast RPC lookups)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UTXOStats {
    pub height: u64,
    pub txouts: u64,
    pub total_amount: u128, // Total in satoshis
    pub hash_serialized_2: [u8; 32],
    pub transactions: u64,
}

/// Chain state information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    pub tip_hash: Hash,
    pub tip_header: BlockHeader,
    pub height: u64,
    pub total_work: u64,
    pub chain_params: ChainParams,
}

/// Chain parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainParams {
    pub network: String,
    pub genesis_hash: Hash,
    pub max_target: u64,
    pub subsidy_halving_interval: u64,
}

impl Default for ChainParams {
    fn default() -> Self {
        Self {
            network: "mainnet".to_string(),
            genesis_hash: Hash::default(),
            max_target: 0x00000000ffff0000u64,
            subsidy_halving_interval: 210000,
        }
    }
}

/// Chain state storage manager
pub struct ChainState {
    #[allow(dead_code)]
    db: Arc<dyn Database>,
    chain_info: Arc<dyn Tree>,
    work_cache: Arc<dyn Tree>, // work per block (individual block work)
    chainwork_cache: Arc<dyn Tree>, // cumulative chainwork per block (for fast RPC lookups)
    utxo_stats_cache: Arc<dyn Tree>, // UTXO set statistics per block (for fast gettxoutsetinfo)
    network_hashrate_cache: Arc<dyn Tree>, // Network hashrate cache (for fast getmininginfo)
    invalid_blocks: Arc<dyn Tree>,
    chain_tips: Arc<dyn Tree>,
}

impl ChainState {
    /// Create a new chain state store
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        let chain_info = Arc::from(db.open_tree("chain_info")?);
        let work_cache = Arc::from(db.open_tree("work_cache")?);
        let chainwork_cache = Arc::from(db.open_tree("chainwork_cache")?);
        let utxo_stats_cache = Arc::from(db.open_tree("utxo_stats_cache")?);
        let network_hashrate_cache = Arc::from(db.open_tree("network_hashrate_cache")?);
        let invalid_blocks = Arc::from(db.open_tree("invalid_blocks")?);
        let chain_tips = Arc::from(db.open_tree("chain_tips")?);

        Ok(Self {
            db,
            chain_info,
            work_cache,
            chainwork_cache,
            utxo_stats_cache,
            network_hashrate_cache,
            invalid_blocks,
            chain_tips,
        })
    }

    /// Initialize chain state with genesis block
    pub fn initialize(&self, genesis_header: &BlockHeader) -> Result<()> {
        let chain_info = ChainInfo {
            tip_hash: self.calculate_hash(genesis_header),
            tip_header: genesis_header.clone(),
            height: 0,
            total_work: 0,
            chain_params: ChainParams::default(),
        };

        self.store_chain_info(&chain_info)?;
        Ok(())
    }

    /// Store chain information
    pub fn store_chain_info(&self, info: &ChainInfo) -> Result<()> {
        let data = bincode::serialize(info)?;
        self.chain_info.insert(b"current", &data)?;
        Ok(())
    }

    /// Load current chain information
    pub fn load_chain_info(&self) -> Result<Option<ChainInfo>> {
        if let Some(data) = self.chain_info.get(b"current")? {
            let info: ChainInfo = bincode::deserialize(&data)?;
            Ok(Some(info))
        } else {
            Ok(None)
        }
    }

    /// Calculate difficulty from block bits (compact target format)
    /// Difficulty = MAX_TARGET / target
    /// For display purposes, normalized to genesis difficulty = 1.0
    fn calculate_difficulty(bits: u64) -> f64 {
        const MAX_TARGET: u64 = 0x00000000FFFF0000u64;

        // Expand target from compact format
        let exponent = (bits >> 24) as u8;
        let mantissa = bits & 0x00ffffff;

        if mantissa == 0 {
            return 1.0;
        }

        // Calculate target: target = mantissa * 2^(8*(exponent-3))
        let target = if exponent <= 3 {
            mantissa >> (8 * (3 - exponent))
        } else {
            mantissa << (8 * (exponent - 3))
        };

        if target == 0 {
            return 1.0;
        }

        // Difficulty = MAX_TARGET / target
        MAX_TARGET as f64 / target as f64
    }

    /// Calculate work from block bits (compact target format)
    /// Work = 2^256 / (target + 1) for Bitcoin
    /// Simplified: work = u128::MAX / (target + 1)
    fn calculate_work_from_bits(bits: u64) -> u64 {
        // Expand target from compact format
        let exponent = (bits >> 24) as u8;
        let mantissa = bits & 0x00ffffff;

        if mantissa == 0 {
            return 0;
        }

        // Calculate target: target = mantissa * 2^(8*(exponent-3))
        // For simplicity, use a simplified calculation
        // Prevent overflow by capping the shift amount
        let target = if exponent <= 3 {
            let shift = 8 * (3 - exponent);
            if shift >= 64 {
                0 // Shift would overflow, return 0
            } else {
                mantissa >> shift
            }
        } else {
            let shift = 8 * (exponent - 3);
            if shift >= 64 {
                u64::MAX // Shift would overflow, return max value
            } else {
                mantissa << shift
            }
        };

        // Work = MAX_TARGET / (target + 1)
        // Use u64::MAX as approximation for MAX_TARGET
        if target == 0 || target == u64::MAX {
            return 1; // Minimum work
        }

        // Prevent division by zero
        u64::MAX / (target + 1).max(1)
    }

    /// Update chain tip and calculate incremental chainwork
    /// This should be called when a new block is connected to the chain
    pub fn update_tip(&self, tip_hash: &Hash, tip_header: &BlockHeader, height: u64) -> Result<()> {
        if let Some(mut info) = self.load_chain_info()? {
            // Calculate work for this block
            let block_work = Self::calculate_work_from_bits(tip_header.bits);
            self.store_work(tip_hash, block_work)?;

            // Calculate cumulative chainwork: chainwork[new] = chainwork[prev] + work[new]
            let prev_chainwork = if height > 0 {
                // Get previous block hash
                if let Ok(Some(prev_hash)) = self.get_prev_block_hash(tip_header) {
                    self.get_chainwork(&prev_hash)?.unwrap_or(0)
                } else {
                    0
                }
            } else {
                // Genesis block: chainwork = work
                0
            };

            let new_chainwork = prev_chainwork + block_work as u128;
            self.store_chainwork(tip_hash, new_chainwork)?;

            info.tip_hash = *tip_hash;
            info.tip_header = tip_header.clone();
            info.height = height;
            self.store_chain_info(&info)?;
        }
        Ok(())
    }

    /// Get previous block hash from header
    fn get_prev_block_hash(&self, header: &BlockHeader) -> Result<Option<Hash>> {
        Ok(Some(header.prev_block_hash))
    }

    /// Get current chain height
    pub fn get_height(&self) -> Result<Option<u64>> {
        if let Some(info) = self.load_chain_info()? {
            Ok(Some(info.height))
        } else {
            Ok(None)
        }
    }

    /// Get current chain tip hash
    pub fn get_tip_hash(&self) -> Result<Option<Hash>> {
        if let Some(info) = self.load_chain_info()? {
            Ok(Some(info.tip_hash))
        } else {
            Ok(None)
        }
    }

    /// Get current chain tip header
    pub fn get_tip_header(&self) -> Result<Option<BlockHeader>> {
        if let Some(info) = self.load_chain_info()? {
            Ok(Some(info.tip_header))
        } else {
            Ok(None)
        }
    }

    /// Store work for a block
    pub fn store_work(&self, hash: &Hash, work: u64) -> Result<()> {
        let key = hash.as_slice();
        let value = work.to_be_bytes();
        self.work_cache.insert(key, &value)?;
        Ok(())
    }

    /// Get work for a block
    pub fn get_work(&self, hash: &Hash) -> Result<Option<u64>> {
        let key = hash.as_slice();
        if let Some(data) = self.work_cache.get(key)? {
            let work = u64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            Ok(Some(work))
        } else {
            Ok(None)
        }
    }

    /// Store cumulative chainwork for a block
    /// Chainwork is the sum of work from genesis to this block
    pub fn store_chainwork(&self, hash: &Hash, chainwork: u128) -> Result<()> {
        let key = hash.as_slice();
        let value = chainwork.to_be_bytes();
        self.chainwork_cache.insert(key, &value)?;
        Ok(())
    }

    /// Get cumulative chainwork for a block
    /// Returns the sum of work from genesis to this block (O(1) lookup)
    pub fn get_chainwork(&self, hash: &Hash) -> Result<Option<u128>> {
        let key = hash.as_slice();
        if let Some(data) = self.chainwork_cache.get(key)? {
            // Ensure we have at least 16 bytes for u128
            if data.len() >= 16 {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&data[..16]);
                Ok(Some(u128::from_be_bytes(bytes)))
            } else {
                // Handle shorter data (shouldn't happen, but be defensive)
                let mut bytes = [0u8; 16];
                for (i, &byte) in data.iter().enumerate() {
                    if i < 16 {
                        bytes[15 - i] = byte; // Pad from right
                    }
                }
                Ok(Some(u128::from_be_bytes(bytes)))
            }
        } else {
            Ok(None)
        }
    }

    /// Calculate total chain work
    pub fn calculate_total_work(&self) -> Result<u64> {
        let mut total = 0u64;

        for result in self.work_cache.iter() {
            let (_, data) = result?;
            let work = u64::from_be_bytes([
                data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
            ]);
            total += work;
        }

        Ok(total)
    }

    /// Check if chain is initialized
    pub fn is_initialized(&self) -> Result<bool> {
        self.chain_info.contains_key(b"current")
    }

    /// Store UTXO set statistics for a block
    /// This caches expensive-to-calculate values for fast RPC lookups
    pub fn store_utxo_stats(&self, block_hash: &Hash, stats: &UTXOStats) -> Result<()> {
        let key = block_hash.as_slice();
        let value = bincode::serialize(stats)?;
        self.utxo_stats_cache.insert(key, &value)?;
        Ok(())
    }

    /// Get UTXO set statistics for a block
    /// Returns cached statistics for fast gettxoutsetinfo RPC calls
    pub fn get_utxo_stats(&self, block_hash: &Hash) -> Result<Option<UTXOStats>> {
        let key = block_hash.as_slice();
        if let Some(data) = self.utxo_stats_cache.get(key)? {
            let stats: UTXOStats = bincode::deserialize(&data)?;
            Ok(Some(stats))
        } else {
            Ok(None)
        }
    }

    /// Get latest UTXO stats (from tip)
    pub fn get_latest_utxo_stats(&self) -> Result<Option<UTXOStats>> {
        if let Some(tip_hash) = self.get_tip_hash()? {
            self.get_utxo_stats(&tip_hash)
        } else {
            Ok(None)
        }
    }

    /// Store network hashrate for a block height
    /// Caches expensive hashrate calculation for fast getmininginfo RPC calls
    pub fn store_network_hashrate(&self, height: u64, hashrate: f64) -> Result<()> {
        let key = height.to_be_bytes();
        let value = hashrate.to_be_bytes();
        self.network_hashrate_cache.insert(&key, &value)?;
        Ok(())
    }

    /// Get cached network hashrate
    /// Returns most recent cached hashrate (from tip or recent block)
    pub fn get_network_hashrate(&self) -> Result<Option<f64>> {
        // Try to get from tip height first
        if let Some(height) = self.get_height()? {
            let key = height.to_be_bytes();
            if let Some(data) = self.network_hashrate_cache.get(&key)? {
                if data.len() >= 8 {
                    let bytes = [
                        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                    ];
                    return Ok(Some(f64::from_be_bytes(bytes)));
                }
            }
            // Fallback: try previous heights (up to 10 blocks back)
            for h in (height.saturating_sub(10)..height).rev() {
                let key = h.to_be_bytes();
                if let Some(data) = self.network_hashrate_cache.get(&key)? {
                    if data.len() >= 8 {
                        let bytes = [
                            data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                        ];
                        return Ok(Some(f64::from_be_bytes(bytes)));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Update UTXO stats cache after a block is connected
    /// This should be called after a block is validated and the UTXO set is updated
    pub fn update_utxo_stats_cache(
        &self,
        block_hash: &Hash,
        height: u64,
        utxo_set: &bllvm_protocol::UtxoSet,
        transaction_count: u64,
    ) -> Result<()> {
        use crate::storage::hashing::double_sha256;

        // Calculate UTXO set statistics
        let txouts = utxo_set.len() as u64;
        let total_amount: u128 = utxo_set.values().map(|utxo| utxo.value as u128).sum();

        // Calculate hash_serialized_2 (double SHA256 of serialized UTXO set)
        // Sort UTXOs for deterministic hashing
        let mut entries: Vec<_> = utxo_set.iter().collect();
        entries.sort_by(|(a, _), (b, _)| match a.hash.cmp(&b.hash) {
            std::cmp::Ordering::Equal => a.index.cmp(&b.index),
            other => other,
        });

        // Serialize each UTXO entry
        let mut serialized = Vec::new();
        for (outpoint, utxo) in entries {
            serialized.extend_from_slice(&outpoint.hash);
            serialized.extend_from_slice(&outpoint.index.to_le_bytes());
            serialized.extend_from_slice(&utxo.value.to_le_bytes());
            serialized.extend_from_slice(&utxo.script_pubkey);
            serialized.extend_from_slice(&utxo.height.to_le_bytes());
        }

        let hash_serialized_2 = double_sha256(&serialized);

        // Store in cache
        let stats = UTXOStats {
            height,
            txouts,
            total_amount,
            hash_serialized_2,
            transactions: transaction_count,
        };

        self.store_utxo_stats(block_hash, &stats)?;
        Ok(())
    }

    /// Calculate and cache network hashrate for a block height
    /// This should be called after a block is connected
    /// Requires access to block storage to get recent block timestamps
    pub fn calculate_and_cache_network_hashrate(
        &self,
        height: u64,
        blocks: &crate::storage::blockstore::BlockStore,
    ) -> Result<()> {
        // Need at least 2 blocks to calculate hashrate
        if height < 1 {
            return Ok(());
        }

        // Get last 144 blocks (approximately 1 day at 10 min/block)
        let num_blocks = (height + 1).min(144);
        let start_height = height.saturating_sub(num_blocks - 1);

        // Get timestamps from blocks
        let mut timestamps = Vec::new();
        for h in start_height..=height {
            if let Ok(Some(hash)) = blocks.get_hash_by_height(h) {
                if let Ok(Some(block)) = blocks.get_block(&hash) {
                    timestamps.push((h, block.header.timestamp));
                }
            }
        }

        if timestamps.len() < 2 {
            return Ok(());
        }

        // Calculate average time between blocks
        let first_timestamp = timestamps[0].1;
        let last_timestamp = timestamps[timestamps.len() - 1].1;
        let time_span = last_timestamp.saturating_sub(first_timestamp);
        let num_intervals = timestamps.len() - 1;

        if time_span == 0 || num_intervals == 0 {
            return Ok(());
        }

        let avg_time_per_block = time_span as f64 / num_intervals as f64;

        // Get difficulty from tip block
        if let Ok(Some(tip_hash)) = blocks.get_hash_by_height(height) {
            if let Ok(Some(tip_block)) = blocks.get_block(&tip_hash) {
                let difficulty = Self::calculate_difficulty(tip_block.header.bits);

                // Calculate hashrate: difficulty * 2^32 / avg_time_per_block
                const HASHES_PER_DIFFICULTY: f64 = 4294967296.0; // 2^32
                let hashrate = (difficulty * HASHES_PER_DIFFICULTY) / avg_time_per_block;

                // Store in cache
                self.store_network_hashrate(height, hashrate)?;
            }
        }

        Ok(())
    }

    /// Reset chain state
    pub fn reset(&self) -> Result<()> {
        self.chain_info.clear()?;
        self.work_cache.clear()?;
        self.chainwork_cache.clear()?;
        self.utxo_stats_cache.clear()?;
        self.network_hashrate_cache.clear()?;
        self.invalid_blocks.clear()?;
        self.chain_tips.clear()?;
        Ok(())
    }

    /// Mark a block as invalid
    pub fn mark_invalid(&self, hash: &Hash) -> Result<()> {
        // Store invalid block with timestamp
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| {
                // Fallback to 0 if system time is before epoch (should never happen)
                tracing::warn!("System time is before UNIX epoch, using 0 as timestamp");
                std::time::Duration::from_secs(0)
            })
            .as_secs();
        let value = timestamp.to_be_bytes();
        self.invalid_blocks.insert(hash.as_slice(), &value)?;
        Ok(())
    }

    /// Remove a block from invalid blocks (reconsider)
    pub fn unmark_invalid(&self, hash: &Hash) -> Result<()> {
        self.invalid_blocks.remove(hash.as_slice())?;
        Ok(())
    }

    /// Check if a block is marked as invalid
    pub fn is_invalid(&self, hash: &Hash) -> Result<bool> {
        self.invalid_blocks.contains_key(hash.as_slice())
    }

    /// Get all invalid block hashes
    pub fn get_invalid_blocks(&self) -> Result<Vec<Hash>> {
        let mut invalid = Vec::new();
        for result in self.invalid_blocks.iter() {
            let (key, _) = result?;
            if key.len() == 32 {
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&key);
                invalid.push(hash);
            }
        }
        Ok(invalid)
    }

    /// Add a chain tip (for fork tracking)
    pub fn add_chain_tip(
        &self,
        hash: &Hash,
        height: u64,
        branchlen: u64,
        status: &str,
    ) -> Result<()> {
        #[derive(Serialize, Deserialize)]
        struct TipInfo {
            height: u64,
            branchlen: u64,
            status: String,
        }

        let tip_info = TipInfo {
            height,
            branchlen,
            status: status.to_string(),
        };
        let data = bincode::serialize(&tip_info)?;
        self.chain_tips.insert(hash.as_slice(), &data)?;
        Ok(())
    }

    /// Remove a chain tip
    pub fn remove_chain_tip(&self, hash: &Hash) -> Result<()> {
        self.chain_tips.remove(hash.as_slice())?;
        Ok(())
    }

    /// Get all chain tips
    pub fn get_chain_tips(&self) -> Result<Vec<(Hash, u64, u64, String)>> {
        #[derive(Deserialize)]
        struct TipInfo {
            height: u64,
            branchlen: u64,
            status: String,
        }

        let mut tips = Vec::new();
        for result in self.chain_tips.iter() {
            let (key, data) = result?;
            if key.len() == 32 {
                if let Ok(tip_info) = bincode::deserialize::<TipInfo>(&data) {
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&key);
                    tips.push((hash, tip_info.height, tip_info.branchlen, tip_info.status));
                }
            }
        }
        Ok(tips)
    }

    /// Calculate block hash using proper Bitcoin double SHA256
    fn calculate_hash(&self, header: &BlockHeader) -> Hash {
        use crate::storage::hashing::double_sha256;

        // Serialize block header for hashing
        let mut header_data = Vec::new();
        header_data.extend_from_slice(&header.version.to_le_bytes());
        header_data.extend_from_slice(&header.prev_block_hash);
        header_data.extend_from_slice(&header.merkle_root);
        header_data.extend_from_slice(&header.timestamp.to_le_bytes());
        header_data.extend_from_slice(&header.bits.to_le_bytes());
        header_data.extend_from_slice(&header.nonce.to_le_bytes());

        // Calculate Bitcoin double SHA256 hash
        double_sha256(&header_data)
    }
}

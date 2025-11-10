//! Chain state storage implementation
//!
//! Stores chain metadata including tip, height, and chain parameters.

use crate::storage::database::{Database, Tree};
use anyhow::Result;
use bllvm_protocol::{BlockHeader, Hash};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
    work_cache: Arc<dyn Tree>,
    invalid_blocks: Arc<dyn Tree>,
    chain_tips: Arc<dyn Tree>,
}

impl ChainState {
    /// Create a new chain state store
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        let chain_info = Arc::from(db.open_tree("chain_info")?);
        let work_cache = Arc::from(db.open_tree("work_cache")?);
        let invalid_blocks = Arc::from(db.open_tree("invalid_blocks")?);
        let chain_tips = Arc::from(db.open_tree("chain_tips")?);

        Ok(Self {
            db,
            chain_info,
            work_cache,
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

    /// Update chain tip
    pub fn update_tip(&self, tip_hash: &Hash, tip_header: &BlockHeader, height: u64) -> Result<()> {
        if let Some(mut info) = self.load_chain_info()? {
            info.tip_hash = *tip_hash;
            info.tip_header = tip_header.clone();
            info.height = height;
            self.store_chain_info(&info)?;
        }
        Ok(())
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
        Ok(self.chain_info.contains_key(b"current")?)
    }

    /// Reset chain state
    pub fn reset(&self) -> Result<()> {
        self.chain_info.clear()?;
        self.work_cache.clear()?;
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
            .unwrap()
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
        Ok(self.invalid_blocks.contains_key(hash.as_slice())?)
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

//! Chain state storage implementation
//!
//! Stores chain metadata including tip, height, and chain parameters.

use anyhow::Result;
use protocol_engine::{BlockHeader, Hash};
use serde::{Deserialize, Serialize};
use sled::Db;

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
    db: Db,
    chain_info: sled::Tree,
    work_cache: sled::Tree,
}

impl ChainState {
    /// Create a new chain state store
    pub fn new(db: Db) -> Result<Self> {
        let chain_info = db.open_tree("chain_info")?;
        let work_cache = db.open_tree("work_cache")?;

        Ok(Self {
            db,
            chain_info,
            work_cache,
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
        self.chain_info.insert(b"current", data)?;
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
        Ok(())
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

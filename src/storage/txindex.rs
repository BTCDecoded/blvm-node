//! Transaction index implementation
//!
//! Provides fast lookup of transactions by hash and maintains transaction metadata.

use anyhow::Result;
use protocol_engine::{Hash, Transaction};
use serde::{Deserialize, Serialize};
use sled::Db;

/// Transaction metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxMetadata {
    pub tx_hash: Hash,
    pub block_hash: Hash,
    pub block_height: u64,
    pub tx_index: u32,
    pub size: u32,
    pub weight: u32,
}

/// Transaction index storage manager
pub struct TxIndex {
    #[allow(dead_code)]
    db: Db,
    tx_by_hash: sled::Tree,
    tx_by_block: sled::Tree,
    tx_metadata: sled::Tree,
}

impl TxIndex {
    /// Create a new transaction index
    pub fn new(db: Db) -> Result<Self> {
        let tx_by_hash = db.open_tree("tx_by_hash")?;
        let tx_by_block = db.open_tree("tx_by_block")?;
        let tx_metadata = db.open_tree("tx_metadata")?;

        Ok(Self {
            db,
            tx_by_hash,
            tx_by_block,
            tx_metadata,
        })
    }

    /// Index a transaction
    pub fn index_transaction(
        &self,
        tx: &Transaction,
        block_hash: &Hash,
        block_height: u64,
        tx_index: u32,
    ) -> Result<()> {
        let tx_hash = self.calculate_tx_hash(tx);
        let tx_data = bincode::serialize(tx)?;

        // Store transaction by hash
        self.tx_by_hash.insert(tx_hash.as_slice(), tx_data)?;

        // Store transaction metadata
        let metadata = TxMetadata {
            tx_hash,
            block_hash: *block_hash,
            block_height,
            tx_index,
            size: self.calculate_tx_size(tx),
            weight: self.calculate_tx_weight(tx),
        };

        let metadata_data = bincode::serialize(&metadata)?;
        self.tx_metadata.insert(tx_hash.as_slice(), metadata_data)?;

        // Index by block
        let block_key = self.block_tx_key(block_hash, tx_index);
        self.tx_by_block.insert(block_key, tx_hash.as_slice())?;

        Ok(())
    }

    /// Get transaction by hash
    pub fn get_transaction(&self, tx_hash: &Hash) -> Result<Option<Transaction>> {
        if let Some(data) = self.tx_by_hash.get(tx_hash.as_slice())? {
            let tx: Transaction = bincode::deserialize(&data)?;
            Ok(Some(tx))
        } else {
            Ok(None)
        }
    }

    /// Get transaction metadata
    pub fn get_metadata(&self, tx_hash: &Hash) -> Result<Option<TxMetadata>> {
        if let Some(data) = self.tx_metadata.get(tx_hash.as_slice())? {
            let metadata: TxMetadata = bincode::deserialize(&data)?;
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    }

    /// Get all transactions in a block
    pub fn get_block_transactions(&self, block_hash: &Hash) -> Result<Vec<Transaction>> {
        let mut transactions = Vec::new();
        let mut tx_index = 0u32;

        loop {
            let block_key = self.block_tx_key(block_hash, tx_index);
            if let Some(tx_hash_data) = self.tx_by_block.get(block_key)? {
                let mut tx_hash = [0u8; 32];
                tx_hash.copy_from_slice(&tx_hash_data);
                if let Some(tx) = self.get_transaction(&tx_hash)? {
                    transactions.push(tx);
                    tx_index += 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(transactions)
    }

    /// Check if transaction exists
    pub fn has_transaction(&self, tx_hash: &Hash) -> Result<bool> {
        Ok(self.tx_by_hash.contains_key(tx_hash.as_slice())?)
    }

    /// Get transaction count
    pub fn transaction_count(&self) -> Result<usize> {
        Ok(self.tx_by_hash.len())
    }

    /// Get transactions by block height range
    pub fn get_transactions_by_height_range(
        &self,
        _start_height: u64,
        _end_height: u64,
    ) -> Result<Vec<Transaction>> {
        let transactions = Vec::new();

        // This is a simplified implementation
        // In a real implementation, we'd need to track block hashes by height
        // For now, we'll just return empty results

        Ok(transactions)
    }

    /// Remove transaction from index
    pub fn remove_transaction(&self, tx_hash: &Hash) -> Result<()> {
        if let Some(metadata) = self.get_metadata(tx_hash)? {
            let block_key = self.block_tx_key(&metadata.block_hash, metadata.tx_index);
            self.tx_by_block.remove(block_key)?;
        }

        self.tx_by_hash.remove(tx_hash.as_slice())?;
        self.tx_metadata.remove(tx_hash.as_slice())?;

        Ok(())
    }

    /// Clear all transactions
    pub fn clear(&self) -> Result<()> {
        self.tx_by_hash.clear()?;
        self.tx_by_block.clear()?;
        self.tx_metadata.clear()?;
        Ok(())
    }

    /// Calculate transaction hash using proper Bitcoin double SHA256
    fn calculate_tx_hash(&self, tx: &Transaction) -> Hash {
        use crate::storage::hashing::double_sha256;

        // Serialize transaction for hashing
        let mut tx_data = Vec::new();
        tx_data.extend_from_slice(&tx.version.to_le_bytes());

        // Input count (varint)
        tx_data.extend_from_slice(&Self::encode_varint(tx.inputs.len() as u64));
        for input in &tx.inputs {
            tx_data.extend_from_slice(&input.prevout.hash);
            tx_data.extend_from_slice(&input.prevout.index.to_le_bytes());
            tx_data.extend_from_slice(&Self::encode_varint(input.script_sig.len() as u64));
            tx_data.extend_from_slice(&input.script_sig);
            tx_data.extend_from_slice(&input.sequence.to_le_bytes());
        }

        // Output count (varint)
        tx_data.extend_from_slice(&Self::encode_varint(tx.outputs.len() as u64));
        for output in &tx.outputs {
            tx_data.extend_from_slice(&output.value.to_le_bytes());
            tx_data.extend_from_slice(&Self::encode_varint(output.script_pubkey.len() as u64));
            tx_data.extend_from_slice(&output.script_pubkey);
        }

        tx_data.extend_from_slice(&tx.lock_time.to_le_bytes());

        // Calculate Bitcoin double SHA256 hash
        double_sha256(&tx_data)
    }

    /// Encode integer as Bitcoin varint
    fn encode_varint(value: u64) -> Vec<u8> {
        if value < 0xfd {
            vec![value as u8]
        } else if value <= 0xffff {
            let mut result = vec![0xfd];
            result.extend_from_slice(&(value as u16).to_le_bytes());
            result
        } else if value <= 0xffffffff {
            let mut result = vec![0xfe];
            result.extend_from_slice(&(value as u32).to_le_bytes());
            result
        } else {
            let mut result = vec![0xff];
            result.extend_from_slice(&value.to_le_bytes());
            result
        }
    }

    /// Calculate transaction size
    fn calculate_tx_size(&self, tx: &Transaction) -> u32 {
        // Simplified size calculation
        let mut size = 4; // version
        size += 1; // input count
        for input in &tx.inputs {
            size += 32; // previous output
            size += 1; // script length
            size += input.script_sig.len() as u32;
            size += 4; // sequence
        }
        size += 1; // output count
        for output in &tx.outputs {
            size += 8; // value
            size += 1; // script length
            size += output.script_pubkey.len() as u32;
        }
        size += 4; // lock time
        size
    }

    /// Calculate transaction weight
    fn calculate_tx_weight(&self, tx: &Transaction) -> u32 {
        // Simplified weight calculation (4x for witness data)
        self.calculate_tx_size(tx) * 4
    }

    /// Create block transaction key
    fn block_tx_key(&self, block_hash: &Hash, tx_index: u32) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(block_hash.as_slice());
        key.extend_from_slice(&tx_index.to_be_bytes());
        key
    }
}

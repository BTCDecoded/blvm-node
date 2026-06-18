//! Transaction index implementation
//!
//! Provides fast lookup of transactions by hash and maintains transaction metadata.

use crate::config::{IndexingConfig, IndexingStrategy};
use crate::storage::database::{Database, Tree};
use crate::storage::hashing::sha256;
use anyhow::{Context, Result};
use blvm_protocol::{Hash, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Address output entry (internal helper)
#[derive(Debug, Clone)]
struct AddressOutputEntry {
    tx_hash: Hash,
    output_index: u32,
}

/// Value entry (internal helper)
#[derive(Debug, Clone)]
struct ValueEntry {
    tx_hash: Hash,
    output_index: u32,
    value: u64,
}

/// Background work: index advanced fields for an entire block off the hot path.
struct BackgroundBlockJob {
    block: blvm_protocol::Block,
    block_hash: Hash,
    block_height: u64,
}

/// Indexing statistics for monitoring
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub total_transactions: usize,
    pub address_index_enabled: bool,
    pub value_index_enabled: bool,
    pub indexed_addresses: usize,
    pub indexed_value_buckets: usize,
}

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
    db: Arc<dyn Database>,
    tx_by_hash: Arc<dyn Tree>,
    tx_by_block: Arc<dyn Tree>,
    tx_metadata: Arc<dyn Tree>,
    address_tx_index: Arc<dyn Tree>,
    address_output_index: Arc<dyn Tree>,
    address_input_index: Arc<dyn Tree>,
    value_index: Arc<dyn Tree>,
    enable_address_index: bool,
    enable_value_index: bool,
    strategy: IndexingStrategy,
    max_indexed_addresses: usize,
    enable_compression: bool,
    background_indexing: bool,
    indexed_addresses: Arc<std::sync::Mutex<HashSet<[u8; 32]>>>,
    background_tx: Option<crossbeam_channel::Sender<BackgroundBlockJob>>,
    _background_worker: Option<JoinHandle<()>>,
}

impl TxIndex {
    /// Create a new transaction index (advanced indexing disabled).
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        Self::with_config(db, IndexingConfig::default())
    }

    /// Create a transaction index with address/value toggles only (eager strategy).
    pub fn with_indexing(
        db: Arc<dyn Database>,
        enable_address_index: bool,
        enable_value_index: bool,
    ) -> Result<Self> {
        Self::with_config(
            db,
            IndexingConfig {
                enable_address_index,
                enable_value_index,
                ..IndexingConfig::default()
            },
        )
    }

    /// Create a transaction index from full `[storage.indexing]` config.
    pub fn with_config(db: Arc<dyn Database>, config: IndexingConfig) -> Result<Self> {
        let tx_by_hash = Arc::from(db.open_tree("tx_by_hash")?);
        let tx_by_block = Arc::from(db.open_tree("tx_by_block")?);
        let tx_metadata = Arc::from(db.open_tree("tx_metadata")?);
        let address_tx_index = Arc::from(db.open_tree("address_tx_index")?);
        let address_output_index = Arc::from(db.open_tree("address_output_index")?);
        let address_input_index = Arc::from(db.open_tree("address_input_index")?);
        let value_index = Arc::from(db.open_tree("value_index")?);

        let mut background_tx = None;
        let mut background_worker = None;

        if config.strategy == IndexingStrategy::Lazy
            && config.background_indexing
            && (config.enable_address_index || config.enable_value_index)
        {
            let (tx, rx) = crossbeam_channel::unbounded::<BackgroundBlockJob>();
            let worker_index = Self {
                db: Arc::clone(&db),
                tx_by_hash: Arc::clone(&tx_by_hash),
                tx_by_block: Arc::clone(&tx_by_block),
                tx_metadata: Arc::clone(&tx_metadata),
                address_tx_index: Arc::clone(&address_tx_index),
                address_output_index: Arc::clone(&address_output_index),
                address_input_index: Arc::clone(&address_input_index),
                value_index: Arc::clone(&value_index),
                enable_address_index: config.enable_address_index,
                enable_value_index: config.enable_value_index,
                strategy: IndexingStrategy::Eager,
                max_indexed_addresses: config.max_indexed_addresses,
                enable_compression: config.enable_compression,
                background_indexing: false,
                indexed_addresses: Arc::new(std::sync::Mutex::new(HashSet::new())),
                background_tx: None,
                _background_worker: None,
            };
            background_worker = Some(thread::Builder::new().name("txindex-bg".into()).spawn(
                move || {
                    while let Ok(job) = rx.recv() {
                        if let Err(e) = worker_index.index_block_advanced(
                            &job.block,
                            &job.block_hash,
                            job.block_height,
                        ) {
                            tracing::warn!(
                                "Background transaction indexing failed at height {}: {}",
                                job.block_height,
                                e
                            );
                        }
                    }
                },
            )?);
            background_tx = Some(tx);
        }

        Ok(Self {
            db,
            tx_by_hash,
            tx_by_block,
            tx_metadata,
            address_tx_index,
            address_output_index,
            address_input_index,
            value_index,
            enable_address_index: config.enable_address_index,
            enable_value_index: config.enable_value_index,
            strategy: config.strategy,
            max_indexed_addresses: config.max_indexed_addresses,
            enable_compression: config.enable_compression,
            background_indexing: config.background_indexing,
            indexed_addresses: Arc::new(std::sync::Mutex::new(HashSet::new())),
            background_tx,
            _background_worker: background_worker,
        })
    }

    /// Index a block (basic metadata always; advanced per strategy / background settings).
    pub fn index_block(
        &self,
        block: &blvm_protocol::Block,
        block_hash: &Hash,
        block_height: u64,
    ) -> Result<()> {
        for (tx_index, tx) in block.transactions.iter().enumerate() {
            self.index_transaction(tx, block_hash, block_height, tx_index as u32)?;
        }

        if self.strategy == IndexingStrategy::Lazy
            && self.background_indexing
            && (self.enable_address_index || self.enable_value_index)
        {
            if let Some(ref tx) = self.background_tx {
                let _ = tx.send(BackgroundBlockJob {
                    block: block.clone(),
                    block_hash: *block_hash,
                    block_height,
                });
            }
        }

        Ok(())
    }

    fn index_block_advanced(
        &self,
        block: &blvm_protocol::Block,
        block_hash: &Hash,
        block_height: u64,
    ) -> Result<()> {
        for (tx_index, tx) in block.transactions.iter().enumerate() {
            self.index_transaction_advanced(tx, block_hash, block_height, tx_index as u32)?;
        }
        Ok(())
    }

    /// Index a transaction (hash metadata + optional eager advanced indexes).
    pub fn index_transaction(
        &self,
        tx: &Transaction,
        block_hash: &Hash,
        block_height: u64,
        tx_index: u32,
    ) -> Result<()> {
        self.index_transaction_basic(tx, block_hash, block_height, tx_index)?;

        if self.strategy == IndexingStrategy::Eager {
            self.index_transaction_advanced(tx, block_hash, block_height, tx_index)?;
        }

        Ok(())
    }

    fn index_transaction_basic(
        &self,
        tx: &Transaction,
        block_hash: &Hash,
        block_height: u64,
        tx_index: u32,
    ) -> Result<()> {
        let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
        let tx_data = bincode::serialize(tx)?;

        let metadata = TxMetadata {
            tx_hash,
            block_hash: *block_hash,
            block_height,
            tx_index,
            size: self.calculate_tx_size(tx),
            weight: self.calculate_tx_weight(tx),
        };
        let metadata_data = bincode::serialize(&metadata)?;
        let block_key = self.block_tx_key(block_hash, tx_index);

        self.tx_by_hash.insert(tx_hash.as_slice(), &tx_data)?;
        self.tx_metadata
            .insert(tx_hash.as_slice(), &metadata_data)?;
        self.tx_by_block.insert(&block_key, tx_hash.as_slice())?;
        Ok(())
    }

    fn index_transaction_advanced(
        &self,
        tx: &Transaction,
        _block_hash: &Hash,
        _block_height: u64,
        _tx_index: u32,
    ) -> Result<()> {
        let tx_hash = blvm_protocol::block::calculate_tx_id(tx);
        if self.enable_address_index {
            self.index_addresses(tx, &tx_hash)?;
        }
        if self.enable_value_index {
            self.index_values(tx, &tx_hash)?;
        }
        Ok(())
    }

    fn can_add_address_key(&self, address_hash: &[u8; 32]) -> Result<bool> {
        if self.max_indexed_addresses == 0 {
            return Ok(true);
        }
        if self.address_tx_index.get(address_hash)?.is_some() {
            return Ok(true);
        }
        Ok(self.address_tx_index.len()? < self.max_indexed_addresses)
    }

    fn encode_index_blob(&self, data: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "compression")]
        {
            if self.enable_compression {
                return zstd::encode_all(data, 3)
                    .context("Transaction index blob compression failed");
            }
        }
        #[cfg(not(feature = "compression"))]
        let _ = self.enable_compression;
        Ok(data.to_vec())
    }

    fn decode_index_blob(&self, data: &[u8]) -> Result<Vec<u8>> {
        #[cfg(feature = "compression")]
        {
            if self.enable_compression && Self::is_zstd(data) {
                return zstd::decode_all(data)
                    .context("Transaction index blob decompression failed");
            }
        }
        Ok(data.to_vec())
    }

    #[cfg(feature = "compression")]
    fn is_zstd(data: &[u8]) -> bool {
        data.len() >= 4 && data[0..4] == [0x28, 0xB5, 0x2F, 0xFD]
    }

    fn tree_insert_encoded(&self, tree: &Arc<dyn Tree>, key: &[u8], payload: &[u8]) -> Result<()> {
        let encoded = self.encode_index_blob(payload)?;
        tree.insert(key, &encoded)
    }

    fn tree_get_decoded(&self, tree: &Arc<dyn Tree>, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match tree.get(key)? {
            Some(data) => Ok(Some(self.decode_index_blob(&data)?)),
            None => Ok(None),
        }
    }

    fn index_addresses(&self, tx: &Transaction, tx_hash: &Hash) -> Result<()> {
        use std::collections::HashMap;

        let mut address_tx_updates: HashMap<[u8; 32], HashSet<Hash>> = HashMap::new();
        let mut address_output_updates: HashMap<[u8; 32], HashSet<(Hash, u32)>> = HashMap::new();

        for (output_index, output) in tx.outputs.iter().enumerate() {
            let address_hash = sha256(&output.script_pubkey);
            address_tx_updates
                .entry(address_hash)
                .or_default()
                .insert(*tx_hash);
            address_output_updates
                .entry(address_hash)
                .or_default()
                .insert((*tx_hash, output_index as u32));
        }

        for (address_hash, new_tx_hashes) in address_tx_updates {
            if !self.can_add_address_key(&address_hash)? {
                continue;
            }
            let mut existing_txs = self.get_address_transactions(&address_hash)?;
            let existing_set: HashSet<Hash> = existing_txs.iter().copied().collect();
            let mut updated = false;
            for hash in new_tx_hashes {
                if !existing_set.contains(&hash) {
                    existing_txs.push(hash);
                    updated = true;
                }
            }
            if updated {
                let tx_list_data = bincode::serialize(&existing_txs)?;
                self.tree_insert_encoded(&self.address_tx_index, &address_hash, &tx_list_data)?;
                self.mark_address_indexed(address_hash);
            }
        }

        for (address_hash, new_outputs) in address_output_updates {
            if !self.can_add_address_key(&address_hash)? {
                continue;
            }
            let existing_outputs = self.get_address_outputs(&address_hash)?;
            let existing_set: HashSet<(Hash, u32)> = existing_outputs
                .iter()
                .map(|o| (o.tx_hash, o.output_index))
                .collect();
            let mut updated_outputs = existing_outputs;
            let mut updated = false;
            for (hash, output_index) in new_outputs {
                if !existing_set.contains(&(hash, output_index)) {
                    updated_outputs.push(AddressOutputEntry {
                        tx_hash: hash,
                        output_index,
                    });
                    updated = true;
                }
            }
            if updated {
                #[derive(Serialize, Deserialize)]
                struct AddressOutputSer {
                    tx_hash: Hash,
                    output_index: u32,
                }
                let output_list: Vec<AddressOutputSer> = updated_outputs
                    .iter()
                    .map(|o| AddressOutputSer {
                        tx_hash: o.tx_hash,
                        output_index: o.output_index,
                    })
                    .collect();
                let output_list_data = bincode::serialize(&output_list)?;
                self.tree_insert_encoded(
                    &self.address_output_index,
                    &address_hash,
                    &output_list_data,
                )?;
            }
        }

        Ok(())
    }

    fn mark_address_indexed(&self, address_hash: [u8; 32]) {
        if let Ok(mut set) = self.indexed_addresses.lock() {
            set.insert(address_hash);
        }
    }

    fn index_values(&self, tx: &Transaction, tx_hash: &Hash) -> Result<()> {
        use std::collections::HashMap;

        fn value_to_bucket(value: u64) -> u64 {
            if value == 0 {
                return 0;
            }
            let log10 = (value as f64).log10().floor() as u64;
            (log10 + 1) * 1000
        }

        let mut bucket_updates: HashMap<u64, Vec<(Hash, u32, u64)>> = HashMap::new();
        for (output_index, output) in tx.outputs.iter().enumerate() {
            let value = output.value as u64;
            let bucket = value_to_bucket(value);
            bucket_updates
                .entry(bucket)
                .or_default()
                .push((*tx_hash, output_index as u32, value));
        }

        for (bucket, new_entries) in bucket_updates {
            let mut existing_entries = self.get_value_entries(&bucket)?;
            let existing_set: HashSet<(Hash, u32)> = existing_entries
                .iter()
                .map(|e| (e.tx_hash, e.output_index))
                .collect();
            let mut updated = false;
            for (hash, output_index, value) in new_entries {
                if !existing_set.contains(&(hash, output_index)) {
                    existing_entries.push(ValueEntry {
                        tx_hash: hash,
                        output_index,
                        value,
                    });
                    updated = true;
                }
            }
            if updated {
                #[derive(Serialize, Deserialize)]
                struct ValueEntrySer {
                    tx_hash: Hash,
                    output_index: u32,
                    value: u64,
                }
                let entry_list: Vec<ValueEntrySer> = existing_entries
                    .iter()
                    .map(|e| ValueEntrySer {
                        tx_hash: e.tx_hash,
                        output_index: e.output_index,
                        value: e.value,
                    })
                    .collect();
                let entries_data = bincode::serialize(&entry_list)?;
                let bucket_key = bucket.to_be_bytes();
                self.tree_insert_encoded(&self.value_index, &bucket_key, &entries_data)?;
            }
        }

        Ok(())
    }

    fn get_address_transactions(&self, address_hash: &[u8; 32]) -> Result<Vec<Hash>> {
        if let Some(data) = self.tree_get_decoded(&self.address_tx_index, address_hash)? {
            let txs: Vec<Hash> = bincode::deserialize(&data)?;
            Ok(txs)
        } else {
            Ok(Vec::new())
        }
    }

    fn get_address_outputs(&self, address_hash: &[u8; 32]) -> Result<Vec<AddressOutputEntry>> {
        #[derive(Serialize, Deserialize)]
        struct AddressOutput {
            tx_hash: Hash,
            output_index: u32,
        }

        if let Some(data) = self.tree_get_decoded(&self.address_output_index, address_hash)? {
            let outputs: Vec<AddressOutput> = bincode::deserialize(&data)?;
            Ok(outputs
                .into_iter()
                .map(|o| AddressOutputEntry {
                    tx_hash: o.tx_hash,
                    output_index: o.output_index,
                })
                .collect())
        } else {
            Ok(Vec::new())
        }
    }

    fn get_value_entries(&self, bucket: &u64) -> Result<Vec<ValueEntry>> {
        let bucket_key = bucket.to_be_bytes();
        if let Some(data) = self.tree_get_decoded(&self.value_index, &bucket_key)? {
            #[derive(Serialize, Deserialize)]
            struct ValueEntrySer {
                tx_hash: Hash,
                output_index: u32,
                value: u64,
            }
            let entries: Vec<ValueEntrySer> = bincode::deserialize(&data)?;
            Ok(entries
                .into_iter()
                .map(|e| ValueEntry {
                    tx_hash: e.tx_hash,
                    output_index: e.output_index,
                    value: e.value,
                })
                .collect())
        } else {
            Ok(Vec::new())
        }
    }

    fn ensure_address_indexed(&self, script_pubkey: &[u8]) -> Result<()> {
        if !self.enable_address_index || self.strategy != IndexingStrategy::Lazy {
            return Ok(());
        }

        let address_hash = sha256(script_pubkey);
        if !self.get_address_transactions(&address_hash)?.is_empty() {
            self.mark_address_indexed(address_hash);
            return Ok(());
        }

        for item in self.tx_by_hash.iter() {
            let (key, data) = item?;
            let mut tx_hash = [0u8; 32];
            if key.len() != 32 {
                continue;
            }
            tx_hash.copy_from_slice(&key);
            let raw = data;
            let tx: Transaction = bincode::deserialize(&raw)?;
            if tx.outputs.iter().any(|o| o.script_pubkey == script_pubkey) {
                self.index_addresses(&tx, &tx_hash)?;
            }
        }

        self.mark_address_indexed(address_hash);
        Ok(())
    }

    fn ensure_value_index_populated(&self, min_value: u64, max_value: u64) -> Result<()> {
        if !self.enable_value_index || self.strategy != IndexingStrategy::Lazy {
            return Ok(());
        }

        for item in self.tx_by_hash.iter() {
            let (key, data) = item?;
            let mut tx_hash = [0u8; 32];
            if key.len() != 32 {
                continue;
            }
            tx_hash.copy_from_slice(&key);
            let raw = data;
            let tx: Transaction = bincode::deserialize(&raw)?;
            if tx
                .outputs
                .iter()
                .any(|o| o.value as u64 >= min_value && o.value as u64 <= max_value)
            {
                self.index_values(&tx, &tx_hash)?;
            }
        }
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
            if let Some(tx_hash_data) = self.tx_by_block.get(&block_key)? {
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
        self.tx_by_hash.contains_key(tx_hash.as_slice())
    }

    /// Get transaction count
    pub fn transaction_count(&self) -> Result<usize> {
        self.tx_by_hash.len()
    }

    /// Get indexing statistics (for monitoring)
    pub fn get_index_stats(&self) -> Result<IndexStats> {
        Ok(IndexStats {
            total_transactions: self.tx_by_hash.len()?,
            address_index_enabled: self.enable_address_index,
            value_index_enabled: self.enable_value_index,
            indexed_addresses: if self.enable_address_index {
                self.address_tx_index.len()?
            } else {
                0
            },
            indexed_value_buckets: if self.enable_value_index {
                self.value_index.len()?
            } else {
                0
            },
        })
    }

    /// Get transactions by block height range
    pub fn get_transactions_by_height_range(
        &self,
        start_height: u64,
        end_height: u64,
        blockstore: &crate::storage::blockstore::BlockStore,
    ) -> Result<Vec<Transaction>> {
        let mut transactions = Vec::new();
        for height in start_height..=end_height {
            if let Ok(Some(block_hash)) = blockstore.get_hash_by_height(height) {
                if let Ok(block_txs) = self.get_block_transactions(&block_hash) {
                    transactions.extend(block_txs);
                }
            }
        }
        Ok(transactions)
    }

    /// Get transactions by address (script pubkey)
    pub fn get_transactions_by_address(&self, script_pubkey: &[u8]) -> Result<Vec<Transaction>> {
        if !self.enable_address_index {
            return Ok(Vec::new());
        }

        if self.strategy == IndexingStrategy::Lazy && !self.background_indexing {
            self.ensure_address_indexed(script_pubkey)?;
        }

        let address_hash = sha256(script_pubkey);
        let tx_hashes = self.get_address_transactions(&address_hash)?;
        let mut transactions = Vec::new();
        for tx_hash in tx_hashes {
            if let Some(tx) = self.get_transaction(&tx_hash)? {
                transactions.push(tx);
            }
        }
        Ok(transactions)
    }

    /// Get transactions by output value range
    pub fn get_transactions_by_value_range(
        &self,
        min_value: u64,
        max_value: u64,
    ) -> Result<Vec<Transaction>> {
        if !self.enable_value_index {
            return Ok(Vec::new());
        }

        if self.strategy == IndexingStrategy::Lazy && !self.background_indexing {
            self.ensure_value_index_populated(min_value, max_value)?;
        }

        fn value_to_bucket(value: u64) -> u64 {
            if value == 0 {
                return 0;
            }
            let log10 = (value as f64).log10().floor() as u64;
            (log10 + 1) * 1000
        }

        let min_bucket = value_to_bucket(min_value);
        let max_bucket = value_to_bucket(max_value);
        let mut tx_hashes = HashSet::new();

        for bucket in min_bucket..=max_bucket {
            let entries = self.get_value_entries(&bucket)?;
            for entry in entries {
                if entry.value >= min_value && entry.value <= max_value {
                    tx_hashes.insert(entry.tx_hash);
                }
            }
        }

        let mut transactions = Vec::new();
        for tx_hash in tx_hashes {
            if let Some(tx) = self.get_transaction(&tx_hash)? {
                transactions.push(tx);
            }
        }
        Ok(transactions)
    }

    /// Remove transaction from index
    pub fn remove_transaction(&self, tx_hash: &Hash) -> Result<()> {
        if let Some(metadata) = self.get_metadata(tx_hash)? {
            let block_key = self.block_tx_key(&metadata.block_hash, metadata.tx_index);
            self.tx_by_block.remove(&block_key)?;
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

        if self.enable_address_index {
            self.address_tx_index.clear()?;
            self.address_output_index.clear()?;
            self.address_input_index.clear()?;
        }

        if self.enable_value_index {
            self.value_index.clear()?;
        }

        if let Ok(mut set) = self.indexed_addresses.lock() {
            set.clear();
        }

        Ok(())
    }

    fn calculate_tx_size(&self, tx: &Transaction) -> u32 {
        let mut size = 4;
        size += 1;
        for input in &tx.inputs {
            size += 32;
            size += 1;
            size += input.script_sig.len() as u32;
            size += 4;
        }
        size += 1;
        for output in &tx.outputs {
            size += 8;
            size += 1;
            size += output.script_pubkey.len() as u32;
        }
        size += 4;
        size
    }

    fn calculate_tx_weight(&self, tx: &Transaction) -> u32 {
        self.calculate_tx_size(tx) * 4
    }

    fn block_tx_key(&self, block_hash: &Hash, tx_index: u32) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(block_hash.as_slice());
        key.extend_from_slice(&tx_index.to_be_bytes());
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::database::{DatabaseBackend, create_database};
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};
    use tempfile::TempDir;

    fn sample_tx(script: &[u8], value: u64) -> Transaction {
        Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: vec![],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: value as i64,
                script_pubkey: script.to_vec(),
            }]
            .into(),
            lock_time: 0,
        }
    }

    fn open_index(config: IndexingConfig) -> (TempDir, TxIndex) {
        let dir = TempDir::new().unwrap();
        let db = Arc::from(create_database(dir.path(), DatabaseBackend::Heed3, None).unwrap());
        let index = TxIndex::with_config(db, config).unwrap();
        (dir, index)
    }

    #[test]
    fn eager_indexes_address_on_connect() {
        let (_dir, index) = open_index(IndexingConfig {
            enable_address_index: true,
            strategy: IndexingStrategy::Eager,
            ..Default::default()
        });
        let script = vec![0x51];
        let tx = sample_tx(&script, 50_000);
        let block_hash = [2u8; 32];
        index.index_transaction(&tx, &block_hash, 1, 0).unwrap();
        let txs = index.get_transactions_by_address(&script).unwrap();
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn lazy_defers_address_index_until_query() {
        let (_dir, index) = open_index(IndexingConfig {
            enable_address_index: true,
            strategy: IndexingStrategy::Lazy,
            background_indexing: false,
            ..Default::default()
        });
        let script = vec![0x52];
        let tx = sample_tx(&script, 50_000);
        let block_hash = [3u8; 32];
        index.index_transaction(&tx, &block_hash, 1, 0).unwrap();
        assert!(
            index
                .get_address_transactions(&sha256(&script))
                .unwrap()
                .is_empty()
        );
        let txs = index.get_transactions_by_address(&script).unwrap();
        assert_eq!(txs.len(), 1);
        assert_eq!(
            index
                .get_address_transactions(&sha256(&script))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn max_indexed_addresses_caps_new_keys() {
        let (_dir, index) = open_index(IndexingConfig {
            enable_address_index: true,
            strategy: IndexingStrategy::Eager,
            max_indexed_addresses: 1,
            ..Default::default()
        });
        let tx1 = sample_tx(&[0x61], 1);
        let tx2 = sample_tx(&[0x62], 2);
        let block_hash = [4u8; 32];
        index.index_transaction(&tx1, &block_hash, 1, 0).unwrap();
        index.index_transaction(&tx2, &block_hash, 1, 1).unwrap();
        assert_eq!(index.get_index_stats().unwrap().indexed_addresses, 1);
    }
}

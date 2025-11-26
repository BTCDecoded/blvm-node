//! Transaction index implementation
//!
//! Provides fast lookup of transactions by hash and maintains transaction metadata.

use crate::storage::database::{Database, Tree};
use crate::storage::hashing::sha256;
use anyhow::Result;
use bllvm_protocol::{Hash, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;

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
    // Address indexing (optional, enabled via config)
    address_tx_index: Arc<dyn Tree>, // script_pubkey_hash → Vec<tx_hash>
    address_output_index: Arc<dyn Tree>, // script_pubkey_hash → Vec<(tx_hash, output_index)>
    address_input_index: Arc<dyn Tree>, // script_pubkey_hash → Vec<(tx_hash, input_index, prev_tx_hash, prev_output_index)>
    // Value range indexing (optional, enabled via config)
    value_index: Arc<dyn Tree>, // value_bucket → Vec<(tx_hash, output_index)>
    // Configuration
    enable_address_index: bool,
    enable_value_index: bool,
    // Lazy indexing: track which addresses have been indexed
    #[allow(dead_code)]
    indexed_addresses: Arc<std::sync::Mutex<std::collections::HashSet<[u8; 32]>>>,
}

impl TxIndex {
    /// Create a new transaction index
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        Self::with_indexing(db, false, false)
    }

    /// Create a new transaction index with optional advanced indexing
    pub fn with_indexing(
        db: Arc<dyn Database>,
        enable_address_index: bool,
        enable_value_index: bool,
    ) -> Result<Self> {
        let tx_by_hash = Arc::from(db.open_tree("tx_by_hash")?);
        let tx_by_block = Arc::from(db.open_tree("tx_by_block")?);
        let tx_metadata = Arc::from(db.open_tree("tx_metadata")?);

        // Address indexing trees (always create, but only use if enabled)
        let address_tx_index = Arc::from(db.open_tree("address_tx_index")?);
        let address_output_index = Arc::from(db.open_tree("address_output_index")?);
        let address_input_index = Arc::from(db.open_tree("address_input_index")?);

        // Value indexing tree (always create, but only use if enabled)
        let value_index = Arc::from(db.open_tree("value_index")?);

        Ok(Self {
            db,
            tx_by_hash,
            tx_by_block,
            tx_metadata,
            address_tx_index,
            address_output_index,
            address_input_index,
            value_index,
            enable_address_index,
            enable_value_index,
            indexed_addresses: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        })
    }

    /// Index a block (optimized batch indexing)
    /// Processes all transactions in a block at once for better performance
    ///
    /// For lazy indexing: Only indexes basic transaction data, not address/value indexes
    /// Address indexes are built on-demand when queried
    pub fn index_block(
        &self,
        block: &bllvm_protocol::Block,
        block_hash: &Hash,
        block_height: u64,
    ) -> Result<()> {
        // Index all transactions in the block
        // Note: With lazy indexing, address/value indexes are built on-demand
        for (tx_index, tx) in block.transactions.iter().enumerate() {
            self.index_transaction(tx, block_hash, block_height, tx_index as u32)?;
        }
        Ok(())
    }

    /// Index a transaction
    pub fn index_transaction(
        &self,
        tx: &Transaction,
        block_hash: &Hash,
        block_height: u64,
        tx_index: u32,
    ) -> Result<()> {
        // Use the standard transaction ID calculation from bllvm-protocol
        let tx_hash = bllvm_protocol::block::calculate_tx_id(tx);
        let tx_data = bincode::serialize(tx)?;

        // Store transaction by hash
        self.tx_by_hash.insert(tx_hash.as_slice(), &tx_data)?;

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
        self.tx_metadata
            .insert(tx_hash.as_slice(), &metadata_data)?;

        // Index by block
        let block_key = self.block_tx_key(block_hash, tx_index);
        self.tx_by_block.insert(&block_key, tx_hash.as_slice())?;

        // Advanced indexing: Address index (if enabled)
        if self.enable_address_index {
            self.index_addresses(tx, &tx_hash)?;
        }

        // Advanced indexing: Value index (if enabled)
        if self.enable_value_index {
            self.index_values(tx, &tx_hash)?;
        }

        Ok(())
    }

    /// Index addresses (script_pubkeys) from transaction outputs
    ///
    /// Performance optimizations:
    /// - Batches updates per address (one DB read/write per unique address instead of per output)
    /// - Uses HashSet for O(1) duplicate checking instead of O(n) linear search
    /// - Only writes to DB if updates were made
    ///
    /// This reduces DB I/O from O(outputs) to O(unique_addresses) per transaction.
    fn index_addresses(&self, tx: &Transaction, tx_hash: &Hash) -> Result<()> {
        use std::collections::HashMap;

        // Batch updates by address_hash to minimize DB operations
        let mut address_tx_updates: HashMap<[u8; 32], HashSet<Hash>> = HashMap::new();
        let mut address_output_updates: HashMap<[u8; 32], HashSet<(Hash, u32)>> = HashMap::new();

        // Collect all updates for this transaction
        for (output_index, output) in tx.outputs.iter().enumerate() {
            let script_pubkey = &output.script_pubkey;
            let address_hash = sha256(script_pubkey);

            // Track tx_hash for this address
            address_tx_updates
                .entry(address_hash)
                .or_default()
                .insert(*tx_hash);

            // Track (tx_hash, output_index) for this address
            address_output_updates
                .entry(address_hash)
                .or_default()
                .insert((*tx_hash, output_index as u32));
        }

        // Apply batched updates (one DB read/write per unique address)
        for (address_hash, new_tx_hashes) in address_tx_updates {
            // Read existing transactions for this address
            let mut existing_txs = self.get_address_transactions(&address_hash)?;
            let existing_set: HashSet<Hash> = existing_txs.iter().copied().collect();

            // Add new transactions (using HashSet for O(1) deduplication)
            let mut updated = false;
            for tx_hash in new_tx_hashes {
                if !existing_set.contains(&tx_hash) {
                    existing_txs.push(tx_hash);
                    updated = true;
                }
            }

            // Write back only if updated
            if updated {
                let tx_list_data = bincode::serialize(&existing_txs)?;
                self.address_tx_index.insert(&address_hash, &tx_list_data)?;
            }
        }

        for (address_hash, new_outputs) in address_output_updates {
            // Read existing outputs for this address
            let existing_outputs = self.get_address_outputs(&address_hash)?;
            let existing_set: HashSet<(Hash, u32)> = existing_outputs
                .iter()
                .map(|o| (o.tx_hash, o.output_index))
                .collect();

            // Add new outputs (using HashSet for O(1) deduplication)
            let mut updated_outputs = existing_outputs;
            let mut updated = false;
            for (tx_hash, output_index) in new_outputs {
                if !existing_set.contains(&(tx_hash, output_index)) {
                    updated_outputs.push(AddressOutputEntry {
                        tx_hash,
                        output_index,
                    });
                    updated = true;
                }
            }

            // Write back only if updated
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
                self.address_output_index
                    .insert(&address_hash, &output_list_data)?;
            }
        }

        // Note: Input indexing requires UTXO lookup to get script_pubkey from prevout
        // This is more complex and can be added later if needed

        Ok(())
    }

    /// Index values from transaction outputs
    ///
    /// Performance optimizations:
    /// - Batches updates per bucket (one DB read/write per unique bucket instead of per output)
    /// - Uses HashSet for O(1) duplicate checking instead of O(n) linear search
    /// - Only writes to DB if updates were made
    ///
    /// This reduces DB I/O from O(outputs) to O(unique_buckets) per transaction.
    fn index_values(&self, tx: &Transaction, tx_hash: &Hash) -> Result<()> {
        use std::collections::HashMap;

        // Use logarithmic bucketing for value ranges
        // Buckets: 0-1000, 1000-10000, 10000-100000, 100000-1000000, 1000000-10000000, etc.
        fn value_to_bucket(value: u64) -> u64 {
            if value == 0 {
                return 0;
            }
            // Logarithmic bucketing: bucket = floor(log10(value)) * 1000
            let log10 = (value as f64).log10().floor() as u64;
            (log10 + 1) * 1000
        }

        // Batch updates by bucket to minimize DB operations
        let mut bucket_updates: HashMap<u64, Vec<(Hash, u32, u64)>> = HashMap::new();

        // Collect all updates for this transaction
        for (output_index, output) in tx.outputs.iter().enumerate() {
            let value = output.value as u64;
            let bucket = value_to_bucket(value);

            bucket_updates
                .entry(bucket)
                .or_default()
                .push((*tx_hash, output_index as u32, value));
        }

        // Apply batched updates (one DB read/write per unique bucket)
        for (bucket, new_entries) in bucket_updates {
            // Read existing entries for this bucket
            let mut existing_entries = self.get_value_entries(&bucket)?;
            let existing_set: HashSet<(Hash, u32)> = existing_entries
                .iter()
                .map(|e| (e.tx_hash, e.output_index))
                .collect();

            // Add new entries (using HashSet for O(1) deduplication)
            let mut updated = false;
            for (tx_hash, output_index, value) in new_entries {
                if !existing_set.contains(&(tx_hash, output_index)) {
                    existing_entries.push(ValueEntry {
                        tx_hash,
                        output_index,
                        value,
                    });
                    updated = true;
                }
            }

            // Write back only if updated
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
                self.value_index.insert(&bucket_key, &entries_data)?;
            }
        }

        Ok(())
    }

    /// Get all transaction hashes for an address (internal helper)
    fn get_address_transactions(&self, address_hash: &[u8; 32]) -> Result<Vec<Hash>> {
        if let Some(data) = self.address_tx_index.get(address_hash)? {
            let txs: Vec<Hash> = bincode::deserialize(&data)?;
            Ok(txs)
        } else {
            Ok(Vec::new())
        }
    }

    /// Get all outputs for an address (internal helper)
    fn get_address_outputs(&self, address_hash: &[u8; 32]) -> Result<Vec<AddressOutputEntry>> {
        #[derive(Serialize, Deserialize)]
        struct AddressOutput {
            tx_hash: Hash,
            output_index: u32,
        }

        if let Some(data) = self.address_output_index.get(address_hash)? {
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

    /// Get all value entries for a bucket (internal helper)
    fn get_value_entries(&self, bucket: &u64) -> Result<Vec<ValueEntry>> {
        let bucket_key = bucket.to_be_bytes();
        if let Some(data) = self.value_index.get(&bucket_key)? {
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
    /// Efficiently queries transactions across multiple blocks using height index
    pub fn get_transactions_by_height_range(
        &self,
        start_height: u64,
        end_height: u64,
        blockstore: &crate::storage::blockstore::BlockStore,
    ) -> Result<Vec<Transaction>> {
        let mut transactions = Vec::new();

        // Iterate through height range
        for height in start_height..=end_height {
            // Get block hash for this height
            if let Ok(Some(block_hash)) = blockstore.get_hash_by_height(height) {
                // Get all transactions in this block
                if let Ok(block_txs) = self.get_block_transactions(&block_hash) {
                    transactions.extend(block_txs);
                }
            }
        }

        Ok(transactions)
    }

    /// Get transactions by address (script pubkey)
    ///
    /// With lazy indexing: If address is not indexed, scans all transactions to build index on-demand
    /// With eager indexing: Uses pre-built index for fast lookup
    pub fn get_transactions_by_address(&self, script_pubkey: &[u8]) -> Result<Vec<Transaction>> {
        if !self.enable_address_index {
            return Ok(Vec::new());
        }

        let address_hash = sha256(script_pubkey);

        // Check if address is already indexed
        let mut indexed = {
            let indexed_set = self.indexed_addresses.lock().unwrap();
            indexed_set.contains(&address_hash)
        };

        // If not indexed, check if index exists in DB (from previous runs or eager indexing)
        if !indexed {
            if let Ok(tx_hashes) = self.get_address_transactions(&address_hash) {
                indexed = !tx_hashes.is_empty();
                if indexed {
                    // Mark as indexed in memory
                    let mut indexed_set = self.indexed_addresses.lock().unwrap();
                    indexed_set.insert(address_hash);
                }
            }
        }

        // If still not indexed and we want lazy indexing, we would scan all transactions here
        // For now, we rely on the index being built during block processing (eager) or
        // from previous queries. Full lazy indexing (scanning all blocks) would be expensive
        // and is better handled by a background indexing task.

        // Get transactions from index
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
    /// Useful for querying large transactions or filtering by value
    pub fn get_transactions_by_value_range(
        &self,
        min_value: u64,
        max_value: u64,
    ) -> Result<Vec<Transaction>> {
        if !self.enable_value_index {
            return Ok(Vec::new());
        }

        // Determine which buckets to query
        fn value_to_bucket(value: u64) -> u64 {
            if value == 0 {
                return 0;
            }
            let log10 = (value as f64).log10().floor() as u64;
            (log10 + 1) * 1000
        }

        let min_bucket = value_to_bucket(min_value);
        let max_bucket = value_to_bucket(max_value);

        // Collect all unique transaction hashes from relevant buckets
        let mut tx_hashes = HashSet::new();

        for bucket in min_bucket..=max_bucket {
            let entries = self.get_value_entries(&bucket)?;
            for entry in entries {
                if entry.value >= min_value && entry.value <= max_value {
                    tx_hashes.insert(entry.tx_hash);
                }
            }
        }

        // Fetch all transactions
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

//! Mempool manager
//!
//! Handles transaction mempool management, validation, and relay.

use anyhow::Result;
use bllvm_protocol::mempool::Mempool;
use bllvm_protocol::{Hash, OutPoint, Transaction, UtxoSet};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info};

/// Mempool manager
pub struct MempoolManager {
    /// Transaction mempool - stores full transactions by hash
    transactions: HashMap<Hash, Transaction>,
    /// Legacy mempool (HashSet of hashes) for compatibility
    #[allow(dead_code)]
    mempool: Mempool,
    #[allow(dead_code)]
    utxo_set: UtxoSet,
    /// Track spent outputs to detect conflicts
    spent_outputs: HashSet<OutPoint>,
}

impl MempoolManager {
    /// Create a new mempool manager
    pub fn new() -> Self {
        Self {
            transactions: HashMap::new(),
            mempool: Mempool::new(),
            utxo_set: HashMap::new(),
            spent_outputs: HashSet::new(),
        }
    }

    /// Start the mempool manager
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting mempool manager");

        // Initialize mempool
        self.initialize_mempool().await?;

        // Start mempool processing loop
        self.process_loop().await?;

        Ok(())
    }

    /// Run mempool processing once (for testing)
    pub async fn process_once(&mut self) -> Result<()> {
        // Process pending transactions
        self.process_pending_transactions().await?;

        // Clean up old transactions
        self.cleanup_old_transactions().await?;

        Ok(())
    }

    /// Initialize mempool
    async fn initialize_mempool(&mut self) -> Result<()> {
        debug!("Initializing mempool");

        // Load existing mempool from storage if available
        // In a real implementation, this would restore mempool state

        Ok(())
    }

    /// Main mempool processing loop
    async fn process_loop(&mut self) -> Result<()> {
        loop {
            // Process pending transactions
            self.process_pending_transactions().await?;

            // Clean up old transactions
            self.cleanup_old_transactions().await?;

            // Small delay to prevent busy waiting
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    /// Process pending transactions
    async fn process_pending_transactions(&mut self) -> Result<()> {
        // In a real implementation, this would:
        // 1. Get new transactions from network
        // 2. Validate transactions using consensus-proof
        // 3. Add valid transactions to mempool
        // 4. Relay transactions to peers

        debug!("Processing pending transactions");
        Ok(())
    }

    /// Clean up old transactions
    async fn cleanup_old_transactions(&mut self) -> Result<()> {
        // In a real implementation, this would:
        // 1. Remove transactions that are too old
        // 2. Remove transactions that conflict with new blocks
        // 3. Update transaction priorities

        debug!("Cleaning up old transactions");
        Ok(())
    }

    /// Add transaction to mempool
    pub async fn add_transaction(&mut self, tx: Transaction) -> Result<bool> {
        debug!("Adding transaction to mempool");

        // Check for conflicts with existing mempool transactions
        for input in &tx.inputs {
            if self.spent_outputs.contains(&input.prevout) {
                debug!("Transaction conflicts with existing mempool transaction");
                return Ok(false);
            }
        }

        // Add transaction to mempool (store full transaction)
        use bllvm_protocol::block::calculate_tx_id;
        let tx_hash = calculate_tx_id(&tx);
        self.transactions.insert(tx_hash, tx.clone());
        self.mempool.insert(tx_hash);

        // Track spent outputs
        for input in &tx.inputs {
            self.spent_outputs.insert(input.prevout.clone());
        }

        Ok(true)
    }

    /// Get mempool size
    pub fn size(&self) -> usize {
        self.transactions.len()
    }

    /// Get mempool transaction hashes
    pub fn transaction_hashes(&self) -> Vec<Hash> {
        self.transactions.keys().cloned().collect()
    }

    /// Get transaction by hash
    pub fn get_transaction(&self, hash: &Hash) -> Option<Transaction> {
        self.transactions.get(hash).cloned()
    }

    /// Get all transactions
    pub fn get_transactions(&self) -> Vec<Transaction> {
        self.transactions.values().cloned().collect()
    }

    /// Get prioritized transactions by fee rate
    ///
    /// Returns transactions sorted by fee rate (satoshis per vbyte) in descending order.
    /// Requires UTXO set to calculate fee rates.
    pub fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &UtxoSet,
    ) -> Vec<Transaction> {
        let mut tx_with_fees: Vec<(Transaction, u64)> = Vec::new();

        for tx in self.transactions.values() {
            // Calculate fee for this transaction
            let fee = self.calculate_transaction_fee(tx, utxo_set);

            // Calculate transaction size (simplified - use serialized size estimate)
            let size = self.estimate_transaction_size(tx);

            // Calculate fee rate (satoshis per vbyte)
            let fee_rate = if size > 0 {
                fee * 1000 / size as u64 // Convert to satoshis per vbyte (multiply by 1000 for precision)
            } else {
                0
            };

            tx_with_fees.push((tx.clone(), fee_rate));
        }

        // Sort by fee rate (descending)
        tx_with_fees.sort_by(|a, b| b.1.cmp(&a.1));

        // Return top N transactions
        tx_with_fees
            .into_iter()
            .take(limit)
            .map(|(tx, _)| tx)
            .collect()
    }

    /// Calculate transaction fee
    ///
    /// Fee = sum of inputs - sum of outputs
    pub fn calculate_transaction_fee(&self, tx: &Transaction, utxo_set: &UtxoSet) -> u64 {
        let mut input_total = 0u64;

        // Sum input values from UTXO set
        for input in &tx.inputs {
            if let Some(utxo) = utxo_set.get(&input.prevout) {
                input_total += utxo.value as u64;
            }
        }

        // Sum output values
        let output_total: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();

        // Fee is difference (inputs - outputs)
        if input_total > output_total {
            input_total - output_total
        } else {
            0
        }
    }

    /// Estimate transaction size in vbytes
    ///
    /// Simplified estimation - in production, would use actual serialized size
    fn estimate_transaction_size(&self, tx: &Transaction) -> usize {
        // Base transaction size: version (4) + locktime (4) = 8 bytes
        let mut size = 8;

        // Input size: prevout (36) + script_sig (var) + sequence (4)
        for input in &tx.inputs {
            size += 36; // prevout
            size += input.script_sig.len();
            size += 4; // sequence
        }

        // Output size: value (8) + script_pubkey (var)
        for output in &tx.outputs {
            size += 8; // value
            size += output.script_pubkey.len();
        }

        // Add witness discount if segwit (simplified - assume no witness for now)
        size
    }

    /// Remove transaction from mempool
    pub fn remove_transaction(&mut self, hash: &Hash) -> bool {
        if let Some(tx) = self.transactions.remove(hash) {
            self.mempool.remove(hash);

            // Remove spent outputs tracking
            for input in &tx.inputs {
                self.spent_outputs.remove(&input.prevout);
            }

            true
        } else {
            false
        }
    }

    /// Clear mempool
    pub fn clear(&mut self) {
        self.transactions.clear();
        self.mempool.clear();
        self.spent_outputs.clear();
    }

    /// Save mempool to disk for persistence
    pub fn save_to_disk<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        use bllvm_protocol::serialization::transaction::serialize_transaction;
        use std::fs::File;
        use std::io::Write;

        let transactions = self.get_transactions();
        let mut file = File::create(path)?;

        // Write transaction count
        file.write_all(&(transactions.len() as u32).to_le_bytes())?;

        // Write each transaction
        for tx in transactions {
            let serialized = serialize_transaction(&tx);
            file.write_all(&(serialized.len() as u32).to_le_bytes())?;
            file.write_all(&serialized)?;
        }

        file.sync_all()?;
        Ok(())
    }
}

impl Default for MempoolManager {
    fn default() -> Self {
        Self::new()
    }
}

// Implement MempoolProvider trait for integration with MiningCoordinator
impl crate::node::miner::MempoolProvider for MempoolManager {
    fn get_transactions(&self) -> Vec<bllvm_protocol::Transaction> {
        self.get_transactions()
    }

    fn get_transaction(&self, hash: &[u8; 32]) -> Option<bllvm_protocol::Transaction> {
        use bllvm_protocol::Hash;
        let hash_array: Hash = *hash;
        self.get_transaction(&hash_array)
    }

    fn get_mempool_size(&self) -> usize {
        self.size()
    }

    fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &bllvm_protocol::UtxoSet,
    ) -> Vec<bllvm_protocol::Transaction> {
        self.get_prioritized_transactions(limit, utxo_set)
    }

    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool {
        use bllvm_protocol::Hash;
        let hash_array: Hash = *hash;
        self.remove_transaction(&hash_array)
    }
}

impl MempoolManager {
    /// Load mempool from disk
    pub fn load_from_disk<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<()> {
        use bllvm_protocol::serialization::transaction::deserialize_transaction;
        use std::fs::File;
        use std::io::Read;

        let mut file = File::open(path)?;
        let mut count_bytes = [0u8; 4];
        file.read_exact(&mut count_bytes)?;
        let count = u32::from_le_bytes(count_bytes) as usize;

        for _ in 0..count {
            let mut len_bytes = [0u8; 4];
            file.read_exact(&mut len_bytes)?;
            let len = u32::from_le_bytes(len_bytes) as usize;

            let mut tx_bytes = vec![0u8; len];
            file.read_exact(&mut tx_bytes)?;

            let tx = deserialize_transaction(&tx_bytes)?;
            let _ = self.add_transaction(tx);
        }

        Ok(())
    }
}

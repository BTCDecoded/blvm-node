//! Mining coordinator
//!
//! Handles block mining, template generation, and mining coordination.

use anyhow::Result;
use bllvm_protocol::{Block, BlockHeader, Transaction};
use std::collections::HashMap;
use tracing::{debug, info};

/// Mempool provider trait for dependency injection
pub trait MempoolProvider: Send + Sync {
    /// Get transactions from mempool
    fn get_transactions(&self) -> Vec<Transaction>;

    /// Get transaction by hash
    fn get_transaction(&self, hash: &[u8; 32]) -> Option<Transaction>;

    /// Get mempool size
    fn get_mempool_size(&self) -> usize;

    /// Get prioritized transactions (by fee rate)
    fn get_prioritized_transactions(&self, limit: usize) -> Vec<Transaction>;

    /// Remove transaction from mempool
    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool;
}

/// Transaction selector for block building
pub struct TransactionSelector {
    /// Maximum block size
    max_block_size: usize,
    /// Maximum block weight
    max_block_weight: u64,
    /// Minimum fee rate (satoshis per byte)
    min_fee_rate: u64,
}

impl TransactionSelector {
    /// Create a new transaction selector
    pub fn new() -> Self {
        Self {
            max_block_size: 1_000_000,   // 1MB
            max_block_weight: 4_000_000, // 4M weight units
            min_fee_rate: 1,             // 1 satoshi per byte
        }
    }

    /// Create with custom parameters
    pub fn with_params(max_block_size: usize, max_block_weight: u64, min_fee_rate: u64) -> Self {
        Self {
            max_block_size,
            max_block_weight,
            min_fee_rate,
        }
    }

    /// Select transactions for block
    pub fn select_transactions(&self, mempool: &dyn MempoolProvider) -> Vec<Transaction> {
        let mut selected = Vec::new();
        let mut current_size = 0;
        let mut current_weight = 0;

        // Get prioritized transactions
        let transactions = mempool.get_prioritized_transactions(1000);

        for tx in transactions {
            let tx_size = self.calculate_transaction_size(&tx);
            let tx_weight = self.calculate_transaction_weight(&tx);

            // Check if adding this transaction would exceed limits
            if current_size + tx_size > self.max_block_size
                || current_weight + tx_weight > self.max_block_weight
            {
                break;
            }

            // Check minimum fee rate
            if self.calculate_fee_rate(&tx) < self.min_fee_rate {
                continue;
            }

            selected.push(tx);
            current_size += tx_size;
            current_weight += tx_weight;
        }

        selected
    }

    /// Calculate transaction size in bytes
    fn calculate_transaction_size(&self, tx: &Transaction) -> usize {
        // Simplified calculation - in real implementation would serialize
        tx.inputs.len() * 148 + tx.outputs.len() * 34 + 10
    }

    /// Calculate transaction weight
    fn calculate_transaction_weight(&self, tx: &Transaction) -> u64 {
        // Simplified calculation - in real implementation would use proper weight calculation
        self.calculate_transaction_size(tx) as u64 * 4
    }

    /// Calculate fee rate (satoshis per byte)
    fn calculate_fee_rate(&self, tx: &Transaction) -> u64 {
        // Simplified calculation - in real implementation would calculate actual fees
        let size = self.calculate_transaction_size(tx);
        if size == 0 {
            return 0;
        }

        // Mock fee calculation
        let total_output_value: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();
        let total_input_value = total_output_value + 1000; // Mock input value
        let fee = total_input_value - total_output_value;

        fee / size as u64
    }

    /// Get maximum block size
    pub fn max_block_size(&self) -> usize {
        self.max_block_size
    }

    /// Get maximum block weight
    pub fn max_block_weight(&self) -> u64 {
        self.max_block_weight
    }

    /// Get minimum fee rate
    pub fn min_fee_rate(&self) -> u64 {
        self.min_fee_rate
    }
}

/// Mining engine for block mining
pub struct MiningEngine {
    /// Mining enabled flag
    mining_enabled: bool,
    /// Mining threads
    mining_threads: u32,
    /// Current block template
    block_template: Option<Block>,
    /// Mining statistics
    stats: MiningStats,
}

#[derive(Debug, Clone)]
pub struct MiningStats {
    pub blocks_mined: u64,
    pub total_hashrate: f64,
    pub average_block_time: f64,
    pub last_block_time: Option<u64>,
}

impl MiningEngine {
    /// Create a new mining engine
    pub fn new() -> Self {
        Self {
            mining_enabled: false,
            mining_threads: 1,
            block_template: None,
            stats: MiningStats {
                blocks_mined: 0,
                total_hashrate: 0.0,
                average_block_time: 0.0,
                last_block_time: None,
            },
        }
    }

    /// Create with custom thread count
    pub fn with_threads(threads: u32) -> Self {
        Self {
            mining_enabled: false,
            mining_threads: threads,
            block_template: None,
            stats: MiningStats {
                blocks_mined: 0,
                total_hashrate: 0.0,
                average_block_time: 0.0,
                last_block_time: None,
            },
        }
    }

    /// Enable mining
    pub fn enable_mining(&mut self) {
        self.mining_enabled = true;
        info!("Mining enabled with {} threads", self.mining_threads);
    }

    /// Disable mining
    pub fn disable_mining(&mut self) {
        self.mining_enabled = false;
        info!("Mining disabled");
    }

    /// Check if mining is enabled
    pub fn is_mining_enabled(&self) -> bool {
        self.mining_enabled
    }

    /// Get mining statistics
    pub fn get_stats(&self) -> &MiningStats {
        &self.stats
    }

    /// Get mining threads
    pub fn get_threads(&self) -> u32 {
        self.mining_threads
    }

    /// Set mining threads
    pub fn set_threads(&mut self, threads: u32) {
        self.mining_threads = threads;
    }

    /// Mine a block template
    pub async fn mine_template(&mut self, template: Block) -> Result<Block> {
        debug!("Mining block template with {} threads", self.mining_threads);

        // Update template
        self.block_template = Some(template.clone());

        // In a real implementation, this would:
        // 1. Use consensus-proof to mine the block
        // 2. Try different nonce values with multiple threads
        // 3. Check proof of work

        // Simulate mining work
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Update statistics
        self.stats.blocks_mined += 1;
        self.stats.last_block_time = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );

        Ok(template)
    }

    /// Get current block template
    pub fn get_block_template(&self) -> Option<&Block> {
        self.block_template.as_ref()
    }

    /// Clear block template
    pub fn clear_template(&mut self) {
        self.block_template = None;
    }

    /// Update hashrate
    pub fn update_hashrate(&mut self, hashrate: f64) {
        self.stats.total_hashrate = hashrate;
    }

    /// Update average block time
    pub fn update_average_block_time(&mut self, block_time: f64) {
        self.stats.average_block_time = block_time;
    }
}

/// Mining coordinator
pub struct MiningCoordinator {
    /// Mining engine
    mining_engine: MiningEngine,
    /// Transaction selector
    transaction_selector: TransactionSelector,
    /// Mempool provider
    mempool_provider: MockMempoolProvider,
    /// Stratum V2 client (optional)
    #[cfg(feature = "stratum-v2")]
    stratum_v2_client: Option<crate::network::stratum_v2::client::StratumV2Client>,
}

impl Default for MiningCoordinator {
    fn default() -> Self {
        Self::new(MockMempoolProvider::new())
    }
}

impl MiningCoordinator {
    /// Create a new mining coordinator with a mempool provider
    pub fn new(mempool_provider: MockMempoolProvider) -> Self {
        Self {
            mining_engine: MiningEngine::new(),
            transaction_selector: TransactionSelector::new(),
            mempool_provider,
            #[cfg(feature = "stratum-v2")]
            stratum_v2_client: None,
        }
    }

    /// Create with custom parameters
    pub fn with_params(
        mempool_provider: MockMempoolProvider,
        threads: u32,
        max_block_size: usize,
        max_block_weight: u64,
        min_fee_rate: u64,
    ) -> Self {
        Self {
            mining_engine: MiningEngine::with_threads(threads),
            transaction_selector: TransactionSelector::with_params(
                max_block_size,
                max_block_weight,
                min_fee_rate,
            ),
            mempool_provider,
            #[cfg(feature = "stratum-v2")]
            stratum_v2_client: None,
        }
    }

    /// Set Stratum V2 client
    #[cfg(feature = "stratum-v2")]
    pub fn set_stratum_v2_client(
        &mut self,
        client: crate::network::stratum_v2::client::StratumV2Client,
    ) {
        self.stratum_v2_client = Some(client);
    }

    /// Get Stratum V2 client (if enabled)
    #[cfg(feature = "stratum-v2")]
    pub fn stratum_v2_client(
        &self,
    ) -> Option<&crate::network::stratum_v2::client::StratumV2Client> {
        self.stratum_v2_client.as_ref()
    }

    /// Start the mining coordinator
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting mining coordinator");

        // Initialize mining
        self.initialize_mining().await?;

        // Start mining loop
        self.mining_loop().await?;

        Ok(())
    }

    /// Initialize mining
    async fn initialize_mining(&mut self) -> Result<()> {
        debug!("Initializing mining");

        // Check if mining should be enabled
        // In a real implementation, this would check configuration

        Ok(())
    }

    /// Main mining loop
    async fn mining_loop(&mut self) -> Result<()> {
        loop {
            if self.mining_engine.is_mining_enabled() {
                self.mine_block().await?;
            } else {
                // Wait for mining to be enabled
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
    }

    /// Mine a block
    async fn mine_block(&mut self) -> Result<()> {
        debug!("Mining block");

        // Generate block template
        let template = self.generate_block_template().await?;

        // Mine the block
        let mined_block = self.mining_engine.mine_template(template).await?;

        // Submit the block
        self.submit_block(mined_block).await?;

        Ok(())
    }

    /// Generate block template
    async fn generate_block_template(&mut self) -> Result<Block> {
        debug!("Generating block template");

        // Select transactions from mempool
        let transactions = self
            .transaction_selector
            .select_transactions(&self.mempool_provider);

        // Create coinbase transaction (simplified)
        let coinbase_tx = self.create_coinbase_transaction().await?;

        // Build block
        let template = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: {
                let mut txs = vec![coinbase_tx];
                txs.extend(transactions);
                txs
            },
        };

        Ok(template)
    }

    /// Create coinbase transaction
    async fn create_coinbase_transaction(&self) -> Result<Transaction> {
        // Simplified coinbase transaction
        Ok(Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![bllvm_protocol::TransactionOutput {
                value: 5000000000, // 50 BTC
                script_pubkey: vec![
                    0x76, 0xa9, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0xac,
                ],
            }],
            lock_time: 0,
        })
    }

    /// Submit mined block
    async fn submit_block(&self, _block: Block) -> Result<()> {
        debug!("Submitting mined block");

        // In a real implementation, this would:
        // 1. Validate the block using consensus-proof
        // 2. Add to blockchain
        // 3. Relay to peers

        Ok(())
    }

    /// Enable mining
    pub fn enable_mining(&mut self) {
        self.mining_engine.enable_mining();
    }

    /// Disable mining
    pub fn disable_mining(&mut self) {
        self.mining_engine.disable_mining();
    }

    /// Check if mining is enabled
    pub fn is_mining_enabled(&self) -> bool {
        self.mining_engine.is_mining_enabled()
    }

    /// Get mining info
    pub fn get_mining_info(&self) -> MiningInfo {
        MiningInfo {
            enabled: self.mining_engine.is_mining_enabled(),
            threads: self.mining_engine.get_threads(),
            has_template: self.mining_engine.get_block_template().is_some(),
        }
    }

    /// Get mining statistics
    pub fn get_mining_stats(&self) -> &MiningStats {
        self.mining_engine.get_stats()
    }

    /// Get access to the mining engine
    pub fn mining_engine(&self) -> &MiningEngine {
        &self.mining_engine
    }

    /// Get mutable access to the mining engine
    pub fn mining_engine_mut(&mut self) -> &mut MiningEngine {
        &mut self.mining_engine
    }

    /// Get access to the transaction selector
    pub fn transaction_selector(&self) -> &TransactionSelector {
        &self.transaction_selector
    }

    /// Get mutable access to the transaction selector
    pub fn transaction_selector_mut(&mut self) -> &mut TransactionSelector {
        &mut self.transaction_selector
    }

    /// Get mempool size
    pub fn get_mempool_size(&self) -> usize {
        self.mempool_provider.get_mempool_size()
    }

    /// Get prioritized transactions
    pub fn get_prioritized_transactions(&self, limit: usize) -> Vec<Transaction> {
        self.mempool_provider.get_prioritized_transactions(limit)
    }

    /// Get mutable access to mempool provider
    pub fn mempool_provider_mut(&mut self) -> &mut MockMempoolProvider {
        &mut self.mempool_provider
    }
}

/// Mining information
#[derive(Debug, Clone)]
pub struct MiningInfo {
    pub enabled: bool,
    pub threads: u32,
    pub has_template: bool,
}

/// Mock mempool provider for testing
pub struct MockMempoolProvider {
    transactions: HashMap<[u8; 32], Transaction>,
    prioritized_transactions: Vec<(Transaction, u64)>,
}

impl MockMempoolProvider {
    pub fn new() -> Self {
        Self {
            transactions: HashMap::new(),
            prioritized_transactions: Vec::new(),
        }
    }

    pub fn add_transaction(&mut self, tx: Transaction) {
        let hash = self.calculate_tx_hash(&tx);
        let fee_rate = self.calculate_fee_rate(&tx);
        self.transactions.insert(hash, tx.clone());
        self.prioritized_transactions.push((tx, fee_rate));
        // Sort by fee rate (simplified)
        self.prioritized_transactions.sort_by(|a, b| b.1.cmp(&a.1));
    }

    pub fn clear(&mut self) {
        self.transactions.clear();
        self.prioritized_transactions.clear();
    }

    fn calculate_tx_hash(&self, tx: &Transaction) -> [u8; 32] {
        // Simplified hash calculation
        let mut hash = [0u8; 32];
        hash[0] = tx.version as u8;
        hash[1] = tx.inputs.len() as u8;
        hash[2] = tx.outputs.len() as u8;
        hash
    }

    fn calculate_fee_rate(&self, tx: &Transaction) -> u64 {
        // Simplified fee rate calculation - make it vary by version
        let total_output_value: u64 = tx.outputs.iter().map(|out| out.value as u64).sum();
        let total_input_value = total_output_value + (tx.version as u64 * 1000); // Mock input value varies by version
        let fee = total_input_value - total_output_value;
        let size = tx.inputs.len() * 148 + tx.outputs.len() * 34 + 10;
        if size == 0 {
            return 0;
        }
        fee / size as u64
    }
}

impl MempoolProvider for MockMempoolProvider {
    fn get_transactions(&self) -> Vec<Transaction> {
        self.transactions.values().cloned().collect()
    }

    fn get_transaction(&self, hash: &[u8; 32]) -> Option<Transaction> {
        self.transactions.get(hash).cloned()
    }

    fn get_mempool_size(&self) -> usize {
        self.transactions.len()
    }

    fn get_prioritized_transactions(&self, limit: usize) -> Vec<Transaction> {
        self.prioritized_transactions
            .iter()
            .take(limit)
            .map(|(tx, _)| tx.clone())
            .collect()
    }

    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool {
        if let Some(tx) = self.transactions.remove(hash) {
            self.prioritized_transactions.retain(|(t, _)| t != &tx);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bllvm_protocol::{OutPoint, TransactionInput, TransactionOutput};

    #[test]
    fn test_transaction_selector_creation() {
        let selector = TransactionSelector::new();
        assert_eq!(selector.max_block_size(), 1_000_000);
        assert_eq!(selector.max_block_weight(), 4_000_000);
        assert_eq!(selector.min_fee_rate(), 1);
    }

    #[test]
    fn test_transaction_selector_with_params() {
        let selector = TransactionSelector::with_params(2_000_000, 8_000_000, 5);
        assert_eq!(selector.max_block_size(), 2_000_000);
        assert_eq!(selector.max_block_weight(), 8_000_000);
        assert_eq!(selector.min_fee_rate(), 5);
    }

    #[test]
    fn test_transaction_selector_transaction_selection() {
        let selector = TransactionSelector::new();
        let mut mempool = MockMempoolProvider::new();

        // Add some test transactions
        let tx1 = create_test_transaction(1, 1000);
        let tx2 = create_test_transaction(2, 2000);
        let tx3 = create_test_transaction(3, 500);

        mempool.add_transaction(tx1);
        mempool.add_transaction(tx2);
        mempool.add_transaction(tx3);

        let selected = selector.select_transactions(&mempool);
        assert!(!selected.is_empty());
        assert!(selected.len() <= 3);
    }

    #[test]
    fn test_transaction_selector_size_calculation() {
        let selector = TransactionSelector::new();
        let tx = create_test_transaction(1, 1000);

        let size = selector.calculate_transaction_size(&tx);
        assert!(size > 0);

        let weight = selector.calculate_transaction_weight(&tx);
        assert!(weight > 0);

        let fee_rate = selector.calculate_fee_rate(&tx);
        assert!(fee_rate > 0);
    }

    #[test]
    fn test_mining_engine_creation() {
        let engine = MiningEngine::new();
        assert!(!engine.is_mining_enabled());
        assert_eq!(engine.get_threads(), 1);
        assert!(engine.get_block_template().is_none());
        assert_eq!(engine.get_stats().blocks_mined, 0);
    }

    #[test]
    fn test_mining_engine_with_threads() {
        let engine = MiningEngine::with_threads(4);
        assert!(!engine.is_mining_enabled());
        assert_eq!(engine.get_threads(), 4);
    }

    #[test]
    fn test_mining_engine_enable_disable() {
        let mut engine = MiningEngine::new();

        assert!(!engine.is_mining_enabled());
        engine.enable_mining();
        assert!(engine.is_mining_enabled());

        engine.disable_mining();
        assert!(!engine.is_mining_enabled());
    }

    #[test]
    fn test_mining_engine_thread_management() {
        let mut engine = MiningEngine::new();

        assert_eq!(engine.get_threads(), 1);
        engine.set_threads(8);
        assert_eq!(engine.get_threads(), 8);
    }

    #[tokio::test]
    async fn test_mining_engine_mine_template() {
        let mut engine = MiningEngine::new();
        let template = create_test_block();

        let result = engine.mine_template(template.clone()).await;
        assert!(result.is_ok());

        let mined_block = result.unwrap();
        assert_eq!(mined_block.header.version, template.header.version);

        // Check that template was stored
        assert!(engine.get_block_template().is_some());
        assert_eq!(engine.get_stats().blocks_mined, 1);
    }

    #[test]
    fn test_mining_engine_template_management() {
        let mut engine = MiningEngine::new();

        assert!(engine.get_block_template().is_none());

        let template = create_test_block();
        engine.block_template = Some(template.clone());

        assert!(engine.get_block_template().is_some());
        assert_eq!(
            engine.get_block_template().unwrap().header.version,
            template.header.version
        );

        engine.clear_template();
        assert!(engine.get_block_template().is_none());
    }

    #[test]
    fn test_mining_engine_statistics() {
        let mut engine = MiningEngine::new();
        let stats = engine.get_stats();

        assert_eq!(stats.blocks_mined, 0);
        assert_eq!(stats.total_hashrate, 0.0);
        assert_eq!(stats.average_block_time, 0.0);
        assert!(stats.last_block_time.is_none());

        engine.update_hashrate(1000.0);
        assert_eq!(engine.get_stats().total_hashrate, 1000.0);

        engine.update_average_block_time(600.0);
        assert_eq!(engine.get_stats().average_block_time, 600.0);
    }

    #[test]
    fn test_mock_mempool_provider_creation() {
        let mempool = MockMempoolProvider::new();
        assert_eq!(mempool.get_mempool_size(), 0);
        assert!(mempool.get_transactions().is_empty());
        assert!(mempool.get_prioritized_transactions(10).is_empty());
    }

    #[test]
    fn test_mock_mempool_provider_transaction_management() {
        let mut mempool = MockMempoolProvider::new();

        let tx1 = create_test_transaction(1, 1000);
        let tx2 = create_test_transaction(2, 2000);

        mempool.add_transaction(tx1.clone());
        mempool.add_transaction(tx2.clone());

        assert_eq!(mempool.get_mempool_size(), 2);
        assert_eq!(mempool.get_transactions().len(), 2);

        let prioritized = mempool.get_prioritized_transactions(10);
        assert_eq!(prioritized.len(), 2);

        // Test transaction removal
        let hash = mempool.calculate_tx_hash(&tx1);
        assert!(mempool.remove_transaction(&hash));
        assert_eq!(mempool.get_mempool_size(), 1);

        // Test removal of non-existent transaction
        let fake_hash = [0u8; 32];
        assert!(!mempool.remove_transaction(&fake_hash));
    }

    #[test]
    fn test_mock_mempool_provider_prioritization() {
        let mut mempool = MockMempoolProvider::new();

        // Add transactions with different fee rates
        let tx_low_fee = create_test_transaction(1, 100); // Low fee
        let tx_high_fee = create_test_transaction(2, 5000); // High fee
        let tx_medium_fee = create_test_transaction(3, 1000); // Medium fee

        mempool.add_transaction(tx_low_fee);
        mempool.add_transaction(tx_high_fee);
        mempool.add_transaction(tx_medium_fee);

        let prioritized = mempool.get_prioritized_transactions(10);
        assert_eq!(prioritized.len(), 3);

        // Transactions should be sorted by fee rate (descending)
        // Version 3 (medium fee) should be first, then version 2 (high fee), then version 1 (low fee)
        assert_eq!(prioritized[0].version, 3);
        assert_eq!(prioritized[1].version, 2);
        assert_eq!(prioritized[2].version, 1);
    }

    #[test]
    fn test_mock_mempool_provider_clear() {
        let mut mempool = MockMempoolProvider::new();

        let tx = create_test_transaction(1, 1000);
        mempool.add_transaction(tx);

        assert_eq!(mempool.get_mempool_size(), 1);

        mempool.clear();
        assert_eq!(mempool.get_mempool_size(), 0);
        assert!(mempool.get_transactions().is_empty());
    }

    #[test]
    fn test_mining_coordinator_creation() {
        let mempool = MockMempoolProvider::new();
        let coordinator = MiningCoordinator::new(mempool);

        assert!(!coordinator.is_mining_enabled());
        assert_eq!(coordinator.get_mempool_size(), 0);
        assert_eq!(coordinator.mining_engine().get_threads(), 1);
        assert_eq!(
            coordinator.transaction_selector().max_block_size(),
            1_000_000
        );
    }

    #[test]
    fn test_mining_coordinator_with_params() {
        let mempool = MockMempoolProvider::new();
        let coordinator = MiningCoordinator::with_params(mempool, 4, 2_000_000, 8_000_000, 5);

        assert_eq!(coordinator.mining_engine().get_threads(), 4);
        assert_eq!(
            coordinator.transaction_selector().max_block_size(),
            2_000_000
        );
        assert_eq!(
            coordinator.transaction_selector().max_block_weight(),
            8_000_000
        );
        assert_eq!(coordinator.transaction_selector().min_fee_rate(), 5);
    }

    #[test]
    fn test_mining_coordinator_enable_disable() {
        let mempool = MockMempoolProvider::new();
        let mut coordinator = MiningCoordinator::new(mempool);

        assert!(!coordinator.is_mining_enabled());
        coordinator.enable_mining();
        assert!(coordinator.is_mining_enabled());

        coordinator.disable_mining();
        assert!(!coordinator.is_mining_enabled());
    }

    #[test]
    fn test_mining_coordinator_info() {
        let mempool = MockMempoolProvider::new();
        let coordinator = MiningCoordinator::new(mempool);

        let info = coordinator.get_mining_info();
        assert!(!info.enabled);
        assert_eq!(info.threads, 1);
        assert!(!info.has_template);
    }

    #[test]
    fn test_mining_coordinator_statistics() {
        let mempool = MockMempoolProvider::new();
        let coordinator = MiningCoordinator::new(mempool);

        let stats = coordinator.get_mining_stats();
        assert_eq!(stats.blocks_mined, 0);
        assert_eq!(stats.total_hashrate, 0.0);
        assert_eq!(stats.average_block_time, 0.0);
        assert!(stats.last_block_time.is_none());
    }

    #[test]
    fn test_mining_coordinator_accessors() {
        let mempool = MockMempoolProvider::new();
        let coordinator = MiningCoordinator::new(mempool);

        // Test immutable access
        let engine = coordinator.mining_engine();
        assert_eq!(engine.get_threads(), 1);

        let selector = coordinator.transaction_selector();
        assert_eq!(selector.max_block_size(), 1_000_000);

        // Test mutable access
        let mut coordinator = coordinator;
        let engine_mut = coordinator.mining_engine_mut();
        engine_mut.set_threads(4);
        assert_eq!(coordinator.mining_engine().get_threads(), 4);

        let selector_mut = coordinator.transaction_selector_mut();
        // Test that we can access the selector
        assert_eq!(selector_mut.max_block_size(), 1_000_000);
    }

    #[test]
    fn test_mining_coordinator_mempool_operations() {
        let mut mempool = MockMempoolProvider::new();
        let tx = create_test_transaction(1, 1000);
        mempool.add_transaction(tx);

        let coordinator = MiningCoordinator::new(mempool);

        assert_eq!(coordinator.get_mempool_size(), 1);

        let prioritized = coordinator.get_prioritized_transactions(10);
        assert_eq!(prioritized.len(), 1);
        assert_eq!(prioritized[0].version, 1);
    }

    #[tokio::test]
    async fn test_mining_coordinator_block_template_generation() {
        let mut mempool = MockMempoolProvider::new();
        let tx = create_test_transaction(1, 1000);
        mempool.add_transaction(tx);

        let mut coordinator = MiningCoordinator::new(mempool);

        let template = coordinator.generate_block_template().await;
        assert!(template.is_ok());

        let block = template.unwrap();
        assert_eq!(block.header.version, 1);
        assert!(!block.transactions.is_empty()); // Should have coinbase + mempool tx
    }

    #[tokio::test]
    async fn test_mining_coordinator_coinbase_creation() {
        let mempool = MockMempoolProvider::new();
        let coordinator = MiningCoordinator::new(mempool);

        let coinbase = coordinator.create_coinbase_transaction().await;
        assert!(coinbase.is_ok());

        let tx = coinbase.unwrap();
        assert_eq!(tx.version, 1);
        assert!(tx.inputs.is_empty()); // Coinbase has no inputs
        assert_eq!(tx.outputs.len(), 1);
        assert_eq!(tx.outputs[0].value, 5000000000); // 50 BTC
        assert_eq!(tx.lock_time, 0);
    }

    // Helper functions for tests
    fn create_test_transaction(version: i32, output_value: u64) -> Transaction {
        Transaction {
            version: version as u64,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0,
                },
                script_sig: vec![
                    0x76, 0xa9, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0xac,
                ],
                sequence: 0xffffffff,
            }],
            outputs: vec![TransactionOutput {
                value: output_value as i64,
                script_pubkey: vec![
                    0x76, 0xa9, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0xac,
                ],
            }],
            lock_time: 0,
        }
    }

    fn create_test_block() -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1231006505,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![create_test_transaction(1, 1000)],
        }
    }
}

//! Mining coordinator
//!
//! Handles block mining, template generation, and mining coordination.

use crate::utils::current_timestamp;
use anyhow::Result;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, BlockHeader, Transaction};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Mempool provider trait for dependency injection
pub trait MempoolProvider: Send + Sync {
    /// Get transactions from mempool
    fn get_transactions(&self) -> Vec<Transaction>;

    /// Get transaction by hash
    fn get_transaction(&self, hash: &[u8; 32]) -> Option<Transaction>;

    /// Get mempool size
    fn get_mempool_size(&self) -> usize;

    /// Get prioritized transactions (by fee rate)
    /// Requires UTXO set for accurate fee calculation
    fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Vec<Transaction>;

    /// Remove transaction from mempool
    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool;

    /// SegWit witness stacks for a mempool transaction (one per input), if stored.
    fn get_transaction_witnesses(&self, hash: &[u8; 32]) -> Option<Vec<Witness>>;
}

/// Outcome of offering one transaction to the template.
#[derive(PartialEq, Eq)]
enum BlockSelect {
    Added,
    Skip,
    Full,
}

/// Transaction selector for block building
pub struct TransactionSelector {
    /// Maximum block size
    max_block_size: usize,
    /// Maximum block weight
    max_block_weight: u64,
    /// Minimum fee rate (satoshis per vbyte)
    min_fee_rate: u64,
}

impl Default for TransactionSelector {
    fn default() -> Self {
        Self::new()
    }
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
    /// Note: Requires UTXO set for fee calculation - caller must provide it
    ///
    /// Candidates arrive in fee-rate order. A spend of a parent still in the pool
    /// is held until that parent is selected, so the template lists the parent first.
    /// A transaction that does not fit is skipped. Later transactions that do fit
    /// are still included. Parents below the fee floor are included when a waiting
    /// descendant pays a package rate at or above that floor. The package contains
    /// that descendant and every unselected ancestor it still spends.
    pub fn select_transactions(
        &self,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Vec<Transaction> {
        use std::collections::{HashSet, VecDeque};

        let mut selected = Vec::new();
        let mut selected_ids = HashSet::new();
        let mut current_size = 0usize;
        let mut current_weight = 0u64;
        let mut waiting: VecDeque<Transaction> = VecDeque::new();

        let transactions = mempool.get_prioritized_transactions(1000, utxo_set);

        for tx in transactions {
            if selected_ids.contains(&blvm_protocol::block::calculate_tx_id(&tx)) {
                continue;
            }
            if self.parent_still_unselected(&tx, mempool, utxo_set, &selected_ids) {
                waiting.push_back(tx);
                continue;
            }
            match self.try_add_to_block(
                &tx,
                mempool,
                utxo_set,
                &mut selected,
                &mut selected_ids,
                &mut current_size,
                &mut current_weight,
                true,
            ) {
                BlockSelect::Added => {
                    self.take_ready_dependents(
                        mempool,
                        utxo_set,
                        &mut waiting,
                        &mut selected,
                        &mut selected_ids,
                        &mut current_size,
                        &mut current_weight,
                    );
                }
                BlockSelect::Skip => {
                    if let Some((parents, child)) =
                        self.paying_package(&tx, mempool, utxo_set, &waiting, &selected_ids)
                    {
                        let mut package: Vec<&Transaction> = parents.iter().collect();
                        package.push(&child);
                        if self.package_fits(&package, mempool, current_size, current_weight) {
                            let mut added = false;
                            for parent_tx in &parents {
                                if selected_ids
                                    .contains(&blvm_protocol::block::calculate_tx_id(parent_tx))
                                {
                                    continue;
                                }
                                if self.try_add_to_block(
                                    parent_tx,
                                    mempool,
                                    utxo_set,
                                    &mut selected,
                                    &mut selected_ids,
                                    &mut current_size,
                                    &mut current_weight,
                                    false,
                                ) == BlockSelect::Added
                                {
                                    added = true;
                                }
                            }
                            if added {
                                self.take_ready_dependents(
                                    mempool,
                                    utxo_set,
                                    &mut waiting,
                                    &mut selected,
                                    &mut selected_ids,
                                    &mut current_size,
                                    &mut current_weight,
                                );
                            }
                        }
                    }
                }
                BlockSelect::Full => {}
            }
        }

        let waiting_children: Vec<Transaction> = waiting.drain(..).collect();
        for child in waiting_children {
            if selected_ids.contains(&blvm_protocol::block::calculate_tx_id(&child)) {
                continue;
            }
            let Some(parents) = self.unselected_ancestors(&child, mempool, utxo_set, &selected_ids)
            else {
                continue;
            };
            if parents.is_empty() {
                continue;
            }
            let mut package_fee = self.transaction_fee(&child, utxo_set, mempool);
            let mut package_vsize = self.tx_vsize(&child, mempool);
            for parent_tx in &parents {
                package_fee =
                    package_fee.saturating_add(self.transaction_fee(parent_tx, utxo_set, mempool));
                package_vsize = package_vsize.saturating_add(self.tx_vsize(parent_tx, mempool));
            }
            if package_vsize == 0 || package_fee / (package_vsize as u64) < self.min_fee_rate {
                continue;
            }
            for parent_tx in &parents {
                if selected_ids.contains(&blvm_protocol::block::calculate_tx_id(parent_tx)) {
                    continue;
                }
                let _ = self.try_add_to_block(
                    parent_tx,
                    mempool,
                    utxo_set,
                    &mut selected,
                    &mut selected_ids,
                    &mut current_size,
                    &mut current_weight,
                    false,
                );
            }
            let _ = self.try_add_to_block(
                &child,
                mempool,
                utxo_set,
                &mut selected,
                &mut selected_ids,
                &mut current_size,
                &mut current_weight,
                false,
            );
        }

        selected
    }

    /// True when an input spends a pool transaction that is not yet in the template.
    fn parent_still_unselected(
        &self,
        tx: &Transaction,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
        selected_ids: &std::collections::HashSet<[u8; 32]>,
    ) -> bool {
        tx.inputs.iter().any(|input| {
            if utxo_set.get(&input.prevout).is_some() {
                return false;
            }
            mempool.get_transaction(&input.prevout.hash).is_some()
                && !selected_ids.contains(&input.prevout.hash)
        })
    }

    fn try_add_to_block(
        &self,
        tx: &Transaction,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
        selected: &mut Vec<Transaction>,
        selected_ids: &mut std::collections::HashSet<[u8; 32]>,
        current_size: &mut usize,
        current_weight: &mut u64,
        enforce_fee: bool,
    ) -> BlockSelect {
        use blvm_protocol::block::calculate_tx_id;

        let txid = calculate_tx_id(tx);
        let input_witnesses = mempool.get_transaction_witnesses(&txid);
        let tx_stripped = blvm_consensus::transaction::calculate_transaction_size(tx);
        let tx_weight = bip141_weight(tx, input_witnesses.as_deref());
        let tx_vsize = transaction_vsize(tx, input_witnesses.as_deref());

        if *current_size + tx_stripped > self.max_block_size
            || *current_weight + tx_weight > self.max_block_weight
        {
            return BlockSelect::Full;
        }

        let fee_rate = self.calculate_fee_rate_with_utxo(tx, utxo_set, tx_vsize, mempool);
        if enforce_fee && fee_rate < self.min_fee_rate {
            return BlockSelect::Skip;
        }

        selected.push(tx.clone());
        selected_ids.insert(txid);
        *current_size += tx_stripped;
        *current_weight += tx_weight;
        BlockSelect::Added
    }

    /// Append deferred spends whose parents are now in the template.
    ///
    /// A ready spend that does not fit is left out. The rest of the queue is still tried.
    fn take_ready_dependents(
        &self,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
        waiting: &mut std::collections::VecDeque<Transaction>,
        selected: &mut Vec<Transaction>,
        selected_ids: &mut std::collections::HashSet<[u8; 32]>,
        current_size: &mut usize,
        current_weight: &mut u64,
    ) {
        use std::collections::VecDeque;

        loop {
            let mut added = false;
            let mut still_waiting = VecDeque::new();
            while let Some(tx) = waiting.pop_front() {
                if selected_ids.contains(&blvm_protocol::block::calculate_tx_id(&tx)) {
                    continue;
                }
                if self.parent_still_unselected(&tx, mempool, utxo_set, selected_ids) {
                    still_waiting.push_back(tx);
                    continue;
                }
                match self.try_add_to_block(
                    &tx,
                    mempool,
                    utxo_set,
                    selected,
                    selected_ids,
                    current_size,
                    current_weight,
                    true,
                ) {
                    BlockSelect::Added => added = true,
                    BlockSelect::Skip | BlockSelect::Full => {}
                }
            }
            *waiting = still_waiting;
            if !added {
                return;
            }
        }
    }

    /// Ancestors a waiting descendant will pay for, together with that descendant.
    ///
    /// The descendant meets the fee floor on its own. Its unselected ancestors are
    /// ordered so each parent precedes its child. The package includes `anchor`.
    fn paying_package(
        &self,
        anchor: &Transaction,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
        waiting: &std::collections::VecDeque<Transaction>,
        selected_ids: &std::collections::HashSet<[u8; 32]>,
    ) -> Option<(Vec<Transaction>, Transaction)> {
        use blvm_protocol::block::calculate_tx_id;

        let anchor_id = calculate_tx_id(anchor);
        for child in waiting {
            let child_vsize = self.tx_vsize(child, mempool);
            if child_vsize == 0 {
                continue;
            }
            let child_fee = self.transaction_fee(child, utxo_set, mempool);
            if child_fee / (child_vsize as u64) < self.min_fee_rate {
                continue;
            }
            let Some(parents) = self.unselected_ancestors(child, mempool, utxo_set, selected_ids)
            else {
                continue;
            };
            if !parents
                .iter()
                .any(|parent| calculate_tx_id(parent) == anchor_id)
            {
                continue;
            }

            let mut package_fee = child_fee;
            let mut package_vsize = child_vsize;
            for parent_tx in &parents {
                package_fee =
                    package_fee.saturating_add(self.transaction_fee(parent_tx, utxo_set, mempool));
                package_vsize = package_vsize.saturating_add(self.tx_vsize(parent_tx, mempool));
            }
            if package_vsize > 0 && package_fee / (package_vsize as u64) >= self.min_fee_rate {
                return Some((parents, child.clone()));
            }
        }
        None
    }

    /// Unselected mempool ancestors of `tx`, each parent before its children.
    fn unselected_ancestors(
        &self,
        tx: &Transaction,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
        selected_ids: &std::collections::HashSet<[u8; 32]>,
    ) -> Option<Vec<Transaction>> {
        let mut ordered = Vec::new();
        let mut visiting = std::collections::HashSet::new();
        let mut done = std::collections::HashSet::new();

        fn walk(
            tx: &Transaction,
            mempool: &dyn MempoolProvider,
            utxo_set: &blvm_protocol::UtxoSet,
            selected_ids: &std::collections::HashSet<[u8; 32]>,
            visiting: &mut std::collections::HashSet<[u8; 32]>,
            done: &mut std::collections::HashSet<[u8; 32]>,
            ordered: &mut Vec<Transaction>,
        ) -> bool {
            for input in &tx.inputs {
                if utxo_set.get(&input.prevout).is_some()
                    || selected_ids.contains(&input.prevout.hash)
                    || done.contains(&input.prevout.hash)
                {
                    continue;
                }
                if !visiting.insert(input.prevout.hash) {
                    return false;
                }
                let Some(parent_tx) = mempool.get_transaction(&input.prevout.hash) else {
                    return false;
                };
                if parent_tx
                    .outputs
                    .get(input.prevout.index as usize)
                    .is_none()
                {
                    return false;
                }
                if !walk(
                    &parent_tx,
                    mempool,
                    utxo_set,
                    selected_ids,
                    visiting,
                    done,
                    ordered,
                ) {
                    return false;
                }
                visiting.remove(&input.prevout.hash);
                done.insert(input.prevout.hash);
                ordered.push(parent_tx);
            }
            true
        }

        if !walk(
            tx,
            mempool,
            utxo_set,
            selected_ids,
            &mut visiting,
            &mut done,
            &mut ordered,
        ) {
            return None;
        }
        Some(ordered)
    }

    fn package_fits(
        &self,
        txs: &[&Transaction],
        mempool: &dyn MempoolProvider,
        current_size: usize,
        current_weight: u64,
    ) -> bool {
        use blvm_protocol::block::calculate_tx_id;

        let mut size = current_size;
        let mut weight = current_weight;
        for tx in txs {
            let witnesses = mempool.get_transaction_witnesses(&calculate_tx_id(tx));
            let stripped = blvm_consensus::transaction::calculate_transaction_size(tx);
            let tx_weight = bip141_weight(tx, witnesses.as_deref());
            size = size.saturating_add(stripped);
            weight = weight.saturating_add(tx_weight);
            if size > self.max_block_size || weight > self.max_block_weight {
                return false;
            }
        }
        true
    }

    fn tx_vsize(&self, tx: &Transaction, mempool: &dyn MempoolProvider) -> usize {
        use blvm_protocol::block::calculate_tx_id;

        let witnesses = mempool.get_transaction_witnesses(&calculate_tx_id(tx));
        transaction_vsize(tx, witnesses.as_deref())
    }

    /// Fee rate (sat/vB) using the chain UTXO set and virtual size.
    ///
    /// A prevout missing from the chain set is priced from the parent transaction
    /// still in the pool. The running input sum stays within `MAX_MONEY`.
    fn calculate_fee_rate_with_utxo(
        &self,
        tx: &Transaction,
        utxo_set: &blvm_protocol::UtxoSet,
        tx_vsize: usize,
        mempool: &dyn MempoolProvider,
    ) -> u64 {
        if tx_vsize == 0 {
            return 0;
        }
        self.transaction_fee(tx, utxo_set, mempool) / tx_vsize as u64
    }

    fn transaction_fee(
        &self,
        tx: &Transaction,
        utxo_set: &blvm_protocol::UtxoSet,
        mempool: &dyn MempoolProvider,
    ) -> u64 {
        let max_money = blvm_protocol::constants::MAX_MONEY;
        let mut input_total = 0u64;
        for input in &tx.inputs {
            let sats = if let Some(utxo) = utxo_set.get(&input.prevout) {
                (0..=max_money)
                    .contains(&utxo.value)
                    .then_some(utxo.value as u64)
            } else {
                mempool
                    .get_transaction(&input.prevout.hash)
                    .and_then(|parent| parent.outputs.get(input.prevout.index as usize).cloned())
                    .and_then(|output| {
                        (0..=max_money)
                            .contains(&output.value)
                            .then_some(output.value as u64)
                    })
            };
            if let Some(sats) = sats {
                let Some(sum) = input_total
                    .checked_add(sats)
                    .filter(|sum| *sum <= max_money as u64)
                else {
                    return 0;
                };
                input_total = sum;
            }
        }

        // An output outside the money range is not a fee. Casting it would wrap.
        let Some(output_total) = crate::node::mempool::output_sum_sats(tx) else {
            return 0;
        };
        input_total.saturating_sub(output_total)
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

/// BIP141 weight: 4 × stripped_size + total_size (witness bytes included in total).
fn bip141_weight(tx: &Transaction, input_witnesses: Option<&[Witness]>) -> u64 {
    let base = blvm_consensus::transaction::calculate_transaction_size(tx) as u64;
    let witness_bytes: u64 = input_witnesses
        .map(|wits| {
            wits.iter()
                .flat_map(|stack| stack.iter())
                .map(|elem| elem.len() as u64)
                .sum()
        })
        .unwrap_or(0);
    let total = base + witness_bytes;
    base.saturating_mul(4).saturating_add(total)
}

/// Virtual size (vbytes) from BIP141 weight.
fn transaction_vsize(tx: &Transaction, input_witnesses: Option<&[Witness]>) -> usize {
    use blvm_consensus::witness::weight_to_vsize;
    weight_to_vsize(bip141_weight(tx, input_witnesses)) as usize
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

impl Default for MiningEngine {
    fn default() -> Self {
        Self::new()
    }
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

    /// Mine a block template using actual proof of work (async, multithreaded)
    pub async fn mine_template(&mut self, template: Block) -> Result<Block> {
        debug!("Mining block template with {} threads", self.mining_threads);

        // Update template
        self.block_template = Some(template.clone());

        // Use consensus layer to mine the block (actual PoW)
        use blvm_protocol::ConsensusProof;
        let consensus = ConsensusProof::new();

        // Calculate max attempts per thread based on difficulty
        // For regtest: low difficulty, should find nonce quickly
        // For testnet/mainnet: high difficulty, may need many attempts
        let max_attempts_per_thread = 1_000_000u64; // Reasonable limit per thread

        // Multi-threaded mining: spawn tasks for each thread
        if self.mining_threads > 1 {
            self.mine_template_multithreaded(template, max_attempts_per_thread, &consensus)
                .await
        } else {
            // Single-threaded: use blocking task to avoid blocking async runtime
            let template_clone = template.clone();
            let (mined_block, result) = tokio::task::spawn_blocking(move || {
                consensus.mine_block(template_clone, max_attempts_per_thread)
            })
            .await
            .map_err(|e| anyhow::anyhow!("Mining task panicked: {}", e))?
            .map_err(|e| anyhow::anyhow!("Mining failed: {}", e))?;

            self.handle_mining_result(mined_block, result)
        }
    }

    /// Multi-threaded mining implementation
    async fn mine_template_multithreaded(
        &mut self,
        template: Block,
        max_attempts_per_thread: u64,
        _consensus: &blvm_protocol::ConsensusProof,
    ) -> Result<Block> {
        use blvm_protocol::mining::MiningResult;
        use blvm_protocol::pow::check_proof_of_work;
        use tokio::sync::oneshot;

        // Calculate nonce range per thread
        let nonces_per_thread = max_attempts_per_thread;
        let total_threads = self.mining_threads as u64;

        // Spawn mining tasks for each thread
        let mut handles = Vec::new();
        for thread_id in 0..total_threads {
            let template_clone = template.clone();
            let start_nonce = thread_id * nonces_per_thread;
            let end_nonce = start_nonce + nonces_per_thread;

            let (tx, rx) = oneshot::channel();

            // Spawn blocking task for CPU-bound mining work
            let handle = tokio::task::spawn_blocking(move || {
                // Try nonces in this thread's range
                for nonce in start_nonce..end_nonce {
                    let mut block = template_clone.clone();
                    block.header.nonce = nonce;

                    // Check proof of work using standalone function
                    if let Ok(valid) = check_proof_of_work(&block.header) {
                        if valid {
                            let _ = tx.send(Ok((block, MiningResult::Success)));
                            return;
                        }
                    }
                }

                // No valid nonce found in this range
                let mut block = template_clone;
                block.header.nonce = start_nonce; // Last nonce tried (failure result)
                let _ = tx.send(Ok((block, MiningResult::Failure)));
            });

            handles.push((handle, rx));
        }

        // Wait for first successful result or all failures
        let mut results = Vec::new();
        for (handle, rx) in handles {
            // Wait for task completion
            handle
                .await
                .map_err(|e| anyhow::anyhow!("Mining task panicked: {}", e))?;

            // Get result
            match rx.await {
                Ok(Ok((block, result))) => {
                    if matches!(result, MiningResult::Success) {
                        // Found valid nonce! Return immediately
                        return self.handle_mining_result(block, result);
                    }
                    results.push((block, result));
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // Channel closed, task may have found solution
                    continue;
                }
            }
        }

        // All threads failed
        if let Some((block, _)) = results.first() {
            self.handle_mining_result(block.clone(), MiningResult::Failure)
        } else {
            Err(anyhow::anyhow!("Mining failed: all threads exhausted"))
        }
    }

    /// Handle mining result and update statistics
    fn handle_mining_result(
        &mut self,
        mined_block: Block,
        result: blvm_protocol::mining::MiningResult,
    ) -> Result<Block> {
        use blvm_protocol::mining::MiningResult;

        match result {
            MiningResult::Success => {
                info!(
                    "Successfully mined block with nonce {}",
                    mined_block.header.nonce
                );

                // Update statistics
                self.stats.blocks_mined += 1;
                self.stats.last_block_time = Some(current_timestamp());

                Ok(mined_block)
            }
            MiningResult::Failure => {
                // Could not find valid nonce in max_attempts
                // This is normal for high difficulty (mainnet)
                warn!("Could not find valid nonce (difficulty may be too high)");
                Err(anyhow::anyhow!("Mining failed: could not find valid nonce"))
            }
        }
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
    /// Mempool manager (real implementation)
    mempool: std::sync::Arc<crate::node::mempool::MempoolManager>,
    /// Storage for UTXO set access
    storage: Option<std::sync::Arc<crate::storage::Storage>>,
    /// Protocol engine for connecting mined blocks
    protocol: Option<std::sync::Arc<blvm_protocol::BitcoinProtocolEngine>>,
    /// Optional module event bus (NewBlock / BlockMined after a connect).
    event_publisher: Option<std::sync::Arc<crate::node::event_publisher::EventPublisher>>,
    /// Shared Commons GBT slot (empty = dummy single-output coinbase).
    commons_gbt: crate::rpc::mining::CommonsGbtSlot,
}

impl MiningCoordinator {
    /// Create a new mining coordinator with real mempool and storage
    pub fn new(
        mempool: std::sync::Arc<crate::node::mempool::MempoolManager>,
        storage: Option<std::sync::Arc<crate::storage::Storage>>,
    ) -> Self {
        Self {
            mining_engine: MiningEngine::new(),
            transaction_selector: TransactionSelector::new(),
            mempool,
            storage,
            protocol: None,
            event_publisher: None,
            commons_gbt: crate::rpc::mining::CommonsGbtSlot::default(),
        }
    }

    /// Create with custom parameters
    pub fn with_params(
        mempool: std::sync::Arc<crate::node::mempool::MempoolManager>,
        storage: Option<std::sync::Arc<crate::storage::Storage>>,
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
            mempool,
            storage,
            protocol: None,
            event_publisher: None,
            commons_gbt: crate::rpc::mining::CommonsGbtSlot::default(),
        }
    }

    /// Wire protocol engine so mined blocks can be connected to the chain.
    pub fn set_protocol_engine(
        &mut self,
        protocol: std::sync::Arc<blvm_protocol::BitcoinProtocolEngine>,
    ) {
        self.protocol = Some(protocol);
    }

    pub fn set_event_publisher(
        &mut self,
        publisher: Option<std::sync::Arc<crate::node::event_publisher::EventPublisher>>,
    ) {
        self.event_publisher = publisher;
    }

    /// Share the RPC Commons GBT slot so a find pays the notebook.
    pub fn set_commons_gbt_slot(&mut self, slot: crate::rpc::mining::CommonsGbtSlot) {
        self.commons_gbt = slot;
    }

    /// Start the mining coordinator
    pub async fn start(&mut self) -> Result<()> {
        info!(
            "Starting mining coordinator (enabled={})",
            self.mining_engine.is_mining_enabled()
        );
        self.mining_loop().await?;
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
    pub async fn generate_block_template(&mut self) -> Result<Block> {
        debug!("Generating block template");

        // Get chain tip from storage for prev_block_hash and difficulty
        let (prev_block_hash, bits, height) = if let Some(ref storage) = self.storage {
            if let Some(tip_header) = storage
                .chain()
                .get_tip_header()
                .map_err(|e| anyhow::anyhow!("Failed to get tip header: {}", e))?
            {
                let tip_hash = storage
                    .chain()
                    .get_tip_hash()
                    .map_err(|e| anyhow::anyhow!("Failed to get tip hash: {}", e))?
                    .unwrap_or([0u8; 32]);
                let chain_height = storage
                    .chain()
                    .get_height()
                    .map_err(|e| anyhow::anyhow!("Failed to get chain height: {}", e))?
                    .unwrap_or(0);
                (tip_hash, tip_header.bits, chain_height)
            } else {
                // No chain tip - use genesis defaults
                ([0u8; 32], 0x1d00ffff, 0)
            }
        } else {
            // No storage - use defaults
            ([0u8; 32], 0x1d00ffff, 0)
        };

        // Get UTXO set from storage for fee calculation
        let utxo_set = if let Some(ref storage) = self.storage {
            storage
                .utxos()
                .get_all_utxos()
                .map_err(|e| anyhow::anyhow!("Failed to get UTXO set: {}", e))?
        } else {
            // No storage - use empty UTXO set (will result in 0 fees)
            blvm_protocol::UtxoSet::default()
        };

        // Select transactions from mempool (with UTXO set for accurate fee calculation)
        let transactions = self
            .transaction_selector
            .select_transactions(&*self.mempool as &dyn MempoolProvider, &utxo_set);

        // Commons outputs when the module is issuing work; else the dummy single payout.
        let coinbase_tx = self
            .create_coinbase_for_template(height + 1, &transactions, &utxo_set)
            .await?;

        // Build transaction list (coinbase first)
        let mut all_transactions = vec![coinbase_tx];
        all_transactions.extend(transactions);

        use blvm_protocol::mining::{append_witness_commitment_from_nested, calculate_merkle_root};
        let mut coinbase = all_transactions[0].clone();
        let probe = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: all_transactions.clone().into_boxed_slice(),
        };
        let nested = self
            .build_witnesses_for_block(&probe, &utxo_set)
            .map_err(|e| anyhow::anyhow!("mempool witnesses unavailable: {e}"))?;
        append_witness_commitment_from_nested(&mut coinbase, &mut all_transactions, Some(&nested))
            .map_err(|e| anyhow::anyhow!("Failed to append witness commitment: {e}"))?;
        let merkle_root = calculate_merkle_root(&all_transactions)
            .map_err(|e| anyhow::anyhow!("Failed to calculate merkle root: {}", e))?;

        // Get current timestamp
        let timestamp = current_timestamp();

        // Build block template
        let template = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash,
                merkle_root,
                timestamp,
                bits,
                nonce: 0,
            },
            transactions: all_transactions.into_boxed_slice(),
        };

        debug!(
            "Generated block template: height={}, prev_hash={:?}, {} transactions, merkle_root={:?}",
            height + 1,
            prev_block_hash,
            template.transactions.len(),
            merkle_root
        );

        Ok(template)
    }

    /// Commons payouts when bound; otherwise the dummy single-output coinbase.
    async fn create_coinbase_for_template(
        &self,
        height: u64,
        selected_transactions: &[Transaction],
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Result<Transaction> {
        if let Some(caller) = self.commons_gbt.get() {
            match caller.fetch_commons_gbt_outputs().await {
                Ok(Some(outs)) if !outs.is_empty() => {
                    return self.create_commons_coinbase(
                        height,
                        selected_transactions,
                        utxo_set,
                        outs,
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(anyhow::anyhow!("{}", e.message));
                }
            }
        }
        self.create_coinbase_transaction(height, selected_transactions, utxo_set)
            .await
    }

    fn consensus_network(&self) -> blvm_protocol::types::Network {
        crate::storage::resolve_consensus_network(self.protocol.as_deref(), self.storage.as_deref())
    }

    fn create_commons_coinbase(
        &self,
        height: u64,
        selected_transactions: &[Transaction],
        utxo_set: &blvm_protocol::UtxoSet,
        outputs: Vec<(i64, Vec<u8>)>,
    ) -> Result<Transaction> {
        use blvm_protocol::ConsensusProof;
        use blvm_protocol::mining::{create_coinbase_with_outputs, fit_payouts_to_reward};

        let consensus = ConsensusProof::new();
        let subsidy = consensus.get_block_subsidy_for_network(height, self.consensus_network());
        let total_fees: i64 = selected_transactions
            .iter()
            .map(|tx| self.mempool.calculate_transaction_fee(tx, utxo_set) as i64)
            .sum();
        let fitted = fit_payouts_to_reward(&outputs, subsidy, total_fees)
            .map_err(|e| anyhow::anyhow!("commons payout fit: {e}"))?;
        create_coinbase_with_outputs(
            height,
            &blvm_protocol::bip_validation::encode_bip34_coinbase_script(height),
            &fitted,
        )
        .map_err(|e| anyhow::anyhow!("commons coinbase: {e}"))
    }

    /// Create coinbase transaction with subsidy + fees
    async fn create_coinbase_transaction(
        &self,
        height: u64,
        selected_transactions: &[Transaction],
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Result<Transaction> {
        use blvm_protocol::ConsensusProof;

        // 1. Get block subsidy from consensus layer
        let consensus = ConsensusProof::new();
        let subsidy =
            consensus.get_block_subsidy_for_network(height, self.consensus_network()) as u64;

        // 2. Calculate total fees from selected transactions
        let total_fees: u64 = selected_transactions
            .iter()
            .map(|tx| self.mempool.calculate_transaction_fee(tx, utxo_set))
            .sum();

        // 3. Coinbase value = subsidy + fees
        let coinbase_value = subsidy.checked_add(total_fees).ok_or_else(|| {
            anyhow::anyhow!(
                "Coinbase value overflow: subsidy {} + fees {}",
                subsidy,
                total_fees
            )
        })?;

        debug!(
            "Creating coinbase: height={}, subsidy={}, fees={}, total={}",
            height, subsidy, total_fees, coinbase_value
        );

        // 4. Create coinbase transaction (BIP34 height in scriptSig; BIP54: lock_time = height - 1, sequence != 0xffffffff)
        let lock_time = height.saturating_sub(1);
        let script_sig = blvm_protocol::bip_validation::encode_bip34_coinbase_script(height);
        Ok(Transaction {
            version: 1,
            inputs: vec![blvm_protocol::TransactionInput {
                prevout: blvm_protocol::OutPoint {
                    hash: [0u8; 32],
                    index: 0xffffffff,
                },
                script_sig,
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: crate::tx_outputs![blvm_protocol::TransactionOutput {
                value: coinbase_value as i64,
                script_pubkey: vec![
                    blvm_protocol::opcodes::OP_DUP,
                    blvm_protocol::opcodes::OP_HASH160,
                    blvm_protocol::opcodes::PUSH_20_BYTES,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    blvm_protocol::opcodes::OP_EQUALVERIFY,
                    blvm_protocol::opcodes::OP_CHECKSIG,
                ],
            }],
            lock_time,
        })
    }

    /// Submit mined block to the chain via [`SyncCoordinator::connect_mined_block`].
    async fn submit_block(&self, block: Block) -> Result<()> {
        debug!("Submitting mined block");

        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Cannot submit mined block: storage not configured"))?;
        let protocol = self.protocol.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Cannot submit mined block: protocol engine not configured")
        })?;

        let (_, tip_height) = storage
            .chain()
            .get_tip_hash_and_height()
            .map_err(|e| anyhow::anyhow!("Failed to get chain tip: {e}"))?;
        let connect_height = tip_height + 1;

        let mut utxo = storage
            .utxos()
            .get_all_utxos()
            .map_err(|e| anyhow::anyhow!("Failed to load UTXO set: {e}"))?;

        let witnesses = self.build_witnesses_for_block(&block, &utxo)?;

        let mut coord = crate::node::sync::SyncCoordinator::new();
        coord.set_mempool(Some(std::sync::Arc::clone(&self.mempool)));
        if let Some(ep) = &self.event_publisher {
            coord.set_event_publisher(Some(std::sync::Arc::clone(ep)));
        }
        let accepted = coord.connect_mined_block(
            storage.blocks().as_ref(),
            protocol.as_ref(),
            storage,
            &block,
            &witnesses,
            connect_height,
            &mut utxo,
        )?;

        if !accepted {
            anyhow::bail!("Mined block rejected at height {connect_height}");
        }
        self.mempool.remove_for_connected_block(&block.transactions);

        info!("Mined block connected at height {connect_height}");
        if let Some(ep) = &self.event_publisher {
            let block_hash = storage.blocks().get_block_hash(&block);
            ep.publish_new_block(&block, &block_hash, connect_height)
                .await;
            ep.publish_block_mined(&block_hash, connect_height, None)
                .await;
        }
        Ok(())
    }

    /// Build per-transaction witness stacks for block connect.
    ///
    /// The mined commitment is `sha256d(witness root || 32 zero bytes)`. The coinbase
    /// input must carry that reserved value; an empty stack is not those 32 bytes.
    fn build_witnesses_for_block(
        &self,
        block: &Block,
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Result<Vec<Vec<Witness>>> {
        use blvm_consensus::transaction::is_coinbase;
        use blvm_consensus::witness::{
            extract_witness_program, extract_witness_version, validate_witness_program_length,
        };
        use blvm_protocol::block::calculate_tx_id;

        block
            .transactions
            .iter()
            .map(|tx| {
                if is_coinbase(tx) {
                    return Ok(tx.inputs.iter().map(|_| vec![vec![0u8; 32]]).collect());
                }
                let txid = calculate_tx_id(tx);
                if let Some(wits) = self.mempool.get_transaction_witnesses(&txid) {
                    if wits.len() != tx.inputs.len() {
                        anyhow::bail!(
                            "witness count {} != input count {} for tx {}",
                            wits.len(),
                            tx.inputs.len(),
                            hex::encode(txid)
                        );
                    }
                    return Ok(wits);
                }

                let spends_witness_utxo = tx.inputs.iter().any(|input| {
                    utxo_set.get(&input.prevout).is_some_and(|utxo| {
                        let script = utxo.script_pubkey.as_ref().to_vec();
                        extract_witness_version(&script)
                            .and_then(|version| {
                                extract_witness_program(&script, version)
                                    .map(|program| (version, program))
                            })
                            .is_some_and(|(version, program)| {
                                validate_witness_program_length(&program, version)
                            })
                    })
                });
                if self.mempool.get_transaction(&txid).is_some() && spends_witness_utxo {
                    anyhow::bail!(
                        "missing mempool witnesses for witness spend tx {}",
                        hex::encode(txid)
                    );
                }

                Ok(tx.inputs.iter().map(|_| Witness::default()).collect())
            })
            .collect()
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
        self.mempool.size()
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

impl Default for MockMempoolProvider {
    fn default() -> Self {
        Self::new()
    }
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
        // Sort by fee rate descending.
        self.prioritized_transactions.sort_by(|a, b| b.1.cmp(&a.1));
    }

    /// Present for `get_transaction`, absent from the prioritized window.
    pub fn add_unranked(&mut self, tx: Transaction) {
        let hash = self.calculate_tx_hash(&tx);
        self.transactions.insert(hash, tx);
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
        let total_input_value = total_output_value + (tx.version * 1000); // Mock input value varies by version
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

    fn get_prioritized_transactions(
        &self,
        limit: usize,
        _utxo_set: &blvm_protocol::UtxoSet,
    ) -> Vec<Transaction> {
        // Mock implementation ignores UTXO set and uses pre-calculated priorities
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

    fn get_transaction_witnesses(&self, _hash: &[u8; 32]) -> Option<Vec<Witness>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::TransactionOutput;

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

        // UTXO set must contain inputs for fee calculation (create_test_transaction uses [0;32], index 0)
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let outpoint = blvm_protocol::OutPoint {
            hash: [0u8; 32],
            index: 0,
        };
        utxo_set.insert(
            outpoint,
            std::sync::Arc::new(blvm_protocol::UTXO {
                value: 100_000_000, // 1 BTC - enough for all test tx outputs
                script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );

        let selected = selector.select_transactions(&mempool, &utxo_set);
        assert!(!selected.is_empty());
        assert!(selected.len() <= 3);
    }

    #[test]
    fn below_floor_parent_outside_the_window_is_mined_with_its_child() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, Transaction, TransactionInput};

        let selector = TransactionSelector::new();
        let mut mempool = MockMempoolProvider::new();
        let parent_key = {
            let mut hash = [0u8; 32];
            hash[0] = 1;
            hash[1] = 1;
            hash[2] = 1;
            hash
        };
        let funding = OutPoint {
            hash: [7u8; 32],
            index: 0,
        };
        let parent = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 9_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        let child = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: parent_key,
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 100,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        mempool.add_unranked(parent.clone());
        mempool.add_transaction(child.clone());
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            funding,
            std::sync::Arc::new(blvm_protocol::UTXO {
                value: 9_001,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let selected = selector.select_transactions(&mempool, &utxo_set);
        assert!(
            selected
                .iter()
                .any(|tx| tx.version == parent.version
                    && tx.outputs[0].value == parent.outputs[0].value),
            "parent outside the window was left out"
        );
        assert!(selected.iter().any(|tx| tx.version == child.version));
    }

    #[test]
    fn test_transaction_selector_size_calculation() {
        let selector = TransactionSelector::new();
        let tx = create_test_transaction(1, 1000);

        let vsize = transaction_vsize(&tx, None);
        assert!(vsize > 0);

        let weight = bip141_weight(&tx, None);
        assert!(weight > 0);

        let mut utxo_set = blvm_protocol::UtxoSet::default();
        let outpoint = blvm_protocol::OutPoint {
            hash: [0u8; 32],
            index: 0,
        };
        utxo_set.insert(
            outpoint,
            std::sync::Arc::new(blvm_protocol::UTXO {
                value: 10000,
                script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let fee_rate = selector.calculate_fee_rate_with_utxo(
            &tx,
            &utxo_set,
            vsize,
            &MockMempoolProvider::new(),
        );
        assert!(fee_rate > 0);
    }

    #[test]
    fn template_fee_ignores_a_negative_output() {
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{OutPoint, Transaction, TransactionInput};

        let selector = TransactionSelector::new();
        let funding = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            funding,
            std::sync::Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let spend = |outputs: Vec<TransactionOutput>| Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: outputs.into(),
            lock_time: 0,
        };
        let negative = spend(vec![
            TransactionOutput {
                value: 1_000,
                script_pubkey: vec![OP_1],
            },
            TransactionOutput {
                value: -1,
                script_pubkey: vec![OP_1],
            },
        ]);
        let positive = spend(vec![TransactionOutput {
            value: 40_000,
            script_pubkey: vec![OP_1],
        }]);
        let mempool = MockMempoolProvider::new();
        assert_eq!(selector.transaction_fee(&negative, &utxo_set, &mempool), 0);
        assert_eq!(
            selector.transaction_fee(&positive, &utxo_set, &mempool),
            10_000
        );
    }

    #[test]
    fn selector_includes_a_smaller_tx_after_one_that_does_not_fit() {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let small = create_test_transaction(1, 1_000);
        let mut large = create_test_transaction(2, 1_000);
        large.outputs[0].script_pubkey = vec![OP_1; 800];

        let small_size = blvm_consensus::transaction::calculate_transaction_size(&small);
        let large_size = blvm_consensus::transaction::calculate_transaction_size(&large);
        assert!(large_size > small_size);

        let selector = TransactionSelector::with_params(small_size, 4_000_000, 1);
        let mut mempool = MockMempoolProvider::new();
        mempool.add_transaction(large.clone());
        mempool.add_transaction(small.clone());

        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            blvm_protocol::OutPoint {
                hash: [0u8; 32],
                index: 0,
            },
            std::sync::Arc::new(blvm_protocol::UTXO {
                value: 100_000_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );

        let selected = selector.select_transactions(&mempool, &utxo_set);
        assert_eq!(selected.len(), 1);
        assert_eq!(calculate_tx_id(&selected[0]), calculate_tx_id(&small));
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

        // Mining may succeed (if low difficulty) or fail (if high difficulty)
        // Both are valid outcomes for real PoW mining
        if let Ok(mined_block) = result {
            // Successfully mined - verify the block
            assert_eq!(mined_block.header.version, template.header.version);
            assert_ne!(mined_block.header.nonce, template.header.nonce); // Nonce should change

            // Verify proof of work
            use blvm_protocol::pow::check_proof_of_work;
            let pow_valid = check_proof_of_work(&mined_block.header).unwrap();
            assert!(pow_valid, "Mined block should have valid proof of work");

            // Check that template was stored
            assert!(engine.get_block_template().is_some());
            assert_eq!(engine.get_stats().blocks_mined, 1);
        } else {
            // Mining failed (high difficulty) - this is expected for mainnet difficulty
            // Just verify the template was stored
            assert!(engine.get_block_template().is_some());
        }
    }

    #[tokio::test]
    async fn test_mining_engine_mine_template_multithreaded() {
        let mut engine = MiningEngine::with_threads(4);
        let template = create_test_block();

        let result = engine.mine_template(template.clone()).await;

        // Mining may succeed (if low difficulty) or fail (if high difficulty)
        if let Ok(mined_block) = result {
            // Successfully mined - verify the block
            assert_eq!(mined_block.header.version, template.header.version);

            // Verify proof of work
            use blvm_protocol::pow::check_proof_of_work;
            let pow_valid = check_proof_of_work(&mined_block.header).unwrap();
            assert!(pow_valid, "Mined block should have valid proof of work");

            // Check that template was stored
            assert!(engine.get_block_template().is_some());
            assert_eq!(engine.get_stats().blocks_mined, 1);
        } else {
            // Mining failed (high difficulty) - this is expected
            assert!(engine.get_block_template().is_some());
        }
    }

    #[tokio::test]
    async fn test_mining_engine_mine_template_regtest_difficulty() {
        // Test with mainnet difficulty - mining may succeed or fail depending on luck
        // This tests that the mining infrastructure works correctly
        let mut engine = MiningEngine::new();
        let template = create_test_block();
        // template already has bits: 0x1d00ffff (mainnet difficulty)

        let result = engine.mine_template(template.clone()).await;

        // Mining may succeed (if we find a nonce) or fail (if we don't within max_attempts)
        // Both are valid outcomes for real PoW mining
        if let Ok(mined_block) = result {
            // Successfully mined - verify the block
            assert_eq!(mined_block.header.version, template.header.version);
            assert_ne!(mined_block.header.nonce, template.header.nonce);

            // Verify proof of work
            use blvm_protocol::pow::check_proof_of_work;
            let pow_valid = check_proof_of_work(&mined_block.header).unwrap();
            assert!(pow_valid, "Mined block should have valid proof of work");

            // Check statistics
            assert_eq!(engine.get_stats().blocks_mined, 1);
        } else {
            // Mining failed (didn't find nonce within max_attempts) - this is expected
            // The important thing is that the mining infrastructure worked correctly
            assert!(engine.get_block_template().is_some());
        }
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
        let empty_utxo_set = blvm_protocol::UtxoSet::default();
        assert!(
            mempool
                .get_prioritized_transactions(10, &empty_utxo_set)
                .is_empty()
        );
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

        let empty_utxo_set = blvm_protocol::UtxoSet::default();
        let prioritized = mempool.get_prioritized_transactions(10, &empty_utxo_set);
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

        let empty_utxo_set = blvm_protocol::UtxoSet::default();
        let prioritized = mempool.get_prioritized_transactions(10, &empty_utxo_set);
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
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let coordinator = MiningCoordinator::new(mempool, None);

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
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let coordinator = MiningCoordinator::with_params(mempool, None, 4, 2_000_000, 8_000_000, 5);

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
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let mut coordinator = MiningCoordinator::new(mempool, None);

        assert!(!coordinator.is_mining_enabled());
        coordinator.enable_mining();
        assert!(coordinator.is_mining_enabled());

        coordinator.disable_mining();
        assert!(!coordinator.is_mining_enabled());
    }

    #[test]
    fn test_mining_coordinator_info() {
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let coordinator = MiningCoordinator::new(mempool, None);

        let info = coordinator.get_mining_info();
        assert!(!info.enabled);
        assert_eq!(info.threads, 1);
        assert!(!info.has_template);
    }

    #[test]
    fn test_mining_coordinator_statistics() {
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let coordinator = MiningCoordinator::new(mempool, None);

        let stats = coordinator.get_mining_stats();
        assert_eq!(stats.blocks_mined, 0);
        assert_eq!(stats.total_hashrate, 0.0);
        assert_eq!(stats.average_block_time, 0.0);
        assert!(stats.last_block_time.is_none());
    }

    #[test]
    fn test_mining_coordinator_accessors() {
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let coordinator = MiningCoordinator::new(mempool, None);

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

    #[tokio::test]
    async fn test_mining_coordinator_mempool_operations() {
        use std::sync::Arc;
        // Create mempool and add transaction before wrapping in Arc
        let mut mempool_manager = crate::node::mempool::MempoolManager::new();
        let tx = create_test_transaction(1, 1000);
        let _ = mempool_manager.add_transaction(tx);
        let mempool = Arc::new(mempool_manager);
        let coordinator = MiningCoordinator::new(mempool, None);

        assert_eq!(coordinator.get_mempool_size(), 1);
    }

    #[tokio::test]
    async fn test_mining_coordinator_block_template_generation() {
        use std::sync::Arc;
        // Create mempool and add transaction before wrapping in Arc
        let mut mempool_manager = crate::node::mempool::MempoolManager::new();
        let tx = create_test_transaction(1, 1000);
        let _ = mempool_manager.add_transaction(tx);
        let mempool = Arc::new(mempool_manager);

        let mut coordinator = MiningCoordinator::new(mempool, None);

        let template = coordinator.generate_block_template().await;
        assert!(template.is_ok());

        let block = template.unwrap();
        assert_eq!(block.header.version, 1);
        assert!(!block.transactions.is_empty()); // Should have coinbase + mempool tx
        let cb = &block.transactions[0];
        assert!(cb.outputs.len() >= 2, "payout plus BIP141 commitment");
        let last = cb.outputs.last().unwrap();
        assert_eq!(last.value, 0);
        assert_eq!(last.script_pubkey[0], 0x6a);
    }

    #[tokio::test]
    async fn test_mining_coordinator_coinbase_creation() {
        use std::sync::Arc;
        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let coordinator = MiningCoordinator::new(mempool, None);

        // Test coinbase creation with no transactions (subsidy only)
        let empty_utxo_set = blvm_protocol::UtxoSet::default();
        let coinbase = coordinator
            .create_coinbase_transaction(0, &[], &empty_utxo_set)
            .await;
        assert!(coinbase.is_ok());

        let tx = coinbase.unwrap();
        assert_eq!(tx.version, 1);
        // Bitcoin coinbase: exactly one input, null prevout + 0xffffffff index (BIP30/BIP34).
        assert_eq!(tx.inputs.len(), 1);
        assert_eq!(tx.inputs[0].prevout.hash, [0u8; 32]);
        assert_eq!(tx.inputs[0].prevout.index, 0xffff_ffff);
        assert_eq!(tx.outputs.len(), 1);
        // Should be 50 BTC (subsidy) at height 0, with no fees
        assert_eq!(tx.outputs[0].value, 5000000000); // 50 BTC
        assert_eq!(tx.lock_time, 0);
    }

    // Helper functions for tests
    fn create_test_transaction(version: i32, output_value: u64) -> Transaction {
        use blvm_protocol::{OutPoint, TransactionInput};
        Transaction {
            version: version as u64,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0,
                },
                script_sig: vec![
                    blvm_protocol::opcodes::OP_DUP,
                    blvm_protocol::opcodes::OP_HASH160,
                    blvm_protocol::opcodes::PUSH_20_BYTES,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    blvm_protocol::opcodes::OP_EQUALVERIFY,
                    blvm_protocol::opcodes::OP_CHECKSIG,
                ],
                sequence: 0xffffffff,
            }],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: output_value as i64,
                script_pubkey: vec![
                    blvm_protocol::opcodes::OP_DUP,
                    blvm_protocol::opcodes::OP_HASH160,
                    blvm_protocol::opcodes::PUSH_20_BYTES,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    blvm_protocol::opcodes::OP_EQUALVERIFY,
                    blvm_protocol::opcodes::OP_CHECKSIG,
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
            transactions: vec![create_test_transaction(1, 1000)].into_boxed_slice(),
        }
    }

    #[tokio::test]
    async fn test_build_witnesses_uses_mempool_stored_witnesses() {
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
        use sha2::{Digest, Sha256};
        use std::sync::Arc;

        fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
            let hash = Sha256::digest(witness_script);
            let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
            spk.extend_from_slice(&hash);
            spk
        }

        let witness_script = vec![0x51]; // OP_1
        let funding_hash = [0xab; 32];
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            OutPoint {
                hash: funding_hash,
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 100_000,
                script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
                height: 0,
                is_coinbase: false,
            }),
        );

        let spend = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: funding_hash,
                    index: 0,
                },
                script_sig: vec![],
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 90_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };
        let txid = blvm_protocol::block::calculate_tx_id(&spend);
        let witness_stack: Witness = vec![witness_script.clone()];

        let mut mempool_manager = crate::node::mempool::MempoolManager::new();
        assert!(
            mempool_manager
                .add_transaction_with_witness(spend.clone(), Some(vec![witness_stack.clone()]))
                .unwrap(),
            "witness tx must enter mempool"
        );
        let mempool = Arc::new(mempool_manager);
        let coordinator = MiningCoordinator::new(mempool, None);

        let coinbase = coordinator
            .create_coinbase_transaction(1, &[spend.clone()], &utxo_set)
            .await
            .unwrap();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![coinbase, spend].into_boxed_slice(),
        };

        let witnesses = coordinator
            .build_witnesses_for_block(&block, &utxo_set)
            .expect("witness spend with stored mempool witnesses");
        assert_eq!(witnesses.len(), 2);
        assert_eq!(witnesses[0], vec![vec![vec![0u8; 32]]]);
        assert_eq!(witnesses[1], vec![witness_stack.clone()]);
        assert_eq!(
            coordinator.mempool.get_transaction_witnesses(&txid),
            Some(vec![witness_stack])
        );

        let cb = block.transactions[0].clone();
        let spend_tx = block.transactions[1].clone();
        let mut empty_cb = cb.clone();
        let mut empty_txs = vec![cb.clone(), spend_tx.clone()];
        blvm_protocol::mining::append_witness_commitment(&mut empty_cb, &mut empty_txs).unwrap();
        let mut real_cb = cb.clone();
        let mut real_txs = vec![cb, spend_tx];
        blvm_protocol::mining::append_witness_commitment_from_nested(
            &mut real_cb,
            &mut real_txs,
            Some(&witnesses),
        )
        .unwrap();
        assert_ne!(
            empty_cb.outputs.last().unwrap().script_pubkey,
            real_cb.outputs.last().unwrap().script_pubkey
        );
    }

    #[tokio::test]
    async fn test_build_witnesses_fails_when_mempool_missing_witness_spend() {
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
        use sha2::{Digest, Sha256};
        use std::sync::Arc;

        fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
            let hash = Sha256::digest(witness_script);
            let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
            spk.extend_from_slice(&hash);
            spk
        }

        let witness_script = vec![0x51];
        let funding_hash = [0xcd; 32];
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            OutPoint {
                hash: funding_hash,
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 100_000,
                script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
                height: 0,
                is_coinbase: false,
            }),
        );

        let spend = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: funding_hash,
                    index: 0,
                },
                script_sig: vec![],
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 90_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };

        let mut mempool_manager = crate::node::mempool::MempoolManager::new();
        assert!(
            mempool_manager.add_transaction(spend.clone()).unwrap(),
            "tx must enter mempool"
        );
        let mempool = Arc::new(mempool_manager);
        let coordinator = MiningCoordinator::new(mempool, None);

        let coinbase = coordinator
            .create_coinbase_transaction(1, &[spend.clone()], &utxo_set)
            .await
            .unwrap();
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![coinbase, spend].into_boxed_slice(),
        };

        let err = coordinator
            .build_witnesses_for_block(&block, &utxo_set)
            .unwrap_err();
        assert!(
            err.to_string().contains("missing mempool witnesses"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn generate_template_refuses_missing_mempool_witnesses() {
        use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput, UTXO};
        use sha2::{Digest, Sha256};
        use std::sync::Arc;
        use tempfile::TempDir;

        fn p2wsh_scriptpubkey(witness_script: &[u8]) -> Vec<u8> {
            let hash = Sha256::digest(witness_script);
            let mut spk = vec![blvm_protocol::opcodes::OP_0, 0x20];
            spk.extend_from_slice(&hash);
            spk
        }

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(crate::storage::Storage::new(temp_dir.path()).unwrap());
        let witness_script = vec![0x51];
        let funding_hash = [0xcd; 32];
        let outpoint = OutPoint {
            hash: funding_hash,
            index: 0,
        };
        let utxo = UTXO {
            value: 100_000,
            script_pubkey: p2wsh_scriptpubkey(&witness_script).into(),
            height: 0,
            is_coinbase: false,
        };
        storage.utxos().add_utxo(&outpoint, &utxo).unwrap();

        let spend = Transaction {
            version: 2,
            inputs: vec![TransactionInput {
                prevout: outpoint,
                script_sig: vec![],
                sequence: 0xfffffffe,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 90_000,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        };
        let mut mempool_manager = crate::node::mempool::MempoolManager::new();
        assert!(
            mempool_manager.add_transaction(spend).unwrap(),
            "tx must enter mempool"
        );
        let mut coordinator = MiningCoordinator::new(Arc::new(mempool_manager), Some(storage));

        let err = coordinator
            .generate_block_template()
            .await
            .expect_err("missing witnesses must refuse the template");
        assert!(
            err.to_string().contains("mempool witnesses unavailable"),
            "unexpected error: {err}"
        );
    }

    /// REV-TN-04: template → mine → `submit_block` → `connect_mined_block` on regtest storage.
    #[tokio::test]
    async fn test_submit_block_connects_regtest() {
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
        use std::sync::Arc;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(crate::storage::Storage::new(temp_dir.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let genesis = protocol.get_network_params().genesis_block.header.clone();
        storage.chain().initialize(&genesis).unwrap();
        // Required-work checks read the parent header from the blockstore, not chain_info.
        let genesis_hash = storage.chain().get_tip_hash().unwrap().unwrap();
        storage
            .blocks()
            .store_header(&genesis_hash, &genesis)
            .unwrap();
        storage.blocks().store_height(0, &genesis_hash).unwrap();

        let mempool = Arc::new(crate::node::mempool::MempoolManager::new());
        let mut coordinator = MiningCoordinator::new(mempool, Some(Arc::clone(&storage)));
        coordinator.set_protocol_engine(Arc::clone(&protocol));

        assert_eq!(
            storage.chain().get_height().unwrap().unwrap_or(0),
            0,
            "genesis only"
        );

        let mut template = coordinator
            .generate_block_template()
            .await
            .expect("block template");
        assert_eq!(template.transactions.len(), 1, "coinbase only");
        let cb = &template.transactions[0];
        assert!(
            cb.outputs
                .last()
                .is_some_and(|o| o.value == 0 && o.script_pubkey.first() == Some(&0x6a)),
            "mined template coinbase must carry BIP141 commitment"
        );
        // BIP90: post-genesis blocks need version ≥ 4 (same as `generatetoaddress` RPC path).
        template.header.version = 4;

        let mined = coordinator
            .mining_engine_mut()
            .mine_template(template)
            .await
            .expect("regtest PoW should succeed");

        coordinator
            .submit_block(mined)
            .await
            .expect("submit connects mined block");

        let height = storage
            .chain()
            .get_height()
            .unwrap()
            .expect("height after connect");
        assert_eq!(height, 1, "mined block extends chain from genesis");
    }

    struct HoldCommons;

    #[async_trait::async_trait]
    impl crate::rpc::mining::CommonsGbtCaller for HoldCommons {
        async fn fetch_commons_gbt_outputs(
            &self,
        ) -> crate::rpc::errors::RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
            Err(crate::rpc::errors::RpcError::internal_error(
                "commons pool holding; not issuing work",
            ))
        }
    }

    struct ScriptCommons;

    #[async_trait::async_trait]
    impl crate::rpc::mining::CommonsGbtCaller for ScriptCommons {
        async fn fetch_commons_gbt_outputs(
            &self,
        ) -> crate::rpc::errors::RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
            Ok(Some(vec![(0, vec![0x51])]))
        }
    }

    #[tokio::test]
    async fn commons_hold_refuses_miner_template() {
        let mempool = std::sync::Arc::new(crate::node::mempool::MempoolManager::new());
        let mut coordinator = MiningCoordinator::new(mempool, None);
        let slot = crate::rpc::mining::CommonsGbtSlot::default();
        slot.set(std::sync::Arc::new(HoldCommons));
        coordinator.set_commons_gbt_slot(slot);

        let err = coordinator
            .generate_block_template()
            .await
            .expect_err("hold must refuse a template");
        assert!(
            err.to_string().contains("holding"),
            "unexpected miner error: {err}"
        );
    }

    #[tokio::test]
    async fn commons_outputs_appear_in_miner_coinbase() {
        let mempool = std::sync::Arc::new(crate::node::mempool::MempoolManager::new());
        let mut coordinator = MiningCoordinator::new(mempool, None);
        let slot = crate::rpc::mining::CommonsGbtSlot::default();
        slot.set(std::sync::Arc::new(ScriptCommons));
        coordinator.set_commons_gbt_slot(slot);

        let template = coordinator
            .generate_block_template()
            .await
            .expect("template");
        let cb = &template.transactions[0];
        assert_eq!(cb.outputs[0].script_pubkey, vec![0x51]);
        assert!(
            cb.outputs
                .last()
                .is_some_and(|o| o.value == 0 && o.script_pubkey.first() == Some(&0x6a)),
            "BIP141 still appended after Commons payouts"
        );
    }
}

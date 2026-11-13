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
            min_fee_rate: 1,             // 1 satoshi per 1,000 virtual bytes
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
        let mut current_size = if self.max_block_size > TEMPLATE_RESERVE_BYTES {
            TEMPLATE_RESERVE_BYTES
        } else {
            0
        };
        let mut current_weight = TEMPLATE_RESERVE_WEIGHT.min(self.max_block_weight);
        let mut current_sigops = 0u64;
        let mut waiting: VecDeque<Transaction> = VecDeque::new();

        let transactions =
            mempool.get_prioritized_transactions(mempool.get_mempool_size(), utxo_set);

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
                &mut current_sigops,
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
                        &mut current_sigops,
                    );
                }
                BlockSelect::Skip => {
                    if let Some((parents, child)) =
                        self.paying_package(&tx, mempool, utxo_set, &waiting, &selected_ids)
                    {
                        let mut package: Vec<&Transaction> = parents.iter().collect();
                        package.push(&child);
                        if self.package_fits(
                            &package,
                            mempool,
                            utxo_set,
                            &selected,
                            current_size,
                            current_weight,
                            current_sigops,
                        ) && self.add_paying_package(
                            &parents,
                            &child,
                            mempool,
                            utxo_set,
                            &mut selected,
                            &mut selected_ids,
                            &mut current_size,
                            &mut current_weight,
                            &mut current_sigops,
                        ) {
                            self.take_ready_dependents(
                                mempool,
                                utxo_set,
                                &mut waiting,
                                &mut selected,
                                &mut selected_ids,
                                &mut current_size,
                                &mut current_weight,
                                &mut current_sigops,
                            );
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
            if !fee_meets_block_floor(package_fee, package_vsize as u64, self.min_fee_rate) {
                continue;
            }
            let _ = self.add_paying_package(
                &parents,
                &child,
                mempool,
                utxo_set,
                &mut selected,
                &mut selected_ids,
                &mut current_size,
                &mut current_weight,
                &mut current_sigops,
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
        current_sigops: &mut u64,
        enforce_fee: bool,
    ) -> BlockSelect {
        use blvm_protocol::block::calculate_tx_id;

        let txid = calculate_tx_id(tx);
        let input_witnesses = mempool.get_transaction_witnesses(&txid);
        let tx_stripped = blvm_consensus::transaction::calculate_transaction_size(tx);
        let tx_weight = bip141_weight(tx, input_witnesses.as_deref());
        let tx_vsize = transaction_vsize(tx, input_witnesses.as_deref());
        let tx_sigops = tx_sigop_cost(tx, utxo_set, selected, input_witnesses.as_deref());

        if *current_size + tx_stripped > self.max_block_size
            || *current_weight + tx_weight > self.max_block_weight
            || current_sigops.saturating_add(tx_sigops)
                > blvm_protocol::constants::MAX_BLOCK_SIGOPS_COST
        {
            return BlockSelect::Full;
        }

        let fee = self.transaction_fee(tx, utxo_set, mempool);
        if enforce_fee && !fee_meets_block_floor(fee, tx_vsize as u64, self.min_fee_rate) {
            return BlockSelect::Skip;
        }

        selected.push(tx.clone());
        selected_ids.insert(txid);
        *current_size += tx_stripped;
        *current_weight += tx_weight;
        *current_sigops = current_sigops.saturating_add(tx_sigops);
        BlockSelect::Added
    }

    /// Add `parents` and `child` as one package.
    ///
    /// Parents may sit below the block fee floor because `child` pays for them.
    /// If `child` does not fit, those parents are taken back out.
    fn add_paying_package(
        &self,
        parents: &[Transaction],
        child: &Transaction,
        mempool: &dyn MempoolProvider,
        utxo_set: &blvm_protocol::UtxoSet,
        selected: &mut Vec<Transaction>,
        selected_ids: &mut std::collections::HashSet<[u8; 32]>,
        current_size: &mut usize,
        current_weight: &mut u64,
        current_sigops: &mut u64,
    ) -> bool {
        use blvm_protocol::block::calculate_tx_id;

        let mark = selected.len();
        let saved_size = *current_size;
        let saved_weight = *current_weight;
        let saved_sigops = *current_sigops;
        let mut added_ids = Vec::new();
        let mut parents_ok = true;
        for parent_tx in parents {
            let id = calculate_tx_id(parent_tx);
            if selected_ids.contains(&id) {
                continue;
            }
            if self.try_add_to_block(
                parent_tx,
                mempool,
                utxo_set,
                selected,
                selected_ids,
                current_size,
                current_weight,
                current_sigops,
                false,
            ) != BlockSelect::Added
            {
                parents_ok = false;
                break;
            }
            added_ids.push(id);
        }
        let child_id = calculate_tx_id(child);
        let child_added = parents_ok
            && (selected_ids.contains(&child_id)
                || self.try_add_to_block(
                    child,
                    mempool,
                    utxo_set,
                    selected,
                    selected_ids,
                    current_size,
                    current_weight,
                    current_sigops,
                    false,
                ) == BlockSelect::Added);
        if child_added {
            return true;
        }
        selected.truncate(mark);
        for id in added_ids {
            selected_ids.remove(&id);
        }
        *current_size = saved_size;
        *current_weight = saved_weight;
        *current_sigops = saved_sigops;
        false
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
        current_sigops: &mut u64,
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
                    current_sigops,
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
            if !fee_meets_block_floor(child_fee, child_vsize as u64, self.min_fee_rate) {
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
            if fee_meets_block_floor(package_fee, package_vsize as u64, self.min_fee_rate) {
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
        utxo_set: &blvm_protocol::UtxoSet,
        selected: &[Transaction],
        current_size: usize,
        current_weight: u64,
        current_sigops: u64,
    ) -> bool {
        use blvm_protocol::block::calculate_tx_id;

        let mut size = current_size;
        let mut weight = current_weight;
        let mut sigops = current_sigops;
        let mut in_block = selected.to_vec();
        for tx in txs {
            let witnesses = mempool.get_transaction_witnesses(&calculate_tx_id(tx));
            let stripped = blvm_consensus::transaction::calculate_transaction_size(tx);
            let tx_weight = bip141_weight(tx, witnesses.as_deref());
            let tx_sigops = tx_sigop_cost(tx, utxo_set, &in_block, witnesses.as_deref());
            size = size.saturating_add(stripped);
            weight = weight.saturating_add(tx_weight);
            sigops = sigops.saturating_add(tx_sigops);
            if size > self.max_block_size
                || weight > self.max_block_weight
                || sigops > blvm_protocol::constants::MAX_BLOCK_SIGOPS_COST
            {
                return false;
            }
            in_block.push((*tx).clone());
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

/// Header, a 3-byte transaction count, and a coinbase that will gain a witness commitment.
pub(crate) const TEMPLATE_RESERVE_WEIGHT: u64 = 4_000;
const TEMPLATE_RESERVE_BYTES: usize = (TEMPLATE_RESERVE_WEIGHT / 4) as usize;

/// BIP141 weight: `3 × stripped size + total size`.
/// Block creation floor. `rate` is satoshis per 1,000 virtual bytes, rounded up.
fn fee_meets_block_floor(fee: u64, vsize: u64, rate: u64) -> bool {
    vsize > 0 && (fee as u128) >= (rate as u128).saturating_mul(vsize as u128).div_ceil(1000)
}

fn bip141_weight(tx: &Transaction, input_witnesses: Option<&[Witness]>) -> u64 {
    blvm_protocol::segwit::transaction_weight_from_stacks(tx, input_witnesses).unwrap_or(u64::MAX)
}

fn tx_sigop_cost(
    tx: &Transaction,
    utxo_set: &blvm_protocol::UtxoSet,
    selected: &[Transaction],
    witnesses: Option<&[Witness]>,
) -> u64 {
    use blvm_consensus::utxo_overlay::UtxoOverlay;
    use blvm_protocol::block::calculate_tx_id;
    use blvm_protocol::{OutPoint, UTXO};

    // Outputs already chosen for this block are visible, as they are at connect.
    // Block sigop cost counts witness programs. P2SH is 0x01. Witness is 0x800.
    let mut overlay = UtxoOverlay::with_capacity(utxo_set, selected.len().saturating_mul(2), 0);
    for parent in selected {
        let id = calculate_tx_id(parent);
        for (index, output) in parent.outputs.iter().enumerate() {
            overlay.insert(
                OutPoint {
                    hash: id,
                    index: index as u32,
                },
                UTXO {
                    value: output.value,
                    script_pubkey: output.script_pubkey.clone().into(),
                    height: 0,
                    is_coinbase: false,
                },
            );
        }
    }
    blvm_protocol::sigop::get_transaction_sigop_cost_with_witness_slices(
        tx,
        &overlay,
        witnesses,
        0x01 | 0x800,
    )
    .unwrap_or(u64::MAX)
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
            .map_err(|e| anyhow::anyhow!("Mining task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("Mining failed: {e}"))?;

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
                    if let Ok(valid) = check_proof_of_work(&block.header)
                        && valid
                    {
                        let _ = tx.send(Ok((block, MiningResult::Success)));
                        return;
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
                .map_err(|e| anyhow::anyhow!("Mining task panicked: {e}"))?;

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
        let (prev_block_hash, parent_bits, parent_time, height) =
            if let Some(ref storage) = self.storage {
                if let Some(tip_header) = storage
                    .chain()
                    .get_tip_header()
                    .map_err(|e| anyhow::anyhow!("Failed to get tip header: {e}"))?
                {
                    let tip_hash = storage
                        .chain()
                        .get_tip_hash()
                        .map_err(|e| anyhow::anyhow!("Failed to get tip hash: {e}"))?
                        .unwrap_or([0u8; 32]);
                    let chain_height = storage
                        .chain()
                        .get_height()
                        .map_err(|e| anyhow::anyhow!("Failed to get chain height: {e}"))?
                        .unwrap_or(0);
                    (
                        tip_hash,
                        tip_header.bits,
                        tip_header.timestamp,
                        chain_height,
                    )
                } else {
                    // No chain tip - use genesis defaults
                    ([0u8; 32], 0x1d00ffff, 0, 0)
                }
            } else {
                // No storage - use defaults
                ([0u8; 32], 0x1d00ffff, 0, 0)
            };

        // Price the pool from the chain. A checkpoint is read by prevout.
        // The table is loaded whole only when it is the chain.
        let utxo_set = if let Some(ref storage) = self.storage {
            let pooled = self.mempool.get_transactions();
            match storage.open_engine_resume_snapshot()? {
                Some(_) => {
                    let mut set = blvm_protocol::UtxoSet::default();
                    for tx in &pooled {
                        for input in &tx.inputs {
                            if set.contains_key(&input.prevout) {
                                continue;
                            }
                            if let Some(utxo) = storage.read_chain_prevout(&input.prevout)? {
                                set.insert(input.prevout, std::sync::Arc::new(utxo));
                            }
                        }
                    }
                    set
                }
                None => storage
                    .utxos()
                    .get_all_utxos()
                    .map_err(|e| anyhow::anyhow!("Failed to get UTXO set: {e}"))?,
            }
        } else {
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
            .map_err(|e| anyhow::anyhow!("Failed to calculate merkle root: {e}"))?;

        let clock = current_timestamp();
        let mintime = self.template_min_time(height, clock);
        let timestamp = clock.max(mintime);
        let template_height = height.saturating_add(1);
        let network = self.consensus_network()?;
        let bits =
            self.template_work_bits(template_height, timestamp, height, parent_bits, parent_time)?;
        let version = blvm_protocol::mining::template_block_version(network, template_height);

        // Build block template
        let template = Block {
            header: BlockHeader {
                version,
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

    fn consensus_network(&self) -> anyhow::Result<blvm_protocol::types::Network> {
        crate::storage::resolve_consensus_network(self.protocol.as_deref(), self.storage.as_deref())
    }

    /// Median of the 11 headers at the chain tip, plus one. `height` is the tip height.
    fn template_min_time(&self, height: u64, clock: u64) -> u64 {
        let Some(storage) = self.storage.as_ref() else {
            return clock;
        };
        if height == 0 {
            return clock;
        }
        let Ok(headers) = storage.blocks().headers_back_from(height, 11) else {
            return clock;
        };
        if headers.is_empty() {
            return clock;
        }
        blvm_protocol::bip113::get_median_time_past(&headers).saturating_add(1)
    }

    /// Compact bits for `template_height`. Parent bits are used only when retarget returns none.
    fn template_work_bits(
        &self,
        template_height: u64,
        block_time: u64,
        parent_height: u64,
        parent_bits: u64,
        parent_time: u64,
    ) -> Result<u64> {
        if template_height == 0 {
            return Ok(parent_bits);
        }
        let headers = if let Some(storage) = self.storage.as_ref() {
            if parent_height > 0 {
                storage
                    .blocks()
                    .headers_back_from(parent_height, 2016)
                    .map_err(|e| anyhow::anyhow!("difficulty headers: {e}"))?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let window_end = parent_height;
        let window_start = if headers.is_empty() {
            window_end
        } else {
            window_end.saturating_sub(headers.len() as u64 - 1)
        };
        let network = self.consensus_network()?;
        let bits =
            blvm_protocol::pow::next_required_bits(network, template_height, block_time, &|h| {
                if h == parent_height {
                    return Some((parent_bits, parent_time));
                }
                if headers.is_empty() || h < window_start || h > window_end {
                    return None;
                }
                headers
                    .get((h - window_start) as usize)
                    .map(|hdr| (hdr.bits, hdr.timestamp))
            })
            .map_err(|e| anyhow::anyhow!("next required bits: {e}"))?;
        Ok(bits.unwrap_or(parent_bits))
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
        let subsidy = consensus.get_block_subsidy_for_network(height, self.consensus_network()?);
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
            consensus.get_block_subsidy_for_network(height, self.consensus_network()?) as u64;

        // 2. Calculate total fees from selected transactions
        let total_fees: u64 = selected_transactions
            .iter()
            .map(|tx| self.mempool.calculate_transaction_fee(tx, utxo_set))
            .sum();

        // 3. Coinbase value = subsidy + fees
        let coinbase_value = subsidy.checked_add(total_fees).ok_or_else(|| {
            anyhow::anyhow!("Coinbase value overflow: subsidy {subsidy} + fees {total_fees}")
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
        let old_bits = storage
            .chain()
            .get_tip_header()
            .ok()
            .flatten()
            .map(|header| header.bits);
        let connect_height = tip_height + 1;

        let mut utxo = match storage.open_engine_resume_snapshot()? {
            Some(_) => blvm_protocol::UtxoSet::default(),
            None => storage
                .utxos()
                .get_all_utxos()
                .map_err(|e| anyhow::anyhow!("Failed to load UTXO set: {e}"))?,
        };

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
        let block_hash = storage.blocks().get_block_hash(&block);
        let obs = crate::node::fee_estimator::block_observation(
            &block,
            &witnesses,
            connect_height,
            block_hash,
        );
        self.mempool
            .remove_for_connected_block_at(obs, &block.transactions);

        info!("Mined block connected at height {connect_height}");
        if let Some(ep) = &self.event_publisher {
            let mtp = storage
                .blocks()
                .headers_before_height_for_mtp(connect_height)
                .ok()
                .filter(|headers| !headers.is_empty())
                .map(|headers| blvm_protocol::bip113::get_median_time_past(&headers))
                .unwrap_or(block.header.timestamp);
            ep.publish_new_block_at(&block, &block_hash, connect_height, mtp)
                .await;
            ep.publish_block_mined(&block_hash, connect_height, None)
                .await;
            if let Some(old_bits) = old_bits.filter(|bits| *bits != block.header.bits) {
                ep.publish_difficulty_adjusted(
                    old_bits as u32,
                    block.header.bits as u32,
                    connect_height,
                )
                .await;
            }
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
        use blvm_consensus::witness::is_standard_witness_program;
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
                        is_standard_witness_program(utxo.script_pubkey.as_ref())
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
        let lock_time = tx.lock_time as u32;
        hash[4..8].copy_from_slice(&lock_time.to_le_bytes());
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
#[path = "miner_tests.rs"]
mod tests;

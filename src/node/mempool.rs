//! Mempool manager
//!
//! Handles transaction mempool management, validation, and relay.

use crate::config::{MempoolPolicyConfig, RbfConfig};
use crate::node::event_publisher::EventPublisher;
use anyhow::Result;
use blvm_protocol::mempool::{
    Mempool, has_conflict_with_tx, replacement_checks_with_witness, signals_rbf,
};
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, Hash, OutPoint, Transaction, TransactionOutput, UtxoSet};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{debug, info, warn};

/// RBF tracking information for a transaction
#[derive(Debug, Clone)]
struct RbfTracking {
    /// Number of times this transaction has been replaced
    replacement_count: u32,
    /// Timestamp of last replacement (Unix timestamp)
    last_replacement_time: u64,
    /// Original transaction hash (before any replacements)
    original_tx_hash: Hash,
}

/// Core mempool state (transactions + spent outputs)
/// Wrapped in Mutex for add_transaction from Arc context (re-broadcast, sendrawtransaction)
struct MempoolPool {
    transactions: HashMap<Hash, Transaction>,
    /// SegWit witness stacks per txid (one stack per input), when known at accept time.
    tx_witnesses: HashMap<Hash, Vec<Witness>>,
    /// Sigop-adjusted virtual size recorded at accept time.
    adjusted_vsize: HashMap<Hash, u64>,
    spent_outputs: HashSet<OutPoint>,
}

/// Mempool manager
pub struct MempoolManager {
    /// Transaction mempool + spent outputs (interior mutability for Arc<MempoolManager>)
    pool: Mutex<MempoolPool>,
    /// Legacy mempool (HashSet of hashes) for compatibility
    #[allow(dead_code)]
    mempool: RwLock<Mempool>,
    /// Shared UTXO set for fee calculation (set via set_utxo_set_arc after construction).
    /// When None, RBF fee checks fall back to zero-fee comparisons and replacement is
    /// rejected. Callers should wire this to the node's live UTXO set.
    utxo_set_arc: RwLock<Option<Arc<tokio::sync::Mutex<UtxoSet>>>>,
    /// Event callback for mempool events (optional)
    /// Called when transactions are added/removed from mempool
    #[allow(dead_code)]
    event_callback: Option<Box<dyn Fn(Hash, String, usize) + Send + Sync>>,
    /// Sorted index by fee rate (descending) - Reverse<u64> for descending order
    /// Maps fee_rate -> Vec<Hash> (multiple transactions can have same fee rate)
    /// Uses RwLock for interior mutability to allow &self methods
    fee_index: RwLock<BTreeMap<Reverse<u64>, Vec<Hash>>>,
    /// Cache fee rates per transaction hash
    /// Uses RwLock for interior mutability to allow &self methods
    fee_cache: RwLock<HashMap<Hash, u64>>,
    /// Mining priority fee deltas (satoshis), cumulative per BIP prioritisetransaction semantics.
    fee_deltas: RwLock<HashMap<Hash, i64>>,
    /// RBF configuration (optional)
    /// Uses RwLock for interior mutability to allow setting config after Arc sharing
    rbf_config: RwLock<Option<RbfConfig>>,
    /// Mempool policy configuration (optional)
    /// Uses RwLock for interior mutability to allow setting config after Arc sharing
    policy_config: RwLock<Option<MempoolPolicyConfig>>,
    /// RBF tracking: transaction hash -> RBF tracking info
    /// Uses RwLock for interior mutability
    rbf_tracking: RwLock<HashMap<Hash, RbfTracking>>,
    /// Transaction timestamps: when each transaction was added
    /// Uses RwLock for interior mutability
    tx_timestamps: RwLock<HashMap<Hash, u64>>,
    /// Transaction dependency graph: child -> parent relationships
    /// Maps transaction hash to set of parent transaction hashes (transactions it depends on)
    /// Uses RwLock for interior mutability
    tx_dependencies: RwLock<HashMap<Hash, HashSet<Hash>>>,
    /// Reverse dependency graph: parent -> children relationships
    /// Maps transaction hash to set of child transaction hashes (transactions that depend on it)
    /// Uses RwLock for interior mutability
    tx_descendants: RwLock<HashMap<Hash, HashSet<Hash>>>,
    /// UTXO set hash for change detection (optimization: only recalculate when UTXO set changes)
    /// Uses RwLock for interior mutability
    utxo_set_hash: RwLock<Option<u64>>,
    /// Event publisher for mempool events (optional)
    /// Uses Arc for shared ownership and interior mutability
    event_publisher: RwLock<Option<Arc<EventPublisher>>>,
    /// Live chain, so admission can read the next block's height and median time.
    storage: RwLock<Option<Arc<crate::storage::Storage>>>,
    /// Test tip: next-block height and median time-past. Ignored when `storage` is set.
    chain_tip: RwLock<Option<(u64, u64)>>,
}

/// Next block height and the median time of the headers before that block.
pub(crate) fn next_block_finality(storage: &crate::storage::Storage) -> Option<(u64, u64)> {
    let tip = storage.chain().get_height().ok().flatten()?;
    let next_height = tip.saturating_add(1);
    let median_time_past = storage
        .blocks()
        .headers_before_height_for_mtp(next_height)
        .ok()
        .filter(|headers| !headers.is_empty())
        .map(|headers| blvm_protocol::bip113::get_median_time_past(&headers))
        .unwrap_or(0);
    Some((next_height, median_time_past))
}

fn is_p2sh_script(script: &[u8]) -> bool {
    use blvm_protocol::opcodes::{OP_EQUAL, OP_HASH160, PUSH_20_BYTES};
    script.len() == 23
        && script[0] == OP_HASH160
        && script[1] == PUSH_20_BYTES
        && script[22] == OP_EQUAL
}

/// Last push in a push-only script, which is the redeem script of a P2SH spend.
fn last_script_push(script: &[u8]) -> Option<Vec<u8>> {
    use blvm_protocol::opcodes::{
        OP_0, OP_1, OP_1NEGATE, OP_16, OP_PUSHDATA1, OP_PUSHDATA2, OP_PUSHDATA4,
    };
    let mut i = 0;
    let mut last = None;
    while i < script.len() {
        let opcode = script[i];
        let (advance, data) = if opcode == OP_0 {
            (1, Vec::new())
        } else if opcode > OP_0 && opcode < OP_PUSHDATA1 {
            let len = opcode as usize;
            if i + 1 + len > script.len() {
                return None;
            }
            (1 + len, script[i + 1..i + 1 + len].to_vec())
        } else if opcode == OP_PUSHDATA1 {
            if i + 1 >= script.len() {
                return None;
            }
            let len = script[i + 1] as usize;
            if i + 2 + len > script.len() {
                return None;
            }
            (2 + len, script[i + 2..i + 2 + len].to_vec())
        } else if opcode == OP_PUSHDATA2 {
            if i + 2 >= script.len() {
                return None;
            }
            let len = u16::from_le_bytes([script[i + 1], script[i + 2]]) as usize;
            if i + 3 + len > script.len() {
                return None;
            }
            (3 + len, script[i + 3..i + 3 + len].to_vec())
        } else if opcode == OP_PUSHDATA4 {
            if i + 4 >= script.len() {
                return None;
            }
            let len =
                u32::from_le_bytes([script[i + 1], script[i + 2], script[i + 3], script[i + 4]])
                    as usize;
            if i + 5 + len > script.len() {
                return None;
            }
            (5 + len, script[i + 5..i + 5 + len].to_vec())
        } else if opcode == OP_1NEGATE || (OP_1..=OP_16).contains(&opcode) {
            (1, Vec::new())
        } else {
            return None;
        };
        last = Some(data);
        i += advance;
    }
    last
}

fn p2sh_redeem_is_witness_program(script_pubkey: &[u8], script_sig: &[u8]) -> bool {
    if !is_p2sh_script(script_pubkey) {
        return false;
    }
    let Some(redeem) = last_script_push(script_sig) else {
        return false;
    };
    blvm_protocol::witness::extract_witness_version(&redeem).is_some()
}

/// Structure, coinbase, witness-count, and script checks.
///
/// `prevouts` is one entry per input. A missing entry was not in the snapshot and
/// was not created by a transaction already in the pool.
fn consensus_checks_reject(
    tx: &Transaction,
    witnesses: Option<&[Witness]>,
    prevouts: &[Option<TransactionOutput>],
) -> bool {
    use blvm_protocol::ValidationResult;
    use blvm_protocol::block::calculate_script_flags_for_block_network;
    use blvm_protocol::script::verify_script_with_context;
    use blvm_protocol::transaction::{check_transaction, is_coinbase};
    use blvm_protocol::types::Network;

    const MEMPOOL_POLICY_HEIGHT: u64 = 1_000_000;

    if is_coinbase(tx) {
        return true;
    }
    if !matches!(check_transaction(tx), Ok(ValidationResult::Valid)) {
        return true;
    }
    if let Some(wits) = witnesses {
        if wits.len() != tx.inputs.len() {
            return true;
        }
    }

    // A witness program, native or inside a P2SH redeem script, is still a
    // witness spend when no stack was supplied. Flag selection otherwise
    // looks only at supplied stacks and at the tx outputs.
    let needs_witness_flags = tx.inputs.iter().enumerate().any(|(i, input)| {
        prevouts
            .get(i)
            .and_then(|output| output.as_ref())
            .is_some_and(|output| {
                let script = output.script_pubkey.as_slice();
                blvm_protocol::witness::extract_witness_version(&script.to_vec()).is_some()
                    || p2sh_redeem_is_witness_program(script, &input.script_sig)
            })
    });
    let has_witness = needs_witness_flags || witnesses.is_some_and(|stacks| !stacks.is_empty());
    let flags = calculate_script_flags_for_block_network(
        tx,
        has_witness,
        MEMPOOL_POLICY_HEIGHT,
        Network::Mainnet,
    );
    let sighash_prevouts: Vec<TransactionOutput> = prevouts
        .iter()
        .map(|output| {
            output.clone().unwrap_or(TransactionOutput {
                value: 0,
                script_pubkey: Vec::new(),
            })
        })
        .collect();

    for (i, input) in tx.inputs.iter().enumerate() {
        let Some(output) = prevouts.get(i).and_then(|output| output.as_ref()) else {
            continue;
        };
        let witness = witnesses.and_then(|stacks| stacks.get(i));
        let Ok(true) = verify_script_with_context(
            &input.script_sig,
            output.script_pubkey.as_ref(),
            witness,
            flags,
            tx,
            i,
            &sighash_prevouts,
            Some(MEMPOOL_POLICY_HEIGHT),
            Network::Mainnet,
        ) else {
            return true;
        };
    }
    false
}

pub(crate) fn money_sats(value: i64) -> Option<u64> {
    if (0..=blvm_protocol::constants::MAX_MONEY).contains(&value) {
        Some(value as u64)
    } else {
        None
    }
}

/// Running sum of money. `None` once the total passes `MAX_MONEY`.
pub(crate) fn add_money(total: u64, sats: u64) -> Option<u64> {
    let max = blvm_protocol::constants::MAX_MONEY as u64;
    total.checked_add(sats).filter(|sum| *sum <= max)
}

/// Sum of output values in range. `None` when any output is outside the money range.
pub(crate) fn output_sum_sats(tx: &Transaction) -> Option<u64> {
    let mut total = 0u64;
    for output in &tx.outputs {
        let sats = money_sats(output.value)?;
        total = add_money(total, sats)?;
    }
    Some(total)
}

impl MempoolManager {
    /// Create a new mempool manager
    pub fn new() -> Self {
        Self {
            pool: Mutex::new(MempoolPool {
                transactions: HashMap::new(),
                tx_witnesses: HashMap::new(),
                adjusted_vsize: HashMap::new(),
                spent_outputs: HashSet::new(),
            }),
            mempool: RwLock::new(Mempool::new()),
            utxo_set_arc: RwLock::new(None),
            event_callback: None,
            fee_index: RwLock::new(BTreeMap::new()),
            fee_cache: RwLock::new(HashMap::new()),
            fee_deltas: RwLock::new(HashMap::new()),
            rbf_config: RwLock::new(None),
            policy_config: RwLock::new(None),
            rbf_tracking: RwLock::new(HashMap::new()),
            tx_timestamps: RwLock::new(HashMap::new()),
            tx_dependencies: RwLock::new(HashMap::new()),
            tx_descendants: RwLock::new(HashMap::new()),
            utxo_set_hash: RwLock::new(None),
            event_publisher: RwLock::new(None),
            storage: RwLock::new(None),
            chain_tip: RwLock::new(None),
        }
    }

    /// Create a new mempool manager with RBF configuration
    pub fn with_rbf_config(rbf_config: Option<RbfConfig>) -> Self {
        Self {
            pool: Mutex::new(MempoolPool {
                transactions: HashMap::new(),
                tx_witnesses: HashMap::new(),
                adjusted_vsize: HashMap::new(),
                spent_outputs: HashSet::new(),
            }),
            mempool: RwLock::new(Mempool::new()),
            utxo_set_arc: RwLock::new(None),
            event_callback: None,
            fee_index: RwLock::new(BTreeMap::new()),
            fee_cache: RwLock::new(HashMap::new()),
            fee_deltas: RwLock::new(HashMap::new()),
            rbf_config: RwLock::new(rbf_config),
            policy_config: RwLock::new(None),
            rbf_tracking: RwLock::new(HashMap::new()),
            tx_timestamps: RwLock::new(HashMap::new()),
            tx_dependencies: RwLock::new(HashMap::new()),
            tx_descendants: RwLock::new(HashMap::new()),
            utxo_set_hash: RwLock::new(None),
            event_publisher: RwLock::new(None),
            storage: RwLock::new(None),
            chain_tip: RwLock::new(None),
        }
    }

    /// Wire the live UTXO set into the mempool so RBF fee checks use real values.
    /// Call this once after constructing MempoolManager and before the first transaction.
    pub fn set_utxo_set_arc(&self, utxo_set: Arc<tokio::sync::Mutex<UtxoSet>>) {
        *self.utxo_set_arc.write().unwrap_or_else(|e| e.into_inner()) = Some(utxo_set);
    }

    /// Wire chain storage so locktime checks use the current tip.
    pub fn set_storage(&self, storage: Arc<crate::storage::Storage>) {
        *self.storage.write().unwrap_or_else(|e| e.into_inner()) = Some(storage);
    }

    /// Next-block height and median time-past, for tests that have no chain storage.
    pub fn set_chain_tip(&self, next_height: u64, median_time_past: u64) {
        *self.chain_tip.write().unwrap_or_else(|e| e.into_inner()) =
            Some((next_height, median_time_past));
    }

    fn finality_context(&self) -> Option<(u64, u64)> {
        if let Some(storage) = self
            .storage
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Some(next_block_finality(storage).unwrap_or((0, 0)));
        }
        *self.chain_tip.read().unwrap_or_else(|e| e.into_inner())
    }

    fn chain_network(&self) -> blvm_protocol::types::Network {
        self.storage
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|storage| storage.chain().consensus_network())
            .unwrap_or(blvm_protocol::types::Network::Mainnet)
    }

    /// Median time of the block before `coin_height`.
    fn prior_median_time(&self, coin_height: u64) -> Option<i64> {
        let storage = self
            .storage
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        let headers = storage
            .blocks()
            .headers_before_height_for_mtp(coin_height)
            .ok()?;
        if headers.is_empty() {
            return None;
        }
        i64::try_from(blvm_protocol::bip113::get_median_time_past(&headers)).ok()
    }

    /// Script and value for each input. The snapshot wins. Otherwise use the
    /// output a transaction already in the pool created.
    fn script_prevouts(
        &self,
        tx: &Transaction,
        utxo_set: &UtxoSet,
    ) -> Vec<Option<TransactionOutput>> {
        let pool = self.pool_lock();
        tx.inputs
            .iter()
            .map(|input| {
                if let Some(utxo) = utxo_set.get(&input.prevout) {
                    Some(TransactionOutput {
                        value: utxo.value,
                        script_pubkey: utxo.script_pubkey.as_ref().to_vec(),
                    })
                } else {
                    pool.transactions
                        .get(&input.prevout.hash)
                        .and_then(|parent| parent.outputs.get(input.prevout.index as usize))
                        .cloned()
                }
            })
            .collect()
    }

    fn mempool_creates(&self, prevout: &OutPoint) -> bool {
        let pool = self.pool_lock();
        pool.transactions
            .get(&prevout.hash)
            .and_then(|parent| parent.outputs.get(prevout.index as usize))
            .is_some()
    }

    fn relative_lock_rejects(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        use blvm_protocol::locktime::{extract_sequence_type_flag, is_sequence_disabled};

        let Some((block_height, block_mtp)) = self.finality_context() else {
            return tx.version >= 2
                && tx.inputs.iter().any(|input| {
                    let seq = input.sequence as u32;
                    !is_sequence_disabled(seq)
                });
        };
        let block_mtp_i = i64::try_from(block_mtp).unwrap_or(-1);
        let mut prev_mtps = Vec::with_capacity(tx.inputs.len());
        for input in &tx.inputs {
            let seq = input.sequence as u32;
            if is_sequence_disabled(seq) || !extract_sequence_type_flag(seq) {
                prev_mtps.push(-1);
                continue;
            }
            if let Some(utxo) = utxo_set.get(&input.prevout) {
                let Some(mtp) = self.prior_median_time(utxo.height) else {
                    return true;
                };
                prev_mtps.push(mtp);
            } else if self.mempool_creates(&input.prevout) && block_mtp_i >= 0 {
                prev_mtps.push(block_mtp_i);
            } else {
                return true;
            }
        }
        match blvm_protocol::mempool::relative_lock_unsatisfied(
            tx,
            utxo_set,
            block_height,
            block_mtp,
            Some(&prev_mtps),
            self.chain_network(),
        ) {
            Ok(unsatisfied) => unsatisfied,
            Err(_) => true,
        }
    }

    fn prevout_value_out_of_range(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        tx.inputs.iter().any(|input| {
            utxo_set
                .get(&input.prevout)
                .is_some_and(|utxo| money_sats(utxo.value).is_none())
        })
    }

    /// Each prevout may sit on `MAX_MONEY`. The running sum may not.
    fn prevout_sum_exceeds_money(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        let mut total = 0u64;
        for input in &tx.inputs {
            let sats = if let Some(utxo) = utxo_set.get(&input.prevout) {
                let Some(sats) = money_sats(utxo.value) else {
                    return true;
                };
                sats
            } else if let Some(sats) = self.mempool_input_value(&input.prevout) {
                sats
            } else {
                continue;
            };
            let Some(sum) = add_money(total, sats) else {
                return true;
            };
            total = sum;
        }
        false
    }

    fn unknown_prevout(&self, tx: &Transaction, utxo_set: &UtxoSet, utxo_wired: bool) -> bool {
        if !utxo_wired {
            return false;
        }
        tx.inputs.iter().any(|input| {
            utxo_set.get(&input.prevout).is_none() && !self.mempool_creates(&input.prevout)
        })
    }

    fn immature_coinbase_spend(&self, tx: &Transaction, utxo_set: &UtxoSet) -> bool {
        let Some((height, _)) = self.finality_context() else {
            return false;
        };
        tx.inputs.iter().any(|input| {
            utxo_set.get(&input.prevout).is_some_and(|utxo| {
                !blvm_protocol::transaction::check_coinbase_maturity(
                    height,
                    utxo.height,
                    utxo.is_coinbase,
                )
            })
        })
    }

    /// Set event publisher for mempool events
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_event_publisher(&self, event_publisher: Option<Arc<EventPublisher>>) {
        *self
            .event_publisher
            .write()
            .unwrap_or_else(|e| e.into_inner()) = event_publisher;
    }

    /// Set RBF configuration
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_rbf_config(&self, rbf_config: Option<RbfConfig>) {
        *self.rbf_config.write().unwrap_or_else(|e| e.into_inner()) = rbf_config;
    }

    /// Set mempool policy configuration
    /// Uses interior mutability so it can be called even when MempoolManager is in an Arc
    pub fn set_policy_config(&self, policy_config: Option<MempoolPolicyConfig>) {
        *self
            .policy_config
            .write()
            .unwrap_or_else(|e| e.into_inner()) = policy_config;
    }

    /// Lock pool for access (transactions + spent_outputs)
    fn pool_lock(&self) -> std::sync::MutexGuard<'_, MempoolPool> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get current timestamp (Unix seconds)
    fn current_timestamp() -> u64 {
        crate::utils::time::current_timestamp()
    }

    /// Start mempool maintenance hooks.
    ///
    /// P2P and RPC intake call [`add_transaction`](Self::add_transaction) directly via
    /// `NetworkManager` and `RpcManager`; there is no separate pending-tx queue to drain here.
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting mempool manager");
        self.initialize_mempool().await?;
        Ok(())
    }

    /// Run periodic mempool maintenance once (expiry cleanup; for tests and manual ticks).
    pub async fn process_once(&mut self) -> Result<()> {
        self.cleanup_old_transactions().await?;
        Ok(())
    }

    /// Initialize mempool
    async fn initialize_mempool(&mut self) -> Result<()> {
        debug!("Initializing mempool");
        Ok(())
    }

    /// Clean up old transactions
    async fn cleanup_old_transactions(&mut self) -> Result<()> {
        let policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        let expiry_time = policy.mempool_expiry_hours * 3600;
        let current_time = Self::current_timestamp();
        // Optimization: Pre-allocate with estimated capacity
        let estimated_removals = self.pool_lock().transactions.len() / 100; // Estimate ~1% will expire
        let mut to_remove = Vec::with_capacity(estimated_removals);

        {
            let timestamps = self.tx_timestamps.read().unwrap_or_else(|e| e.into_inner());
            for (hash, timestamp) in timestamps.iter() {
                if current_time.saturating_sub(*timestamp) > expiry_time {
                    to_remove.push(*hash);
                }
            }
        }

        for hash in to_remove {
            debug!("Removing expired transaction {}", hex::encode(hash));
            self.remove_transaction(&hash);
        }

        // Check mempool size limits and evict if necessary
        self.enforce_mempool_limits().await?;

        Ok(())
    }

    /// Enforce mempool size limits by evicting transactions if necessary
    async fn enforce_mempool_limits(&mut self) -> Result<()> {
        let policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        // Calculate current mempool size
        let current_size_mb = self.calculate_mempool_size_mb();
        let current_tx_count = self.pool_lock().transactions.len();

        // Check if we need to evict
        let needs_eviction =
            current_size_mb > policy.max_mempool_mb || current_tx_count > policy.max_mempool_txs;

        if !needs_eviction {
            return Ok(());
        }

        // Publish MempoolThresholdExceeded for module event subscribers
        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let threshold = policy.max_mempool_txs;
            let current = current_tx_count;
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_threshold_exceeded(current, threshold)
                    .await;
            });
        }

        debug!(
            "Mempool size limit exceeded: {} MB / {} MB, {} txs / {} txs. Evicting transactions...",
            current_size_mb, policy.max_mempool_mb, current_tx_count, policy.max_mempool_txs
        );

        // Capture min fee rate before eviction (for FeeRateChanged event)
        let old_min_fee_rate = self.get_min_fee_rate_sat_per_vb();

        // Evict transactions based on strategy
        let target_size_mb = policy.max_mempool_mb;
        let target_tx_count = policy.max_mempool_txs;
        let policy = &policy;

        match &policy.eviction_strategy {
            crate::config::EvictionStrategy::LowestFeeRate => {
                self.evict_lowest_fee_rate(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::OldestFirst => {
                self.evict_oldest_first(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::LargestFirst => {
                self.evict_largest_first(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::NoDescendantsFirst => {
                self.evict_no_descendants_first(target_size_mb, target_tx_count)
                    .await?;
            }
            crate::config::EvictionStrategy::Hybrid => {
                self.evict_hybrid(target_size_mb, target_tx_count).await?;
            }
            crate::config::EvictionStrategy::SpamFirst => {
                self.evict_spam_first(target_size_mb, target_tx_count)
                    .await?;
            }
        }

        // Publish FeeRateChanged when min fee rate increased due to eviction
        let new_min_fee_rate = self.get_min_fee_rate_sat_per_vb();
        if new_min_fee_rate != old_min_fee_rate {
            if let Some(ref event_pub) = *self
                .event_publisher
                .read()
                .unwrap_or_else(|e| e.into_inner())
            {
                let old_f64 = old_min_fee_rate as f64;
                let new_f64 = new_min_fee_rate as f64;
                let mempool_size = self.pool_lock().transactions.len();
                let event_pub_clone = Arc::clone(event_pub);
                tokio::spawn(async move {
                    event_pub_clone
                        .publish_fee_rate_changed(old_f64, new_f64, mempool_size)
                        .await;
                });
            }
        }

        Ok(())
    }

    /// Get the minimum fee rate (sat/vB) in the mempool, or 0 if empty.
    fn get_min_fee_rate_sat_per_vb(&self) -> u64 {
        let fee_index = self.fee_index.read().unwrap_or_else(|e| e.into_inner());
        fee_index
            .iter()
            .last()
            .map(|(Reverse(r), _)| *r)
            .unwrap_or(0)
    }

    /// Calculate current mempool size in MB
    fn calculate_mempool_size_mb(&self) -> u64 {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        let total_bytes: usize = self
            .pool_lock()
            .transactions
            .values()
            .map(|tx| serialize_transaction(tx).len())
            .sum();

        // Convert to MB (1 MB = 1,048,576 bytes)
        (total_bytes as u64) / 1_048_576
    }

    /// Evict transactions with lowest fee rate
    async fn evict_lowest_fee_rate(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions sorted by fee rate (ascending - lowest first)
        let mut tx_fee_rates: Vec<(Hash, u64, usize)> = self
            .pool_lock()
            .transactions
            .iter()
            .map(|(hash, tx)| {
                let fee_rate = self
                    .fee_cache
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(hash)
                    .copied()
                    .unwrap_or(0);
                let size = serialize_transaction(tx).len();
                (*hash, fee_rate, size)
            })
            .collect();

        // Sort by fee rate (ascending) - lowest fee rate first
        tx_fee_rates.sort_by_key(|(_, fee_rate, _)| *fee_rate);

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.pool_lock().transactions.len();

        for (hash, _fee_rate, size) in tx_fee_rates {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants (would orphan them)
            let has_descendants = {
                let descendants = self
                    .tx_descendants
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!("Evicting low fee rate transaction {}", hex::encode(hash));
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict oldest transactions first (FIFO)
    async fn evict_oldest_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions with timestamps, sorted by age (oldest first)
        let mut tx_ages: Vec<(Hash, u64, usize)> = {
            let timestamps = self.tx_timestamps.read().unwrap_or_else(|e| e.into_inner());
            self.pool_lock()
                .transactions
                .iter()
                .filter_map(|(hash, tx)| {
                    timestamps.get(hash).map(|&timestamp| {
                        let size = serialize_transaction(tx).len();
                        (*hash, timestamp, size)
                    })
                })
                .collect()
        };

        // Sort by timestamp (ascending) - oldest first
        tx_ages.sort_by_key(|(_, timestamp, _)| *timestamp);

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.pool_lock().transactions.len();

        for (hash, _timestamp, size) in tx_ages {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self
                    .tx_descendants
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!("Evicting old transaction {}", hex::encode(hash));
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict largest transactions first
    async fn evict_largest_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions sorted by size (descending - largest first)
        let mut tx_sizes: Vec<(Hash, usize)> = self
            .pool_lock()
            .transactions
            .iter()
            .map(|(hash, tx)| {
                let size = serialize_transaction(tx).len();
                (*hash, size)
            })
            .collect();

        // Sort by size (descending) - largest first
        tx_sizes.sort_by_key(|(_, size)| std::cmp::Reverse(*size));

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.pool_lock().transactions.len();

        for (hash, size) in tx_sizes {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self
                    .tx_descendants
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!(
                    "Evicting large transaction {} ({} bytes)",
                    hex::encode(hash),
                    size
                );
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict transactions with no descendants first (safest)
    async fn evict_no_descendants_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        // Get all transactions with no descendants, sorted by fee rate (lowest first)
        let mut tx_no_descendants: Vec<(Hash, u64, usize)> = {
            let descendants = self
                .tx_descendants
                .read()
                .unwrap_or_else(|e| e.into_inner());
            let fee_cache = self.fee_cache.read().unwrap_or_else(|e| e.into_inner());

            self.pool_lock()
                .transactions
                .iter()
                .filter_map(|(hash, tx)| {
                    let has_descendants = descendants
                        .get(hash)
                        .map(|d| !d.is_empty())
                        .unwrap_or(false);

                    if !has_descendants {
                        let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);
                        let size = serialize_transaction(tx).len();
                        Some((*hash, fee_rate, size))
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Sort by fee rate (ascending) - lowest fee rate first
        tx_no_descendants.sort_by_key(|(_, fee_rate, _)| *fee_rate);

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.pool_lock().transactions.len();

        for (hash, _fee_rate, size) in tx_no_descendants {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            debug!(
                "Evicting transaction with no descendants {}",
                hex::encode(hash)
            );
            self.remove_transaction(&hash);
            current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
            current_tx_count -= 1;
        }

        Ok(())
    }

    /// Hybrid eviction: combine fee rate and age
    async fn evict_hybrid(&mut self, target_size_mb: u64, target_tx_count: usize) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;

        // Calculate score: lower fee rate + older age = higher eviction priority
        let current_time = Self::current_timestamp();
        let mut tx_scores: Vec<(Hash, u64, usize)> = {
            let timestamps = self.tx_timestamps.read().unwrap_or_else(|e| e.into_inner());
            let fee_cache = self.fee_cache.read().unwrap_or_else(|e| e.into_inner());

            self.pool_lock()
                .transactions
                .iter()
                .map(|(hash, tx)| {
                    let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);
                    let age = timestamps
                        .get(hash)
                        .map(|&t| current_time.saturating_sub(t))
                        .unwrap_or(0);

                    // Score: normalize fee rate (lower = higher score) + age weight
                    // Use inverse fee rate (higher for lower fees) + age in seconds
                    // Normalize fee rate: use 1 / (fee_rate + 1) to avoid division by zero
                    let fee_score = if fee_rate > 0 {
                        1_000_000 / (fee_rate + 1) // Higher score for lower fee
                    } else {
                        1_000_000 // Max score for zero fee
                    };

                    // Age weight: 1 point per hour old
                    let age_score = age / 3600;

                    // Combined score (higher = evict first)
                    let score = fee_score + age_score;

                    let size = serialize_transaction(tx).len();
                    (*hash, score, size)
                })
                .collect()
        };

        // Sort by score (descending) - highest score (most evictable) first
        tx_scores.sort_by_key(|(_, score, _)| std::cmp::Reverse(*score));

        // Evict until we're under limits
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.pool_lock().transactions.len();

        for (hash, _score, size) in tx_scores {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self
                    .tx_descendants
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!(
                    "Evicting transaction (hybrid strategy) {}",
                    hex::encode(hash)
                );
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// Evict spam transactions first (when mempool is full)
    async fn evict_spam_first(
        &mut self,
        target_size_mb: u64,
        target_tx_count: usize,
    ) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;
        use blvm_protocol::spam_filter::SpamFilter;

        // Get all transactions, classify as spam or not
        let spam_filter = SpamFilter::new();
        let mut spam_txs: Vec<(Hash, u64, usize)> = Vec::new();
        let mut non_spam_txs: Vec<(Hash, u64, usize)> = Vec::new();

        let entries: Vec<(Hash, Transaction)> = {
            let pool = self.pool_lock();
            pool.transactions
                .iter()
                .map(|(h, t)| (*h, t.clone()))
                .collect()
        };
        let fee_cache = self.fee_cache.read().unwrap_or_else(|e| e.into_inner());
        for (hash, tx) in &entries {
            let size = serialize_transaction(tx).len();
            let fee_rate = fee_cache.get(hash).copied().unwrap_or(0);

            let result = spam_filter.is_spam(tx);
            if result.is_spam {
                spam_txs.push((*hash, fee_rate, size));
            } else {
                non_spam_txs.push((*hash, fee_rate, size));
            }
        }
        drop(fee_cache);

        // Sort spam transactions by fee rate (lowest first - evict first)
        spam_txs.sort_by_key(|(_, fee_rate, _)| *fee_rate);

        // Sort non-spam transactions by fee rate (lowest first - evict last)
        non_spam_txs.sort_by_key(|(_, fee_rate, _)| *fee_rate);

        // Evict spam transactions first, then non-spam if needed
        let mut current_size_mb = self.calculate_mempool_size_mb();
        let mut current_tx_count = self.pool_lock().transactions.len();

        // First, evict spam transactions
        for (hash, _fee_rate, size) in spam_txs {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self
                    .tx_descendants
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!("Evicting spam transaction {}", hex::encode(hash));
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        // If still over limits, evict non-spam transactions (lowest fee rate first)
        for (hash, _fee_rate, size) in non_spam_txs {
            if current_size_mb <= target_size_mb && current_tx_count <= target_tx_count {
                break;
            }

            // Don't evict if it has descendants
            let has_descendants = {
                let descendants = self
                    .tx_descendants
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                descendants
                    .get(&hash)
                    .map(|d| !d.is_empty())
                    .unwrap_or(false)
            };

            if !has_descendants {
                debug!("Evicting non-spam transaction {}", hex::encode(hash));
                self.remove_transaction(&hash);
                current_size_mb = current_size_mb.saturating_sub((size as u64) / 1_048_576);
                current_tx_count -= 1;
            }
        }

        Ok(())
    }

    /// In-mempool replacement package: `root` plus all transitive descendants.
    fn replacement_package(&self, root: &Hash) -> HashSet<Hash> {
        let descendants_map = self
            .tx_descendants
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let pool = self.pool_lock();
        let mut package = HashSet::new();
        let mut stack = vec![*root];
        while let Some(current) = stack.pop() {
            if !package.insert(current) {
                continue;
            }
            if let Some(children) = descendants_map.get(&current) {
                for child in children {
                    if pool.transactions.contains_key(child) {
                        stack.push(*child);
                    }
                }
            }
        }
        package
    }

    /// Sum fees and virtual size for a set of in-mempool transactions.
    fn package_fee_and_vsize(&self, package: &HashSet<Hash>, utxo_set: &UtxoSet) -> (i64, u64) {
        let mut total_fee = 0i64;
        let mut total_vsize = 0u64;
        for hash in package {
            let tx = {
                let pool = self.pool_lock();
                pool.transactions.get(hash).cloned()
            };
            if let Some(tx) = tx {
                total_fee += self.calculate_transaction_fee(&tx, utxo_set) as i64;
                total_vsize += self.pooled_vsize(hash, &tx, utxo_set);
            }
        }
        (total_fee, total_vsize)
    }

    /// Value of a prevout created by an in-mempool parent transaction.
    fn mempool_input_value(&self, prevout: &OutPoint) -> Option<u64> {
        let pool = self.pool_lock();
        let parent = pool.transactions.get(&prevout.hash)?;
        parent
            .outputs
            .get(prevout.index as usize)
            .and_then(|output| money_sats(output.value))
    }

    /// Check if a transaction can replace an existing one (RBF)
    ///
    /// This wraps the consensus layer replacement_checks with RBF mode-specific logic
    ///
    /// `storage` is optional - if provided, can be used for conservative mode confirmation checks
    pub fn check_rbf_replacement(
        &self,
        new_tx: &Transaction,
        existing_tx: &Transaction,
        utxo_set: &UtxoSet,
        storage: Option<&crate::storage::Storage>,
    ) -> Result<bool> {
        self.check_rbf_replacement_with_witness(new_tx, existing_tx, utxo_set, storage, None)
    }

    fn check_rbf_replacement_with_witness(
        &self,
        new_tx: &Transaction,
        existing_tx: &Transaction,
        utxo_set: &UtxoSet,
        storage: Option<&crate::storage::Storage>,
        new_witnesses: Option<&[Witness]>,
    ) -> Result<bool> {
        use blvm_protocol::block::calculate_tx_id;

        let rbf_config = match self
            .rbf_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(config) => config.clone(),
            None => {
                // No RBF config - use default BIP125 behavior
                return replacement_checks_with_witness(
                    new_tx,
                    existing_tx,
                    utxo_set,
                    &self.mempool.read().unwrap_or_else(|e| e.into_inner()),
                    new_witnesses,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("RBF check failed: {}", e));
            }
        };

        // Check if RBF is disabled
        if matches!(rbf_config.mode, crate::config::RbfMode::Disabled) {
            return Ok(false);
        }

        // Use the cloned config for the rest of the function
        let rbf_config = &rbf_config;

        // Check if existing transaction signals RBF
        if !signals_rbf(existing_tx) {
            return Ok(false);
        }

        let existing_tx_hash = calculate_tx_id(existing_tx);
        let new_tx_hash = calculate_tx_id(new_tx);

        // Check replacement count limit
        if let Some(tracking) = self
            .rbf_tracking
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&existing_tx_hash)
        {
            if tracking.replacement_count >= rbf_config.max_replacements_per_tx {
                warn!(
                    "RBF replacement rejected: max replacements ({}) exceeded for tx {}",
                    rbf_config.max_replacements_per_tx,
                    hex::encode(existing_tx_hash)
                );
                return Ok(false);
            }

            // Check cooldown period
            let current_time = Self::current_timestamp();
            let time_since_last = current_time.saturating_sub(tracking.last_replacement_time);
            if time_since_last < rbf_config.cooldown_seconds {
                warn!(
                    "RBF replacement rejected: cooldown period not met ({}s remaining) for tx {}",
                    rbf_config.cooldown_seconds - time_since_last,
                    hex::encode(existing_tx_hash)
                );
                return Ok(false);
            }
        }

        // Calculate fees and fee rates. Aggressive package mode compares against the
        // full in-mempool package (root + descendants), not the root tx alone.
        let use_package_fees = matches!(rbf_config.mode, crate::config::RbfMode::Aggressive)
            && rbf_config.allow_package_replacements;
        let package = if use_package_fees {
            self.replacement_package(&existing_tx_hash)
        } else {
            HashSet::from([existing_tx_hash])
        };

        let (existing_fee, existing_tx_size) = if package.len() > 1 {
            self.package_fee_and_vsize(&package, utxo_set)
        } else {
            (
                self.calculate_transaction_fee(existing_tx, utxo_set) as i64,
                self.pooled_vsize(&existing_tx_hash, existing_tx, utxo_set),
            )
        };

        let new_fee = self.calculate_transaction_fee(new_tx, utxo_set) as i64;
        let new_tx_size = self.admit_vsize(new_tx, new_witnesses, utxo_set).0;

        if new_tx_size == 0 || existing_tx_size == 0 {
            return Ok(false);
        }

        if use_package_fees && package.len() > 1 {
            debug!(
                "Aggressive package replacement: comparing against {} txs ({} sat total fee)",
                package.len(),
                existing_fee
            );
        }

        // Check fee rate multiplier (mode-specific)
        let new_fee_scaled = (new_fee as u128)
            .checked_mul(existing_tx_size as u128)
            .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?;
        let existing_fee_scaled = (existing_fee as u128)
            .checked_mul(new_tx_size as u128)
            .ok_or_else(|| anyhow::anyhow!("Fee rate calculation overflow"))?;

        // Apply mode-specific multiplier
        let required_fee_scaled =
            (existing_fee_scaled as f64 * rbf_config.min_fee_rate_multiplier) as u128;
        if new_fee_scaled <= required_fee_scaled {
            warn!(
                "RBF replacement rejected: fee rate increase insufficient (required: {:.2}x, got: {:.2}x) for tx {}",
                rbf_config.min_fee_rate_multiplier,
                (new_fee_scaled as f64) / (existing_fee_scaled as f64),
                hex::encode(existing_tx_hash)
            );
            return Ok(false);
        }

        // Check absolute fee bump
        let min_fee_bump = rbf_config.min_fee_bump_satoshis as i64;
        if new_fee <= existing_fee + min_fee_bump {
            warn!(
                "RBF replacement rejected: absolute fee bump insufficient (required: {} sat, got: {} sat) for tx {}",
                min_fee_bump,
                new_fee - existing_fee,
                hex::encode(existing_tx_hash)
            );
            return Ok(false);
        }

        // Conservative mode: Check minimum confirmations
        // Note: Transactions in mempool have 0 confirmations. This check ensures that
        // if a transaction has been confirmed (which shouldn't be in mempool), we require
        // it to have minimum confirmations before allowing replacement.
        // In practice, mempool transactions will always have 0 confirmations, so this
        // check mainly serves as a safety mechanism.
        if matches!(rbf_config.mode, crate::config::RbfMode::Conservative)
            && rbf_config.min_confirmations > 0
        {
            if let Some(storage) = storage {
                // Check if transaction is in blockchain and has enough confirmations
                if let Ok(Some(metadata)) = storage.transactions().get_metadata(&existing_tx_hash) {
                    // Transaction is in a block - check confirmations
                    let block_hash = metadata.block_hash;
                    if let Ok(Some(block_height)) = storage.blocks().get_height_by_hash(&block_hash)
                    {
                        if let Ok(Some(tip_height)) = storage.chain().get_height() {
                            let confirmations = tip_height.saturating_sub(block_height) + 1;
                            if confirmations < rbf_config.min_confirmations as u64 {
                                warn!(
                                    "RBF replacement rejected: conservative mode requires {} confirmations, tx {} has {}",
                                    rbf_config.min_confirmations,
                                    hex::encode(existing_tx_hash),
                                    confirmations
                                );
                                return Ok(false);
                            }
                        }
                    }
                }
                // If transaction is not in blockchain (mempool only), confirmations = 0
                // For conservative mode, we might want to reject replacements of unconfirmed transactions
                // if min_confirmations > 0, but that would prevent all mempool RBF replacements.
                // So we allow it - the transaction is still in mempool and can be replaced.
            }
        }

        // Check conflict (must spend at least one input from existing tx)
        if !has_conflict_with_tx(new_tx, existing_tx) {
            return Ok(false);
        }

        // For the remaining BIP125 checks (new dependencies), use the consensus replacement_checks
        // but we've already applied our mode-specific fee requirements above
        // Note: replacement_checks will re-check fee rate, but we've already validated with our multiplier
        // So we call it to verify the other BIP125 rules (dependencies, etc.)
        // However, since we've already done stricter checks, if replacement_checks passes, we're good
        let bip125_result = replacement_checks_with_witness(
            new_tx,
            existing_tx,
            utxo_set,
            &self.mempool.read().unwrap_or_else(|e| e.into_inner()),
            new_witnesses,
            None,
        )?;
        if !bip125_result {
            // BIP125 check failed (likely new dependencies issue)
            return Ok(false);
        }

        // All checks passed
        Ok(true)
    }

    /// Check ancestor/descendant limits for a transaction
    fn check_ancestor_descendant_limits(
        &self,
        tx: &Transaction,
        tx_hash: &Hash,
        policy: &MempoolPolicyConfig,
        candidate_vsize: u64,
    ) -> Result<bool> {
        // The candidate is not in the pool yet. Seed the walk from its inputs.
        let mut ancestors = HashSet::with_capacity(10);
        let mut to_process = Vec::with_capacity(10);
        {
            let pool = self.pool_lock();
            for input in &tx.inputs {
                if pool.transactions.contains_key(&input.prevout.hash) {
                    ancestors.insert(input.prevout.hash);
                    to_process.push(input.prevout.hash);
                }
            }
        }
        let mut processed = HashSet::with_capacity(10);

        while let Some(current_hash) = to_process.pop() {
            if processed.contains(&current_hash) {
                continue;
            }
            processed.insert(current_hash);

            // Single pool critical section — nested pool_lock() would deadlock (non-reentrant Mutex).
            {
                let pool = self.pool_lock();
                if let Some(current_tx) = pool.transactions.get(&current_hash) {
                    let parent_keys: Vec<Hash> = pool.transactions.keys().copied().collect();
                    for input in &current_tx.inputs {
                        for parent_hash in &parent_keys {
                            if parent_hash == &input.prevout.hash {
                                if !ancestors.contains(parent_hash) {
                                    ancestors.insert(*parent_hash);
                                    to_process.push(*parent_hash);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Calculate ancestor count and size
        let ancestor_count = ancestors.len() as u32;
        let ancestor_size: u64 = {
            let pool = self.pool_lock();
            ancestors
                .iter()
                .map(|h| pool.adjusted_vsize.get(h).copied().unwrap_or(0))
                .sum()
        };

        // Check ancestor limits
        if ancestor_count + 1 > policy.max_ancestor_count {
            warn!(
                "Transaction {} exceeds max ancestor count: {} > {}",
                hex::encode(tx_hash),
                ancestor_count + 1,
                policy.max_ancestor_count
            );
            return Ok(false);
        }

        if ancestor_size + candidate_vsize > policy.max_ancestor_size {
            warn!(
                "Transaction {} exceeds max ancestor size: {} > {}",
                hex::encode(tx_hash),
                ancestor_size + candidate_vsize,
                policy.max_ancestor_size
            );
            return Ok(false);
        }

        // Find all descendants (transactions that depend on this tx)
        // Optimization: Pre-allocate with estimated capacity (most txs have < 10 descendants)
        let mut descendants = HashSet::with_capacity(10);
        let mut to_process = Vec::with_capacity(10);
        to_process.push(*tx_hash);
        let mut processed = HashSet::with_capacity(10);

        while let Some(current_hash) = to_process.pop() {
            if processed.contains(&current_hash) {
                continue;
            }
            processed.insert(current_hash);

            {
                let pool = self.pool_lock();
                if let Some(current_tx) = pool.transactions.get(&current_hash) {
                    let output_outpoints: Vec<_> = (0..current_tx.outputs.len())
                        .map(|idx| OutPoint {
                            hash: current_hash,
                            index: idx as u32,
                        })
                        .collect();

                    for (child_hash, child_tx) in &pool.transactions {
                        for input in &child_tx.inputs {
                            if output_outpoints.contains(&input.prevout) {
                                if !descendants.contains(child_hash) {
                                    descendants.insert(*child_hash);
                                    to_process.push(*child_hash);
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Calculate descendant count and size
        let descendant_count = descendants.len() as u32;
        let descendant_size: u64 = {
            let pool = self.pool_lock();
            descendants
                .iter()
                .map(|h| pool.adjusted_vsize.get(h).copied().unwrap_or(0))
                .sum()
        };

        // Check descendant limits
        if descendant_count + 1 > policy.max_descendant_count {
            warn!(
                "Transaction {} exceeds max descendant count: {} > {}",
                hex::encode(tx_hash),
                descendant_count + 1,
                policy.max_descendant_count
            );
            return Ok(false);
        }

        if descendant_size + candidate_vsize > policy.max_descendant_size {
            warn!(
                "Transaction {} exceeds max descendant size: {} > {}",
                hex::encode(tx_hash),
                descendant_size + candidate_vsize,
                policy.max_descendant_size
            );
            return Ok(false);
        }

        Ok(true)
    }

    /// Update dependency graph when a transaction is added
    fn update_dependency_graph(&self, tx: &Transaction, tx_hash: &Hash) {
        let mut dependencies = self
            .tx_dependencies
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let mut descendants = self
            .tx_descendants
            .write()
            .unwrap_or_else(|e| e.into_inner());

        // Initialize empty sets for this transaction
        dependencies.entry(*tx_hash).or_default();
        descendants.entry(*tx_hash).or_default();

        // Find parent transactions (ancestors) - transactions that created inputs
        for input in &tx.inputs {
            // Find transaction that created this output
            for parent_hash in self.pool_lock().transactions.keys() {
                if parent_hash == &input.prevout.hash {
                    // This transaction depends on parent
                    dependencies
                        .entry(*tx_hash)
                        .or_default()
                        .insert(*parent_hash);

                    // Parent has this as a descendant
                    descendants
                        .entry(*parent_hash)
                        .or_default()
                        .insert(*tx_hash);

                    break;
                }
            }
        }
    }

    /// Add transaction to mempool
    /// Uses interior mutability so it can be called with Arc<MempoolManager> (re-broadcast, sendrawtransaction)
    pub fn add_transaction(&self, tx: Transaction) -> Result<bool> {
        self.add_transaction_with_witness(tx, None)
    }

    /// Add transaction with optional SegWit witness stacks (one per input).
    pub fn add_transaction_with_witness(
        &self,
        tx: Transaction,
        witnesses: Option<Vec<Witness>>,
    ) -> Result<bool> {
        debug!("Adding transaction to mempool");

        use blvm_protocol::block::calculate_tx_id;
        let tx_hash = calculate_tx_id(&tx);

        // Reject duplicate — already in pool.
        if self.pool_lock().transactions.contains_key(&tx_hash) {
            debug!("Transaction {} already in mempool", hex::encode(tx_hash));
            return Ok(false);
        }

        // Reject transactions with duplicate inputs (invalid by consensus).
        {
            let mut seen = HashSet::with_capacity(tx.inputs.len());
            for input in &tx.inputs {
                if !seen.insert(input.prevout) {
                    warn!(
                        "Transaction {} rejected: duplicate input {:?}",
                        hex::encode(tx_hash),
                        input.prevout
                    );
                    return Ok(false);
                }
            }
        }

        // Reject empty transactions.
        if tx.inputs.is_empty() || tx.outputs.is_empty() {
            warn!(
                "Transaction {} rejected: empty inputs ({}) or outputs ({})",
                hex::encode(tx_hash),
                tx.inputs.len(),
                tx.outputs.len()
            );
            return Ok(false);
        }

        // Policy checks (min fee, spam filter).
        let effective_policy = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();

        if effective_policy.reject_spam_in_mempool {
            use blvm_protocol::spam_filter::SpamFilter;
            let filter = effective_policy
                .spam_filter
                .as_ref()
                .map(|cfg| SpamFilter::with_config(cfg.clone().into()))
                .unwrap_or_default();
            if filter.is_spam(&tx).is_spam {
                warn!(
                    "Transaction {} rejected: classified as spam",
                    hex::encode(tx_hash)
                );
                return Ok(false);
            }
        }

        // Min-relay-fee gate: reject transactions whose fee rate is below the
        // configured floor.  We need the UTXO set to compute fees; if it hasn't
        // been wired in yet we skip this check (startup path).
        // try_lock stays non-blocking. A wired set that is busy is not an empty
        // set: admitting against that would skip script, prevout, and fee checks.
        let (utxo_snapshot, utxo_wired) = {
            let utxo_slot = self.utxo_set_arc.read().unwrap_or_else(|e| e.into_inner());
            if let Some(arc) = utxo_slot.as_ref() {
                match arc.try_lock() {
                    Ok(guard) => (guard.clone(), true),
                    Err(_) => {
                        warn!(
                            "Transaction {} rejected: UTXO set is locked",
                            hex::encode(tx_hash)
                        );
                        return Ok(false);
                    }
                }
            } else {
                (UtxoSet::default(), false)
            }
        };

        let witness_ref = witnesses.as_deref();
        if let Some((height, median_time_past)) = self.finality_context() {
            if !blvm_protocol::mempool::is_final_tx(&tx, height, median_time_past) {
                warn!(
                    "Transaction {} rejected: locktime is not final",
                    hex::encode(tx_hash)
                );
                return Ok(false);
            }
        }
        if self.relative_lock_rejects(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: relative locktime is not final",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.immature_coinbase_spend(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: coinbase output is immature",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.unknown_prevout(&tx, &utxo_snapshot, utxo_wired) {
            warn!(
                "Transaction {} rejected: input prevout is not available",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.prevout_value_out_of_range(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: prevout value is outside the money range",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if self.prevout_sum_exceeds_money(&tx, &utxo_snapshot) {
            warn!(
                "Transaction {} rejected: prevout sum exceeds MAX_MONEY",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        if consensus_checks_reject(&tx, witness_ref, &self.script_prevouts(&tx, &utxo_snapshot)) {
            warn!(
                "Transaction {} rejected: failed consensus checks",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }
        let (candidate_vsize, sigop_cost) = self.admit_vsize(&tx, witness_ref, &utxo_snapshot);
        if sigop_cost > blvm_protocol::mempool::MAX_STANDARD_TX_SIGOPS_COST {
            warn!(
                "Transaction {} rejected: sigop cost {} exceeds standard limit {}",
                hex::encode(tx_hash),
                sigop_cost,
                blvm_protocol::mempool::MAX_STANDARD_TX_SIGOPS_COST
            );
            return Ok(false);
        }

        let mut fee_rate_sat_vb = 0u64;
        if utxo_wired {
            let fee = self.calculate_transaction_fee(&tx, &utxo_snapshot);
            let tx_size = candidate_vsize;
            fee_rate_sat_vb = if tx_size > 0 { fee / tx_size } else { 0 };
            if fee_rate_sat_vb < effective_policy.min_relay_fee_rate {
                warn!(
                    "Transaction {} rejected: fee rate {} sat/vB below min relay fee rate {} sat/vB",
                    hex::encode(tx_hash),
                    fee_rate_sat_vb,
                    effective_policy.min_relay_fee_rate
                );
                return Ok(false);
            }
            if fee < effective_policy.min_tx_fee {
                warn!(
                    "Transaction {} rejected: absolute fee {} sat below min_tx_fee {} sat",
                    hex::encode(tx_hash),
                    fee,
                    effective_policy.min_tx_fee
                );
                return Ok(false);
            }
        }

        // Check for conflicts with existing mempool transactions
        // If conflict exists, check if RBF replacement is allowed
        let mut conflicting_tx_hashes: Vec<Hash> = Vec::new();
        for input in &tx.inputs {
            if let Some(existing_tx) = self
                .pool_lock()
                .transactions
                .values()
                .find(|t| t.inputs.iter().any(|i| i.prevout == input.prevout))
                .cloned()
            {
                let existing_hash = calculate_tx_id(&existing_tx);
                if !conflicting_tx_hashes.contains(&existing_hash) {
                    conflicting_tx_hashes.push(existing_hash);
                }
            }
        }

        // BIP125: displacement set includes descendants of each directly conflicting tx.
        let direct_conflicts = conflicting_tx_hashes.clone();
        if !conflicting_tx_hashes.is_empty() {
            let mut displacement_set = HashSet::new();
            for hash in &direct_conflicts {
                displacement_set.extend(self.replacement_package(hash));
            }
            conflicting_tx_hashes = displacement_set.into_iter().collect();
        }

        // If there are conflicts, attempt RBF replacement.
        // BIP125 Rule 2: at most 100 displaced transactions.
        if !conflicting_tx_hashes.is_empty() {
            if conflicting_tx_hashes.len() > 100 {
                debug!(
                    "Transaction conflicts with {} existing transactions (> 100 limit)",
                    conflicting_tx_hashes.len()
                );
                return Ok(false);
            }

            // RBF eligibility is checked against each directly conflicting tx only.
            for &existing_hash in &direct_conflicts {
                let existing_clone = {
                    let pool = self.pool_lock();
                    pool.transactions.get(&existing_hash).cloned()
                };
                let Some(ref existing_tx) = existing_clone else {
                    continue;
                };
                if !self.check_rbf_replacement_with_witness(
                    &tx,
                    existing_tx,
                    &utxo_snapshot,
                    None,
                    witness_ref,
                )? {
                    debug!(
                        "RBF replacement rejected for conflicting tx {}",
                        hex::encode(existing_hash)
                    );
                    return Ok(false);
                }
            }

            // All checks passed — remove every displaced transaction.
            for existing_hash in &conflicting_tx_hashes {
                debug!(
                    "RBF replacement: removing displaced transaction {}",
                    hex::encode(existing_hash)
                );
                self.remove_transaction(existing_hash);
            }

            // Update RBF tracking (anchor to the primary direct conflict).
            let primary_hash = direct_conflicts[0];
            let original_hash = {
                let tracking = self.rbf_tracking.read().unwrap_or_else(|e| e.into_inner());
                tracking
                    .get(&primary_hash)
                    .map(|t| t.original_tx_hash)
                    .unwrap_or(primary_hash)
            };
            let replacement_count = {
                let tracking = self.rbf_tracking.read().unwrap_or_else(|e| e.into_inner());
                tracking
                    .get(&primary_hash)
                    .map(|t| t.replacement_count + 1)
                    .unwrap_or(1)
            };
            {
                let mut tracking = self.rbf_tracking.write().unwrap_or_else(|e| e.into_inner());
                tracking.insert(
                    tx_hash,
                    RbfTracking {
                        replacement_count,
                        last_replacement_time: Self::current_timestamp(),
                        original_tx_hash: original_hash,
                    },
                );
                for h in &conflicting_tx_hashes {
                    tracking.remove(h);
                }
            }
        } else {
            // No conflict - check if inputs are already spent
            for input in &tx.inputs {
                if self.pool_lock().spent_outputs.contains(&input.prevout) {
                    debug!("Transaction conflicts with existing mempool transaction");
                    return Ok(false);
                }
            }
        }

        // Check ancestor/descendant limits before adding (uses effective_policy from above).
        if !self.check_ancestor_descendant_limits(
            &tx,
            &tx_hash,
            &effective_policy,
            candidate_vsize,
        )? {
            warn!(
                "Transaction {} rejected: exceeds ancestor/descendant limits",
                hex::encode(tx_hash)
            );
            return Ok(false);
        }

        // Add transaction to mempool (store full transaction + optional witnesses)
        {
            let mut pool = self.pool_lock();
            pool.transactions.insert(tx_hash, tx.clone());
            pool.adjusted_vsize.insert(tx_hash, candidate_vsize);
            if let Some(wits) = witnesses {
                if wits.len() == tx.inputs.len() {
                    pool.tx_witnesses.insert(tx_hash, wits);
                } else {
                    warn!(
                        "Transaction {} witness count {} != input count {}, not storing witnesses",
                        hex::encode(tx_hash),
                        wits.len(),
                        tx.inputs.len()
                    );
                }
            }
            for input in &tx.inputs {
                pool.spent_outputs.insert(input.prevout);
            }
        }
        self.mempool
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tx_hash);

        // Update dependency graph
        self.update_dependency_graph(&tx, &tx_hash);

        // Record timestamp
        self.tx_timestamps
            .write()
            .unwrap()
            .insert(tx_hash, Self::current_timestamp());

        // Cache fee rate at insert (sat/vB) so eviction/sorting do not see stale zero rates.
        let fee_rate = fee_rate_sat_vb;
        self.fee_cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tx_hash, fee_rate);
        self.fee_index
            .write()
            .unwrap()
            .entry(Reverse(fee_rate))
            .or_default()
            .push(tx_hash);
        self.invalidate_fee_ranking();

        // Publish mempool transaction added event
        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let mempool_size = self.pool_lock().transactions.len();
            let fee_rate_f64 = fee_rate as f64;
            let tx_hash_clone = tx_hash;
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_transaction_added(&tx_hash_clone, fee_rate_f64, mempool_size)
                    .await;
            });
            // NewTransaction (use publish_event to avoid ZMQ Send issues in spawn)
            let tx_hash_ev = tx_hash;
            let ep_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                let _ = ep_clone
                    .publish_event(
                        crate::module::traits::EventType::NewTransaction,
                        crate::module::ipc::protocol::EventPayload::NewTransaction {
                            tx_hash: tx_hash_ev,
                        },
                    )
                    .await;
            });
        }

        Ok(true)
    }

    /// Get mempool size
    pub fn size(&self) -> usize {
        self.pool_lock().transactions.len()
    }

    /// Get mempool transaction hashes
    pub fn transaction_hashes(&self) -> Vec<Hash> {
        self.pool_lock().transactions.keys().cloned().collect()
    }

    /// Get stored SegWit witness stacks for a mempool transaction (one per input).
    pub fn get_transaction_witnesses(&self, hash: &Hash) -> Option<Vec<Witness>> {
        self.pool_lock().tx_witnesses.get(hash).cloned()
    }

    /// Satoshis per virtual byte recorded when the transaction was accepted.
    pub fn cached_fee_rate(&self, hash: &Hash) -> Option<u64> {
        self.fee_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
    }

    /// Unix time when the transaction entered the pool.
    pub fn accepted_at(&self, hash: &Hash) -> u64 {
        self.tx_timestamps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    /// In-pool parents this transaction spends.
    pub fn dependency_hashes(&self, hash: &Hash) -> Vec<Hash> {
        self.tx_dependencies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// In-pool children that spend this transaction.
    pub fn descendant_hashes(&self, hash: &Hash) -> Vec<Hash> {
        self.tx_descendants
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// True when a pool transaction spends this outpoint.
    pub fn spends_outpoint(&self, outpoint: &OutPoint) -> bool {
        self.pool_lock().spent_outputs.contains(outpoint)
    }

    /// Cumulative mining priority fee delta (satoshis) from `prioritisetransaction`.
    pub fn get_fee_delta(&self, hash: &Hash) -> i64 {
        self.fee_deltas
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
            .copied()
            .unwrap_or(0)
    }

    /// Get transaction by hash
    pub fn get_transaction(&self, hash: &Hash) -> Option<Transaction> {
        self.pool_lock().transactions.get(hash).cloned()
    }

    /// Get all transactions
    pub fn get_transactions(&self) -> Vec<Transaction> {
        self.pool_lock().transactions.values().cloned().collect()
    }

    /// Get prioritized transactions by fee rate
    ///
    /// Returns transactions sorted by fee rate (satoshis per vbyte) in descending order.
    /// Requires UTXO set to calculate fee rates.
    ///
    /// Optimization: Uses sorted index (BTreeMap) for O(log n) insertion, O(1) top-N retrieval
    /// instead of O(n log n) sort on every call.
    pub fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &UtxoSet,
    ) -> Vec<Transaction> {
        // Recalculate fee rates and update index
        // Note: In a production system, we'd track UTXO set changes and only recalculate when needed
        self.update_fee_index(utxo_set);

        // Collect hashes under fee_index read only; do not call pool_lock while holding fee_index
        // (update_fee_index may interleave with remove_transaction — opposite lock order would deadlock).
        let mut ordered_hashes: Vec<Hash> = Vec::with_capacity(limit);
        {
            let fee_index = self.fee_index.read().unwrap_or_else(|e| e.into_inner());
            for (Reverse(_fee_rate), tx_hashes) in fee_index.iter() {
                for tx_hash in tx_hashes {
                    ordered_hashes.push(*tx_hash);
                    if ordered_hashes.len() >= limit {
                        break;
                    }
                }
                if ordered_hashes.len() >= limit {
                    break;
                }
            }
        }

        let mut result = Vec::with_capacity(limit.min(ordered_hashes.len()));
        let pool = self.pool_lock();
        for h in ordered_hashes {
            if let Some(tx) = pool.transactions.get(&h) {
                result.push(tx.clone());
                if result.len() >= limit {
                    break;
                }
            }
        }
        result
    }

    /// Apply a mining priority fee delta to a mempool transaction (cumulative).
    ///
    /// Returns `false` when the transaction is not in the mempool.
    pub fn prioritise_transaction(&self, hash: &Hash, fee_delta: i64) -> bool {
        if !self.pool_lock().transactions.contains_key(hash) {
            return false;
        }
        {
            let mut deltas = self.fee_deltas.write().unwrap_or_else(|e| e.into_inner());
            let entry = deltas.entry(*hash).or_insert(0);
            *entry = entry.saturating_add(fee_delta);
        }
        self.invalidate_fee_ranking();
        true
    }

    /// The fee index is keyed only by the chain UTXO set. Pool changes must drop
    /// that key so the next ranking rebuilds rates from the transactions now stored.
    fn invalidate_fee_ranking(&self) {
        *self
            .utxo_set_hash
            .write()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Calculate a simple hash of the UTXO set for change detection
    ///
    /// Uses a fast hash of UTXO set size and a sample of keys to detect changes.
    /// This is a heuristic - not perfect but fast enough for optimization purposes.
    fn calculate_utxo_set_hash(utxo_set: &UtxoSet) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        utxo_set.len().hash(&mut hasher);

        // Sample first 10 keys for change detection (fast heuristic)
        let sample_size = utxo_set.len().min(10);
        for (i, (outpoint, utxo)) in utxo_set.iter().enumerate() {
            if i >= sample_size {
                break;
            }
            outpoint.hash(&mut hasher);
            utxo.value.hash(&mut hasher);
        }

        hasher.finish()
    }

    /// Update fee index with current UTXO set
    ///
    /// Recalculates fee rates for all transactions and rebuilds the sorted index.
    ///
    /// Optimization: Only recalculates when UTXO set changes (incremental updates)
    /// Optimization: Batch UTXO lookups across all transactions for better cache locality
    fn update_fee_index(&self, utxo_set: &UtxoSet) {
        // Calculate current UTXO set hash
        let current_hash = Self::calculate_utxo_set_hash(utxo_set);

        // Check if UTXO set changed
        let mut last_hash = self
            .utxo_set_hash
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if Some(current_hash) == *last_hash {
            // UTXO set unchanged - skip recalculation
            drop(last_hash);
            return;
        }

        // UTXO set changed - update hash and recalculate
        *last_hash = Some(current_hash);
        drop(last_hash);

        // Clear existing index (we'll rebuild it)
        let mut fee_index = self.fee_index.write().unwrap_or_else(|e| e.into_inner());
        fee_index.clear();
        drop(fee_index);

        {
            let mut fee_cache = self.fee_cache.write().unwrap_or_else(|e| e.into_inner());
            fee_cache.clear();
        }

        // Snapshot under pool lock only — never hold fee_cache/fee_index writes while locking pool
        // (avoids deadlock with remove_transaction: pool → fee_cache).
        let txs_snapshot: Vec<(Hash, Transaction)> = {
            let pool = self.pool_lock();
            pool.transactions
                .iter()
                .map(|(h, t)| (*h, t.clone()))
                .collect()
        };

        let all_prevouts: Vec<(Hash, OutPoint)> = txs_snapshot
            .iter()
            .flat_map(|(tx_hash, tx)| tx.inputs.iter().map(move |input| (*tx_hash, input.prevout)))
            .collect();

        let mut utxo_cache: HashMap<&OutPoint, u64> = HashMap::with_capacity(all_prevouts.len());
        let parents: HashMap<&Hash, &Transaction> =
            txs_snapshot.iter().map(|(hash, tx)| (hash, tx)).collect();
        for (_, prevout) in &all_prevouts {
            if let Some(utxo) = utxo_set.get(prevout) {
                if let Some(sats) = money_sats(utxo.value) {
                    utxo_cache.insert(prevout, sats);
                }
                continue;
            }
            let Some(parent) = parents.get(&prevout.hash) else {
                continue;
            };
            let Some(output) = parent.outputs.get(prevout.index as usize) else {
                continue;
            };
            if let Some(sats) = money_sats(output.value) {
                utxo_cache.insert(prevout, sats);
            }
        }

        for (tx_hash, tx) in &txs_snapshot {
            let mut input_total = 0u64;
            let mut within_money = true;
            for input in &tx.inputs {
                if let Some(&value) = utxo_cache.get(&input.prevout) {
                    match add_money(input_total, value) {
                        Some(sum) => input_total = sum,
                        None => {
                            within_money = false;
                            break;
                        }
                    }
                }
            }

            let fee = if within_money {
                match output_sum_sats(tx) {
                    Some(output_total) => input_total.saturating_sub(output_total),
                    None => 0,
                }
            } else {
                0
            };
            let size = self.pooled_vsize(tx_hash, tx, utxo_set) as usize;
            let delta = self
                .fee_deltas
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(tx_hash)
                .copied()
                .unwrap_or(0);
            let effective_fee = (fee as i64 + delta).max(0) as u64;
            // Store fee rate in sat/vB (consistent with min_relay_fee_rate config units).
            let fee_rate = if size > 0 {
                effective_fee / size as u64
            } else {
                0
            };

            {
                let mut fee_cache = self.fee_cache.write().unwrap_or_else(|e| e.into_inner());
                fee_cache.insert(*tx_hash, fee_rate);
            }
            let mut fee_index = self.fee_index.write().unwrap_or_else(|e| e.into_inner());
            fee_index
                .entry(Reverse(fee_rate))
                .or_default()
                .push(*tx_hash);
        }
    }

    /// Fee rates (sat/vB) for mempool transactions after refreshing the fee index.
    pub fn fee_rates_sat_vb(&self, utxo_set: &UtxoSet) -> Vec<u64> {
        self.update_fee_index(utxo_set);
        self.fee_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .copied()
            .collect()
    }

    /// Calculate transaction fee
    ///
    /// Fee = sum of inputs - sum of outputs
    ///
    /// Optimization: Uses batch UTXO lookup pattern for better cache locality
    pub fn calculate_transaction_fee(&self, tx: &Transaction, utxo_set: &UtxoSet) -> u64 {
        // Optimization: Batch UTXO lookups - collect all prevouts first, then lookup
        // This improves cache locality and reduces HashMap traversal overhead
        let prevouts: Vec<&OutPoint> = tx.inputs.iter().map(|input| &input.prevout).collect();

        // Batch UTXO lookup (single pass through HashMap)
        let mut input_total = 0u64;
        for prevout in prevouts {
            let sats = if let Some(utxo) = utxo_set.get(prevout) {
                money_sats(utxo.value)
            } else {
                self.mempool_input_value(prevout)
            };
            if let Some(sats) = sats {
                let Some(sum) = add_money(input_total, sats) else {
                    return 0;
                };
                input_total = sum;
            }
        }

        // An output outside the money range is not a fee. Casting it would wrap.
        let Some(output_total) = output_sum_sats(tx) else {
            return 0;
        };

        input_total.saturating_sub(output_total)
    }

    /// Legacy sigops count without prevouts. P2SH and witness sigops use chain UTXOs,
    /// and for a parent that exists only in this pool, that parent's `script_pubkey`.
    /// The spending witness is `witnesses`, not the parent's witness.
    fn admit_vsize(
        &self,
        tx: &Transaction,
        witnesses: Option<&[Witness]>,
        utxo_set: &UtxoSet,
    ) -> (u64, u64) {
        use blvm_protocol::UTXO;
        use blvm_protocol::mempool::{DEFAULT_BYTES_PER_SIGOP, sigop_adjusted_vsize};
        use blvm_protocol::script::flags::SEGWIT_STANDARD_FLAGS;
        use blvm_protocol::sigop::get_transaction_sigop_cost_with_utxos;

        let bytes_per_sigop = self
            .policy_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.bytes_per_sigop)
            .unwrap_or(DEFAULT_BYTES_PER_SIGOP);

        // Empty chain snapshot: P2SH and witness terms stay zero. Legacy sigops still count.
        // Non-empty snapshot: chain UTXO, else the unconfirmed parent's script_pubkey.
        let owned_utxos: Vec<Option<UTXO>> = if utxo_set.is_empty() {
            vec![None; tx.inputs.len()]
        } else {
            let pool = self.pool_lock();
            tx.inputs
                .iter()
                .map(|input| {
                    if let Some(utxo) = utxo_set.get(&input.prevout) {
                        return Some((**utxo).clone());
                    }
                    pool.transactions
                        .get(&input.prevout.hash)
                        .and_then(|parent| parent.outputs.get(input.prevout.index as usize))
                        .map(|output| UTXO {
                            value: output.value,
                            script_pubkey: output.script_pubkey.clone().into(),
                            height: 0,
                            is_coinbase: false,
                        })
                })
                .collect()
        };
        let refs: Vec<Option<&UTXO>> = owned_utxos.iter().map(|utxo| utxo.as_ref()).collect();
        let sigop_cost =
            get_transaction_sigop_cost_with_utxos(tx, &refs, witnesses, SEGWIT_STANDARD_FLAGS)
                .unwrap_or_else(|_| blvm_protocol::sigop::get_legacy_sigop_count(tx) as u64 * 4);

        let weight = self.transaction_weight(tx, witnesses);
        (
            sigop_adjusted_vsize(weight, sigop_cost, bytes_per_sigop),
            sigop_cost,
        )
    }

    /// Stored adjusted vsize for a pooled transaction, recomputed if the map has no entry.
    fn pooled_vsize(&self, hash: &Hash, tx: &Transaction, utxo_set: &UtxoSet) -> u64 {
        if let Some(vsize) = self.pool_lock().adjusted_vsize.get(hash).copied() {
            return vsize;
        }
        let witnesses = self.pool_lock().tx_witnesses.get(hash).cloned();
        self.admit_vsize(tx, witnesses.as_deref(), utxo_set).0
    }

    /// Transaction weight in weight units.
    ///
    /// Non-empty stacks use the serialized witness weight. Stacks that are all
    /// empty are the legacy encoding, so the weight is four times the stripped size.
    fn transaction_weight(&self, tx: &Transaction, witnesses: Option<&[Witness]>) -> u64 {
        use blvm_protocol::segwit::transaction_weight_from_stacks;

        if let Some(wits) = witnesses {
            if let Ok(weight) = transaction_weight_from_stacks(tx, Some(wits)) {
                return weight;
            }
        }
        self.estimate_transaction_weight(tx)
    }

    /// Guess used when witness stacks were not supplied.
    /// An empty `script_sig` is counted as a native segwit input.
    fn estimate_transaction_weight(&self, tx: &Transaction) -> u64 {
        let mut base_size: usize = 10;
        let mut witness_size: usize = 0;
        let mut segwit_inputs = 0usize;

        for input in &tx.inputs {
            base_size += 41 + input.script_sig.len();
            if input.script_sig.is_empty() {
                witness_size += 107;
                segwit_inputs += 1;
            }
        }
        for output in &tx.outputs {
            base_size += 9 + output.script_pubkey.len();
        }
        if segwit_inputs > 0 {
            witness_size += 2;
            (base_size * 4 + witness_size) as u64
        } else {
            (base_size as u64).saturating_mul(4)
        }
    }

    /// Virtual size for fee-rate and RBF comparisons (vbytes).
    fn transaction_virtual_size(&self, tx: &Transaction, tx_hash: &Hash) -> usize {
        use blvm_protocol::segwit::transaction_weight_from_stacks;
        use blvm_protocol::witness::weight_to_vsize;

        if let Some(witnesses) = self.get_transaction_witnesses(tx_hash) {
            if let Ok(weight) = transaction_weight_from_stacks(tx, Some(witnesses.as_slice())) {
                return weight_to_vsize(weight) as usize;
            }
        }
        self.estimate_transaction_size(tx)
    }

    /// Estimate transaction size in vbytes.
    ///
    /// For transactions with SegWit inputs (detected by empty `script_sig`), applies
    /// an approximate witness weight discount.  The `Transaction` type in this
    /// codebase does not carry witness data inline, so we use the following
    /// heuristic per segwit input:
    ///   * P2WPKH witness: ~107 bytes at 1/4 weight → 107/4 ≈ 27 vbytes
    ///   * The 2-byte segwit marker/flag overhead is ~0.5 vbytes (negligible)
    ///
    /// Inputs with non-empty `script_sig` are assumed non-witness (or P2SH-wrapped).
    pub fn estimate_transaction_size(&self, tx: &Transaction) -> usize {
        // Base: version (4) + input count (var, ~1) + output count (var, ~1) + locktime (4)
        let mut base_size: usize = 10;
        let mut witness_size: usize = 0; // witness bytes (counted at 1/4 weight)
        let mut segwit_inputs = 0usize;

        for input in &tx.inputs {
            // prevout (36) + sequence (4) + script_sig length varint (~1) + script_sig
            base_size += 41 + input.script_sig.len();
            if input.script_sig.is_empty() {
                // Likely a native SegWit input; estimate P2WPKH witness (~107 bytes)
                // or P2WSH (~220 bytes). Use P2WPKH as the conservative estimate.
                witness_size += 107;
                segwit_inputs += 1;
            }
        }

        for output in &tx.outputs {
            // value (8) + script_pubkey length varint (~1) + script_pubkey
            base_size += 9 + output.script_pubkey.len();
        }

        if segwit_inputs > 0 {
            // SegWit marker (1) + flag (1) also counted at discount weight
            witness_size += 2;
            // vsize = ceil((base_size * 4 + witness_size) / 4)
            (base_size * 4 + witness_size).div_ceil(4)
        } else {
            base_size
        }
    }

    /// Apply a chain reorg to the pool.
    ///
    /// Transactions confirmed or conflicted by `connected` are removed. Non-coinbase
    /// transactions from `disconnected` are admitted again against `utxo_set`, oldest
    /// block first, so a child can spend a parent that was just restored.
    /// `disconnected_witnesses` is one list of input stacks per transaction in those
    /// blocks, including the coinbase slot.
    pub fn apply_reorg(
        &self,
        disconnected: &[Block],
        disconnected_witnesses: &[Vec<Vec<Witness>>],
        connected: &[Block],
        utxo_set: &UtxoSet,
    ) {
        use blvm_protocol::transaction::is_coinbase;

        self.install_utxo_snapshot(utxo_set);
        for block in connected {
            self.remove_for_connected_block(&block.transactions);
        }
        for (block_index, block) in disconnected.iter().enumerate() {
            let block_witnesses = disconnected_witnesses.get(block_index);
            for (tx_index, tx) in block.transactions.iter().enumerate() {
                if is_coinbase(tx) {
                    continue;
                }
                let witnesses = block_witnesses
                    .and_then(|stacks| stacks.get(tx_index))
                    .cloned();
                let _ = self.add_transaction_with_witness(tx.clone(), witnesses);
            }
        }
    }

    fn install_utxo_snapshot(&self, utxo_set: &UtxoSet) {
        let slot = self.utxo_set_arc.read().unwrap_or_else(|e| e.into_inner());
        let Some(arc) = slot.as_ref() else {
            return;
        };
        if let Ok(mut guard) = arc.try_lock() {
            *guard = utxo_set.clone();
        }
    }

    /// Drop transactions a newly connected block confirmed or conflicted with.
    ///
    /// A transaction included in the block is removed. A different transaction
    /// that spends an output the block already spent is removed, along with its
    /// descendants. A child that spends an output the block just created stays.
    pub fn remove_for_connected_block(&self, transactions: &[Transaction]) {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::transaction::is_coinbase;

        let mut confirmed = HashSet::new();
        let mut spent = HashSet::new();
        for tx in transactions {
            if is_coinbase(tx) {
                continue;
            }
            confirmed.insert(calculate_tx_id(tx));
            for input in &tx.inputs {
                spent.insert(input.prevout);
            }
        }

        let mut remove_ids = confirmed.clone();
        for tx in self.get_transactions() {
            let id = calculate_tx_id(&tx);
            if tx.inputs.iter().any(|input| spent.contains(&input.prevout)) {
                remove_ids.insert(id);
            }
        }
        let conflicts: Vec<Hash> = remove_ids.difference(&confirmed).copied().collect();
        for id in conflicts {
            remove_ids.extend(self.replacement_package(&id));
        }
        for id in &remove_ids {
            self.remove_transaction(id);
        }
    }

    /// Remove transaction from mempool
    pub fn remove_transaction(&self, hash: &Hash) -> bool {
        let tx = {
            let mut pool = self.pool_lock();
            let Some(tx) = pool.transactions.remove(hash) else {
                return false;
            };
            pool.tx_witnesses.remove(hash);
            pool.adjusted_vsize.remove(hash);
            for input in &tx.inputs {
                pool.spent_outputs.remove(&input.prevout);
            }
            tx
        };

        self.mempool
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        // Remove from fee index
        if let Some(fee_rate) = self
            .fee_cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash)
        {
            let mut fee_index = self.fee_index.write().unwrap_or_else(|e| e.into_inner());
            if let Some(tx_hashes) = fee_index.get_mut(&Reverse(fee_rate)) {
                tx_hashes.retain(|&h| h != *hash);
                if tx_hashes.is_empty() {
                    fee_index.remove(&Reverse(fee_rate));
                }
            }
        }

        // Remove RBF tracking
        self.rbf_tracking
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        // Remove timestamp
        self.tx_timestamps
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        self.fee_deltas
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(hash);

        // Remove from dependency graph
        {
            let mut dependencies = self
                .tx_dependencies
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let mut descendants = self
                .tx_descendants
                .write()
                .unwrap_or_else(|e| e.into_inner());

            if let Some(children) = descendants.remove(hash) {
                for child_hash in children {
                    if let Some(parents) = dependencies.get_mut(&child_hash) {
                        parents.remove(hash);
                    }
                }
            }

            if let Some(parents) = dependencies.remove(hash) {
                for parent_hash in parents {
                    if let Some(children) = descendants.get_mut(&parent_hash) {
                        children.remove(hash);
                    }
                }
            }
        }

        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let mempool_size = self.pool_lock().transactions.len();
            let hash_clone = *hash;
            let reason = "removed".to_string();
            let event_pub_clone = Arc::clone(event_pub);
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_transaction_removed(&hash_clone, reason, mempool_size)
                    .await;
            });
        }

        self.invalidate_fee_ranking();
        true
    }

    /// Clear mempool
    pub fn clear(&self) {
        let (cleared_count,) = {
            let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let n = pool.transactions.len();
            pool.transactions.clear();
            pool.spent_outputs.clear();
            (n,)
        };
        self.mempool
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_index
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.fee_deltas
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.rbf_tracking
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.tx_timestamps
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.invalidate_fee_ranking();

        // Publish mempool cleared event
        if let Some(ref event_pub) = *self
            .event_publisher
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            let event_pub_clone = Arc::clone(event_pub);
            let cleared_count_clone = cleared_count;
            tokio::spawn(async move {
                event_pub_clone
                    .publish_mempool_cleared(cleared_count_clone)
                    .await;
            });
        }
    }

    /// Save mempool to disk for persistence
    pub fn save_to_disk<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        use blvm_protocol::serialization::transaction::serialize_transaction;
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

// MempoolManager is safe to share across threads: all interior state is
// protected by Mutex or RwLock, which derive Send+Sync automatically.
// The explicit impls below are not needed and have been removed.

impl crate::node::miner::MempoolProvider for MempoolManager {
    fn get_transactions(&self) -> Vec<blvm_protocol::Transaction> {
        self.get_transactions()
    }

    fn get_transaction(&self, hash: &[u8; 32]) -> Option<blvm_protocol::Transaction> {
        use blvm_protocol::Hash;
        let hash_array: Hash = *hash;
        self.get_transaction(&hash_array)
    }

    fn get_mempool_size(&self) -> usize {
        self.size()
    }

    fn get_prioritized_transactions(
        &self,
        limit: usize,
        utxo_set: &blvm_protocol::UtxoSet,
    ) -> Vec<blvm_protocol::Transaction> {
        self.get_prioritized_transactions(limit, utxo_set)
    }

    fn remove_transaction(&mut self, hash: &[u8; 32]) -> bool {
        use blvm_protocol::Hash;
        let hash_array: Hash = *hash;
        MempoolManager::remove_transaction(self, &hash_array)
    }

    fn get_transaction_witnesses(&self, hash: &[u8; 32]) -> Option<Vec<Witness>> {
        self.pool_lock().tx_witnesses.get(hash).cloned()
    }
}

impl MempoolManager {
    /// Load mempool from disk
    pub fn load_from_disk<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<()> {
        use blvm_protocol::serialization::transaction::deserialize_transaction;
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
            drop(self.add_transaction(tx));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MempoolManager;
    use blvm_protocol::serialization::transaction::serialize_transaction;
    use blvm_protocol::{OutPoint, Transaction, TransactionInput, TransactionOutput};

    fn legacy_empty_script() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1000,
                script_pubkey: Vec::new(),
            }]
            .into(),
            lock_time: 0,
        }
    }

    fn funded_at(script_pubkey: Vec<u8>, value: i64, height: u64) -> blvm_protocol::UtxoSet {
        use std::sync::Arc;
        let tx = legacy_empty_script();
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            tx.inputs[0].prevout,
            Arc::new(blvm_protocol::UTXO {
                value,
                script_pubkey: script_pubkey.into(),
                height,
                is_coinbase: false,
            }),
        );
        utxo_set
    }

    fn funded(script_pubkey: Vec<u8>, value: i64) -> blvm_protocol::UtxoSet {
        funded_at(script_pubkey, value, 0)
    }

    fn funded_coinbase(height: u64, is_coinbase: bool) -> blvm_protocol::UtxoSet {
        use std::sync::Arc;
        let tx = legacy_empty_script();
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        utxo_set.insert(
            tx.inputs[0].prevout,
            Arc::new(blvm_protocol::UTXO {
                value: 2_000,
                script_pubkey: vec![blvm_protocol::opcodes::OP_1].into(),
                height,
                is_coinbase,
            }),
        );
        utxo_set
    }

    #[test]
    fn mempool_rejects_consensus_invalid_transactions() {
        use blvm_protocol::opcodes::{OP_0, OP_1};

        let mempool = MempoolManager::new();
        let mut negative = legacy_empty_script();
        negative.outputs[0].value = -1;
        assert!(!mempool.add_transaction(negative).unwrap());

        let mempool = MempoolManager::new();
        let mut coinbase = legacy_empty_script();
        coinbase.inputs[0].prevout.hash = [0u8; 32];
        coinbase.inputs[0].prevout.index = 0xffff_ffff;
        coinbase.inputs[0]
            .script_sig
            .extend_from_slice(&[OP_1, OP_1]);
        assert!(!mempool.add_transaction(coinbase).unwrap());

        let mempool = MempoolManager::new();
        let tx = legacy_empty_script();
        assert!(
            !mempool
                .add_transaction_with_witness(tx, Some(Vec::new()))
                .unwrap()
        );

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_0],
            2_000,
        ))));
        assert!(!mempool.add_transaction(legacy_empty_script()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            2_000,
        ))));
        assert!(mempool.add_transaction(legacy_empty_script()).unwrap());
    }

    #[test]
    fn mempool_rejects_a_prevout_value_outside_money_range() {
        use blvm_protocol::constants::MAX_MONEY;
        use blvm_protocol::opcodes::OP_1;

        let tx = legacy_empty_script();

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            -1,
        ))));
        assert!(!mempool.add_transaction(tx.clone()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            MAX_MONEY + 1,
        ))));
        assert!(!mempool.add_transaction(tx.clone()).unwrap());

        let mut at_cap = tx;
        at_cap.outputs[0].value = MAX_MONEY - 1_000;
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            MAX_MONEY,
        ))));
        assert!(mempool.add_transaction(at_cap).unwrap());
    }

    #[test]
    fn mempool_rejects_a_prevout_sum_above_max_money() {
        use std::sync::Arc;

        use blvm_protocol::UTXO;
        use blvm_protocol::constants::MAX_MONEY;
        use blvm_protocol::opcodes::OP_1;

        let mut policy = crate::config::MempoolPolicyConfig::default();
        policy.min_relay_fee_rate = 0;
        policy.min_tx_fee = 0;

        let mut over = legacy_empty_script();
        over.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        });
        over.outputs[0].value = 1_000;
        let mut utxo_set = funded(vec![OP_1], MAX_MONEY);
        utxo_set.insert(
            over.inputs[1].prevout,
            Arc::new(UTXO {
                value: MAX_MONEY,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy.clone()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
        assert!(!mempool.add_transaction(over).unwrap());

        let mut at_cap = legacy_empty_script();
        at_cap.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: [3u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        });
        at_cap.outputs[0].value = 1_000;
        let mut utxo_set = funded(vec![OP_1], MAX_MONEY / 2);
        utxo_set.insert(
            at_cap.inputs[1].prevout,
            Arc::new(UTXO {
                value: MAX_MONEY - (MAX_MONEY / 2),
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy.clone()));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
        assert!(mempool.add_transaction(at_cap).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        let mut utxo_set = funded(vec![OP_1], 60_000);
        utxo_set.insert(
            OutPoint {
                hash: [4u8; 32],
                index: 0,
            },
            Arc::new(UTXO {
                value: MAX_MONEY,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));
        let mut parent = legacy_empty_script();
        parent.outputs[0].value = 50_000;
        parent.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(parent.clone()).unwrap());

        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: blvm_protocol::block::calculate_tx_id(&parent),
            index: 0,
        };
        child.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: [4u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        });
        child.outputs[0].value = 1_000;
        assert!(!mempool.add_transaction(child).unwrap());
    }

    #[test]
    fn selector_includes_a_child_that_spends_a_mempool_parent() {
        use std::sync::Arc;

        use crate::node::miner::{MempoolProvider, TransactionSelector};
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let utxo_set = funded(vec![OP_1], 200_000);
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

        let mut parent = legacy_empty_script();
        parent.outputs[0].value = 190_000;
        parent.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(parent.clone()).unwrap());

        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&parent),
            index: 0,
        };
        child.outputs[0].value = 100_000;
        assert!(mempool.add_transaction(child.clone()).unwrap());

        let rates = mempool.fee_rates_sat_vb(&utxo_set);
        assert!(rates.len() >= 2);
        assert!(rates.iter().all(|rate| *rate >= 1));

        let ordered = mempool.get_prioritized_transactions(2, &utxo_set);
        assert_eq!(calculate_tx_id(&ordered[0]), calculate_tx_id(&child));

        let parent_id = calculate_tx_id(&parent);
        let child_id = calculate_tx_id(&child);
        let selected = TransactionSelector::new().select_transactions(&mempool, &utxo_set);
        let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
        let parent_at = selected_ids.iter().position(|id| *id == parent_id).unwrap();
        let child_at = selected_ids.iter().position(|id| *id == child_id).unwrap();
        assert!(parent_at < child_at);
    }

    #[test]
    fn selector_includes_a_parent_paid_for_by_its_child() {
        use std::sync::Arc;

        use crate::config::MempoolPolicyConfig;
        use crate::node::miner::{MempoolProvider, TransactionSelector};
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let mut policy = MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 0;

        let utxo_set = funded(vec![OP_1], 60_000);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

        let mut parent = legacy_empty_script();
        parent.outputs[0].value = 59_999;
        parent.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(parent.clone()).unwrap());

        let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 5);
        let alone = selector.select_transactions(&mempool, &utxo_set);
        assert!(alone.is_empty());

        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&parent),
            index: 0,
        };
        child.outputs[0].value = 10_000;
        child.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(child.clone()).unwrap());

        let parent_id = calculate_tx_id(&parent);
        let child_id = calculate_tx_id(&child);
        let selected = selector.select_transactions(&mempool, &utxo_set);
        let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
        let parent_at = selected_ids.iter().position(|id| *id == parent_id).unwrap();
        let child_at = selected_ids.iter().position(|id| *id == child_id).unwrap();
        assert!(parent_at < child_at);
    }

    #[test]
    fn selector_includes_two_parents_paid_for_by_their_child() {
        use std::sync::Arc;

        use crate::config::MempoolPolicyConfig;
        use crate::node::miner::{MempoolProvider, TransactionSelector};
        use blvm_protocol::TransactionInput;
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;

        let mut policy = MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 0;

        let mut utxo_set = funded(vec![OP_1], 30_000);
        utxo_set.insert(
            OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 30_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

        let mut parent_a = legacy_empty_script();
        parent_a.outputs[0].value = 29_999;
        parent_a.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(parent_a.clone()).unwrap());

        let mut parent_b = legacy_empty_script();
        parent_b.inputs[0].prevout.hash = [2u8; 32];
        parent_b.outputs[0].value = 29_999;
        parent_b.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(parent_b.clone()).unwrap());

        let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 5);
        assert!(selector.select_transactions(&mempool, &utxo_set).is_empty());

        let parent_a_id = calculate_tx_id(&parent_a);
        let parent_b_id = calculate_tx_id(&parent_b);
        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: parent_a_id,
            index: 0,
        };
        child.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: parent_b_id,
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: SEQUENCE_FINAL as u64,
        });
        child.outputs[0].value = 10_000;
        child.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(child.clone()).unwrap());

        let child_id = calculate_tx_id(&child);
        let selected = selector.select_transactions(&mempool, &utxo_set);
        let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
        let parent_a_at = selected_ids
            .iter()
            .position(|id| *id == parent_a_id)
            .unwrap();
        let parent_b_at = selected_ids
            .iter()
            .position(|id| *id == parent_b_id)
            .unwrap();
        let child_at = selected_ids.iter().position(|id| *id == child_id).unwrap();
        assert!(parent_a_at < child_at);
        assert!(parent_b_at < child_at);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn selector_includes_a_grandparent_paid_for_by_its_grandchild() {
        use std::sync::Arc;

        use crate::config::MempoolPolicyConfig;
        use crate::node::miner::{MempoolProvider, TransactionSelector};
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let mut policy = MempoolPolicyConfig::default();
        policy.min_tx_fee = 0;
        policy.min_relay_fee_rate = 0;

        let utxo_set = funded(vec![OP_1], 40_000);
        let mempool = MempoolManager::new();
        mempool.set_policy_config(Some(policy));
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set.clone())));

        let mut grandparent = legacy_empty_script();
        grandparent.outputs[0].value = 39_999;
        grandparent.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(grandparent.clone()).unwrap());

        let mut middle = legacy_empty_script();
        middle.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&grandparent),
            index: 0,
        };
        middle.outputs[0].value = 39_998;
        middle.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(middle.clone()).unwrap());

        let selector = TransactionSelector::with_params(1_000_000, 4_000_000, 5);
        assert!(selector.select_transactions(&mempool, &utxo_set).is_empty());

        let mut grandchild = legacy_empty_script();
        grandchild.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&middle),
            index: 0,
        };
        grandchild.outputs[0].value = 10_000;
        grandchild.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(grandchild.clone()).unwrap());

        let grandparent_id = calculate_tx_id(&grandparent);
        let middle_id = calculate_tx_id(&middle);
        let grandchild_id = calculate_tx_id(&grandchild);
        let selected = selector.select_transactions(&mempool, &utxo_set);
        let selected_ids: Vec<_> = selected.iter().map(calculate_tx_id).collect();
        let grandparent_at = selected_ids
            .iter()
            .position(|id| *id == grandparent_id)
            .unwrap();
        let middle_at = selected_ids.iter().position(|id| *id == middle_id).unwrap();
        let grandchild_at = selected_ids
            .iter()
            .position(|id| *id == grandchild_id)
            .unwrap();
        assert!(grandparent_at < middle_at);
        assert!(middle_at < grandchild_at);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn ranking_recomputes_a_tx_admitted_while_the_utxo_lock_is_held() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let mut utxo_set = funded(vec![OP_1], 50_000);
        utxo_set.insert(
            OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 80_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let utxo_arc = Arc::new(tokio::sync::Mutex::new(utxo_set.clone()));
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::clone(&utxo_arc));

        let mut low_fee = legacy_empty_script();
        low_fee.outputs[0].value = 40_000;
        assert!(mempool.add_transaction(low_fee).unwrap());
        assert_eq!(mempool.get_prioritized_transactions(10, &utxo_set).len(), 1);

        let mut high_fee = legacy_empty_script();
        high_fee.inputs[0].prevout = OutPoint {
            hash: [2u8; 32],
            index: 0,
        };
        high_fee.outputs[0].value = 10_000;
        let _held = utxo_arc.blocking_lock();
        assert!(!mempool.add_transaction(high_fee.clone()).unwrap());
        drop(_held);
        assert!(mempool.add_transaction(high_fee.clone()).unwrap());

        let ranked = mempool.get_prioritized_transactions(1, &utxo_set);
        assert_eq!(ranked.len(), 1);
        assert_eq!(calculate_tx_id(&ranked[0]), calculate_tx_id(&high_fee));
    }

    #[test]
    fn mempool_rejects_admission_while_the_utxo_lock_is_held() {
        use std::sync::Arc;

        use blvm_protocol::opcodes::{OP_0, OP_1};

        let mut utxo_set = funded(vec![OP_1], 50_000);
        utxo_set.insert(
            OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![OP_0].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let utxo_arc = Arc::new(tokio::sync::Mutex::new(utxo_set));
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::clone(&utxo_arc));

        let valid = legacy_empty_script();
        let mut unspendable = legacy_empty_script();
        unspendable.inputs[0].prevout = OutPoint {
            hash: [2u8; 32],
            index: 0,
        };
        let _held = utxo_arc.blocking_lock();
        assert!(!mempool.add_transaction(valid.clone()).unwrap());
        assert!(!mempool.add_transaction(unspendable.clone()).unwrap());
        drop(_held);
        assert_eq!(mempool.size(), 0);

        assert!(!mempool.add_transaction(unspendable).unwrap());
        assert!(mempool.add_transaction(valid).unwrap());
        assert_eq!(mempool.size(), 1);
    }

    #[test]
    fn mempool_rejects_a_spend_when_the_wired_utxo_set_is_empty() {
        use std::sync::Arc;

        use blvm_protocol::opcodes::OP_1;

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(
            blvm_protocol::UtxoSet::default(),
        )));
        let tx = legacy_empty_script();
        assert!(!mempool.add_transaction(tx.clone()).unwrap());
        assert_eq!(mempool.size(), 0);

        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            50_000,
        ))));
        assert!(mempool.add_transaction(tx).unwrap());
    }

    #[test]
    fn connected_block_removes_confirmed_and_conflicting_spends() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let mut utxo_set = funded(vec![OP_1], 100_000);
        utxo_set.insert(
            OutPoint {
                hash: [3u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo_set)));

        let mut confirmed = legacy_empty_script();
        confirmed.outputs[0].value = 80_000;
        confirmed.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(confirmed.clone()).unwrap());

        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&confirmed),
            index: 0,
        };
        child.outputs[0].value = 60_000;
        assert!(mempool.add_transaction(child.clone()).unwrap());

        let mut unrelated = legacy_empty_script();
        unrelated.inputs[0].prevout = OutPoint {
            hash: [3u8; 32],
            index: 0,
        };
        unrelated.outputs[0].value = 40_000;
        assert!(mempool.add_transaction(unrelated.clone()).unwrap());

        mempool.remove_for_connected_block(&[confirmed.clone()]);
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&confirmed))
                .is_none()
        );
        assert!(mempool.get_transaction(&calculate_tx_id(&child)).is_some());
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&unrelated))
                .is_some()
        );

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            100_000,
        ))));
        let mut conflict = legacy_empty_script();
        conflict.outputs[0].value = 80_000;
        conflict.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(conflict.clone()).unwrap());
        let mut conflict_child = legacy_empty_script();
        conflict_child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&conflict),
            index: 0,
        };
        conflict_child.outputs[0].value = 60_000;
        assert!(mempool.add_transaction(conflict_child.clone()).unwrap());

        let mut in_block = conflict.clone();
        in_block.outputs[0].value = 70_000;
        mempool.remove_for_connected_block(&[in_block]);
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&conflict))
                .is_none()
        );
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&conflict_child))
                .is_none()
        );
        assert_eq!(mempool.size(), 0);
    }

    #[test]
    fn reorg_restores_disconnected_spends_and_drops_conflicts() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;
        use blvm_protocol::{Block, BlockHeader};

        fn block_with(txs: Vec<Transaction>) -> Block {
            Block {
                header: BlockHeader {
                    version: 1,
                    prev_block_hash: [0u8; 32],
                    merkle_root: [0u8; 32],
                    timestamp: 0,
                    bits: 0,
                    nonce: 0,
                },
                transactions: txs.into_boxed_slice(),
            }
        }

        let mut before = funded(vec![OP_1], 100_000);
        before.insert(
            OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 100_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(before)));

        let mut conflict = legacy_empty_script();
        conflict.inputs[0].prevout.hash = [2u8; 32];
        conflict.outputs[0].value = 80_000;
        conflict.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(conflict.clone()).unwrap());
        let mut conflict_child = legacy_empty_script();
        conflict_child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&conflict),
            index: 0,
        };
        conflict_child.outputs[0].value = 60_000;
        assert!(mempool.add_transaction(conflict_child.clone()).unwrap());

        let mut coinbase = legacy_empty_script();
        coinbase.inputs[0].prevout.hash = [0u8; 32];
        coinbase.inputs[0].prevout.index = 0xffff_ffff;
        coinbase.inputs[0].script_sig = vec![OP_1, OP_1];
        coinbase.outputs[0].value = 50_000;

        let mut resurrected = legacy_empty_script();
        resurrected.outputs[0].value = 80_000;
        resurrected.outputs[0].script_pubkey = vec![OP_1];
        let mut resurrected_child = legacy_empty_script();
        resurrected_child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&resurrected),
            index: 0,
        };
        resurrected_child.outputs[0].value = 60_000;

        let mut connected_spend = legacy_empty_script();
        connected_spend.inputs[0].prevout.hash = [2u8; 32];
        connected_spend.outputs[0].value = 70_000;

        let after = funded(vec![OP_1], 100_000);
        mempool.apply_reorg(
            &[block_with(vec![
                coinbase,
                resurrected.clone(),
                resurrected_child.clone(),
            ])],
            &[],
            &[block_with(vec![connected_spend])],
            &after,
        );

        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&conflict))
                .is_none()
        );
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&conflict_child))
                .is_none()
        );
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&resurrected))
                .is_some()
        );
        assert!(
            mempool
                .get_transaction(&calculate_tx_id(&resurrected_child))
                .is_some()
        );
        assert_eq!(mempool.size(), 2);
    }

    #[test]
    fn reorg_restores_a_segwit_spend_with_its_witness() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
        use blvm_protocol::segwit::Witness;
        use blvm_protocol::{Block, BlockHeader};
        use sha2::{Digest, Sha256};

        fn block_with(txs: Vec<Transaction>) -> Block {
            Block {
                header: BlockHeader {
                    version: 1,
                    prev_block_hash: [0u8; 32],
                    merkle_root: [0u8; 32],
                    timestamp: 0,
                    bits: 0,
                    nonce: 0,
                },
                transactions: txs.into_boxed_slice(),
            }
        }

        let witness_script = vec![OP_1];
        let script_hash = Sha256::digest(&witness_script);
        let mut script_pubkey = vec![OP_0, PUSH_32_BYTES];
        script_pubkey.extend_from_slice(&script_hash);

        let utxo = funded(script_pubkey, 100_000);
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo.clone())));

        let mut spend = legacy_empty_script();
        spend.outputs[0].value = 80_000;
        spend.outputs[0].script_pubkey = vec![OP_1];
        let witness: Vec<Witness> = vec![vec![witness_script]];

        let mut coinbase = legacy_empty_script();
        coinbase.inputs[0].prevout.hash = [0u8; 32];
        coinbase.inputs[0].prevout.index = 0xffff_ffff;
        coinbase.inputs[0].script_sig = vec![OP_1, OP_1];
        coinbase.outputs[0].value = 50_000;

        mempool.apply_reorg(
            &[block_with(vec![coinbase, spend.clone()])],
            &[vec![Vec::new(), witness.clone()]],
            &[],
            &utxo,
        );

        let txid = calculate_tx_id(&spend);
        assert!(mempool.get_transaction(&txid).is_some());
        assert_eq!(mempool.get_transaction_witnesses(&txid), Some(witness));
        assert_eq!(mempool.size(), 1);
    }

    #[test]
    fn mempool_rejects_a_witness_program_spend_without_a_witness() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
        use blvm_protocol::segwit::Witness;
        use sha2::{Digest, Sha256};

        let witness_script = vec![OP_1];
        let script_hash = Sha256::digest(&witness_script);
        let mut script_pubkey = vec![OP_0, PUSH_32_BYTES];
        script_pubkey.extend_from_slice(&script_hash);

        let mut utxo = funded(script_pubkey, 100_000);
        utxo.insert(
            OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: vec![OP_1].into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));

        let mut legacy = legacy_empty_script();
        legacy.inputs[0].prevout.hash = [2u8; 32];
        legacy.outputs[0].value = 40_000;
        legacy.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(legacy).unwrap());

        let mut spend = legacy_empty_script();
        spend.outputs[0].value = 80_000;
        spend.outputs[0].script_pubkey = vec![OP_1];
        assert!(!mempool.add_transaction(spend.clone()).unwrap());

        let witness: Vec<Witness> = vec![vec![witness_script]];
        assert!(
            mempool
                .add_transaction_with_witness(spend.clone(), Some(witness.clone()))
                .unwrap()
        );
        assert_eq!(
            mempool.get_transaction_witnesses(&calculate_tx_id(&spend)),
            Some(witness)
        );
        assert_eq!(mempool.size(), 2);
    }

    #[test]
    fn mempool_rejects_a_p2sh_witness_redeem_without_a_witness() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::{
            OP_0, OP_1, OP_EQUAL, OP_HASH160, PUSH_20_BYTES, PUSH_32_BYTES,
        };
        use blvm_protocol::segwit::Witness;
        use ripemd::{Digest, Ripemd160};
        use sha2::Sha256;

        fn hash160(data: &[u8]) -> [u8; 20] {
            let sha = Sha256::digest(data);
            Ripemd160::digest(sha).into()
        }

        fn p2sh_script(redeem: &[u8]) -> Vec<u8> {
            let hash = hash160(redeem);
            let mut script = vec![OP_HASH160, PUSH_20_BYTES];
            script.extend_from_slice(&hash);
            script.push(OP_EQUAL);
            script
        }

        fn push_data(data: &[u8]) -> Vec<u8> {
            let mut script = Vec::with_capacity(data.len() + 1);
            script.push(data.len() as u8);
            script.extend_from_slice(data);
            script
        }

        let witness_script = vec![OP_1];
        let program = Sha256::digest(&witness_script);
        let mut redeem = vec![OP_0, PUSH_32_BYTES];
        redeem.extend_from_slice(&program);

        let plain_redeem = vec![OP_1];
        let mut utxo = funded(p2sh_script(&redeem), 100_000);
        utxo.insert(
            OutPoint {
                hash: [2u8; 32],
                index: 0,
            },
            Arc::new(blvm_protocol::UTXO {
                value: 50_000,
                script_pubkey: p2sh_script(&plain_redeem).into(),
                height: 0,
                is_coinbase: false,
            }),
        );
        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(utxo)));

        let mut plain = legacy_empty_script();
        plain.inputs[0].prevout.hash = [2u8; 32];
        plain.inputs[0].script_sig = push_data(&plain_redeem);
        plain.outputs[0].value = 40_000;
        plain.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(plain).unwrap());

        let mut spend = legacy_empty_script();
        spend.inputs[0].script_sig = push_data(&redeem);
        spend.outputs[0].value = 80_000;
        spend.outputs[0].script_pubkey = vec![OP_1];
        assert!(!mempool.add_transaction(spend.clone()).unwrap());

        let witness: Vec<Witness> = vec![vec![witness_script]];
        assert!(
            mempool
                .add_transaction_with_witness(spend.clone(), Some(witness.clone()))
                .unwrap()
        );
        assert_eq!(
            mempool.get_transaction_witnesses(&calculate_tx_id(&spend)),
            Some(witness)
        );
        assert_eq!(mempool.size(), 2);
    }

    #[test]
    fn mempool_rejects_a_witness_spend_of_a_mempool_parent_without_a_witness() {
        use std::sync::Arc;

        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::{OP_0, OP_1, PUSH_32_BYTES};
        use blvm_protocol::segwit::Witness;
        use sha2::{Digest, Sha256};

        let witness_script = vec![OP_1];
        let program = Sha256::digest(&witness_script);
        let mut parent_script = vec![OP_0, PUSH_32_BYTES];
        parent_script.extend_from_slice(&program);

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            100_000,
        ))));

        let mut parent = legacy_empty_script();
        parent.outputs[0].value = 80_000;
        parent.outputs[0].script_pubkey = parent_script;
        assert!(mempool.add_transaction(parent.clone()).unwrap());

        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&parent),
            index: 0,
        };
        child.outputs[0].value = 60_000;
        child.outputs[0].script_pubkey = vec![OP_1];
        assert!(!mempool.add_transaction(child.clone()).unwrap());

        let witness: Vec<Witness> = vec![vec![witness_script]];
        assert!(
            mempool
                .add_transaction_with_witness(child.clone(), Some(witness.clone()))
                .unwrap()
        );
        assert_eq!(
            mempool.get_transaction_witnesses(&calculate_tx_id(&child)),
            Some(witness)
        );
        assert_eq!(mempool.size(), 2);
    }

    #[test]
    fn mempool_rejects_an_unknown_prevout() {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::opcodes::OP_1;

        let mempool = MempoolManager::new();
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded(
            vec![OP_1],
            5_000,
        ))));

        let mut mixed = legacy_empty_script();
        mixed.inputs.push(TransactionInput {
            prevout: OutPoint {
                hash: [9u8; 32],
                index: 0,
            },
            script_sig: Vec::new(),
            sequence: 0xffffffff,
        });
        assert!(!mempool.add_transaction(mixed).unwrap());

        let mut parent = legacy_empty_script();
        parent.outputs[0].value = 3_000;
        parent.outputs[0].script_pubkey = vec![OP_1];
        assert!(mempool.add_transaction(parent.clone()).unwrap());

        let mut child = legacy_empty_script();
        child.inputs[0].prevout = OutPoint {
            hash: calculate_tx_id(&parent),
            index: 0,
        };
        child.outputs[0].value = 1_000;
        assert!(mempool.add_transaction(child).unwrap());
    }

    #[test]
    fn mempool_rejects_an_immature_coinbase_spend() {
        use blvm_protocol::constants::COINBASE_MATURITY;

        let created = 1_000u64;
        let immature = created + COINBASE_MATURITY - 1;
        let mature = created + COINBASE_MATURITY;
        let tx = legacy_empty_script();

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(immature, 0);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
            funded_coinbase(created, true),
        )));
        assert!(!mempool.add_transaction(tx.clone()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(mature, 0);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
            funded_coinbase(created, true),
        )));
        assert!(mempool.add_transaction(tx.clone()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(immature, 0);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(
            funded_coinbase(created, false),
        )));
        assert!(mempool.add_transaction(tx).unwrap());
    }

    #[test]
    fn mempool_rejects_a_relative_locktime() {
        use blvm_protocol::opcodes::OP_1;

        let next_height = 500_000;
        let mut young = legacy_empty_script();
        young.version = 2;
        young.inputs[0].sequence = 10;
        let mempool = MempoolManager::new();
        mempool.set_chain_tip(next_height, 1_700_000_000);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
            vec![OP_1],
            2_000,
            499_995,
        ))));
        assert!(!mempool.add_transaction(young.clone()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(next_height, 1_700_000_000);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
            vec![OP_1],
            2_000,
            499_000,
        ))));
        assert!(mempool.add_transaction(young.clone()).unwrap());

        young.version = 1;
        let mempool = MempoolManager::new();
        mempool.set_chain_tip(next_height, 1_700_000_000);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
            vec![OP_1],
            2_000,
            499_995,
        ))));
        assert!(mempool.add_transaction(young).unwrap());

        let mut time_locked = legacy_empty_script();
        time_locked.version = 2;
        time_locked.inputs[0].sequence = (1 << 22) | 1;
        let mempool = MempoolManager::new();
        mempool.set_chain_tip(next_height, 1_700_000_000);
        mempool.set_utxo_set_arc(std::sync::Arc::new(tokio::sync::Mutex::new(funded_at(
            vec![OP_1],
            2_000,
            499_000,
        ))));
        assert!(!mempool.add_transaction(time_locked).unwrap());
    }

    #[test]
    fn mempool_rejects_a_non_final_locktime() {
        use blvm_protocol::constants::{LOCKTIME_THRESHOLD, SEQUENCE_FINAL};

        let not_final = (SEQUENCE_FINAL as u64).saturating_sub(1);
        let mut locked = legacy_empty_script();
        locked.lock_time = 200;
        locked.inputs[0].sequence = not_final;

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(100, 0);
        assert!(!mempool.add_transaction(locked.clone()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(100, 0);
        locked.inputs[0].sequence = SEQUENCE_FINAL as u64;
        assert!(mempool.add_transaction(locked).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(100, 0);
        let mut height_final = legacy_empty_script();
        height_final.lock_time = 50;
        height_final.inputs[0].sequence = not_final;
        assert!(mempool.add_transaction(height_final).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(100, 0);
        let mut time_locked = legacy_empty_script();
        time_locked.lock_time = LOCKTIME_THRESHOLD as u64;
        time_locked.inputs[0].sequence = not_final;
        assert!(!mempool.add_transaction(time_locked.clone()).unwrap());

        let mempool = MempoolManager::new();
        mempool.set_chain_tip(100, (LOCKTIME_THRESHOLD as u64).saturating_add(1));
        assert!(mempool.add_transaction(time_locked).unwrap());
    }

    #[test]
    fn empty_witness_stacks_use_stripped_weight() {
        let mempool = MempoolManager::new();
        let tx = legacy_empty_script();
        let stripped = (serialize_transaction(&tx).len() as u64).saturating_mul(4);
        assert_eq!(
            mempool.transaction_weight(&tx, Some(&[Vec::new()])),
            stripped
        );
        assert!(mempool.estimate_transaction_weight(&tx) > stripped);
    }

    #[test]
    fn fee_index_ignores_a_negative_output() {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::constants::SEQUENCE_FINAL;
        use blvm_protocol::opcodes::OP_1;
        use std::sync::Arc;

        let mempool = MempoolManager::new();
        let funding = OutPoint {
            hash: [1u8; 32],
            index: 0,
        };
        let negative_prevout = OutPoint {
            hash: [2u8; 32],
            index: 0,
        };
        let mut utxo_set = blvm_protocol::UtxoSet::default();
        for prevout in [funding, negative_prevout] {
            utxo_set.insert(
                prevout,
                Arc::new(blvm_protocol::UTXO {
                    value: 50_000,
                    script_pubkey: vec![OP_1].into(),
                    height: 0,
                    is_coinbase: false,
                }),
            );
        }

        let positive = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: funding,
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 40_000,
                script_pubkey: vec![OP_1],
            }]
            .into(),
            lock_time: 0,
        };
        assert!(mempool.add_transaction(positive.clone()).unwrap());

        let negative = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: negative_prevout,
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL as u64,
            }]
            .into(),
            outputs: vec![
                TransactionOutput {
                    value: 1_000,
                    script_pubkey: vec![OP_1],
                },
                TransactionOutput {
                    value: -1,
                    script_pubkey: vec![OP_1],
                },
            ]
            .into(),
            lock_time: 0,
        };
        let negative_id = calculate_tx_id(&negative);
        mempool
            .pool_lock()
            .transactions
            .insert(negative_id, negative);

        mempool.fee_rates_sat_vb(&utxo_set);
        let rates = mempool.fee_cache.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(rates.get(&negative_id).copied(), Some(0));
        assert!(rates.get(&calculate_tx_id(&positive)).copied().unwrap() > 0);
    }
}

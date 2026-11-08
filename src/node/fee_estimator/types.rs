//! Values the fee estimator exchanges with the mempool and its callers.
//!
//! The table keeps history, mempool, and combined columns separate. This phase
//! fills the mempool column and the combined column from it. History stays empty
//! until that layer exists.

use anyhow::Result;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, Hash};
use std::time::Instant;

/// Highest confirmation target.
///
/// Bitcoin Core `CBlockPolicyEstimator::LONG_BLOCK_PERIODS` (42) times
/// `LONG_SCALE` (24).
pub const MAX_FEE_TARGET: u32 = 1008;

/// Which percentile pair a quote uses. `unset` is conservative at the RPC edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeMode {
    Conservative,
    Economical,
}

impl FeeMode {
    pub(crate) fn index(self) -> usize {
        match self {
            FeeMode::Conservative => 0,
            FeeMode::Economical => 1,
        }
    }
}

/// Which published column `quote` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceView {
    Combined,
    History,
    Mempool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeSource {
    History,
    Mempool,
    /// Both layers had a value. The published rate is the lower one.
    Min,
    /// A module table replaced the native quote.
    External,
    /// A module table lowered the native quote.
    ExternalMin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeQuote {
    pub sat_per_kvb: u64,
    pub blocks: u32,
    pub source: FeeSource,
    /// `native` for the built-in estimator. A module quote uses its registered name.
    pub estimator: String,
}

pub(crate) fn native_quote(sat_per_kvb: u64, blocks: u32, source: FeeSource) -> FeeQuote {
    FeeQuote {
        sat_per_kvb,
        blocks,
        source,
        estimator: "native".to_string(),
    }
}

/// A history estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryResult {
    pub sat_per_kvb: u64,
    pub returned_target: u32,
}

/// `blocks` is the requested target, unless a history-only quote carries a
/// larger `returned_target`.
pub(crate) fn quote_blocks(requested: u32, source: FeeSource, returned_target: u32) -> u32 {
    if source == FeeSource::History && returned_target > requested {
        returned_target
    } else {
        requested
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeUnavailable {
    Disabled,
    WarmingUp,
    MempoolUnhealthy,
    EmptyMempool,
    /// The selected external estimator is missing or stale and `fallback = "none"`.
    ExternalStale,
}

/// How a module table combines with the native quote. The merge itself is a later step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalMode {
    LowerOnly,
    Replace,
}

/// Design 20.5 and 20.4. At most this many quotes on a submit, or targets on a read.
pub const MAX_EXTERNAL_QUOTES: usize = 64;

/// Design 20.3. Description longer than this is rejected.
pub const MAX_ESTIMATOR_DESCRIPTION_BYTES: usize = 120;

/// A name a module wants to publish quotes under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeEstimatorSpec {
    pub name: String,
    pub description: String,
    pub modes: Vec<FeeMode>,
    pub default_merge: ExternalMode,
    pub auto_priority: Option<u32>,
}

/// One pushed table. `module_id` is not here. The session supplies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalFeeTable {
    pub estimator: String,
    pub tip_hash: Hash,
    pub upstream_height: u64,
    pub mode: Option<FeeMode>,
    pub quotes: Vec<(u32, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeNameError {
    Reserved,
    BadFormat,
    Taken,
    DescriptionTooLong,
    EmptyModes,
    BadMerge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeEstimatorRegistration {
    pub accepted: Vec<String>,
    pub rejected: Vec<(String, FeeNameError)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitFeeTableError {
    NotOwner,
    UnknownEstimator,
    EmptyQuotes,
    TooManyQuotes,
    BadTarget,
    TargetsNotIncreasing,
    RateTooLow,
    RateTooHigh,
    StaleTip,
    HeightLag,
    RateLimited,
    BadMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeFeeRead {
    TooManyTargets,
    Unavailable(FeeUnavailable),
}

/// Which estimator `estimator = ...` resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EstimatorSelection {
    pub requested: String,
    pub active: String,
    pub degraded: bool,
    pub unresolved: bool,
}

/// One published snapshot. Index `target - 1` into a column. Mode 0 is conservative.
#[derive(Clone, Debug)]
pub struct FeeTable {
    pub tip_hash: Hash,
    pub tip_height: u64,
    pub block_seq: u64,
    pub generation: u64,
    pub generated_at: Instant,
    pub mempool_healthy: bool,
    /// Configured `health_window_blocks`.
    pub health_window: u64,
    /// Aggregate found/total over the blocks currently in the ring.
    pub health_ratio: f64,
    /// Configured `health_min_ratio`.
    pub health_required_ratio: f64,
    pub(crate) window_full: bool,
    pub(crate) pool_empty: bool,
    pub(crate) history: [Vec<Option<HistoryResult>>; 2],
    pub(crate) mempool: [Vec<Option<u64>>; 2],
    pub(crate) combined: [Vec<Option<FeeQuote>>; 2],
    /// Config `estimator` at the time this table was built.
    pub requested_estimator: String,
    /// `native` or the registered name whose table was merged.
    pub active_estimator: String,
    /// The resolved name was missing or stale and the table is the native one.
    pub degraded: bool,
}

/// One row of `listfeeestimators`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeEstimatorRow {
    pub name: String,
    pub module: Option<String>,
    pub description: String,
    pub resolved: bool,
    pub fresh: bool,
    pub last_submit_age_secs: Option<u64>,
    pub active: bool,
}

impl FeeTable {
    pub fn quote(
        &self,
        target: u32,
        mode: FeeMode,
        view: SourceView,
    ) -> std::result::Result<FeeQuote, FeeUnavailable> {
        let Some(index) = target.checked_sub(1).map(|index| index as usize) else {
            return Err(self.missing());
        };
        if index >= self.combined[0].len() {
            return Err(self.missing());
        }
        let mode = mode.index();
        match view {
            SourceView::Combined => self.combined[mode][index]
                .clone()
                .ok_or_else(|| self.missing()),
            SourceView::Mempool => self.mempool[mode][index]
                .map(|sat_per_kvb| native_quote(sat_per_kvb, target, FeeSource::Mempool))
                .ok_or_else(|| self.missing()),
            SourceView::History => self.history[mode][index]
                .map(|result| {
                    native_quote(
                        result.sat_per_kvb,
                        quote_blocks(target, FeeSource::History, result.returned_target),
                        FeeSource::History,
                    )
                })
                .ok_or_else(|| self.missing()),
        }
    }

    fn missing(&self) -> FeeUnavailable {
        if !self.mempool_healthy {
            if self.window_full {
                FeeUnavailable::MempoolUnhealthy
            } else {
                FeeUnavailable::WarmingUp
            }
        } else if self.pool_empty {
            FeeUnavailable::EmptyMempool
        } else {
            FeeUnavailable::WarmingUp
        }
    }
}

/// One transaction as the mempool recorded it at admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxAdmission {
    pub txid: Hash,
    pub fee_sats: u64,
    /// Sigop-adjusted virtual size stored on the pool entry.
    pub vsize: u64,
    /// Chain tip height at admission.
    pub admit_height: u64,
    /// In-pool parents after the dependency graph update.
    pub parents: Vec<Hash>,
    /// Restored from `mempool.dat`. Kept for projection, excluded from history.
    pub from_reload: bool,
    /// Admitted through the package entry point.
    pub from_package: bool,
}

/// A connected block, seen before its transactions leave the mempool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockObservation {
    pub height: u64,
    pub hash: Hash,
    /// Full block weight, including the coinbase.
    pub weight: u64,
    /// Non-coinbase transactions and the weight of each, from its witness stacks.
    pub tx_weights: Vec<(Hash, u64)>,
}

impl BlockObservation {
    /// Build an observation from the block that was just connected.
    ///
    /// `witnesses` is one list of input stacks per transaction, coinbase included,
    /// matching `MempoolManager::apply_reorg`.
    pub fn from_block(
        block: &Block,
        witnesses: &[Vec<Witness>],
        height: u64,
        hash: Hash,
    ) -> Result<Self> {
        use blvm_protocol::block::calculate_tx_id;
        use blvm_protocol::segwit::{
            calculate_block_weight_from_nested, transaction_weight_from_stacks,
        };
        use blvm_protocol::transaction::is_coinbase;

        let weight = calculate_block_weight_from_nested(block, witnesses)?;
        let mut tx_weights = Vec::new();
        for (index, tx) in block.transactions.iter().enumerate() {
            if is_coinbase(tx) {
                continue;
            }
            let stacks = witnesses.get(index).map(Vec::as_slice);
            let tx_weight = transaction_weight_from_stacks(tx, stacks)?;
            tx_weights.push((calculate_tx_id(tx), tx_weight));
        }
        Ok(Self {
            height,
            hash,
            weight,
            tx_weights,
        })
    }
}

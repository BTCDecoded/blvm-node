//! Confirmation history ported from Bitcoin Core `TxConfirmStats` and
//! `CBlockPolicyEstimator::estimateSmartFee`.
//!
//! Bucket edges and horizon constants are the Core symbols named below. The
//! minimum bucket is Core's fixed `MIN_BUCKET_FEERATE`, not the relay floor.

use super::{FeeMode, HistoryResult};
use blvm_protocol::Hash;
use std::collections::HashMap;

/// `CBlockPolicyEstimator::SHORT_BLOCK_PERIODS`.
pub(crate) const SHORT_BLOCK_PERIODS: usize = 12;
/// `CBlockPolicyEstimator::SHORT_SCALE`.
pub(crate) const SHORT_SCALE: u64 = 1;
/// `CBlockPolicyEstimator::MED_BLOCK_PERIODS`.
pub(crate) const MED_BLOCK_PERIODS: usize = 24;
/// `CBlockPolicyEstimator::MED_SCALE`.
pub(crate) const MED_SCALE: u64 = 2;
/// `CBlockPolicyEstimator::LONG_BLOCK_PERIODS`.
pub(crate) const LONG_BLOCK_PERIODS: usize = 42;
/// `CBlockPolicyEstimator::LONG_SCALE`.
pub(crate) const LONG_SCALE: u64 = 24;
/// `CBlockPolicyEstimator::SHORT_DECAY`.
pub(crate) const SHORT_DECAY: f64 = 0.962;
/// `CBlockPolicyEstimator::MED_DECAY`.
pub(crate) const MED_DECAY: f64 = 0.9952;
/// `CBlockPolicyEstimator::LONG_DECAY`.
pub(crate) const LONG_DECAY: f64 = 0.99931;
/// `CBlockPolicyEstimator::HALF_SUCCESS_PCT`.
const HALF_SUCCESS_PCT: f64 = 0.6;
/// `CBlockPolicyEstimator::SUCCESS_PCT`.
const SUCCESS_PCT: f64 = 0.85;
/// `CBlockPolicyEstimator::DOUBLE_SUCCESS_PCT`.
const DOUBLE_SUCCESS_PCT: f64 = 0.95;
/// `CBlockPolicyEstimator::SUFFICIENT_FEETXS`.
const SUFFICIENT_FEETXS: f64 = 0.1;
/// `CBlockPolicyEstimator::SUFFICIENT_TXS_SHORT`.
const SUFFICIENT_TXS_SHORT: f64 = 0.5;
/// `CBlockPolicyEstimator::OLDEST_ESTIMATE_HISTORY`.
const OLDEST_ESTIMATE_HISTORY: u64 = 6 * 1008;
/// `CBlockPolicyEstimator::MIN_BUCKET_FEERATE`.
pub(crate) const MIN_BUCKET_FEERATE: f64 = 100.0;
/// `CBlockPolicyEstimator::MAX_BUCKET_FEERATE`.
pub(crate) const MAX_BUCKET_FEERATE: f64 = 1e7;
/// `CBlockPolicyEstimator::FEE_SPACING`.
pub(crate) const FEE_SPACING: f64 = 1.05;
/// File-local `INF_FEERATE` in `block_policy_estimator.cpp`.
pub(crate) const INF_FEERATE: f64 = 1e99;

/// Satoshis per 1000 virtual bytes. Core buckets `CFeeRate::GetFeePerK` the same way.
pub(crate) fn fee_sat_per_kvb(fee_sats: u64, vsize: u64) -> Option<u64> {
    if vsize == 0 {
        return None;
    }
    let rate = u128::from(fee_sats).saturating_mul(1000) / u128::from(vsize);
    Some(u64::try_from(rate).unwrap_or(u64::MAX))
}

fn core_buckets() -> Vec<f64> {
    let mut boundary = MIN_BUCKET_FEERATE;
    let mut buckets = Vec::new();
    while boundary <= MAX_BUCKET_FEERATE {
        buckets.push(boundary);
        boundary *= FEE_SPACING;
    }
    buckets.push(INF_FEERATE);
    buckets
}

/// FNV-1a over each bucket edge's `to_bits`. A changed `MIN_BUCKET_FEERATE`,
/// `FEE_SPACING`, `MAX_BUCKET_FEERATE`, or `INF_FEERATE` changes the hash.
fn bucket_params_hash(buckets: &[f64]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for edge in buckets {
        for byte in edge.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

fn bucket_index(buckets: &[f64], feerate: f64) -> usize {
    buckets
        .iter()
        .position(|bound| *bound >= feerate)
        .unwrap_or(buckets.len().saturating_sub(1))
}

struct SeenTx {
    height: u64,
    bucket: usize,
    fee_sat_per_kvb: u64,
}

/// One horizon of `TxConfirmStats`.
struct TxConfirmStats {
    decay: f64,
    scale: u64,
    max_periods: usize,
    /// `confAvg[period][bucket]`.
    conf_avg: Vec<Vec<f64>>,
    /// `failAvg[period][bucket]`.
    fail_avg: Vec<Vec<f64>>,
    /// `txCtAvg[bucket]`.
    tx_ct_avg: Vec<f64>,
    /// `m_feerate_avg[bucket]`.
    feerate_avg: Vec<f64>,
    /// `unconfTxs[height % bins][bucket]`.
    unconf_txs: Vec<Vec<i64>>,
    /// `oldUnconfTxs[bucket]`.
    old_unconf_txs: Vec<i64>,
}

impl TxConfirmStats {
    fn new(bucket_count: usize, max_periods: usize, decay: f64, scale: u64) -> Self {
        let row = vec![0.0; bucket_count];
        let max_confirms = scale as usize * max_periods;
        Self {
            decay,
            scale,
            max_periods,
            conf_avg: vec![row.clone(); max_periods],
            fail_avg: vec![row.clone(); max_periods],
            tx_ct_avg: row.clone(),
            feerate_avg: row,
            unconf_txs: vec![vec![0; bucket_count]; max_confirms],
            old_unconf_txs: vec![0; bucket_count],
        }
    }

    fn max_confirms(&self) -> u64 {
        self.scale * self.max_periods as u64
    }

    /// `TxConfirmStats::ClearCurrent`.
    fn clear_current(&mut self, height: u64) {
        let bins = self.unconf_txs.len();
        if bins == 0 {
            return;
        }
        let slot = (height % bins as u64) as usize;
        for (bucket, old) in self.old_unconf_txs.iter_mut().enumerate() {
            *old += self.unconf_txs[slot][bucket];
            self.unconf_txs[slot][bucket] = 0;
        }
    }

    /// `TxConfirmStats::UpdateMovingAverages`.
    fn decay_averages(&mut self) {
        for bucket in 0..self.tx_ct_avg.len() {
            for period in 0..self.max_periods {
                self.conf_avg[period][bucket] *= self.decay;
                self.fail_avg[period][bucket] *= self.decay;
            }
            self.feerate_avg[bucket] *= self.decay;
            self.tx_ct_avg[bucket] *= self.decay;
        }
    }

    /// `TxConfirmStats::Record`. `blocks_to_confirm` is 1-based.
    fn record(&mut self, blocks_to_confirm: u64, feerate: f64, bucket: usize) {
        if blocks_to_confirm < 1 {
            return;
        }
        let periods = (blocks_to_confirm + self.scale - 1) / self.scale;
        let mut period = periods;
        while period <= self.max_periods as u64 {
            self.conf_avg[(period - 1) as usize][bucket] += 1.0;
            period += 1;
        }
        self.tx_ct_avg[bucket] += 1.0;
        self.feerate_avg[bucket] += feerate;
    }

    /// `TxConfirmStats::NewTx`.
    fn new_tx(&mut self, height: u64, bucket: usize) {
        let bins = self.unconf_txs.len();
        if bins == 0 {
            return;
        }
        let slot = (height % bins as u64) as usize;
        self.unconf_txs[slot][bucket] += 1;
    }

    /// `TxConfirmStats::removeTx`.
    ///
    /// A failure is recorded only when `!in_block && blocks_ago >= scale`.
    fn remove_tx(&mut self, entry_height: u64, best_seen: u64, bucket: usize, in_block: bool) {
        let blocks_ago = if best_seen == 0 {
            0
        } else if best_seen < entry_height {
            return;
        } else {
            best_seen - entry_height
        };
        let bins = self.unconf_txs.len() as u64;
        if bins == 0 {
            return;
        }
        if blocks_ago >= bins {
            if self.old_unconf_txs[bucket] > 0 {
                self.old_unconf_txs[bucket] -= 1;
            }
        } else {
            let slot = (entry_height % bins) as usize;
            if self.unconf_txs[slot][bucket] > 0 {
                self.unconf_txs[slot][bucket] -= 1;
            }
        }
        if !in_block && blocks_ago >= self.scale {
            let periods_ago = blocks_ago / self.scale;
            for period in 0..self.max_periods {
                if period as u64 >= periods_ago {
                    break;
                }
                self.fail_avg[period][bucket] += 1.0;
            }
        }
    }

    /// `TxConfirmStats::EstimateMedianVal`. Returns Core's `-1` when no bucket passes.
    fn estimate_median(
        &self,
        conf_target: u64,
        sufficient_tx: f64,
        success: f64,
        height: u64,
    ) -> f64 {
        if conf_target == 0 || self.scale == 0 || self.tx_ct_avg.is_empty() {
            return -1.0;
        }
        let period_target = (conf_target + self.scale - 1) / self.scale;
        if period_target == 0 || period_target > self.max_periods as u64 {
            return -1.0;
        }
        let period = (period_target - 1) as usize;
        let max_bucket = self.tx_ct_avg.len() - 1;
        let mut cur_near = max_bucket;
        let mut best_near = max_bucket;
        let mut cur_far;
        let mut best_far = max_bucket;
        let mut n_conf = 0.0;
        let mut total_num = 0.0;
        let mut extra_num: i64 = 0;
        let mut fail_num = 0.0;
        let mut partial_num = 0.0;
        let mut found = false;
        let mut new_range = true;
        let bins = self.unconf_txs.len() as u64;
        let enough = sufficient_tx / (1.0 - self.decay);

        for bucket in (0..=max_bucket).rev() {
            if new_range {
                cur_near = bucket;
                new_range = false;
            }
            cur_far = bucket;
            n_conf += self.conf_avg[period][bucket];
            partial_num += self.tx_ct_avg[bucket];
            total_num += self.tx_ct_avg[bucket];
            fail_num += self.fail_avg[period][bucket];
            if bins > 0 {
                let mut age = conf_target;
                let max_confirms = self.max_confirms();
                while age < max_confirms {
                    let slot = (height.wrapping_sub(age) % bins) as usize;
                    extra_num += self.unconf_txs[slot][bucket];
                    age += 1;
                }
            }
            extra_num += self.old_unconf_txs[bucket];
            if partial_num < enough {
                continue;
            }
            partial_num = 0.0;
            let denominator = total_num + fail_num + extra_num as f64;
            let cur_pct = if denominator == 0.0 {
                0.0
            } else {
                n_conf / denominator
            };
            if cur_pct < success {
                continue;
            }
            found = true;
            n_conf = 0.0;
            total_num = 0.0;
            fail_num = 0.0;
            extra_num = 0;
            best_near = cur_near;
            best_far = cur_far;
            new_range = true;
        }

        if !found {
            return -1.0;
        }
        let min_bucket = best_near.min(best_far);
        let max_pass = best_near.max(best_far);
        let mut tx_sum: f64 = (min_bucket..=max_pass)
            .map(|bucket| self.tx_ct_avg[bucket])
            .sum();
        if tx_sum == 0.0 {
            return -1.0;
        }
        tx_sum /= 2.0;
        for bucket in min_bucket..=max_pass {
            if self.tx_ct_avg[bucket] < tx_sum {
                tx_sum -= self.tx_ct_avg[bucket];
            } else {
                return self.feerate_avg[bucket] / self.tx_ct_avg[bucket];
            }
        }
        -1.0
    }

    fn export(&self) -> ExportedHorizon {
        ExportedHorizon {
            decay: self.decay,
            scale: self.scale,
            conf_avg: self.conf_avg.clone(),
            fail_avg: self.fail_avg.clone(),
            tx_ct_avg: self.tx_ct_avg.clone(),
            feerate_avg: self.feerate_avg.clone(),
        }
    }

    fn same_shape(&self, saved: &ExportedHorizon) -> bool {
        saved.decay.to_bits() == self.decay.to_bits()
            && saved.scale == self.scale
            && saved.conf_avg.len() == self.conf_avg.len()
            && saved.fail_avg.len() == self.fail_avg.len()
            && saved.tx_ct_avg.len() == self.tx_ct_avg.len()
            && saved.feerate_avg.len() == self.feerate_avg.len()
            && saved
                .conf_avg
                .iter()
                .chain(saved.fail_avg.iter())
                .all(|row| row.len() == self.tx_ct_avg.len())
    }

    fn import_averages(&mut self, saved: &ExportedHorizon) {
        self.conf_avg = saved.conf_avg.clone();
        self.fail_avg = saved.fail_avg.clone();
        self.tx_ct_avg = saved.tx_ct_avg.clone();
        self.feerate_avg = saved.feerate_avg.clone();
    }
}

/// One horizon of the decayed averages. Unconfirmed counters are not stored.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExportedHorizon {
    pub decay: f64,
    pub scale: u64,
    pub conf_avg: Vec<Vec<f64>>,
    pub fail_avg: Vec<Vec<f64>>,
    pub tx_ct_avg: Vec<f64>,
    pub feerate_avg: Vec<f64>,
}

/// Averages and the recorded span. Not the in-memory unconfirmed map.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExportedHistory {
    pub bucket_params_hash: u64,
    pub short: ExportedHorizon,
    pub medium: ExportedHorizon,
    pub long: ExportedHorizon,
    pub first_recorded_height: u64,
    pub historical_first: u64,
    pub historical_best: u64,
}

/// Three horizons and the unconfirmed map from `CBlockPolicyEstimator`.
pub(crate) struct HistoryTracker {
    buckets: Vec<f64>,
    short: TxConfirmStats,
    medium: TxConfirmStats,
    long: TxConfirmStats,
    seen: HashMap<Hash, SeenTx>,
    first_recorded_height: u64,
    historical_first: u64,
    historical_best: u64,
}

impl HistoryTracker {
    pub(crate) fn new() -> Self {
        let buckets = core_buckets();
        let count = buckets.len();
        Self {
            short: TxConfirmStats::new(count, SHORT_BLOCK_PERIODS, SHORT_DECAY, SHORT_SCALE),
            medium: TxConfirmStats::new(count, MED_BLOCK_PERIODS, MED_DECAY, MED_SCALE),
            long: TxConfirmStats::new(count, LONG_BLOCK_PERIODS, LONG_DECAY, LONG_SCALE),
            buckets,
            seen: HashMap::new(),
            first_recorded_height: 0,
            historical_first: 0,
            historical_best: 0,
        }
    }

    /// Saved bucket edges are usable only when they are this tracker's edges.
    pub(crate) fn buckets_match(&self, saved: &[f64]) -> bool {
        saved.len() == self.buckets.len()
            && saved
                .iter()
                .zip(&self.buckets)
                .all(|(saved, live)| saved.to_bits() == live.to_bits())
    }

    pub(crate) fn bucket_edges(&self) -> &[f64] {
        &self.buckets
    }

    /// `ClearCurrent` then `UpdateMovingAverages` on every horizon.
    ///
    /// Called after `best_seen_height` is set to this block and before confirmations
    /// are recorded, matching `CBlockPolicyEstimator::processBlock`.
    pub(crate) fn begin_block(&mut self, height: u64) {
        self.short.clear_current(height);
        self.medium.clear_current(height);
        self.long.clear_current(height);
        self.short.decay_averages();
        self.medium.decay_averages();
        self.long.decay_averages();
    }

    /// `processTransaction` for a history-eligible transaction.
    ///
    /// Core returns immediately when `txHeight != nBestSeenHeight`. A best height
    /// that has not been set yet is `0`, which is Core's initial `nBestSeenHeight`.
    pub(crate) fn admit(
        &mut self,
        txid: Hash,
        height: u64,
        fee_sat_per_kvb: u64,
        best_seen: Option<u64>,
    ) {
        if self.seen.contains_key(&txid) {
            return;
        }
        let best = best_seen.unwrap_or(0);
        if height != best {
            return;
        }
        let bucket = bucket_index(&self.buckets, fee_sat_per_kvb as f64);
        self.short.new_tx(height, bucket);
        self.medium.new_tx(height, bucket);
        self.long.new_tx(height, bucket);
        self.seen.insert(
            txid,
            SeenTx {
                height,
                bucket,
                fee_sat_per_kvb,
            },
        );
    }

    /// `processBlockTx`: in-block `removeTx`, then `Record` when `blocksToConfirm >= 1`.
    pub(crate) fn confirm(&mut self, txid: Hash, blocks_to_confirm: u64, height: u64) -> bool {
        let Some(seen) = self.seen.remove(&txid) else {
            return false;
        };
        let fee = seen.fee_sat_per_kvb as f64;
        self.short.remove_tx(seen.height, height, seen.bucket, true);
        self.medium
            .remove_tx(seen.height, height, seen.bucket, true);
        self.long.remove_tx(seen.height, height, seen.bucket, true);
        if blocks_to_confirm < 1 {
            return false;
        }
        self.short.record(blocks_to_confirm, fee, seen.bucket);
        self.medium.record(blocks_to_confirm, fee, seen.bucket);
        self.long.record(blocks_to_confirm, fee, seen.bucket);
        if self.first_recorded_height == 0 {
            self.first_recorded_height = height;
        }
        true
    }

    /// `_removeTx(..., inBlock = false)`.
    pub(crate) fn remove(&mut self, txid: &Hash, best_seen: Option<u64>) {
        let Some(seen) = self.seen.remove(txid) else {
            return;
        };
        let best = best_seen.unwrap_or(0);
        self.short.remove_tx(seen.height, best, seen.bucket, false);
        self.medium.remove_tx(seen.height, best, seen.bucket, false);
        self.long.remove_tx(seen.height, best, seen.bucket, false);
    }

    /// Drop every unconfirmed entry without incrementing `failAvg`.
    ///
    /// `in_block` is set so `TxConfirmStats::removeTx` skips the failure arm.
    /// The age still selects the unconfirmed bin.
    pub(crate) fn discard_without_failures(&mut self, best_seen: Option<u64>) {
        let best = best_seen.unwrap_or(0);
        let pending: Vec<(Hash, SeenTx)> = self.seen.drain().collect();
        for (_, seen) in pending {
            self.short.remove_tx(seen.height, best, seen.bucket, true);
            self.medium.remove_tx(seen.height, best, seen.bucket, true);
            self.long.remove_tx(seen.height, best, seen.bucket, true);
        }
    }

    /// `estimateSmartFee`. `Conservative` passes Core's `conservative` flag.
    pub(crate) fn estimate(
        &self,
        target: u32,
        mode: FeeMode,
        best_seen: u64,
    ) -> Option<HistoryResult> {
        let conservative = mode == FeeMode::Conservative;
        let long_max = self.long.max_confirms();
        if target == 0 || u64::from(target) > long_max {
            return None;
        }
        let mut conf_target = u64::from(target);
        if conf_target == 1 {
            conf_target = 2;
        }
        let max_usable = self.max_usable(best_seen);
        if conf_target > max_usable {
            conf_target = max_usable;
        }
        if conf_target <= 1 {
            return None;
        }
        let half = self.estimate_combined(conf_target / 2, HALF_SUCCESS_PCT, true, best_seen);
        let mut median = half;
        let full = self.estimate_combined(conf_target, SUCCESS_PCT, true, best_seen);
        if full > median {
            median = full;
        }
        let double = self.estimate_combined(
            conf_target.saturating_mul(2),
            DOUBLE_SUCCESS_PCT,
            !conservative,
            best_seen,
        );
        if double > median {
            median = double;
        }
        if conservative || median < 0.0 {
            let conservative_est =
                self.estimate_conservative(conf_target.saturating_mul(2), best_seen);
            if conservative_est > median {
                median = conservative_est;
            }
        }
        if median < 0.0 {
            return None;
        }
        Some(HistoryResult {
            sat_per_kvb: median.round().max(0.0) as u64,
            returned_target: u32::try_from(conf_target).unwrap_or(u32::MAX),
        })
    }

    fn estimate_combined(
        &self,
        conf_target: u64,
        success: f64,
        check_shorter: bool,
        height: u64,
    ) -> f64 {
        if conf_target < 1 || conf_target > self.long.max_confirms() {
            return -1.0;
        }
        let mut estimate = if conf_target <= self.short.max_confirms() {
            self.short
                .estimate_median(conf_target, SUFFICIENT_TXS_SHORT, success, height)
        } else if conf_target <= self.medium.max_confirms() {
            self.medium
                .estimate_median(conf_target, SUFFICIENT_FEETXS, success, height)
        } else {
            self.long
                .estimate_median(conf_target, SUFFICIENT_FEETXS, success, height)
        };
        if check_shorter {
            if conf_target > self.medium.max_confirms() {
                let med_max = self.medium.estimate_median(
                    self.medium.max_confirms(),
                    SUFFICIENT_FEETXS,
                    success,
                    height,
                );
                if med_max > 0.0 && (estimate < 0.0 || med_max < estimate) {
                    estimate = med_max;
                }
            }
            if conf_target > self.short.max_confirms() {
                let short_max = self.short.estimate_median(
                    self.short.max_confirms(),
                    SUFFICIENT_TXS_SHORT,
                    success,
                    height,
                );
                if short_max > 0.0 && (estimate < 0.0 || short_max < estimate) {
                    estimate = short_max;
                }
            }
        }
        estimate
    }

    /// `estimateConservativeFee`.
    fn estimate_conservative(&self, double_target: u64, height: u64) -> f64 {
        let mut estimate = -1.0;
        if double_target <= self.short.max_confirms() {
            estimate = self.medium.estimate_median(
                double_target,
                SUFFICIENT_FEETXS,
                DOUBLE_SUCCESS_PCT,
                height,
            );
        }
        if double_target <= self.medium.max_confirms() {
            let long_estimate = self.long.estimate_median(
                double_target,
                SUFFICIENT_FEETXS,
                DOUBLE_SUCCESS_PCT,
                height,
            );
            if long_estimate > estimate {
                estimate = long_estimate;
            }
        }
        estimate
    }

    fn block_span(&self, best_seen: u64) -> u64 {
        if self.first_recorded_height == 0 || best_seen < self.first_recorded_height {
            return 0;
        }
        best_seen - self.first_recorded_height
    }

    fn historical_block_span(&self, best_seen: u64) -> u64 {
        if self.historical_first == 0 || self.historical_best < self.historical_first {
            return 0;
        }
        if best_seen.saturating_sub(self.historical_best) > OLDEST_ESTIMATE_HISTORY {
            return 0;
        }
        self.historical_best - self.historical_first
    }

    fn max_usable(&self, best_seen: u64) -> u64 {
        self.long.max_confirms().min(
            self.block_span(best_seen)
                .max(self.historical_block_span(best_seen))
                / 2,
        )
    }

    pub(crate) fn export(&self) -> ExportedHistory {
        ExportedHistory {
            bucket_params_hash: bucket_params_hash(&self.buckets),
            short: self.short.export(),
            medium: self.medium.export(),
            long: self.long.export(),
            first_recorded_height: self.first_recorded_height,
            historical_first: self.historical_first,
            historical_best: self.historical_best,
        }
    }

    /// Install saved averages. Unconfirmed counters and the seen-tx map stay as
    /// they are: those are not part of the state file. Returns false when the
    /// bucket edges or a horizon's shape do not match this process.
    pub(crate) fn import(&mut self, saved: &ExportedHistory) -> bool {
        if saved.bucket_params_hash != bucket_params_hash(&self.buckets) {
            return false;
        }
        if !self.short.same_shape(&saved.short)
            || !self.medium.same_shape(&saved.medium)
            || !self.long.same_shape(&saved.long)
        {
            return false;
        }
        self.short.import_averages(&saved.short);
        self.medium.import_averages(&saved.medium);
        self.long.import_averages(&saved.long);
        self.first_recorded_height = saved.first_recorded_height;
        self.historical_first = saved.historical_first;
        self.historical_best = saved.historical_best;
        true
    }

    pub(crate) fn bucket_params_hash(&self) -> u64 {
        bucket_params_hash(&self.buckets)
    }

    #[cfg(test)]
    pub(crate) fn short_tx_count(&self) -> f64 {
        self.short.tx_ct_avg.iter().sum()
    }

    #[cfg(test)]
    fn short_fail_count(&self) -> f64 {
        self.short.fail_avg.iter().flatten().sum()
    }

    #[cfg(test)]
    fn medium_fail_count(&self) -> f64 {
        self.medium.fail_avg.iter().flatten().sum()
    }

    /// `TxConfirmStats::confAvg[period][bucket]` on the short horizon.
    #[cfg(test)]
    fn short_conf_at(&self, period: usize, feerate: f64) -> f64 {
        let bucket = bucket_index(&self.buckets, feerate);
        self.short.conf_avg[period][bucket]
    }

    /// `TxConfirmStats::EstimateMedianVal` on the short horizon at `SUCCESS_PCT`.
    #[cfg(test)]
    fn short_median(&self, target: u64, height: u64) -> f64 {
        self.short
            .estimate_median(target, SUFFICIENT_TXS_SHORT, SUCCESS_PCT, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_edges_follow_the_core_loop() {
        let tracker = HistoryTracker::new();
        let edges = tracker.bucket_edges();
        assert_eq!(edges[0], MIN_BUCKET_FEERATE);
        assert_eq!(edges[1], MIN_BUCKET_FEERATE * FEE_SPACING);
        assert!(edges[edges.len() - 2] <= MAX_BUCKET_FEERATE);
        assert!(edges[edges.len() - 2] * FEE_SPACING > MAX_BUCKET_FEERATE);
        assert_eq!(*edges.last().unwrap(), INF_FEERATE);
        assert!(tracker.buckets_match(edges));
        assert!(!tracker.buckets_match(&edges[..edges.len() - 1]));
    }

    #[test]
    fn a_confirmation_is_not_a_failure_and_decay_runs_before_the_next_record() {
        let mut tracker = HistoryTracker::new();
        let txid = [1u8; 32];
        let rate = 5_000;
        tracker.admit(txid, 0, rate, None);
        tracker.begin_block(1);
        assert!(tracker.confirm(txid, 1, 1));
        assert_eq!(tracker.short_fail_count(), 0.0);
        assert_eq!(tracker.short_tx_count(), 1.0);
        tracker.begin_block(2);
        let decayed = tracker.short_tx_count();
        assert!((decayed - SHORT_DECAY).abs() < 1e-12);
    }

    #[test]
    fn a_removal_records_a_failure_only_after_that_horizon_scale() {
        let mut tracker = HistoryTracker::new();
        let txid = [2u8; 32];
        tracker.admit(txid, 0, 5_000, None);
        tracker.begin_block(1);
        tracker.remove(&txid, Some(1));
        assert!(tracker.short_fail_count() > 0.0);
        assert_eq!(tracker.medium_fail_count(), 0.0);
    }

    #[test]
    fn clearing_the_pool_does_not_record_failures() {
        let mut tracker = HistoryTracker::new();
        tracker.admit([3u8; 32], 0, 5_000, None);
        tracker.begin_block(1);
        tracker.discard_without_failures(Some(1));
        assert_eq!(tracker.short_fail_count(), 0.0);
        assert_eq!(tracker.short_tx_count(), 0.0);
    }

    #[test]
    fn an_estimate_appears_once_one_bucket_has_enough_undecayed_confirms() {
        let mut tracker = HistoryTracker::new();
        let rate = 5_000u64;
        for index in 0..20u8 {
            let mut txid = [4u8; 32];
            txid[0] = index;
            tracker.admit(txid, 0, rate, None);
        }
        tracker.begin_block(1);
        for index in 0..20u8 {
            let mut txid = [4u8; 32];
            txid[0] = index;
            assert!(tracker.confirm(txid, 1, 1));
        }
        assert!(tracker.estimate(2, FeeMode::Economical, 1).is_none());
        for height in 2..=5 {
            tracker.begin_block(height);
        }
        let quote = tracker
            .estimate(2, FeeMode::Economical, 5)
            .expect("span and bucket are sufficient");
        assert_eq!(quote.sat_per_kvb, rate);
        assert_eq!(quote.returned_target, 2);
    }

    #[test]
    fn a_side_chain_height_is_not_tracked() {
        let mut tracker = HistoryTracker::new();
        tracker.admit([5u8; 32], 4, 5_000, Some(1));
        tracker.begin_block(2);
        assert!(!tracker.confirm([5u8; 32], 1, 2));
        assert_eq!(tracker.short_tx_count(), 0.0);
    }

    #[test]
    fn the_facade_records_an_eligible_confirmation() {
        use crate::config::FeeEstimatorConfig;
        use crate::node::fee_estimator::{BlockObservation, FeeEstimator, TxAdmission};

        let estimator = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        let txid = [9u8; 32];
        estimator.on_tx_admitted(TxAdmission {
            txid,
            fee_sats: 500,
            vsize: 100,
            admit_height: 0,
            parents: Vec::new(),
            from_reload: false,
            from_package: false,
        });
        estimator.on_block_connected(&BlockObservation {
            height: 1,
            hash: [1u8; 32],
            weight: 4_000,
            tx_weights: vec![(txid, 400)],
        });
        assert_eq!(estimator.history_short_tx_count(), 1.0);
        assert_eq!(estimator.hook_counts().confirmed, 1);
        assert_eq!(estimator.hook_counts().failed, 0);
    }

    fn txid(tag: u8, index: u8) -> Hash {
        let mut id = [tag; 32];
        id[0] = index;
        id
    }

    fn admit_group(
        tracker: &mut HistoryTracker,
        tag: u8,
        count: u8,
        height: u64,
        rate: u64,
        best: Option<u64>,
    ) {
        for index in 0..count {
            tracker.admit(txid(tag, index), height, rate, best);
        }
    }

    fn confirm_group(tracker: &mut HistoryTracker, tag: u8, count: u8, blocks: u64, height: u64) {
        for index in 0..count {
            assert!(tracker.confirm(txid(tag, index), blocks, height));
        }
    }

    #[test]
    fn a_slow_confirmation_does_not_fill_earlier_periods_and_decay_compounds() {
        let mut tracker = HistoryTracker::new();
        let rate = 5_000.0;
        tracker.admit(txid(1, 0), 0, 5_000, None);
        tracker.begin_block(1);
        assert!(tracker.confirm(txid(1, 0), 1, 1));
        assert!((tracker.short_conf_at(0, rate) - 1.0).abs() < 1e-12);
        assert!((tracker.short_conf_at(11, rate) - 1.0).abs() < 1e-12);

        tracker.begin_block(2);
        let once = SHORT_DECAY;
        assert!((tracker.short_conf_at(0, rate) - once).abs() < 1e-12);

        tracker.admit(txid(1, 1), 2, 5_000, Some(2));
        tracker.begin_block(3);
        tracker.begin_block(4);
        tracker.begin_block(5);
        let decayed = once * SHORT_DECAY.powi(3);
        assert!(tracker.confirm(txid(1, 1), 3, 5));
        assert!((tracker.short_conf_at(0, rate) - decayed).abs() < 1e-9);
        assert!((tracker.short_conf_at(1, rate) - decayed).abs() < 1e-9);
        assert!((tracker.short_conf_at(2, rate) - (decayed + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn short_horizon_estimates_fall_as_the_target_grows() {
        let mut tracker = HistoryTracker::new();
        let high = 20_000.0;
        let low = 1_000.0;
        admit_group(&mut tracker, 2, 40, 0, high as u64, None);
        admit_group(&mut tracker, 3, 40, 0, low as u64, None);
        tracker.begin_block(1);
        confirm_group(&mut tracker, 2, 40, 1, 1);
        for height in 2..=6 {
            tracker.begin_block(height);
        }
        confirm_group(&mut tracker, 3, 40, 6, 6);

        let fast = tracker.short_median(2, 6);
        let slow = tracker.short_median(6, 6);
        assert!((fast - high).abs() < 1e-6);
        assert!((slow - low).abs() < 1e-6);

        let mut previous = f64::MAX;
        for target in 2..=SHORT_BLOCK_PERIODS as u64 {
            let median = tracker.short_median(target, 6);
            if median >= 0.0 {
                assert!(median <= previous + 1e-6);
                previous = median;
            }
        }
    }

    #[test]
    fn a_failure_in_one_bucket_moves_the_estimate_off_that_bucket() {
        let mut tracker = HistoryTracker::new();
        let high = 20_000.0;
        let low = 1_000.0;
        admit_group(&mut tracker, 4, 20, 0, high as u64, None);
        admit_group(&mut tracker, 5, 20, 0, low as u64, None);
        tracker.begin_block(1);
        confirm_group(&mut tracker, 4, 20, 1, 1);
        confirm_group(&mut tracker, 5, 20, 1, 1);
        assert!((tracker.short_median(2, 1) - low).abs() < 1e-6);

        admit_group(&mut tracker, 6, 6, 1, low as u64, Some(1));
        tracker.begin_block(2);
        tracker.begin_block(3);
        for index in 0..6u8 {
            tracker.remove(&txid(6, index), Some(3));
        }
        assert!((tracker.short_median(2, 3) - high).abs() < 1e-6);
        assert!(tracker.short_fail_count() > 0.0);
    }

    #[test]
    fn reload_package_and_parent_transactions_never_enter_history() {
        use crate::config::FeeEstimatorConfig;
        use crate::node::fee_estimator::{BlockObservation, FeeEstimator, TxAdmission};

        let estimator = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        let reload = txid(7, 1);
        let package = txid(7, 2);
        let child = txid(7, 3);
        let eligible = txid(7, 4);
        for admission in [
            TxAdmission {
                txid: reload,
                fee_sats: 500,
                vsize: 100,
                admit_height: 0,
                parents: Vec::new(),
                from_reload: true,
                from_package: false,
            },
            TxAdmission {
                txid: package,
                fee_sats: 500,
                vsize: 100,
                admit_height: 0,
                parents: Vec::new(),
                from_reload: false,
                from_package: true,
            },
            TxAdmission {
                txid: child,
                fee_sats: 500,
                vsize: 100,
                admit_height: 0,
                parents: vec![reload],
                from_reload: false,
                from_package: false,
            },
            TxAdmission {
                txid: eligible,
                fee_sats: 500,
                vsize: 100,
                admit_height: 0,
                parents: Vec::new(),
                from_reload: false,
                from_package: false,
            },
        ] {
            estimator.on_tx_admitted(admission);
        }
        estimator.on_block_connected(&BlockObservation {
            height: 1,
            hash: [1u8; 32],
            weight: 4_000,
            tx_weights: vec![(reload, 400), (package, 400), (child, 400), (eligible, 400)],
        });
        assert_eq!(estimator.history_short_tx_count(), 1.0);
        assert_eq!(estimator.hook_counts().confirmed, 1);

        estimator.on_tx_admitted(TxAdmission {
            txid: txid(7, 5),
            fee_sats: 500,
            vsize: 100,
            admit_height: 1,
            parents: Vec::new(),
            from_reload: false,
            from_package: true,
        });
        estimator.on_block_connected(&BlockObservation {
            height: 2,
            hash: [2u8; 32],
            weight: 4_000,
            tx_weights: Vec::new(),
        });
        estimator.on_tx_removed(&txid(7, 5));
        assert_eq!(estimator.hook_counts().failed, 0);
        assert_eq!(estimator.hook_counts().confirmed, 1);
    }

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn short_horizon_medians_stay_non_increasing(
            fast in 20u8..48,
            slow in 20u8..48,
            depth in 2u64..12,
        ) {
            let mut tracker = HistoryTracker::new();
            admit_group(&mut tracker, 8, fast, 0, 20_000, None);
            admit_group(&mut tracker, 9, slow, 0, 1_000, None);
            tracker.begin_block(1);
            confirm_group(&mut tracker, 8, fast, 1, 1);
            for height in 2..=depth {
                tracker.begin_block(height);
            }
            confirm_group(&mut tracker, 9, slow, depth, depth);

            let mut previous: Option<f64> = None;
            for target in 2..=SHORT_BLOCK_PERIODS as u64 {
                let median = tracker.short_median(target, depth);
                if median < 0.0 {
                    continue;
                }
                if let Some(earlier) = previous {
                    prop_assert!(median <= earlier + 1e-6);
                }
                previous = Some(median);
            }
        }
    }
}

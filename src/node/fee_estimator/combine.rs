//! Fee table build: per-target min, monotone prefix, then floor and multiplier.
//!
//! The mempool value can only lower a history estimate. A longer target never
//! costs more than a shorter one that already has a value.

use super::projection::{ProjTx, percentile_rate, project_blocks};
use super::types::{
    ExternalMode, FeeMode, FeeQuote, FeeSource, FeeTable, HistoryResult, MAX_FEE_TARGET,
    native_quote, quote_blocks,
};
use crate::config::FeeEstimatorConfig;
use crate::node::miner::TEMPLATE_RESERVE_WEIGHT;
use blvm_protocol::Hash;
use std::time::Instant;

/// `MAX_BLOCK_WEIGHT` in Bitcoin Core `consensus.h`.
const MAX_BLOCK_WEIGHT: u64 = 4_000_000;

/// Inputs copied out of the estimator lock before the table is built.
pub(crate) struct MempoolSnapshot {
    pub txs: Vec<ProjTx>,
    pub health_ok: bool,
    pub health_window: u64,
    pub health_ratio: f64,
    pub health_required_ratio: f64,
    pub window_full: bool,
    pub tip_hash: Hash,
    pub tip_height: u64,
    pub block_seq: u64,
    pub generation: u64,
    pub generated_at: Instant,
    pub cfg: FeeEstimatorConfig,
    pub relay_floor_sat_per_kvb: u64,
    /// One column per mode. Index is `target - 1`. Empty until the tracker has an answer.
    pub history: [Vec<Option<HistoryResult>>; 2],
    /// The config name, the resolved name, and whether this build fell back.
    pub requested_estimator: String,
    pub active_estimator: String,
    pub degraded: bool,
    /// Present only when the resolved estimator has a fresh table.
    pub external: Option<ExternalMerge>,
}

/// One module table applied after section 8.1 and before the prefix-min.
pub(crate) struct ExternalMerge {
    pub name: String,
    pub mode: ExternalMode,
    pub fee_mode: Option<FeeMode>,
    pub quotes: Vec<(u32, u64)>,
}

#[derive(Clone, Copy)]
struct RawQuote {
    rate: u64,
    source: FeeSource,
    returned_target: u32,
}

pub(crate) fn build_fee_table(input: MempoolSnapshot) -> FeeTable {
    let floor = fee_floor(input.relay_floor_sat_per_kvb, input.cfg.min_fee_sat_per_kvb);
    let history = input.history.clone();
    let mut mempool = [blank::<u64>(), blank::<u64>()];
    let mut combined = [blank::<FeeQuote>(), blank::<FeeQuote>()];

    let project = input.health_ok && input.cfg.mempool_enabled;
    let blocks = if project {
        project_blocks(
            &input.txs,
            input.cfg.mempool_horizon_blocks as usize,
            MAX_BLOCK_WEIGHT,
            TEMPLATE_RESERVE_WEIGHT,
        )
    } else {
        Vec::new()
    };
    let percentiles = [
        input.cfg.mempool_percentile_conservative,
        input.cfg.mempool_percentile_economical,
    ];
    if project {
        for (mode, pct) in percentiles.into_iter().enumerate() {
            for target in 1..=input.cfg.mempool_horizon_blocks.min(MAX_FEE_TARGET) {
                let index = (target - 1) as usize;
                let Some(block) = blocks.get(index) else {
                    break;
                };
                mempool[mode][index] = Some(percentile_rate(
                    block,
                    pct,
                    MAX_BLOCK_WEIGHT,
                    input.cfg.projection_full_threshold,
                    floor,
                ));
            }
        }
    }

    let external_name = input.external.as_ref().map(|merge| merge.name.clone());
    for mode in 0..2 {
        let mut running: Option<RawQuote> = None;
        for target in 1..=MAX_FEE_TARGET {
            let index = (target - 1) as usize;
            let native = raw_quote(
                history[mode][index],
                mempool[mode][index],
                target,
                input.cfg.allow_mempool_only,
            );
            let external_rate = input.external.as_ref().and_then(|merge| {
                if merge_applies(merge.fee_mode, mode) {
                    external_at(&merge.quotes, target)
                } else {
                    None
                }
            });
            let mode_kind = input
                .external
                .as_ref()
                .map(|merge| merge.mode)
                .unwrap_or(ExternalMode::LowerOnly);
            if let Some(next) = merge_raw(native, external_rate, mode_kind, target) {
                running = Some(match running {
                    Some(prev) if prev.rate <= next.rate => prev,
                    _ => next,
                });
            }
            let Some(win) = running else {
                continue;
            };
            let mut quote = native_quote(
                apply_floor_and_multiplier(win.rate, input.cfg.multiplier, floor),
                quote_blocks(target, win.source, win.returned_target),
                win.source,
            );
            if matches!(win.source, FeeSource::External | FeeSource::ExternalMin)
                && let Some(name) = &external_name
            {
                quote.estimator = name.clone();
            }
            combined[mode][index] = Some(quote);
        }
    }

    FeeTable {
        tip_hash: input.tip_hash,
        tip_height: input.tip_height,
        block_seq: input.block_seq,
        generation: input.generation,
        generated_at: input.generated_at,
        mempool_healthy: input.health_ok,
        health_window: input.health_window,
        health_ratio: input.health_ratio,
        health_required_ratio: input.health_required_ratio,
        window_full: input.window_full,
        pool_empty: input.txs.is_empty(),
        history,
        mempool,
        combined,
        requested_estimator: input.requested_estimator,
        active_estimator: input.active_estimator,
        degraded: input.degraded,
    }
}

fn merge_applies(fee_mode: Option<FeeMode>, mode_index: usize) -> bool {
    match fee_mode {
        None => true,
        Some(mode) => mode.index() == mode_index,
    }
}

/// Largest submitted target that is not above `target`.
fn external_at(quotes: &[(u32, u64)], target: u32) -> Option<u64> {
    let mut found: Option<(u32, u64)> = None;
    for (quote_target, rate) in quotes {
        if *quote_target > target {
            continue;
        }
        if found.is_none_or(|(prev, _)| *quote_target >= prev) {
            found = Some((*quote_target, *rate));
        }
    }
    found.map(|(_, rate)| rate)
}

/// Section 20.6. `None` for the external rate leaves the native section 8.1 result.
fn merge_raw(
    native: Option<RawQuote>,
    external: Option<u64>,
    mode: ExternalMode,
    target: u32,
) -> Option<RawQuote> {
    match (mode, native, external) {
        (_, native, None) => native,
        (ExternalMode::LowerOnly, Some(native), Some(rate)) => Some(RawQuote {
            rate: native.rate.min(rate),
            source: FeeSource::ExternalMin,
            returned_target: target,
        }),
        (ExternalMode::LowerOnly, None, Some(_)) => None,
        (ExternalMode::Replace, _, Some(rate)) => Some(RawQuote {
            rate,
            source: FeeSource::External,
            returned_target: target,
        }),
    }
}

/// Section 8.1. Mempool-only is omitted when the operator disallows it.
fn raw_quote(
    history: Option<HistoryResult>,
    mempool: Option<u64>,
    target: u32,
    allow_mempool_only: bool,
) -> Option<RawQuote> {
    match (history, mempool) {
        (Some(history), Some(mempool)) => Some(RawQuote {
            rate: history.sat_per_kvb.min(mempool),
            source: FeeSource::Min,
            returned_target: history.returned_target,
        }),
        (Some(history), None) => Some(RawQuote {
            rate: history.sat_per_kvb,
            source: FeeSource::History,
            returned_target: history.returned_target,
        }),
        (None, Some(mempool)) if allow_mempool_only => Some(RawQuote {
            rate: mempool,
            source: FeeSource::Mempool,
            returned_target: target,
        }),
        _ => None,
    }
}

pub(crate) fn empty_history() -> [Vec<Option<HistoryResult>>; 2] {
    [blank(), blank()]
}

fn fee_floor(relay_floor_sat_per_kvb: u64, min_fee_sat_per_kvb: Option<u64>) -> u64 {
    relay_floor_sat_per_kvb.max(min_fee_sat_per_kvb.unwrap_or(0))
}

/// `max(ceil(estimate * multiplier), floor)`. The multiplier is applied to the
/// estimate; the floor is only the lower bound afterwards.
pub(crate) fn apply_floor_and_multiplier(raw: u64, multiplier: f64, floor: u64) -> u64 {
    let scaled = (raw as f64) * multiplier;
    if !scaled.is_finite() || scaled < 0.0 {
        return floor;
    }
    let ceiled = scaled.ceil();
    if !ceiled.is_finite() || ceiled >= (u64::MAX as f64) {
        return u64::MAX;
    }
    (ceiled as u64).max(floor)
}

fn blank<T: Clone>() -> Vec<Option<T>> {
    vec![None; MAX_FEE_TARGET as usize]
}

#[cfg(test)]
#[path = "combine_tests.rs"]
mod tests;

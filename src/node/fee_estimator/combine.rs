//! Fee table build: per-target min, monotone prefix, then floor and multiplier.
//!
//! The mempool value can only lower a history estimate. A longer target never
//! costs more than a shorter one that already has a value.

use super::projection::{ProjTx, percentile_rate, project_blocks};
use super::types::{
    native_quote, ExternalMode, FeeMode, FeeQuote, FeeSource, FeeTable, HistoryResult,
    MAX_FEE_TARGET, quote_blocks,
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
            if matches!(win.source, FeeSource::External | FeeSource::ExternalMin) {
                if let Some(name) = &external_name {
                    quote.estimator = name.clone();
                }
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
mod tests {
    use super::*;
    use crate::node::fee_estimator::types::{FeeMode, FeeUnavailable, SourceView};
    use proptest::prelude::*;

    fn tx(id: u8, fee: u64, vsize: u64) -> ProjTx {
        ProjTx {
            id: [id; 32],
            fee,
            vsize,
            parents: Vec::new(),
        }
    }

    fn snapshot(txs: Vec<ProjTx>, cfg: FeeEstimatorConfig) -> MempoolSnapshot {
        MempoolSnapshot {
            txs,
            health_ok: true,
            health_window: cfg.health_window_blocks as u64,
            health_ratio: 1.0,
            health_required_ratio: cfg.health_min_ratio,
            window_full: true,
            tip_hash: [9u8; 32],
            tip_height: 4,
            block_seq: 4,
            generation: 8,
            generated_at: Instant::now(),
            cfg,
            relay_floor_sat_per_kvb: 100,
            history: empty_history(),
            requested_estimator: "auto".to_string(),
            active_estimator: "native".to_string(),
            degraded: false,
            external: None,
        }
    }

    fn cfg_open() -> FeeEstimatorConfig {
        let mut cfg = FeeEstimatorConfig::default();
        cfg.projection_full_threshold = 0.0;
        cfg.allow_mempool_only = true;
        cfg.multiplier = 1.0;
        cfg
    }

    #[test]
    fn the_table_has_one_slot_per_target_and_history_stays_empty() {
        let table = build_fee_table(snapshot(vec![tx(1, 5_000, 100)], cfg_open()));
        assert_eq!(table.history[0].len(), MAX_FEE_TARGET as usize);
        assert_eq!(table.mempool[1].len(), MAX_FEE_TARGET as usize);
        assert_eq!(table.combined[0].len(), MAX_FEE_TARGET as usize);
        assert!(
            table
                .history
                .iter()
                .all(|column| column.iter().all(Option::is_none))
        );
        assert!(table.mempool_healthy);
        assert_eq!(table.tip_height, 4);
    }

    #[test]
    fn mempool_only_quotes_require_the_allow_flag_and_a_healthy_gate() {
        let txs = vec![tx(1, 5_000, 100)];
        let allowed = build_fee_table(snapshot(txs.clone(), cfg_open()));
        let quote = allowed
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(quote.sat_per_kvb, 50_000);
        assert_eq!(quote.blocks, 1);
        assert_eq!(quote.source, FeeSource::Mempool);
        let carried = allowed
            .quote(2, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(carried.sat_per_kvb, quote.sat_per_kvb);
        assert_eq!(carried.blocks, 2);
        assert_eq!(carried.source, FeeSource::Mempool);

        let mut blocked = cfg_open();
        blocked.allow_mempool_only = false;
        let blocked = build_fee_table(snapshot(txs.clone(), blocked));
        assert_eq!(blocked.mempool[0][0], Some(50_000));
        assert_eq!(
            blocked.quote(1, FeeMode::Conservative, SourceView::Combined),
            Err(FeeUnavailable::WarmingUp)
        );
        assert_eq!(
            blocked
                .quote(1, FeeMode::Conservative, SourceView::Mempool)
                .unwrap()
                .sat_per_kvb,
            50_000
        );

        let mut unhealthy = snapshot(txs, cfg_open());
        unhealthy.health_ok = false;
        unhealthy.window_full = false;
        let unhealthy = build_fee_table(unhealthy);
        assert!(unhealthy.mempool[0].iter().all(Option::is_none));
        assert_eq!(
            unhealthy.quote(1, FeeMode::Economical, SourceView::Combined),
            Err(FeeUnavailable::WarmingUp)
        );

        let mut ratio_failed = snapshot(vec![tx(1, 5_000, 100)], cfg_open());
        ratio_failed.health_ok = false;
        ratio_failed.window_full = true;
        let ratio_failed = build_fee_table(ratio_failed);
        assert_eq!(
            ratio_failed.quote(1, FeeMode::Conservative, SourceView::Combined),
            Err(FeeUnavailable::MempoolUnhealthy)
        );
    }

    #[test]
    fn modes_use_their_own_percentiles_and_an_empty_pool_is_empty() {
        let mut cfg = cfg_open();
        cfg.mempool_percentile_conservative = 50;
        cfg.mempool_percentile_economical = 75;
        let table = build_fee_table(snapshot(vec![tx(1, 10_000, 100), tx(2, 1_000, 100)], cfg));
        assert_eq!(
            table
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .unwrap()
                .sat_per_kvb,
            100_000
        );
        assert_eq!(
            table
                .quote(1, FeeMode::Economical, SourceView::Combined)
                .unwrap()
                .sat_per_kvb,
            10_000
        );

        let empty = build_fee_table(snapshot(Vec::new(), cfg_open()));
        assert_eq!(
            empty.quote(1, FeeMode::Conservative, SourceView::Combined),
            Err(FeeUnavailable::EmptyMempool)
        );
    }

    #[test]
    fn the_multiplier_scales_the_estimate_and_the_floor_is_only_a_bound() {
        let mut cfg = cfg_open();
        cfg.multiplier = 2.0;
        let scaled = build_fee_table(snapshot(vec![tx(1, 5_000, 100)], cfg));
        assert_eq!(
            scaled
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .unwrap()
                .sat_per_kvb,
            100_000
        );
        assert_eq!(scaled.mempool[0][0], Some(50_000));

        let mut cfg = cfg_open();
        cfg.multiplier = 2.0;
        cfg.min_fee_sat_per_kvb = Some(100);
        let mut input = snapshot(vec![tx(1, 4, 100)], cfg);
        input.relay_floor_sat_per_kvb = 100;
        let bounded = build_fee_table(input);
        assert_eq!(bounded.mempool[0][0], Some(40));
        assert_eq!(
            bounded
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .unwrap()
                .sat_per_kvb,
            100
        );

        let mut cfg = cfg_open();
        cfg.multiplier = 1.5;
        let mut input = snapshot(vec![tx(1, 1, 3)], cfg);
        input.relay_floor_sat_per_kvb = 0;
        let ceiled = build_fee_table(input);
        assert_eq!(ceiled.mempool[0][0], Some(333));
        assert_eq!(
            ceiled
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .unwrap()
                .sat_per_kvb,
            500
        );
    }

    #[test]
    fn the_min_rule_then_the_prefix_keeps_the_table_non_increasing() {
        let mut cfg = cfg_open();
        cfg.mempool_horizon_blocks = 2;
        cfg.multiplier = 1.0;
        let mut input = snapshot(vec![tx(1, 5_000, 100), tx(2, 1_000, 100)], cfg);
        input.history[0][0] = Some(HistoryResult {
            sat_per_kvb: 30_000,
            returned_target: 1,
        });
        input.history[0][1] = Some(HistoryResult {
            sat_per_kvb: 80_000,
            returned_target: 4,
        });
        input.history[0][5] = Some(HistoryResult {
            sat_per_kvb: 90_000,
            returned_target: 6,
        });
        let table = build_fee_table(input);
        let first = table
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        let second = table
            .quote(2, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        let sixth = table
            .quote(6, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(first.source, FeeSource::Min);
        assert!(first.sat_per_kvb <= 30_000);
        assert_eq!(second.source, FeeSource::Min);
        assert!(second.sat_per_kvb <= 80_000);
        assert!(second.sat_per_kvb <= first.sat_per_kvb);
        assert!(sixth.sat_per_kvb <= second.sat_per_kvb);
        assert_eq!(sixth.blocks, 6);
        let history_only = table
            .quote(6, FeeMode::Conservative, SourceView::History)
            .unwrap();
        assert_eq!(history_only.sat_per_kvb, 90_000);
        assert_eq!(history_only.blocks, 6);

        let mut history_only_cfg = cfg_open();
        history_only_cfg.mempool_enabled = false;
        let mut history_only_input = snapshot(Vec::new(), history_only_cfg);
        history_only_input.history[0][0] = Some(HistoryResult {
            sat_per_kvb: 12_000,
            returned_target: 3,
        });
        let history_only = build_fee_table(history_only_input);
        let quote = history_only
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(quote.source, FeeSource::History);
        assert_eq!(quote.sat_per_kvb, 12_000);
        assert_eq!(quote.blocks, 3);
        let carried = history_only
            .quote(2, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(carried.sat_per_kvb, 12_000);
        assert_eq!(carried.blocks, 3);
        assert_eq!(carried.source, FeeSource::History);
    }

    #[test]
    fn a_disabled_mempool_layer_leaves_the_columns_empty() {
        let mut cfg = cfg_open();
        cfg.mempool_enabled = false;
        let table = build_fee_table(snapshot(vec![tx(1, 5_000, 100)], cfg));
        assert!(table.mempool[0].iter().all(Option::is_none));
        assert!(table.combined[0][0].is_none());
    }

    fn history_at(rate: u64) -> [Vec<Option<HistoryResult>>; 2] {
        let mut history = empty_history();
        for mode in 0..2 {
            for target in 1..=MAX_FEE_TARGET {
                history[mode][(target - 1) as usize] = Some(HistoryResult {
                    sat_per_kvb: rate,
                    returned_target: target,
                });
            }
        }
        history
    }

    #[test]
    fn lower_only_and_replace_follow_the_merge_before_the_prefix_and_the_floor() {
        let mut cfg = cfg_open();
        cfg.mempool_enabled = false;
        cfg.min_fee_sat_per_kvb = Some(1_000);
        let mut input = snapshot(Vec::new(), cfg);
        input.relay_floor_sat_per_kvb = 1_000;
        input.history = history_at(9_000);
        input.active_estimator = "blend".to_string();
        input.external = Some(ExternalMerge {
            name: "blend".to_string(),
            mode: ExternalMode::LowerOnly,
            fee_mode: None,
            quotes: vec![(2, 4_000), (4, 1)],
        });
        let table = build_fee_table(input);
        let first = table
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(first.sat_per_kvb, 9_000);
        assert_eq!(first.source, FeeSource::History);
        assert_eq!(first.estimator, "native");
        let lowered = table
            .quote(2, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(lowered.sat_per_kvb, 4_000);
        assert_eq!(lowered.source, FeeSource::ExternalMin);
        assert_eq!(lowered.estimator, "blend");
        let floored = table
            .quote(4, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(floored.sat_per_kvb, 1_000);
        assert_eq!(floored.source, FeeSource::ExternalMin);
        assert!(floored.sat_per_kvb <= lowered.sat_per_kvb);

        let mut replace_cfg = cfg_open();
        replace_cfg.mempool_enabled = false;
        replace_cfg.min_fee_sat_per_kvb = Some(1_000);
        let mut replace_input = snapshot(Vec::new(), replace_cfg);
        replace_input.relay_floor_sat_per_kvb = 1_000;
        replace_input.active_estimator = "blend".to_string();
        replace_input.external = Some(ExternalMerge {
            name: "blend".to_string(),
            mode: ExternalMode::Replace,
            fee_mode: Some(FeeMode::Conservative),
            quotes: vec![(1, 8_000), (3, 3_000)],
        });
        let replaced = build_fee_table(replace_input);
        let at_one = replaced
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(at_one.sat_per_kvb, 8_000);
        assert_eq!(at_one.source, FeeSource::External);
        assert_eq!(at_one.estimator, "blend");
        let at_three = replaced
            .quote(3, FeeMode::Conservative, SourceView::Combined)
            .unwrap();
        assert_eq!(at_three.sat_per_kvb, 3_000);
        assert_eq!(at_three.source, FeeSource::External);
        assert!(replaced
            .quote(1, FeeMode::Economical, SourceView::Combined)
            .is_err());
        let under_floor = {
            let mut cfg = cfg_open();
            cfg.mempool_enabled = false;
            cfg.min_fee_sat_per_kvb = Some(1_000);
            let mut input = snapshot(Vec::new(), cfg);
            input.relay_floor_sat_per_kvb = 1_000;
            input.external = Some(ExternalMerge {
                name: "blend".to_string(),
                mode: ExternalMode::Replace,
                fee_mode: None,
                quotes: vec![(1, 1)],
            });
            build_fee_table(input)
        };
        assert_eq!(
            under_floor
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .unwrap()
                .sat_per_kvb,
            1_000
        );
        let mut hole_cfg = cfg_open();
        hole_cfg.mempool_enabled = false;
        let mut hole = snapshot(Vec::new(), hole_cfg);
        hole.external = Some(ExternalMerge {
            name: "blend".to_string(),
            mode: ExternalMode::LowerOnly,
            fee_mode: None,
            quotes: vec![(1, 5_000)],
        });
        let hole_table = build_fee_table(hole);
        assert!(hole_table
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .is_err());
    }

    proptest::proptest! {
        #[test]
        fn scaled_rate_stays_at_or_above_the_floor_without_scaling_the_floor(
            raw in 1u64..1_000_000,
            floor in 0u64..10_000,
            steps in 0u32..100,
        ) {
            let multiplier = 0.5 + f64::from(steps) * 0.095;
            let rate = apply_floor_and_multiplier(raw, multiplier, floor);
            prop_assert!(rate >= floor);
            let scaled = (raw as f64 * multiplier).ceil();
            if scaled < floor as f64 {
                prop_assert_eq!(rate, floor);
            } else if scaled.is_finite() && scaled < u64::MAX as f64 {
                prop_assert_eq!(rate, scaled as u64);
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn merged_quotes_stay_monotone_and_at_or_above_the_floor(
            replace in proptest::bool::ANY,
            pairs in prop::collection::vec((1u32..=64, 1u64..200_000u64), 1..8),
            floor in 1u64..20_000,
        ) {
            let mut quotes = pairs;
            quotes.sort_by_key(|(target, _)| *target);
            quotes.dedup_by_key(|(target, _)| *target);
            let mut cfg = cfg_open();
            cfg.mempool_enabled = false;
            cfg.multiplier = 1.0;
            cfg.min_fee_sat_per_kvb = Some(floor);
            let mut input = snapshot(Vec::new(), cfg);
            input.relay_floor_sat_per_kvb = floor;
            input.history = history_at(40_000);
            input.active_estimator = "blend".to_string();
            input.external = Some(ExternalMerge {
                name: "blend".to_string(),
                mode: if replace { ExternalMode::Replace } else { ExternalMode::LowerOnly },
                fee_mode: None,
                quotes,
            });
            let table = build_fee_table(input);
            for mode in [FeeMode::Conservative, FeeMode::Economical] {
                let mut previous: Option<u64> = None;
                for target in 1..=MAX_FEE_TARGET {
                    let Ok(quote) = table.quote(target, mode, SourceView::Combined) else {
                        continue;
                    };
                    prop_assert!(quote.sat_per_kvb >= floor);
                    if !replace {
                        prop_assert!(quote.sat_per_kvb <= 40_000.max(floor));
                    }
                    if let Some(previous) = previous {
                        prop_assert!(quote.sat_per_kvb <= previous);
                    }
                    previous = Some(quote.sat_per_kvb);
                }
            }
        }
    }
}

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
    assert!(
        replaced
            .quote(1, FeeMode::Economical, SourceView::Combined)
            .is_err()
    );
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
    assert!(
        hole_table
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .is_err()
    );
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

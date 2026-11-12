use super::*;

fn test_lock() -> crate::ibd_test_lock::Guard {
    test_tip_atomics_lock()
}

fn reset_tracker() {
    HEIGHT.store(0, Ordering::Relaxed);
    TAKEN_THROUGH.store(0, Ordering::Relaxed);
    NEEDED_MS.store(0, Ordering::Relaxed);
    GETDATA_MS.store(0, Ordering::Relaxed);
    BODY_MS.store(0, Ordering::Relaxed);
    REORDER_MS.store(0, Ordering::Relaxed);
    FEEDER_MS.store(0, Ordering::Relaxed);
    SOFT_RETRIES.store(0, Ordering::Relaxed);
    WAN_BODY_TIP.store(0, Ordering::Relaxed);
    LOCAL_STREAM_LAST_MS.store(0, Ordering::Relaxed);
    GETDATA_BODY_EWMA_MS.store(0, Ordering::Relaxed);
    GETDATA_BODY_EWMA_N.store(0, Ordering::Relaxed);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    HANDOFF_SLA_FIRED.store(false, Ordering::Relaxed);
    AHEAD_FREEZE_SOFT.store(false, Ordering::Relaxed);
    TIP_FAILOVER_ARMED.store(false, Ordering::Relaxed);
    clear_post_local_ahead_grace();
    if let Ok(mut g) = recent_stamps().lock() {
        g.getdata.clear();
        g.body.clear();
        g.getdata_peer.clear();
    }
    if let Ok(mut g) = peer_gd_ewmas().lock() {
        g.clear();
    }
    clear_owner_body_ia();
}

#[test]
fn owner_ia_median_separates_fast_3_5_from_slow_19() {
    let _lock = test_lock();
    test_seed_owner_body_ia(4, 16);
    let (med, n) = owner_body_ia_median().expect("fast");
    assert_eq!(n, 16);
    assert_eq!(med, 4);
    assert!(med < OWNER_IA_DEMOTE_MEDIAN_MS);
    test_seed_owner_body_ia(19, 16);
    let (med, _) = owner_body_ia_median().expect("slow");
    assert_eq!(med, 19);
    assert!(med >= OWNER_IA_DEMOTE_MEDIAN_MS);
    test_reset_owner_body_ia();
    assert!(owner_body_ia_median().is_none());
}

/// Phase 0b.3: forbid scoring wall_bps / dens as product tip_crawl.
#[test]
fn regime_scoreboard_product_tip_crawl_requires_next_gt_body() {
    assert_eq!(
        classify_ibd_regime(400_001, 400_000, false),
        IbdSyncRegime::TrueWanTipCrawl
    );
    assert!(product_tip_crawl_metric_ok(IbdSyncRegime::TrueWanTipCrawl));

    assert_eq!(
        classify_ibd_regime(400_000, 400_000, false),
        IbdSyncRegime::LocalAhead
    );
    assert_eq!(
        classify_ibd_regime(399_999, 400_000, false),
        IbdSyncRegime::LocalAhead
    );
    assert!(!product_tip_crawl_metric_ok(IbdSyncRegime::LocalAhead));

    // Synth dens wins even when next > body (lab wall_bps ≠ tip90 product).
    assert_eq!(
        classify_ibd_regime(450_000, 400_000, true),
        IbdSyncRegime::SynthDens
    );
    assert!(!product_tip_crawl_metric_ok(IbdSyncRegime::SynthDens));
}

#[test]
#[serial_test::serial(ibd)]
fn tip_crawl_supply_healthy_requires_past_body_and_fast_gd_or_stream() {
    // Share lock with index Land E demote test (same process-global atomics).
    let _lock = test_tip_atomics_lock();
    reset_tracker();
    publish_wan_body_tip(437_309);
    mark_needed(437_310);
    assert!(
        !tip_crawl_supply_healthy_now(),
        "no GD / STREAM yet → unhealthy"
    );
    test_seed_getdata_body_ewma(40, 32);
    assert!(
        tip_crawl_supply_healthy_now(),
        "fast GD under tip crawl → healthy"
    );
    test_seed_getdata_body_ewma(500, 32);
    assert!(
        !tip_crawl_supply_healthy_now(),
        "slow GD without STREAM → unhealthy"
    );
    note_tip_local_stream();
    assert!(
        tip_crawl_supply_healthy_now(),
        "fresh LOCAL_STREAM → healthy even if GD slow"
    );
    mark_needed(400_000);
    assert!(
        !tip_crawl_supply_healthy_now(),
        "LOCAL_AHEAD (next ≤ body_tip) → not tip-crawl hold"
    );
    reset_tracker();
}

#[test]
#[serial_test::serial(ibd)]
fn mousetrap_pipe_frontier_ignores_uncovered_gap() {
    let _ibd = test_tip_atomics_lock();
    let _lock = test_lock();
    reset_tracker();
    let tip = 304_664u64;
    HEIGHT.store(tip, Ordering::Relaxed);
    // GetData only tip..tip+7 (grown=8); no stamps at tip+8+.
    for h in tip..tip + 8 {
        mark_getdata(h);
    }
    assert_eq!(pipe_frontier(tip), tip + 7);
    assert_eq!(pipe_contig(tip), 8);
    // Hole at tip+8 — frontier must not jump even if a far height was stamped.
    mark_getdata(tip + 127);
    assert_eq!(pipe_frontier(tip), tip + 7, "must stop at first pipe hole");
    assert_eq!(pipe_contig(tip), 8);
    reset_tracker();
}

#[test]
#[serial_test::serial(ibd)]
fn mousetrap_pipe_frontier_empty_when_tip_uncovered() {
    let _ibd = test_tip_atomics_lock();
    let _lock = test_lock();
    reset_tracker();
    let tip = 100u64;
    mark_getdata(101);
    assert_eq!(pipe_frontier(tip), tip.saturating_sub(1));
    assert_eq!(pipe_contig(tip), 0);
    reset_tracker();
}

#[test]
fn w68_pre_roll_getdata_credited_on_tip_roll() {
    let _lock = test_lock();
    reset_tracker();
    // Deep pipe GetData while tip still at 0.
    mark_getdata(100);
    assert_eq!(
        GETDATA_MS.load(Ordering::Relaxed),
        0,
        "untracked must not stamp live"
    );
    // Tip rolls to 100 — credit original GetData time.
    mark_needed(100);
    let gd = GETDATA_MS.load(Ordering::Relaxed);
    assert!(gd > 0, "W68 must credit pre-roll GetData");
    // Late-body freeze must key off GetData, not open immediately.
    assert!(
        !tip_ahead_frozen_for_late_body(),
        "fresh credit must not look like a 2s late body"
    );
    reset_tracker();
}

#[test]
fn w68_pre_roll_body_clears_failover_on_credit() {
    let _lock = test_lock();
    reset_tracker();
    mark_getdata(50);
    mark_body(50);
    TIP_FAILOVER_ARMED.store(true, Ordering::Relaxed);
    AHEAD_FREEZE_SOFT.store(true, Ordering::Relaxed);
    mark_needed(50);
    assert!(BODY_MS.load(Ordering::Relaxed) > 0);
    assert!(!tip_failover_armed());
    assert!(!tip_ahead_frozen_for_soft_retry());
    reset_tracker();
}

#[test]
fn pre_roll_body_feeder_measures_from_needed_not_body_age() {
    // body 10s before needed; feeder 5ms after needed → handoff ~5ms, not ~10005ms.
    assert_eq!(handoff_body_feeder_ms(10_000, 0, 10_005), -1);
    assert_eq!(handoff_body_feeder_ms(10_000, 0, 0), -1);
    assert_eq!(handoff_body_feeder_ms(10_000, 50, 10_005), 5);
    // Just-in-time body (public WAN): body after needed → classic body→feeder.
    assert_eq!(handoff_body_feeder_ms(10_000, 10_100, 10_103), 3);
}

#[test]
#[serial_test::serial]
fn pre_roll_body_does_not_trip_handoff_sla() {
    let _lock = test_lock();
    reset_tracker();
    {
        let Ok(mut g) = recent_stamps().lock() else {
            panic!("recent_stamps");
        };
        let now = wall_ms();
        g.body.insert(201, now.saturating_sub(10_000));
        g.getdata.insert(201, now.saturating_sub(10_200));
    }
    mark_needed(201);
    assert!(BODY_MS.load(Ordering::Relaxed) > 0);
    assert!(BODY_MS.load(Ordering::Relaxed) < NEEDED_MS.load(Ordering::Relaxed));
    assert!(
        !body_feeder_sla_breached(),
        "pre-roll body age must not trip handoff SLA before tip-needed window"
    );
    reset_tracker();
}

#[test]
fn w68_finish_validated_credits_next_height() {
    let _lock = test_lock();
    reset_tracker();
    mark_needed(10);
    mark_getdata(10);
    mark_body(10);
    mark_getdata(11); // pre-roll next
    mark_feeder(10);
    finish_validated(10);
    assert_eq!(tracked_tip_height(), 11);
    assert!(
        GETDATA_MS.load(Ordering::Relaxed) > 0,
        "roll must credit pre-roll GetData for h+1"
    );
    reset_tracker();
}

#[test]
fn w68_missing_getdata_still_late_body_after_freeze_secs() {
    let _lock = test_lock();
    reset_tracker();
    mark_needed(77);
    // No GetData credit — after freeze window, late body must still fire.
    NEEDED_MS.store(wall_ms().saturating_sub(3_000), Ordering::Relaxed);
    assert!(tip_ahead_frozen_for_late_body());
    reset_tracker();
}

#[test]
fn w77_download_mark_body_does_not_clear_late_freeze_while_gap_missing() {
    let _lock = test_lock();
    reset_tracker();
    mark_needed(370_588);
    mark_getdata(370_588);
    // Download stamped body, but coordinator never admitted tip (RESEND path).
    mark_body(370_588);
    assert!(BODY_MS.load(Ordering::Relaxed) > 0);
    // Post-roll GetData (getdata >= needed) aged past freeze window.
    let now = wall_ms();
    NEEDED_MS.store(now.saturating_sub(4_000), Ordering::Relaxed);
    GETDATA_MS.store(now.saturating_sub(3_000), Ordering::Relaxed);
    NEEDED_MS.store(now.saturating_sub(4_000), Ordering::Relaxed);
    GETDATA_MS.store(now.saturating_sub(3_000), Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    assert!(
        !tip_ahead_frozen_for_late_body(),
        "without gap_missing, BODY_MS still clears freeze"
    );
    crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    assert!(
        tip_ahead_frozen_for_late_body(),
        "W77: gap_missing must ignore download BODY_MS for ahead freeze"
    );
    crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    reset_tracker();
}

#[test]
fn w81_pre_roll_getdata_does_not_instant_freeze_under_gap_missing() {
    let _lock = test_lock();
    reset_tracker();
    // Deep pipe GetData 4s before tip roll (typical WAN getdata→body p50).
    mark_getdata(200);
    {
        let Ok(mut g) = recent_stamps().lock() else {
            panic!("recent_stamps lock");
        };
        if let Some(ms) = g.getdata.get_mut(&200) {
            *ms = wall_ms().saturating_sub(4_000);
        }
    }
    mark_needed(200);
    assert!(
        GETDATA_MS.load(Ordering::Relaxed) > 0,
        "W68 must credit pre-roll GetData"
    );
    assert!(
        GETDATA_MS.load(Ordering::Relaxed) < NEEDED_MS.load(Ordering::Relaxed),
        "pre-roll GetData must be older than mark_needed"
    );
    crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    assert!(
        !tip_ahead_frozen_for_late_body(),
        "W81: pre-roll GetData lead must not look like 2s tip distress"
    );
    // Real post-roll wait still freezes.
    NEEDED_MS.store(wall_ms().saturating_sub(3_000), Ordering::Relaxed);
    GETDATA_MS.store(0, Ordering::Relaxed);
    assert!(
        tip_ahead_frozen_for_late_body(),
        "W81: needed-age freeze still arms without GetData"
    );
    crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    reset_tracker();
}

#[test]
fn w165_unproven_sla_breaches_before_floor() {
    // Live W164 @334087: demoted sticky score=0.001 held floor SLA 15s.
    let _lock = test_lock();
    reset_tracker();
    mark_needed(334_087);
    test_backdate_awaiting_ms(6_000);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        tip_sla_breached_for_owner_score(0.001),
        "W165: unproven owner breaches at 6s (SLA=5)"
    );
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        !tip_sla_breached_for_owner_score(0.100),
        "W165: floor owner still needs 15s"
    );
    test_backdate_awaiting_ms(16_000);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        tip_sla_breached_for_owner_score(0.100),
        "W165: floor owner breaches at 16s"
    );
    reset_tracker();
}

#[test]
fn w170_floor_sla_covers_mid_band_sticky() {
    // Live W169 @317772: score=0.148 held mid SLA 90s under old ≤0.12 threshold.
    let _lock = test_lock();
    reset_tracker();
    mark_needed(317_772);
    test_backdate_awaiting_ms(16_000);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        tip_owner_uses_floor_sla(0.148),
        "W170: 0.148 is floor-SLA band"
    );
    assert_eq!(tip_sla_secs_for_owner_score(0.148), tip_sla_floor_secs());
    assert!(
        tip_sla_breached_for_owner_score(0.148),
        "W170: score=0.148 breaches floor SLA at 16s (not 90s)"
    );
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        !tip_sla_breached_for_owner_score(0.50),
        "W170: mid/high sticky still needs full SLA"
    );
    reset_tracker();
}

#[test]
fn w178_post_local_ahead_grace_uses_floor_sla_for_unproven() {
    // Live W177 @403622: cold score=0.001 + unproven SLA=5s → TIP_SLA carousel → freeze.
    let _lock = test_lock();
    reset_tracker();
    arm_post_local_ahead_grace();
    assert!(post_local_ahead_grace_active());
    assert_eq!(
        tip_sla_secs_for_owner_score(0.001),
        tip_sla_floor_secs(),
        "W178 grace: unproven uses floor SLA not 5s"
    );
    mark_needed(403_622);
    test_backdate_awaiting_ms(6_000);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        !tip_sla_breached_for_owner_score(0.001),
        "W178 grace: 6s must not breach floor SLA"
    );
    test_backdate_awaiting_ms(16_000);
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    assert!(
        tip_sla_breached_for_owner_score(0.001),
        "W178 grace: floor SLA still breaches at 16s"
    );
    // W179: first tip body must NOT clear wall-clock grace (scores still cold).
    TIP_SLA_FIRED.store(false, Ordering::Relaxed);
    mark_body(403_622);
    assert!(
        post_local_ahead_grace_active(),
        "W179: grace survives first tip body"
    );
    assert_eq!(tip_sla_secs_for_owner_score(0.001), tip_sla_floor_secs());
    clear_post_local_ahead_grace();
    assert_eq!(tip_sla_secs_for_owner_score(0.001), tip_sla_unproven_secs());
    reset_tracker();
}

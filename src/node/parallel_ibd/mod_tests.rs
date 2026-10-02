//! Coordinator / admit / ahead-policy tests extracted from `mod.rs`.
//! Complements `chunk_assigner_tests` (assignment) and `download_tests` (pipe).

use super::*;
use std::collections::VecDeque;

#[serial_test::serial(ibd)]
#[test]
fn a5_tip_admit_tight_aligns_ahead_cap_with_admit() {
    // SAFETY: single-threaded test; env restored before exit.
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_ADMIT_TIGHT");
        let (_, cap_default) = wan_ahead_policy(true, true, true, 2);
        assert_eq!(
            cap_default,
            wan_bulk_tip_gap_ahead_cap(),
            "default tip-starve ahead stays tip-gap cap (256)"
        );
        std::env::set_var("BLVM_IBD_TIP_ADMIT_TIGHT", "1");
        let (kind, cap) = wan_ahead_policy(true, true, true, 2);
        assert_eq!(kind, "wan_tip_tight");
        assert_eq!(
            cap,
            wan_gap_admit_window(),
            "TIGHT tip-starve ahead must match admit window (A5 KEEP; A6 tip-first REVERT)"
        );
        // Sole + GD_SLOW: do not deepen starve under wan_tip_tight.
        tip_stage::test_seed_getdata_body_ewma(1_500, 32);
        let (kind_sole, cap_sole) = wan_ahead_policy(true, true, true, 1);
        assert_eq!(kind_sole, "wan_bulk_gap_sole");
        assert_eq!(cap_sole, wan_bulk_tip_gap_ahead_cap());
        tip_stage::test_reset_getdata_body_ewma();
        std::env::remove_var("BLVM_IBD_TIP_ADMIT_TIGHT");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn a4_tip_admit_tight_opt_in_ignores_bulk_catchup() {
    // SAFETY: single-threaded test; env restored before exit.
    unsafe {
        // Default (tight off): tip+bulk still selects bulk admit (pre-A4 public DNA).
        std::env::remove_var("BLVM_IBD_TIP_ADMIT_TIGHT");
        assert!(!tip_admit_tight_enabled());
        assert_eq!(
            effective_gap_admit_window(true, true),
            wan_bulk_admit_window(),
            "default tip+bulk must keep bulk admit until public confirm"
        );
        assert_eq!(
            effective_gap_admit_window(true, false),
            wan_gap_admit_window()
        );
        // Opt-in tight: tip crawl ignores bulk (archive fabric KEEP mech).
        std::env::set_var("BLVM_IBD_TIP_ADMIT_TIGHT", "1");
        assert!(tip_admit_tight_enabled());
        assert_eq!(
            effective_gap_admit_window(true, true),
            wan_gap_admit_window(),
            "TIP_ADMIT_TIGHT=1 must ignore bulk catchup"
        );
        std::env::remove_var("BLVM_IBD_TIP_ADMIT_TIGHT");
        // Pre-tip / LOCAL_GAP path unchanged.
        assert_eq!(effective_gap_admit_window(false, true), gap_admit_window());
        assert_eq!(effective_gap_admit_window(false, false), gap_admit_window());
    }
}

#[serial_test::serial(ibd)]
#[test]
fn hole_under_sparse_confirmed_does_not_hide_wan_gap() {
    // Live 2026-08-20: confirmed=185817 (binary-search cheese) / contiguous=70669 /
    // hole at 70713. Using confirmed as live_body_tip made wan_gap=false and
    // GetData never armed — new-user IBD must not wait on leftover disk.
    assert_eq!(
        wan_live_body_tip(0, 70669),
        70669,
        "confirmed=0 must not zero a real contiguous range (fixture / genesis spawn)"
    );
    assert_eq!(
        wan_live_body_tip(0, 0),
        0,
        "empty store still has no body tip"
    );
    assert_eq!(wan_live_body_tip(70806, 70669), 70669);
    assert_eq!(wan_live_body_tip(70669, 70669), 70669);
    assert_eq!(
        pull_wan_body_tip_for_hole(70806, 70592),
        70591,
        "local miss under leftover tip must drop warehouse so GetData owns the hole"
    );
    assert_eq!(pull_wan_body_tip_for_hole(0, 70839), 0);
    assert_eq!(pull_wan_body_tip_for_hole(100, 200), 100);
    assert!(should_pull_wan_body_tip_on_inject(
        true, false, 70735, 70713
    ));
    assert!(
        !should_pull_wan_body_tip_on_inject(false, false, 70735, 70713),
        "LOCAL_GAP_FILL=0 is not a disk miss"
    );
    assert!(!should_pull_wan_body_tip_on_inject(
        true, true, 70735, 70713
    ));
    assert!(leftover_hole_needs_getdata(true, 70713, 70735, 0));
    assert!(
        leftover_hole_needs_getdata(true, 70736, 70735, 0),
        "first height past leftover tip is WAN handoff GetData"
    );
    assert!(
        leftover_hole_needs_getdata(true, 70713, 70735, 1),
        "leftover covering is not a feeder pipeline — still GetData the hole"
    );
    assert!(!leftover_hole_needs_getdata(false, 70713, 70735, 0));
    assert!(
        leftover_trace_watch(true, 70_736, 70_735),
        "leftover_force must TRACE get_work after leftover_HANDOFF"
    );
    assert!(
        leftover_trace_watch(false, 70_736, 70_735),
        "leftover tip+1 is leftover-band even before leftover_force"
    );
    assert!(
        leftover_trace_watch(false, 70_300, 70_735),
        "last leftover 512 heights are leftover-band"
    );
    assert!(
        !leftover_trace_watch(false, 1, 70_735),
        "1→70k must not flood leftover_TRACE (next ≪ leftover tip-512)"
    );
    assert!(
        !leftover_hole_needs_getdata(false, 1, 70735, 0),
        "spawn tip_gap_missing must not GetData (1,1) under leftover cheese"
    );
    assert!(leftover_replay_stall_is_disk_hole(70705, 70735));
    assert!(
        leftover_replay_stall_is_disk_hole(70736, 70735),
        "stall at leftover tip+1 must arm GetData (live 70736 freeze)"
    );
    assert!(
        leftover_replay_stall_is_disk_hole(70736, 172_791),
        "sparse leftover max must still treat 70736 as leftover-stall (live dest)"
    );
    assert!(
        leftover_force_aborts_inflight_stripe(true, 70_735, 70_625, 70_736),
        "leftover stripe 70625–70735 must abort when stall is WAN handoff 70736"
    );
    assert!(leftover_force_aborts_inflight_stripe(
        true, 70_735, 70_625, 70_689
    ));
    assert!(
        !leftover_force_aborts_inflight_stripe(false, 70_735, 70_625, 70_736),
        "leftover_force off — do not abort leftover download"
    );
    assert!(
        !leftover_force_aborts_inflight_stripe(true, 70_735, 70_736, 70_736),
        "WAN handoff chunk 70736+ is the GetData we want — do not abort"
    );
    assert!(
        leftover_force_survives_disk_inject(true, 70_736, 70_735),
        "assigner next=70736 must keep leftover_force (not stale coord val_h=70656)"
    );
    assert!(
        !leftover_force_survives_disk_inject(true, 70_657, 70_735),
        "leftover-band disk inject may still clear leftover_force"
    );
    assert!(!leftover_force_survives_disk_inject(false, 70_736, 70_735));
    assert!(
        leftover_stall_skips_disk_load(70_736, 70_735),
        "WAN handoff must skip leftover heed3 load (live 70736 hang)"
    );
    assert!(
        !stall_may_arm_leftover_force(true),
        "Stage 1: filled store must not FORCE GetData on validation stall"
    );
    assert!(
        stall_may_arm_leftover_force(false),
        "empty store still arms leftover_force (coordinator silent / leftover hole)"
    );
    assert!(
        leftover_stall_skips_disk_load(91_698, 0),
        "genesis TRUE WAN stall must skip heed3 (header without body)"
    );
    assert!(
        leftover_disk_hole_should_inject(0, 2, true, false, false, false),
        "R-223 IBD:1 store=1 feeder=0 reorder_has=0 live_tip=0 must leftover-inject"
    );
    assert!(
        !leftover_disk_hole_should_inject(0, 2, false, false, false, false),
        "empty store + unpublished tip must not leftover-FORCE from height 1"
    );
    assert!(
        leftover_disk_hole_should_inject(70_735, 70_657, false, false, false, false),
        "classic leftover under published cheese tip still injects"
    );
    assert!(
        !leftover_disk_hole_should_inject(0, 2, true, true, false, false),
        "already in reorder — Case B, not re-inject"
    );
    assert!(!leftover_disk_hole_should_inject(
        0, 2, true, false, true, false
    ));
    assert!(!leftover_disk_hole_should_inject(
        0, 2, true, false, false, true
    ));
    assert!(!leftover_disk_hole_should_inject(
        0, 0, true, false, false, false
    ));
    // R-287: args are (injected, tip_in_feeder, flight_tip, stalled_ms, next_needed).
    // OR of two gates — immediate at/above the 248k floor, stall-gated below it.
    assert!(
        leftover_inject_should_feeder(true, false, 0, 0, 249_000),
        "R-273 leftover: at/above the floor promote immediately, no stall wait \
         (R-273 in_reorder_not_feeder 17 vs R-283 350 / R-284 276)"
    );
    assert!(
        leftover_inject_should_feeder(true, false, 0, 0, 248_000),
        "floor is inclusive"
    );
    assert!(
        !leftover_inject_should_feeder(true, false, 0, 0, 40_000),
        "dump advances next_needed every few ms — must not fire below the floor. \
         R-286 dropped the floor and cost 10–50k 1337 vs 3246"
    );
    assert!(!leftover_inject_should_feeder(true, false, 0, 999, 40_000));
    assert!(
        leftover_inject_should_feeder(true, false, 0, 90_000, 176_199),
        "r278a h=176199 froze 90s below the floor — the stall gate must still fire"
    );
    assert!(
        !leftover_inject_should_feeder(true, false, 0, 0, 176_199),
        "same height, no stall — the stall gate must not fire on a healthy band"
    );
    assert!(
        leftover_inject_should_feeder(true, false, 1, 0, 249_000),
        "R-272 300–340k sit: flight_tip=1 must not block store_has emit"
    );
    assert!(!leftover_inject_should_feeder(
        true, true, 0, 90_000, 249_000
    ));
    assert!(!leftover_inject_should_feeder(
        false, false, 0, 90_000, 249_000
    ));
}

#[test]
fn all_local_retake_backs_off_only_off_tip() {
    // R-273 137000→138000: 58 all-local ranges re-taken 185,683× in 60s.
    assert_eq!(
        all_local_retake_backoff_ms(0, 137_534, 137_549, 137_154),
        all_local_retake_backoff_base_ms(),
        "all-local range ahead of a stuck tip must back off"
    );
    assert_eq!(
        all_local_retake_backoff_ms(0, 137_150, 137_165, 137_154),
        0,
        "range covering next_needed must stay re-takeable"
    );
    assert_eq!(
        all_local_retake_backoff_ms(1, 137_534, 137_549, 137_154),
        0,
        "a chunk that fetched a body over the wire is real work"
    );
}

#[test]
fn tip_stale_cover_rerace_fires_only_on_a_quiet_seated_cover() {
    let th = 300;
    // R-273 p90 22ms / p99 205ms — healthy stages must never re-race.
    assert!(!tip_stale_cover_should_rerace(
        true, 22, th, 1, 2, false, false, false
    ));
    assert!(!tip_stale_cover_should_rerace(
        true, 205, th, 1, 2, false, false, false
    ));
    assert!(
        tip_stale_cover_should_rerace(true, 300, th, 1, 2, false, false, false),
        "quiet seated cover past threshold is the 293s R-273 tail"
    );
    assert!(
        !tip_stale_cover_should_rerace(true, 5_000, th, 0, 2, false, false, false),
        "uncovered H is HOLE_ANY's job, not the re-race"
    );
    assert!(
        !tip_stale_cover_should_rerace(true, 5_000, th, 2, 2, false, false, false),
        "max_covering caps at incumbent + one racer"
    );
    assert!(
        !tip_stale_cover_should_rerace(true, 5_000, th, 1, 2, false, true, false),
        "incumbent must not re-race its own quiet pipe"
    );
    assert!(
        !tip_stale_cover_should_rerace(true, 5_000, th, 1, 2, true, false, false),
        "peer already covering H does not duplicate itself"
    );
    assert!(
        !tip_stale_cover_should_rerace(true, 5_000, th, 1, 2, false, false, true),
        "one racer per height"
    );
    assert!(
        !tip_stale_cover_should_rerace(true, 5_000, 0, 1, 2, false, false, false),
        "threshold 0 disables the re-race"
    );
    assert!(!tip_stale_cover_should_rerace(
        false, 5_000, th, 1, 2, false, false, false
    ));
    assert_eq!(
        tip_stale_cover_rerace_ms(),
        0,
        "R-274 dested 300ms: 3350s vs R-273 1580s. Default stays off."
    );
    IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    assert!(
        leftover_hole_needs_getdata(true, 91_698, 0, 2),
        "genesis leftover_force must assign (H,H) despite covering=2"
    );
    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    assert!(
        !leftover_hole_needs_getdata(true, 91_698, 0, 2),
        "genesis leftover_force must not stay (H,H) after tip lands"
    );
    assert!(
        !leftover_hole_needs_getdata(false, 91_698, 0, 2),
        "do not GetData (1,1) on spawn tip_gap without leftover_force"
    );
    assert!(
        !leftover_stall_skips_disk_load(70_678, 70_735),
        "leftover-band stall still loads disk"
    );
    assert!(
        !leftover_replay_stall_is_disk_hole(70705, 0),
        "no local-replay max — not leftover cheese"
    );
    assert!(!leftover_replay_stall_is_disk_hole(80000, 70735));
    assert!(local_ahead_start_within_window(70_257, 70_001));
    assert!(
        !local_ahead_start_within_window(70_657, 70_001),
        "leftover 70657 is past next+256 — must not assign"
    );
}

/// R-300: stall-gated hedge of next-needed H. N=1 is today's exclusive pipe.
#[serial_test::serial(ibd)]
#[test]
fn r300_tip_hedge_stays_inert_until_h_is_stale_on_a_distinct_delivering_peer() {
    test_tip_hedge_reset();
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HEDGE_N");
        std::env::remove_var("BLVM_IBD_TIP_HEDGE_MS");
    }
    assert_eq!(
        tip_hedge_n(),
        1,
        "unset BLVM_IBD_TIP_HEDGE_N must be 1 — R-298 exclusive-H, feature off"
    );
    assert!(
        !tip_hedge_should_fire(1, 10_000, 300, 1, false, false, true, 0),
        "N=1 is today's behavior — never issue a second GetData for H"
    );
    assert!(
        !tip_hedge_should_fire(3, 299, 300, 1, false, false, true, 0),
        "299ms is below the 300ms stall — hedging immediately is a bandwidth tax \
         on the common case where H arrives (R-280 asked H is slow, not unasked)"
    );
    assert!(
        !tip_hedge_should_fire(3, 800, 300, 1, true, false, true, 0),
        "sticky owner is the quiet pipe — a second GetData on that socket is A2/tc172, not a hedge"
    );
    assert!(
        !tip_hedge_should_fire(3, 800, 300, 1, false, true, true, 0),
        "peer already covering H must not duplicate itself"
    );
    assert!(
        !tip_hedge_should_fire(3, 800, 300, 1, false, false, false, 0),
        "pick by delivered bytes, not score — a zero-byte bench peer is the R-289 victim"
    );
    assert!(
        !tip_hedge_should_fire(3, 800, 300, 0, false, false, true, 0),
        "uncovered H is HOLE_ANY, not a hedge"
    );
    assert!(
        tip_hedge_should_fire(3, 800, 300, 1, false, false, true, 0),
        "H outstanding 800ms (R-280 store_absent p50 794ms) + N=3 + distinct \
         delivering peer that does not cover H — this is the hedge"
    );
    assert!(
        !tip_hedge_should_fire(3, 800, 300, 3, false, false, true, 2),
        "N=3 allows 2 extra racers; covering==n or issued==n-1 is the cap"
    );
    test_tip_hedge_reset();
}

#[serial_test::serial(ibd)]
#[test]
fn r259_leftover_w22_floor_is_248k_not_180k() {
    // Dump CHEESE @33 / dest-bc 0–10k 298: cursor ahead must stay W22-delivered.
    assert!(
        leftover_w22_cursor_is_delivered(33, Some(34), false, false, 0),
        "dump warehouse must not requeue on cursor-ahead"
    );
    assert!(!leftover_w22_cursor_lie(33, Some(34), false, false, 0));
    assert!(
        leftover_w22_cursor_is_delivered(3_112, Some(3_113), false, false, 0),
        "R-257 dump lie-shape @3112 must stay W22 (dest-bc 0-10k)"
    );
    assert!(!leftover_w22_cursor_lie(
        179_999,
        Some(180_000),
        false,
        false,
        0
    ));
    // R-258 180k floor dested dump occupancy FAIL (lie 0). Fat 181k stays W22.
    assert!(leftover_w22_cursor_is_delivered(
        181_000,
        Some(181_001),
        false,
        false,
        0
    ));
    assert!(!leftover_w22_cursor_lie(
        181_000,
        Some(181_001),
        false,
        false,
        0
    ));
    assert!(!leftover_w22_cursor_lie(
        195_669,
        Some(195_670),
        false,
        false,
        0
    ));
    assert!(!leftover_w22_cursor_lie(
        248_000,
        Some(248_001),
        false,
        false,
        0
    ));
    // R-245 restore: leftover cursor-ahead stays W22-delivered (R-257–R-261 dested).
    assert!(!leftover_w22_cursor_lie(
        300_000,
        Some(300_001),
        false,
        false,
        0
    ));
    assert!(leftover_w22_cursor_is_delivered(
        300_000,
        Some(300_001),
        false,
        false,
        0
    ));
    assert!(
        !leftover_w22_cursor_lie(248_000, Some(248_001), false, false, 0),
        "R-245 restore: no leftover W22 lie"
    );
    // Seated GetData: keep W22 (W26b / R-235 fat drip).
    assert!(!leftover_w22_cursor_lie(
        300_000,
        Some(300_001),
        false,
        false,
        1
    ));
    assert!(leftover_w22_cursor_is_delivered(
        300_000,
        Some(300_001),
        false,
        false,
        1
    ));
    // Validation already holds H.
    assert!(!leftover_w22_cursor_lie(
        300_000,
        Some(300_001),
        false,
        true,
        0
    ));
    assert!(leftover_w22_cursor_is_delivered(
        300_000,
        Some(300_001),
        false,
        true,
        0
    ));
    // Feeder has H.
    assert!(!leftover_w22_cursor_lie(
        300_000,
        Some(300_001),
        true,
        false,
        0
    ));
    assert!(leftover_w22_cursor_is_delivered(
        300_000,
        Some(300_001),
        true,
        false,
        0
    ));
    // Cursor not ahead: not delivered.
    assert!(!leftover_w22_cursor_is_delivered(
        300_000,
        Some(300_000),
        false,
        false,
        0
    ));
    assert!(!leftover_w22_cursor_lie(
        300_000,
        Some(300_000),
        false,
        false,
        0
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn c1f_tip_runway_mode_classifies_tip_hole_ahead() {
    assert_eq!(
        tip_runway_mode(false, 0, 64, 0, false),
        "TIP_HOLE_AHEAD",
        "holes=0 + ahead buffered + tip missing must not look like filled runway"
    );
    assert_eq!(tip_runway_mode(false, 0, 0, 0, false), "EMPTY_TIP");
    assert_eq!(tip_runway_mode(true, 32, 0, 0, false), "FILLED_RUNWAY");
    assert_eq!(tip_runway_mode(true, 8, 20, 12, false), "CHEESE");
    // C1q: tip in feeder + ahead buffered = filled runway (not TIP_HOLE_AHEAD).
    assert_eq!(
        tip_runway_mode(false, 0, 64, 0, true),
        "FILLED_RUNWAY",
        "tip in feeder must not be classified as tip hole"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn cheese_fast_hero_sparse_sit_keep_only() {
    // dest-ab @255073: ≥80 hero + holes=17 + first=+64 + reorder=3.
    assert!(cheese_fast_hero_sparse_sit(
        true,
        17,
        Some(255_137),
        255_073,
        true,
        3
    ));
    // dest-ac FAIL: same cheese, mute preferred — do not pin.
    assert!(!cheese_fast_hero_sparse_sit(
        true,
        17,
        Some(255_137),
        255_073,
        false,
        3
    ));
    // dest-ae FAIL: loose holes≥5 / first≥H+8 with reorder already ≥8
    // (dest-ab starve already owns that) or brief sparse.
    assert!(!cheese_fast_hero_sparse_sit(
        true,
        5,
        Some(457),
        449,
        true,
        3
    ));
    assert!(!cheese_fast_hero_sparse_sit(
        true,
        17,
        Some(255_137),
        255_073,
        true,
        8
    ));
    // Healthy first=H+1, holes=0.
    assert!(!cheese_fast_hero_sparse_sit(
        true,
        0,
        Some(450),
        449,
        true,
        3
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn wan_mute_cheese_fast_leftover_keep_hero_stays_45s() {
    // dest-ag @340221 / dest-ai @244590: mute + holes≥5 + WAN → 15s.
    // dest-ai leftover was 45s when IBD_TIP_GAP_MISSING was false.
    assert_eq!(
        wan_mute_cheese_fast_leftover_wait_secs(true, false, 21),
        Some(15)
    );
    assert_eq!(
        wan_mute_cheese_fast_leftover_wait_secs(true, false, 24),
        Some(15)
    );
    // dest-ab @255073 KEEP hero — do not steal leftover from autopsy pin.
    assert_eq!(
        wan_mute_cheese_fast_leftover_wait_secs(true, true, 17),
        None
    );
    // Healthy holes=0 mute crawl — leftover stays 45s, not 15s storm.
    assert_eq!(
        wan_mute_cheese_fast_leftover_wait_secs(true, false, 0),
        None
    );
}

#[serial_test::serial(ibd)]
#[test]
fn tip_nudge_skips_healthy_handoff_shapes() {
    // True TIP_HOLE_AHEAD / EMPTY_TIP — nudge allowed.
    assert!(tip_nudge_true_body_gap(false, false, false, false));
    // Healthy handoff: tip left reorder into feeder / bridge / validation.
    assert!(
        !tip_nudge_true_body_gap(false, true, false, false),
        "tip in feeder must not TIP_NUDGE"
    );
    assert!(
        !tip_nudge_true_body_gap(false, false, true, false),
        "tip in bridge pending must not TIP_NUDGE"
    );
    assert!(
        !tip_nudge_true_body_gap(false, false, false, true),
        "tip_taken must not TIP_NUDGE (dens: covering thrash)"
    );
    assert!(
        !tip_nudge_true_body_gap(true, false, false, false),
        "tip in reorder needs no nudge"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn pinned_ibd_peers_skips_archive_dns_seed() {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("BLVM_IBD_PEERS");
        std::env::remove_var("BLVM_IBD_PIN_PEERS");
    }
    assert!(!skip_ibd_archive_dns_seed());
    unsafe {
        std::env::set_var("BLVM_IBD_PEERS", "127.0.0.1:18333");
    }
    assert!(skip_ibd_archive_dns_seed());
    unsafe {
        std::env::remove_var("BLVM_IBD_PEERS");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1f_reorder_contig_runway_counts_from_tip() {
    use std::sync::Arc;
    let mut reorder: std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)> =
        std::collections::BTreeMap::new();
    let tip = 100u64;
    // Tip hole, ahead present — contig=0, ahead=2
    let dummy_block = Arc::new(Block {
        header: BlockHeader {
            version: 1,
            timestamp: 1,
            ..Default::default()
        },
        transactions: vec![].into(),
    });
    let dummy_w: SharedWitnesses = Arc::new(vec![]);
    reorder.insert(tip + 2, (dummy_block.clone(), dummy_w.clone()));
    reorder.insert(tip + 3, (dummy_block.clone(), dummy_w.clone()));
    assert_eq!(reorder_contig_runway(&reorder, tip), 0);
    assert_eq!(reorder_ahead_buffered(&reorder, tip), 2);
    assert_eq!(reorder_first_ahead(&reorder, tip), Some(tip + 2));
    // Fill tip..tip+1 → contiguous through tip+3 (already buffered).
    reorder.insert(tip, (dummy_block.clone(), dummy_w.clone()));
    reorder.insert(tip + 1, (dummy_block, dummy_w));
    assert_eq!(reorder_contig_runway(&reorder, tip), 4);
}

/// L2b lands the warehouse in the feeder. Hero hole must walk that have,
/// not stop at H+1 because reorder emptied (R-102 skip max 128, desert 151).
#[serial_test::serial(ibd)]
#[test]
fn r103_have_contig_walks_feeder_not_gap() {
    use std::sync::Arc;
    let mut reorder: std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)> =
        std::collections::BTreeMap::new();
    let dummy_block = Arc::new(Block {
        header: BlockHeader {
            version: 1,
            timestamp: 1,
            ..Default::default()
        },
        transactions: vec![].into(),
    });
    let dummy_w: SharedWitnesses = Arc::new(vec![]);
    let tip = 8254u64;
    // Stripe in feeder only (L2b emit). Gap at tip+2048.
    let feeder: std::collections::BTreeSet<u64> = (tip..tip + 2048).collect();
    assert_eq!(
        have_contig_runway(&reorder, |h| feeder.contains(&h), tip),
        2048
    );
    // Hole in the middle stops the walk. Inflight past the hole is not have.
    let feeder_gap: std::collections::BTreeSet<u64> =
        (tip..tip + 64).chain(tip + 128..tip + 2048).collect();
    assert_eq!(
        have_contig_runway(&reorder, |h| feeder_gap.contains(&h), tip),
        64
    );
    // Union: H in feeder, H+1.. in reorder.
    reorder.insert(tip + 1, (dummy_block.clone(), dummy_w.clone()));
    reorder.insert(tip + 2, (dummy_block, dummy_w));
    let feeder_tip: std::collections::BTreeSet<u64> = [tip].into_iter().collect();
    assert_eq!(
        have_contig_runway(&reorder, |h| feeder_tip.contains(&h), tip),
        3
    );
    // first_hole = tip + have; contig runway stays reorder-only (0 here).
    assert_eq!(reorder_contig_runway(&reorder, tip), 0);
}

/// Isolate tests from shell `BLVM_IBD_*` (e.g. left over from manual IBD runs).
fn with_ibd_env_cleared<F: FnOnce()>(f: F) {
    let peers = std::env::var("BLVM_IBD_PEERS").ok();
    let mode = std::env::var("BLVM_IBD_MODE").ok();
    let wan_single = std::env::var("BLVM_IBD_WAN_SINGLE_PEER").ok();
    unsafe {
        std::env::remove_var("BLVM_IBD_PEERS");
        std::env::remove_var("BLVM_IBD_MODE");
        std::env::remove_var("BLVM_IBD_WAN_SINGLE_PEER");
    }
    f();
    unsafe {
        if let Some(v) = peers {
            std::env::set_var("BLVM_IBD_PEERS", v);
        } else {
            std::env::remove_var("BLVM_IBD_PEERS");
        }
        if let Some(v) = mode {
            std::env::set_var("BLVM_IBD_MODE", v);
        } else {
            std::env::remove_var("BLVM_IBD_MODE");
        }
        if let Some(v) = wan_single {
            std::env::set_var("BLVM_IBD_WAN_SINGLE_PEER", v);
        } else {
            std::env::remove_var("BLVM_IBD_WAN_SINGLE_PEER");
        }
    }
}

/// N15: engine admit leaves tx_ids empty; legacy still fills.
#[serial_test::serial(ibd)]
#[test]
fn n15_prepare_coord_dispatch_defers_engine_txids() {
    use blvm_protocol::{Transaction, TransactionOutput};
    let block = Block {
        header: BlockHeader {
            version: 1,
            timestamp: 1,
            ..Default::default()
        },
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 50,
                script_pubkey: vec![0x51],
            }],
            lock_time: 0,
        }]
        .into(),
    };
    let mut tx_ids = vec![[9u8; 32]];
    let mut keys = vec![[1u8; 40]];
    prepare_coord_dispatch_bufs(true, &block, &mut tx_ids, &mut keys);
    assert!(tx_ids.is_empty(), "engine defer: no SHA on admit");
    assert!(keys.is_empty());
    // Validation-side fill matches non-empty hash count.
    compute_tx_ids_only(&block, &mut tx_ids);
    assert_eq!(tx_ids.len(), block.transactions.len());
}

#[serial_test::serial(ibd)]
#[test]
fn phase3_path_promotes_when_tip_ckpt_ready() {
    use crate::storage::ibd_engine::{Phase3Finish, phase3_path};
    assert_eq!(
        phase3_path(957_950, 957_950, 957_950, true),
        Phase3Finish::PromotedAlias
    );
}

#[serial_test::serial(ibd)]
#[test]
fn phase3_path_catchup_when_export_lags_tip() {
    use crate::storage::ibd_engine::{Phase3Finish, phase3_path};
    // Live soak: export_h=880k, tip=957950, nonempty ckpt at 880k.
    assert_eq!(
        phase3_path(880_000, 957_950, 880_000, true),
        Phase3Finish::CatchupThenAlias
    );
}

#[serial_test::serial(ibd)]
#[test]
fn phase3_path_full_when_no_ckpt() {
    use crate::storage::ibd_engine::{Phase3Finish, phase3_path};
    assert_eq!(
        phase3_path(0, 100_000, 0, false),
        Phase3Finish::FullWatermarkExport
    );
}

#[serial_test::serial(ibd)]
#[test]
fn export_isolation_inactive_when_export_not_running() {
    // Regardless of env, isolation cannot be "active" without an in-flight export.
    IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    assert!(!export_isolation_active());
}

fn engine_gap_export_defer_until_height_cases() {
    // Live zeus: wm=230k, start=230001, RAM replay cap=172791 < start → no defer.
    assert_eq!(
        engine_gap_export_defer_until_height(230_001, 172_791, 957_272),
        0
    );
    // Active local replay window: defer through min(bodies, tip).
    assert_eq!(
        engine_gap_export_defer_until_height(230_001, 657_030, 957_272),
        657_030
    );
    // Fresh start from genesis with RAM cap.
    assert_eq!(
        engine_gap_export_defer_until_height(1, 200_000, 500_000),
        200_000
    );
}

#[serial_test::serial(ibd)]
#[test]
fn bps_scaling_shrinks_interval_when_validation_is_slow() {
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 60,
        muhash_persist_interval: 200,
    };
    // Cheap last export → BPS may shrink for resume tightness.
    let utxo_iv = utxo_scaled_checkpoint_interval(640_068_968, 30.0, &d);
    assert_eq!(utxo_iv, 80_000);
    let slow_cap = bps_scaled_checkpoint_interval_cap(2.0, 60, 500, utxo_iv);
    let mid_cap = bps_scaled_checkpoint_interval_cap(16.0, 60, 500, utxo_iv);
    let fast_cap = bps_scaled_checkpoint_interval_cap(80.0, 60, 500, utxo_iv);
    assert_eq!(
        slow_cap, 500,
        "2 bps × 60s = 120, clamped to min_interval 500"
    );
    assert_eq!(mid_cap, 960, "16 bps × 60s");
    assert_eq!(fast_cap, 4800, "80 bps × 60s");
    assert!(slow_cap < mid_cap && mid_cap < fast_cap && fast_cap < utxo_iv);
    assert_eq!(
        adaptive_checkpoint_interval(640_068_968, 30.0, 16.0, &d),
        960,
        "cheap export + slow BPS → resume-tight interval"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn dest_be_tee_wall_must_not_bps_undercut_interval_to_1k() {
    // dest-be @223k: ~10.6M UTXOs, last wall 13.9s, validation ~20 BPS.
    // Old path: 20 × 60s = 1200 → LAG_EXEMPT every ~1k while compact is 15s.
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 60,
        muhash_persist_interval: 200,
    };
    let iv = adaptive_checkpoint_interval(10_615_659, 13.9, 20.0, &d);
    assert!(
        iv >= 10_000,
        "dest-be 13.9s tee must keep utxo_iv ≥10k, got {iv}"
    );
    // First real 1→1 tee @184k: 2.88M UTXOs, 8.3s wall, ~135 BPS.
    let iv2 = adaptive_checkpoint_interval(2_883_000, 8.3, 135.0, &d);
    assert!(
        iv2 >= 10_000,
        "dest-be 8.3s tee must keep utxo_iv ≥10k, got {iv2}"
    );
    // dest-bc-like no-op compact: sub-2s overlay may still BPS-cap.
    let cheap = adaptive_checkpoint_interval(2_883_000, 0.5, 20.0, &d);
    assert_eq!(
        cheap, 1_200,
        "sub-2s overlay may still BPS-cap to 20×60s, got {cheap}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn dest_bd_sit_sample_bps_must_not_collapse_interval_to_500() {
    // dest-bd @83k: overlay ~18ms, sit sample bps=5.9, utxo_iv=10000.
    // Old path: 5.9×60s → min_interval 500; LAG_EXEMPT aligned last+500.
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 60,
        muhash_persist_interval: 200,
    };
    let iv = adaptive_checkpoint_interval(1_200_000, 0.018, 5.9, &d);
    assert!(
        iv >= 10_000,
        "dest-bd sit-sample 5.9 BPS overlay must keep utxo_iv ≥10k, got {iv}"
    );
    assert_eq!(
        checkpoint_schedule_interval(500, 10_000, 1_200_000),
        10_000,
        "collapsed 500 must not be the genesis schedule step"
    );
    assert_eq!(
        checkpoint_schedule_interval(960, 80_000, 640_068_968),
        960,
        "640M resume tightness still uses BPS-capped interval"
    );
    assert_eq!(adopt_checkpoint_bps_sample(55.3, 5.9), 55.3);
    assert_eq!(adopt_checkpoint_bps_sample(0.0, 5.9), 5.9);
    assert_eq!(adopt_checkpoint_bps_sample(55.3, 80.0), 80.0);
    // dest-bc-like overlay 20 still replaces a non-burst prev.
    assert_eq!(adopt_checkpoint_bps_sample(55.3, 20.0), 20.0);
}

#[serial_test::serial(ibd)]
#[test]
fn dest_be_high_utxo_tee_wall_must_not_bps_undercut_to_500() {
    // dest-be @345853: crossed 40M UTXOs, last wall ~30s, sample 0.1 BPS
    // during compact → old path min_interval 500 while utxo_iv=50k.
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 60,
        muhash_persist_interval: 200,
    };
    let iv = adaptive_checkpoint_interval(49_170_639, 30.1, 0.1, &d);
    assert_eq!(
        iv, 50_000,
        "≥40M + 30s tee must keep high-UTXO ceiling, got {iv}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn slice_a_448m_journal_must_shrink_50k_interval_below_allcold_budget() {
    // Slice A: dest-be floor held interval=50000 after 81M @399546. Next dump
    // ingested 447_868_337. 313M TeeScan returned in 19 min; 448M was still
    // grinding at the 20 min disk stop. Shrink the *ceiling*, keep dest-be 10k floor.
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 60,
        muhash_persist_interval: 200,
    };
    let adaptive = adaptive_checkpoint_interval(81_263_724, 1252.0, 19.2, &d);
    assert_eq!(
        adaptive, 50_000,
        "HIGH_UTXO + 21 min tee still *ceilings* at 50k"
    );
    let sched = checkpoint_schedule_interval(adaptive, 50_000, 81_263_724);
    assert_eq!(
        sched, 50_000,
        "schedule step was the dest-be HIGH_UTXO ceiling"
    );
    let floor = d.checkpoint_min_interval.max(DEST_BE_INTERVAL_FLOOR);
    assert_eq!(floor, 10_000);
    assert_eq!(
        journal_scaled_checkpoint_interval(sched, 0, floor),
        50_000,
        "empty journal must not shrink (dest-be high-UTXO path intact)"
    );
    assert_eq!(
        journal_scaled_checkpoint_interval(sched, CHECKPOINT_COMPACT_INPUT_TARGET, floor),
        50_000,
        "at the AllCold budget the 50k ceiling may hold"
    );
    let at_313m = journal_scaled_checkpoint_interval(sched, 313_222_168, floor);
    assert!(
        at_313m < 50_000 && at_313m >= floor,
        "313M (returned TeeScan) must shrink below 50k, got {at_313m}"
    );
    let at_448m = journal_scaled_checkpoint_interval(sched, 447_868_337, floor);
    assert!(
        at_448m < 50_000 && at_448m >= floor,
        "448M (Slice A hang input) must shrink below 50k, got {at_448m}"
    );
    assert!(
        at_448m <= at_313m,
        "larger journal must not lengthen the interval ({at_448m} > {at_313m})"
    );
    assert_eq!(
        journal_scaled_checkpoint_interval(sched, 2_000_000_000, floor),
        floor,
        "unbounded journal clamps to dest-be floor, not dest-bd 500"
    );
    // dest-be sit-sample collapse still blocked when journal is small.
    let iv_sit = adaptive_checkpoint_interval(1_200_000, 0.018, 5.9, &d);
    assert!(iv_sit >= 10_000);
    assert_eq!(
        journal_scaled_checkpoint_interval(iv_sit, 1_000_000, floor),
        iv_sit
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w173_expensive_midchain_export_keeps_sparse_interval() {
    // Live W173: TARGET_SECS=300, ~50M UTXOs, 90–208s piggyback walls, tip60~80–100.
    // Old scaler: BASE*25M/count → ~5k, duration scale never fired (175 < 300),
    // BPS min() kept ~5k → 10 full exports in ~26 min.
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 300,
        muhash_persist_interval: 200,
    };
    let utxo_iv = utxo_scaled_checkpoint_interval(50_000_000, 175.0, &d);
    assert_eq!(utxo_iv, 50_000, "≥40M UTXOs → high-UTXO ceiling");
    let adaptive = adaptive_checkpoint_interval(50_000_000, 175.0, 80.0, &d);
    assert_eq!(
        adaptive, 50_000,
        "expensive export must not be undercut by BPS×target (80×300=24k)"
    );
    // Below HIGH threshold: interval grows with UTXO count (never shrinks).
    let early = utxo_scaled_checkpoint_interval(30_000_000, 100.0, &d);
    assert!(
        early >= 20_000,
        "30M UTXOs + 100s export must stay sparse, got {early}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w175_restored_midchain_export_wall_counts_expensive() {
    // Live W174: restored last_export_wall_secs=81, utxos≈25.6M, TARGET=300.
    // Threshold was min(target,90)=90 → 81 treated cheap → BPS interval 7890.
    let d = crate::config::ibd::IbdEngineDurabilityConfig {
        checkpoint_interval: None,
        checkpoint_min_interval: 500,
        checkpoint_max_interval: 50_000,
        checkpoint_target_secs: 300,
        muhash_persist_interval: 200,
    };
    assert_eq!(export_cost_scale_threshold_secs(300), 60.0);
    let adaptive = adaptive_checkpoint_interval(25_643_324, 81.0, 26.3, &d);
    assert!(
        adaptive >= 20_000,
        "restored 81s wall must not be BPS-undercut to ~7.8k, got {adaptive}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn aligned_checkpoint_height_steps_from_last_exported() {
    // Live soak: export_h=880000, 80k global alignment missed 931k; relative 960 iv catches up.
    assert_eq!(aligned_checkpoint_height(931_000, 880_000, 80_000), 880_000);
    assert_eq!(aligned_checkpoint_height(931_000, 880_000, 960), 930_880);
    assert_eq!(aligned_checkpoint_height(880_960, 880_000, 960), 880_960);
    assert_eq!(aligned_checkpoint_height(880_959, 880_000, 960), 880_000);
}

#[serial_test::serial(ibd)]
#[test]
fn dest_x_vs_persist_180k_export_alignment() {
    // dest-bf…bl: last_exported on the 10k grid after schedule clamp. Catch-up
    // from 110000 at cl=180000 is exactly 180000 (dest-bf first in-band export).
    assert_eq!(aligned_checkpoint_height(180_000, 110_000, 10_000), 180_000);
    assert_eq!(aligned_checkpoint_height(179_999, 110_000, 10_000), 170_000);
    // dest-x: last_exported=30336, live interval 3465 (pre-clamp). Catch-up at
    // cl=180k is 179331, not 180000. First export after the skip was
    // 213981 = 30336 + 53×3465 at engine_height=215563.
    assert_eq!(aligned_checkpoint_height(180_000, 30_336, 3_465), 179_331);
    assert_eq!(aligned_checkpoint_height(215_563, 30_336, 3_465), 213_981);
    // dest-bd offset 10k grid missed 180000 (next 185840). Local max ts 164.
    assert_eq!(aligned_checkpoint_height(180_000, 175_840, 10_000), 175_840);
    assert_eq!(aligned_checkpoint_height(185_840, 175_840, 10_000), 185_840);
}

#[serial_test::serial(ibd)]
#[test]
fn dest_x_burst_bps_defers_export_dest_bi_197_does_not() {
    // dest-x 180–200k ts 247 and dest-bc 741: 30s EMA stays in the burst bin.
    // dest-x 180k IBD print 164.5 is a 1k window; the gate is the EMA.
    assert!(checkpoint_export_defer_for_burst_bps(247.0));
    assert!(checkpoint_export_defer_for_burst_bps(556.0));
    assert!(checkpoint_export_defer_for_burst_bps(200.1));
    assert!(!checkpoint_export_defer_for_burst_bps(200.0));
    // dest-bi @180000 logged bps=197.3 (just under); dest-bf logged 29.6.
    assert!(!checkpoint_export_defer_for_burst_bps(197.3));
    assert!(!checkpoint_export_defer_for_burst_bps(29.6));
}

#[serial_test::serial(ibd)]
#[test]
fn dest_bl_180k_sit_must_not_clear_w75_burst_ema() {
    // dest-bl 178k inst 541 then sit 29.2 used to release 180000. Burst EMA
    // must survive the sit so W75 keeps deferring — dest-x 180–200k
    // export_on=0. dest-bi 197.3 is a real leave-burst and still replaces.
    assert_eq!(aligned_checkpoint_height(180_126, 170_000, 10_000), 180_000);
    assert_eq!(adopt_checkpoint_bps_sample(541.6, 29.2), 541.6);
    assert!(checkpoint_export_defer_for_burst_bps(
        adopt_checkpoint_bps_sample(541.6, 29.2)
    ));
    assert_eq!(adopt_checkpoint_bps_sample(387.0, 29.6), 387.0);
    assert!(checkpoint_export_defer_for_burst_bps(
        adopt_checkpoint_bps_sample(387.0, 29.6)
    ));
    assert_eq!(adopt_checkpoint_bps_sample(247.0, 197.3), 197.3);
    assert!(!checkpoint_export_defer_for_burst_bps(
        adopt_checkpoint_bps_sample(247.0, 197.3)
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn apply4_sit_21_8_must_not_clear_w75_burst_ema() {
    // Apply 4: IBD 190000 → export 130s, engine_height=191859, bps=21.8.
    // 181–190k 10k-print was 192; W75 only stays on if the 30s EMA is >200
    // (do not change checkpoint_export_defer_for_burst_bps). Sit 21.8 must
    // not replace that burst EMA — dump on covering=0 was the leak.
    assert_eq!(adopt_checkpoint_bps_sample(247.0, 21.8), 247.0);
    assert!(checkpoint_export_defer_for_burst_bps(
        adopt_checkpoint_bps_sample(247.0, 21.8)
    ));
    // Apply 5 prev ~73 is not burst: sit 20.4 still adopted, dump proceeds.
    assert_eq!(adopt_checkpoint_bps_sample(73.0, 20.4), 20.4);
    assert!(!checkpoint_export_defer_for_burst_bps(
        adopt_checkpoint_bps_sample(73.0, 20.4)
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn apply6_resume_must_not_count_bps_from_height_zero() {
    // Apply 6: export thread origin 0 + resume vh=180000 → first 30s sample
    // 180000/30 ≈6000, sit-keep froze W75. Arm origin at current vh.
    assert_eq!(checkpoint_bps_arm_sample_origin(0, 180_000), Some(180_000));
    assert_eq!(checkpoint_bps_arm_sample_origin(180_000, 180_500), None);
    assert_eq!(checkpoint_bps_arm_sample_origin(0, 0), None);
    let fake = 180_000.0 / 30.0;
    assert!(fake > CHECKPOINT_EXPORT_BURST_BPS);
    assert_eq!(adopt_checkpoint_bps_sample(fake, 21.8), fake);
}

#[serial_test::serial(ibd)]
#[test]
fn checkpoint_export_requires_validation_caught_up() {
    // Live 2026-07-14: CL claimed 49716 while vh was ~5800 — must not export 40000.
    assert!(!checkpoint_export_validation_caught_up(40_000, 5_800));
    assert!(!checkpoint_export_validation_caught_up(40_000, 39_999));
    assert!(checkpoint_export_validation_caught_up(40_000, 40_000));
    assert!(checkpoint_export_validation_caught_up(40_000, 48_702));
    assert!(!checkpoint_export_validation_caught_up(0, 100));
}

#[serial_test::serial(ibd)]
#[test]
fn w75_tip_gap_body_in_pipeline_requires_pending_or_feeder() {
    // Live 344348: bridge_next==tip with pending=0 must fall through to Case C.
    // W78: second arg is tip_in_feeder (bool), not feeder_len.
    assert!(!tip_gap_body_in_pipeline(false, false));
    assert!(tip_gap_body_in_pipeline(true, false));
    assert!(tip_gap_body_in_pipeline(false, true));
    assert!(tip_gap_body_in_pipeline(true, true));
}

#[serial_test::serial(ibd)]
#[test]
fn w78_feeder_len_alone_is_not_in_pipeline() {
    // Live 381335: feeder=46 / gap_missing / bridge_next>>tip — must not short-circuit.
    assert!(
        !tip_gap_body_in_pipeline(false, false),
        "occupancy without tip key must fall through to Case C / TIP_REWIND"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w79_export_gate_steady_state_ok_and_stall_defers() {
    // Live genesis→250k: gap_missing+feeder=0 forever under W75 → zero exports.
    // Single test: shared atomics race if split across threads.
    let prev_kill = std::env::var_os("BLVM_PROC_ANON_KILL_MB");
    // SAFETY: test-only env mutation; restored below.
    unsafe {
        std::env::set_var("BLVM_PROC_ANON_KILL_MB", "999999999");
    }
    IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    IBD_FEEDER_BUFFER_CAP.store(128, Ordering::Relaxed);
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    tip_stage::clear_tip_ahead_soft_freeze();
    tip_stage::mark_needed(9_000_001);
    assert!(
        export_start_gate_allows(),
        "healthy WAN tip crawl must allow periodic checkpoint export"
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    IBD_VALIDATION_STALL_WALL_MS.store(now_ms, Ordering::Relaxed);
    assert!(!export_start_gate_allows());
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    assert!(export_start_gate_allows());

    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    IBD_FEEDER_BUFFER_BLOCKS.store(64, Ordering::Relaxed);
    // SAFETY: restore prior test env.
    unsafe {
        match prev_kill {
            Some(v) => std::env::set_var("BLVM_PROC_ANON_KILL_MB", v),
            None => std::env::remove_var("BLVM_PROC_ANON_KILL_MB"),
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w174_export_gate_defers_on_severe_tip_holes() {
    let prev_kill = std::env::var_os("BLVM_PROC_ANON_KILL_MB");
    unsafe {
        std::env::set_var("BLVM_PROC_ANON_KILL_MB", "999999999");
    }
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    tip_stage::clear_tip_ahead_soft_freeze();
    tip_stage::mark_needed(9_000_002);
    // Fresh mark_needed → awaiting≈0 so W176 awaiting≥5 path stays off.
    IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    IBD_TIP_BRIDGE_HOLES.store(15, Ordering::Relaxed);
    assert!(
        export_start_gate_allows(),
        "holes=15 must still allow export (W176 threshold 16)"
    );
    IBD_TIP_BRIDGE_HOLES.store(16, Ordering::Relaxed);
    assert!(
        !export_start_gate_allows(),
        "holes≥16 + gap_missing must defer export (W176; was 32)"
    );
    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    assert!(
        export_start_gate_allows(),
        "holes alone without gap_missing must not defer"
    );
    IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    unsafe {
        match prev_kill {
            Some(v) => std::env::set_var("BLVM_PROC_ANON_KILL_MB", v),
            None => std::env::remove_var("BLVM_PROC_ANON_KILL_MB"),
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w176_export_gate_defers_when_tip_already_awaiting() {
    let prev_kill = std::env::var_os("BLVM_PROC_ANON_KILL_MB");
    unsafe {
        std::env::set_var("BLVM_PROC_ANON_KILL_MB", "999999999");
    }
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    tip_stage::clear_tip_ahead_soft_freeze();
    tip_stage::mark_needed(9_000_003);
    tip_stage::test_backdate_awaiting_ms(6_000);
    IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert!(
        !export_start_gate_allows(),
        "gap_missing + awaiting≥5s must defer export (W176)"
    );
    // Body landed → late-body freeze clears; gap_missing false → awaiting gate off.
    tip_stage::mark_body(9_000_003);
    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    assert!(
        export_start_gate_allows(),
        "healthy tip (body landed, no gap) must allow export"
    );
    tip_stage::mark_needed(0);
    unsafe {
        match prev_kill {
            Some(v) => std::env::set_var("BLVM_PROC_ANON_KILL_MB", v),
            None => std::env::remove_var("BLVM_PROC_ANON_KILL_MB"),
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w177_export_gate_defers_during_local_body_ahead() {
    let prev_kill = std::env::var_os("BLVM_PROC_ANON_KILL_MB");
    unsafe {
        std::env::set_var("BLVM_PROC_ANON_KILL_MB", "999999999");
    }
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    tip_stage::clear_tip_ahead_soft_freeze();
    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    IBD_LOCAL_BODY_AHEAD.store(true, Ordering::Relaxed);
    assert!(
        !export_start_gate_allows(),
        "local body ahead must defer export (W177 soft-resume)"
    );
    IBD_LOCAL_BODY_AHEAD.store(false, Ordering::Relaxed);
    assert!(
        export_start_gate_allows(),
        "past body tip must allow export when tip healthy"
    );
    unsafe {
        match prev_kill {
            Some(v) => std::env::set_var("BLVM_PROC_ANON_KILL_MB", v),
            None => std::env::remove_var("BLVM_PROC_ANON_KILL_MB"),
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn lag_exempt_skips_w176_when_validation_past_interval() {
    let prev_kill = std::env::var_os("BLVM_PROC_ANON_KILL_MB");
    unsafe {
        std::env::set_var("BLVM_PROC_ANON_KILL_MB", "999999999");
    }
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    tip_stage::clear_tip_ahead_soft_freeze();
    IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    IBD_TIP_BRIDGE_HOLES.store(16, Ordering::Relaxed);
    IBD_LOCAL_BODY_AHEAD.store(false, Ordering::Relaxed);
    assert!(
        !export_start_gate_allows(),
        "W176 still defers when not lag-exempt"
    );
    assert!(
        export_start_gate_allows_at(614_973, 594_973, 20_000),
        "vh-last_exported >= interval must skip W176/stall (dest-bc 96s)"
    );
    assert!(
        !export_start_gate_allows_at(351_353, 345_853, 500),
        "collapsed 500-block interval must not LAG_EXEMPT at 5.5k lag"
    );
    assert!(
        export_start_gate_allows_at(365_853, 345_853, 500),
        "20k lag still LAG_EXEMPT when interval collapsed to 500"
    );
    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    unsafe {
        match prev_kill {
            Some(v) => std::env::set_var("BLVM_PROC_ANON_KILL_MB", v),
            None => std::env::remove_var("BLVM_PROC_ANON_KILL_MB"),
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn lag_exempt_overrides_critical_pressure() {
    let prev_kill = std::env::var_os("BLVM_PROC_ANON_KILL_MB");
    let prev_force = std::env::var_os("BLVM_IBD_FORCE_PRESSURE");
    unsafe {
        std::env::set_var("BLVM_PROC_ANON_KILL_MB", "999999999");
        std::env::remove_var("BLVM_IBD_FORCE_PRESSURE");
    }
    IBD_VALIDATION_STALL_WALL_MS.store(0, Ordering::Relaxed);
    tip_stage::clear_tip_ahead_soft_freeze();
    IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    IBD_LOCAL_BODY_AHEAD.store(false, Ordering::Relaxed);
    memory::publish_ibd_pressure(memory::PressureLevel::Critical);
    assert!(
        !export_start_gate_allows_at(10_000, 0, 10_000),
        "Critical must still refuse when lag is below LAG_EXEMPT_MIN"
    );
    assert!(
        export_start_gate_allows_at(25_000, 0, 10_000),
        "LAG_EXEMPT must start an export even when pressure is Critical"
    );
    memory::publish_ibd_pressure(memory::PressureLevel::None);
    unsafe {
        match prev_kill {
            Some(v) => std::env::set_var("BLVM_PROC_ANON_KILL_MB", v),
            None => std::env::remove_var("BLVM_PROC_ANON_KILL_MB"),
        }
        match prev_force {
            Some(v) => std::env::set_var("BLVM_IBD_FORCE_PRESSURE", v),
            None => std::env::remove_var("BLVM_IBD_FORCE_PRESSURE"),
        }
    }
}

#[serial_test::serial(ibd)]
#[test]
fn ibd_block_flush_opts_default_enables_parallel_serialize() {
    let opts = IbdBlockFlushOpts::default();
    assert!(opts.parallel_serialize);
    assert!(!opts.log_progress);
}

#[serial_test::serial(ibd)]
#[test]
fn ibd_block_flush_opts_shutdown_sync_is_serial_with_progress() {
    let opts = IbdBlockFlushOpts::shutdown_sync();
    assert!(!opts.parallel_serialize);
    assert!(opts.log_progress);
}

#[serial_test::serial(ibd)]
#[test]
fn test_parallel_ibd_config_default() {
    let config = ParallelIBDConfig::default();
    assert!(config.num_workers > 0);
    // chunk_size: 128 default, or BLVM_IBD_CHUNK_SIZE (16-2000) if set
    assert!(
        config.chunk_size >= 16 && config.chunk_size <= 2000,
        "chunk_size={}",
        config.chunk_size
    );
    assert_eq!(config.max_concurrent_per_peer, 64);
}

#[serial_test::serial(ibd)]
#[test]
fn empty_blvm_ibd_peers_env_allows_auto_lan() {
    with_ibd_env_cleared(|| {
        unsafe {
            std::env::set_var("BLVM_IBD_PEERS", "");
        }
        let peers = vec!["192.168.2.100:8333".to_string(), "8.8.8.8:8333".to_string()];
        let config = ParallelIBDConfig::resolve_for_session(None, 0, &peers);
        assert_eq!(config.preferred_peers, vec!["192.168.2.100:8333"]);
    });
}

#[serial_test::serial(ibd)]
#[test]
fn wan_multi_peer_keeps_all_peers_by_default() {
    let peers = vec!["8.8.8.8:8333".to_string(), "1.1.1.1:8333".to_string()];
    let out = ParallelIBDConfig::collapse_wan_only_download_peers(peers);
    assert_eq!(out.len(), 2);
}

#[serial_test::serial(ibd)]
#[test]
fn collapse_keeps_multi_peer_when_lan_present() {
    let peers = vec!["192.168.1.1:8333".to_string(), "8.8.8.8:8333".to_string()];
    let out = ParallelIBDConfig::collapse_wan_only_download_peers(peers);
    assert_eq!(out.len(), 2);
}

#[serial_test::serial(ibd)]
#[test]
fn resolve_wan_only_keeps_parallel_mode() {
    with_ibd_env_cleared(|| {
        let peers = vec!["8.8.8.8:8333".to_string(), "1.1.1.1:8333".to_string()];
        let config = ParallelIBDConfig::resolve_for_session(None, 100_000, &peers);
        assert_eq!(config.mode, "parallel");
        assert!(config.preferred_peers.is_empty());
        assert_eq!(config.min_peers_for_ibd(), 1);
    });
}

#[serial_test::serial(ibd)]
#[test]
fn resolve_auto_prefers_lan_peers() {
    with_ibd_env_cleared(|| {
        let peers = vec!["192.168.2.100:8333".to_string(), "8.8.8.8:8333".to_string()];
        let config = ParallelIBDConfig::resolve_for_session(None, 100_000, &peers);
        assert_eq!(config.preferred_peers, vec!["192.168.2.100:8333"]);
        assert_eq!(config.min_peers_for_ibd(), 1);
    });
}

#[serial_test::serial(ibd)]
#[test]
fn filter_ibd_download_peers_falls_back_when_none_connected() {
    let preferred = vec!["192.168.1.10:8333".to_string()];
    let connected = vec!["8.8.8.8:8333".to_string(), "1.1.1.1:8333".to_string()];
    let out = super::filter_ibd_download_peers(&preferred, connected.clone());
    assert_eq!(out, connected);
}

#[serial_test::serial(ibd)]
#[test]
fn filter_ibd_download_peers_falls_back_when_only_one_preferred_connected() {
    let preferred = vec![
        "66.45.230.178:8333".to_string(),
        "63.254.176.191:8333".to_string(),
    ];
    let connected = vec![
        "66.45.230.178:8333".to_string(),
        "172.105.25.248:8333".to_string(),
        "99.56.151.125:8333".to_string(),
    ];
    let out = super::filter_ibd_download_peers(&preferred, connected.clone());
    assert_eq!(out, connected);
}

#[serial_test::serial(ibd)]
#[test]
fn filter_ibd_download_peers_matches_host_without_port() {
    let preferred = vec!["192.168.1.10".to_string(), "192.168.1.11".to_string()];
    let connected = vec![
        "192.168.1.10:8333".to_string(),
        "192.168.1.11:8333".to_string(),
        "8.8.8.8:8333".to_string(),
    ];
    let out = super::filter_ibd_download_peers(&preferred, connected);
    assert_eq!(
        out,
        vec![
            "192.168.1.10:8333".to_string(),
            "192.168.1.11:8333".to_string()
        ]
    );
}

#[serial_test::serial(ibd)]
#[test]
fn resolve_fresh_chain_keeps_parallel_mode() {
    with_ibd_env_cleared(|| {
        let peers = vec!["192.168.1.1:8333".to_string()];
        let config = ParallelIBDConfig::resolve_for_session(None, 0, &peers);
        assert_eq!(config.mode, "parallel");
    });
}

#[serial_test::serial(ibd)]
#[test]
fn test_create_chunks() {
    let config = ParallelIBDConfig {
        chunk_size: 100,
        ..Default::default()
    };
    let ibd = ParallelIBD::new(config);
    let peer_ids = vec!["peer1".to_string(), "peer2".to_string()];

    let chunks = ibd.create_chunks(0, 250, &peer_ids, None);

    // Bootstrap chunk is always ≥128 blocks so 99 and 100 are in same chunk (stall fix)
    assert_eq!(chunks.len(), 3); // 0-127, 128-227, 228-250
    assert_eq!(chunks[0].start_height, 0);
    assert_eq!(
        chunks[0].end_height, 127,
        "Bootstrap chunk must include 99 and 100"
    );
    assert_eq!(chunks[1].start_height, 128);
    assert_eq!(chunks[1].end_height, 227);
    assert_eq!(chunks[2].start_height, 228);
    assert_eq!(chunks[2].end_height, 250);

    // Note: With weighted assignment, peer selection depends on scores
    // All peers have equal score (1.0) by default, so they get equal chunks
    // Just verify all chunks have a valid peer assigned
    for chunk in &chunks {
        assert!(
            peer_ids.contains(&chunk.peer_id),
            "Chunk should be assigned to a valid peer, got: {}",
            chunk.peer_id
        );
    }
}

/// Ensures bootstrap chunk includes both block 99 and 100 — prevents stall at 99.
#[serial_test::serial(ibd)]
#[test]
fn test_bootstrap_chunk_includes_99_and_100() {
    let config = ParallelIBDConfig {
        chunk_size: 16, // Small chunk_size would normally put 99/100 in different chunks
        ..Default::default()
    };
    let ibd = ParallelIBD::new(config);
    let peer_ids = vec!["peer1".to_string()];
    let chunks = ibd.create_chunks(0, 500, &peer_ids, None);
    assert!(!chunks.is_empty(), "Must have at least one chunk");
    let bootstrap = &chunks[0];
    assert!(
        bootstrap.end_height >= 100,
        "Bootstrap chunk must include block 100 (end={})",
        bootstrap.end_height
    );
    assert!(
        bootstrap.start_height <= 99,
        "Bootstrap chunk must include block 99 (start={})",
        bootstrap.start_height
    );
}

// Regression: chunk queue must drain in height order (FIFO). Vec::pop would yield highest
// heights first and break sequential validation.

#[serial_test::serial(ibd)]
#[test]
fn test_work_queue_fifo_order_not_lifo() {
    // Queue uses VecDeque::pop_front — lowest-height chunk leaves first.

    // Simulate the work queue as created in sync_parallel
    let chunks: Vec<(u64, u64, Option<String>)> = vec![
        (0u64, 99u64, None),
        (100u64, 199u64, None),
        (200u64, 299u64, None),
        (931000u64, 931099u64, None),
    ];

    let mut work_queue: VecDeque<(u64, u64, Option<String>)> = chunks.into_iter().collect();

    // Verify FIFO order (first chunk in = first chunk out)
    let (s, e, _) = work_queue.pop_front().unwrap();
    assert_eq!((s, e), (0, 99), "First chunk should be (0, 99)");

    let (s, e, _) = work_queue.pop_front().unwrap();
    assert_eq!((s, e), (100, 199), "Second chunk should be (100, 199)");

    let (s, e, _) = work_queue.pop_front().unwrap();
    assert_eq!((s, e), (200, 299), "Third chunk should be (200, 299)");

    let (s, e, _) = work_queue.pop_front().unwrap();
    assert_eq!(
        (s, e),
        (931000, 931099),
        "Fourth chunk should be the high-height chunk"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_vec_pop_is_lifo_bug() {
    // Vec::pop takes from the end — wrong order if used as a download work queue.

    let mut vec_queue: Vec<(u64, u64)> = vec![(0, 99), (100, 199), (200, 299)];

    let popped = vec_queue.pop().unwrap();
    assert_eq!(
        popped,
        (200, 299),
        "Vec::pop() returns LAST element (LIFO behavior)"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_vecdeque_pop_front_is_fifo_correct() {
    let mut deque_queue: VecDeque<(u64, u64, Option<String>)> =
        VecDeque::from(vec![(0, 99, None), (100, 199, None), (200, 299, None)]);

    let (s, e, _) = deque_queue.pop_front().unwrap();
    assert_eq!(
        (s, e),
        (0, 99),
        "VecDeque::pop_front() returns FIRST element (FIFO behavior)"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_failed_chunk_requeue_excludes_failing_peer() {
    // Verify that failed chunks are re-queued with exclude_peer so a DIFFERENT peer retries.
    // Same peer retrying would likely fail again (e.g. disconnected).

    let mut work_queue: VecDeque<(u64, u64, Option<String>)> =
        VecDeque::from(vec![(100, 199, None), (200, 299, None)]);

    // Simulate peer "flaky:8333" failing chunk 0-99 - re-queue with exclude
    work_queue.push_front((0, 99, Some("flaky:8333".to_string())));

    let (start, end, exclude) = work_queue.pop_front().unwrap();
    assert_eq!((start, end), (0, 99));
    assert_eq!(exclude.as_deref(), Some("flaky:8333"));
    // Worker for flaky:8333 would skip this; worker for other peer would take it
}

// ============================================================
// Chunk Creation Order Tests
// ============================================================

#[serial_test::serial(ibd)]
#[test]
fn test_chunks_created_in_ascending_height_order() {
    let config = ParallelIBDConfig {
        chunk_size: 1000,
        ..Default::default()
    };
    let ibd = ParallelIBD::new(config);
    let peer_ids = vec!["peer1".to_string()];

    let chunks = ibd.create_chunks(0, 10000, &peer_ids, None);

    // Verify chunks are in ascending order
    for i in 1..chunks.len() {
        assert!(
            chunks[i].start_height > chunks[i - 1].start_height,
            "Chunk {} start ({}) should be > chunk {} start ({})",
            i,
            chunks[i].start_height,
            i - 1,
            chunks[i - 1].start_height
        );
        assert!(
            chunks[i].start_height == chunks[i - 1].end_height + 1,
            "Chunk {} start ({}) should immediately follow chunk {} end ({})",
            i,
            chunks[i].start_height,
            i - 1,
            chunks[i - 1].end_height
        );
    }

    // First chunk must start at 0
    assert_eq!(
        chunks[0].start_height, 0,
        "First chunk must start at height 0"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_create_chunks_covers_full_range() {
    let config = ParallelIBDConfig {
        chunk_size: 500,
        ..Default::default()
    };
    let ibd = ParallelIBD::new(config);
    let peer_ids = vec!["peer1".to_string(), "peer2".to_string()];

    let start = 0u64;
    let end = 935000u64; // Approximate mainnet height
    let chunks = ibd.create_chunks(start, end, &peer_ids, None);

    // First chunk starts at start
    assert_eq!(chunks.first().unwrap().start_height, start);

    // Last chunk ends at or after end
    assert!(chunks.last().unwrap().end_height >= end);

    // No gaps between chunks
    for i in 1..chunks.len() {
        assert_eq!(
            chunks[i].start_height,
            chunks[i - 1].end_height + 1,
            "Gap detected between chunk {} and {}",
            i - 1,
            i
        );
    }
}

// ============================================================
// Checkpoint Tests
// ============================================================

#[serial_test::serial(ibd)]
#[test]
fn test_mainnet_checkpoints_exist() {
    assert_ne!(
        checkpoints::MAINNET_CHECKPOINTS.len(),
        0,
        "Checkpoints should be defined"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_mainnet_checkpoints_start_at_genesis() {
    let (height, _hash) = checkpoints::MAINNET_CHECKPOINTS[0];
    assert_eq!(
        height, 0,
        "First checkpoint should be genesis block (height 0)"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_mainnet_checkpoints_in_ascending_order() {
    for i in 1..checkpoints::MAINNET_CHECKPOINTS.len() {
        let (prev_height, _) = checkpoints::MAINNET_CHECKPOINTS[i - 1];
        let (curr_height, _) = checkpoints::MAINNET_CHECKPOINTS[i];
        assert!(
            curr_height > prev_height,
            "Checkpoint {} (height {}) should be > checkpoint {} (height {})",
            i,
            curr_height,
            i - 1,
            prev_height
        );
    }
}

#[serial_test::serial(ibd)]
#[test]
fn test_mainnet_genesis_hash() {
    // Verify the genesis block hash is correct
    let (height, hash) = checkpoints::MAINNET_CHECKPOINTS[0];
    assert_eq!(height, 0);

    assert_eq!(
        hash,
        blvm_protocol::GENESIS_BLOCK_HASH_INTERNAL,
        "Genesis block hash should match"
    );
}

// ============================================================
// Configuration Tests
// ============================================================

#[serial_test::serial(ibd)]
#[test]
fn test_config_chunk_size_reasonable() {
    let config = ParallelIBDConfig::default();
    // 16 = Core-like minimum, 128 = default, 2000 = max (BLVM_IBD_CHUNK_SIZE override)
    assert!(
        config.chunk_size >= 16 && config.chunk_size <= 2000,
        "chunk_size={}",
        config.chunk_size
    );
}

#[serial_test::serial(ibd)]
#[test]
fn test_config_timeout_reasonable() {
    let config = ParallelIBDConfig::default();
    // Timeout should accommodate slow peers and large blocks
    assert!(
        config.download_timeout_secs >= 30,
        "Timeout too short for large blocks"
    );
    assert!(
        config.download_timeout_secs <= 300,
        "Timeout too long, will stall on dead peers"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn checkpoint_export_does_not_exit_when_vh_or_cl_hit_end_while_export_lags() {
    // Slice A 2026-08-28T02:22: skip-path raced tip to END; last_exported=468301.
    assert!(!checkpoint_export_thread_should_exit(
        669992, 670000, 670000, 468301, 32863
    ));
    // July F-C1 shape: vh at end, last_ckpt an interval behind — hold, do not join.
    assert!(!checkpoint_export_thread_should_exit(
        957804, 957000, 957804, 880000, 10000
    ));
    assert!(!checkpoint_export_thread_should_exit(
        957804, 0, 957804, 0, 10000
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn checkpoint_export_exits_when_last_committed_near_end() {
    // last_ckpt >= end - interval
    assert!(checkpoint_export_thread_should_exit(
        670000, 670000, 670000, 637137, 32863
    ));
    assert!(checkpoint_export_thread_should_exit(
        0, 957000, 957804, 957804, 10000
    ));
    assert!(!checkpoint_export_thread_should_exit(
        0, 957000, 957804, 880000, 10000
    ));
    assert!(checkpoint_export_thread_should_exit(0, 0, 0, 0, 0)); // end_h<=0
    // interval<=0: must have committed at end_h itself
    assert!(!checkpoint_export_thread_should_exit(100, 100, 100, 99, 0));
    assert!(checkpoint_export_thread_should_exit(100, 100, 100, 100, 0));
}

#[serial_test::serial(ibd)]
#[test]
fn tip_skip_advances_near_effective_end_without_1000_boundary() {
    // Live: tip stuck 957632..957804 with no %1000 in range.
    assert!(should_advance_tip_on_skip_path(957632, 957804));
    assert!(should_advance_tip_on_skip_path(957804, 957804));
    assert!(should_advance_tip_on_skip_path(957000, 957804)); // %1000
    // Far from end and not on 1000 boundary:
    assert!(!should_advance_tip_on_skip_path(900001, 957804));
    assert!(!should_advance_tip_on_skip_path(0, 100));
}

#[serial_test::serial(ibd)]
#[test]
fn tip_follow_extends_when_peer_advances() {
    assert_eq!(
        tip_follow_new_effective_end(957_850, 957_900, 957_900),
        Some(957_900)
    );
    assert_eq!(
        tip_follow_new_effective_end(957_850, 957_900, 957_870),
        Some(957_870)
    );
    assert_eq!(
        tip_follow_new_effective_end(957_900, 957_850, 957_900),
        None
    );
    assert_eq!(
        tip_follow_new_effective_end(957_850, 957_850, 957_900),
        None
    );
}

/// R-238 dump 20–30k: 5s TIP_FOLLOW_TIMEOUT while feeder=0 / FILLED_RUNWAY.
/// Coordinator must not park. dest-bc 0–10k 298 lives.
#[serial_test::serial(ibd)]
#[test]
fn r238_tip_follow_does_not_park_hungry_apply() {
    assert!(!tip_follow_may_block_coord(
        0,
        25_493,
        370_000,
        Some(370_000)
    ));
    assert!(!tip_follow_may_block_coord(
        64,
        25_493,
        370_000,
        Some(370_000)
    ));
    assert!(!tip_follow_may_block_coord(
        689,
        10_000,
        370_000,
        Some(370_000)
    ));
    assert!(!tip_follow_may_block_coord(64, 25_493, 370_000, None));
    assert!(tip_follow_may_block_coord(64, 960_000, 965_000, None));
    assert!(!tip_follow_may_block_coord(0, 960_000, 965_000, None));
}

/// R-240 freeze: last CRAWL @107124 then mute CAP, then coordinator silent.
/// Ready refresh must not call blocking `peer_addresses_for_ibd()`.
#[test]
fn r241_ready_refresh_does_not_use_blocking_peer_scan() {
    let src = include_str!("mod.rs");
    assert!(
        src.contains("peer_addresses_for_ibd_connected().await"),
        "coordinator ready-refresh must await connected-only scan"
    );
    let refresh = src
        .split("let refresh = async move {")
        .nth(1)
        .expect("ready-refresh future");
    let refresh = refresh
        .split("tokio::time::timeout(Duration::from_millis(250), refresh)")
        .next()
        .unwrap();
    let code: String = refresh
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect();
    assert!(
        !code.contains("peer_addresses_for_ibd()"),
        "block_in_place peer_addresses_for_ibd inside refresh defeats 250ms timeout"
    );
}

/// R-242 freeze: covering hero RST, dispatch `Peer disconnected:` without
/// cancelling pending GetData (handshake path already did). Live path must
/// cancel + `ibd_peer_gone`.
#[test]
fn r243_dispatch_rst_cancels_pending_getdata() {
    let src = include_str!("../../network/network_message_dispatch.rs");
    let handler = src
        .split("async fn handle_peer_disconnected")
        .nth(1)
        .expect("handle_peer_disconnected");
    let handler = handler.split("#[cfg(test)]").next().unwrap();
    assert!(
        handler.contains("cancel_pending_block_requests_for_disconnected_peer"),
        "RST dispatch must drop GetData oneshots (R-242 parked 186264 on dead 3.136)"
    );
    assert!(
        handler.contains("ibd_peer_gone"),
        "RST dispatch must release assigner inflight so walk-promote cannot retitle a corpse"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn emergency_drain_block_rx_admits_gap_height_only() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    tx.try_send((100u64, Arc::clone(&block), Arc::clone(&w)))
        .unwrap();
    tx.try_send((102u64, Arc::clone(&block), Arc::clone(&w)))
        .unwrap();

    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let mut total = 0u64;
    assert!(!emergency_drain_block_rx_for_gap(
        &mut rx,
        &mut reorder,
        101,
        16,
        64,
        &mut total,
        0,
        256
    ));
    assert_eq!(reorder.len(), 1);
    assert!(reorder.contains_key(&102));

    assert!(emergency_drain_block_rx_for_gap(
        &mut rx,
        &mut reorder,
        102,
        16,
        64,
        &mut total,
        0,
        256
    ));
    assert!(emergency_gap_admission_unblocked(&reorder, 102, 16));
}

#[serial_test::serial(ibd)]
#[test]
fn emergency_gap_admission_requires_present_height() {
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    assert!(!emergency_gap_admission_unblocked(&reorder, 1, 16));
}

#[serial_test::serial(ibd)]
#[test]
fn emergency_gap_admission_requires_buffer_headroom() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    for h in 1..=16u64 {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    assert!(!emergency_may_bulk_recv(&reorder, 16));
    assert!(emergency_has_gap_block(&reorder, 1));
    assert!(!emergency_gap_admission_unblocked(&reorder, 1, 16));
}

#[serial_test::serial(ibd)]
#[test]
fn insert_reorder_gap_aware_drops_far_ahead_when_gap_missing() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let limit = 64usize;
    let window = 16u64;
    // W29: gap_missing always enforces the window (not only at half-full).
    // Near-gap heights within window are admitted.
    for h in (next_needed + 1)..=(next_needed + window) {
        assert!(insert_reorder_gap_aware(
            &mut reorder,
            h,
            Arc::clone(&block),
            Arc::clone(&w),
            next_needed,
            limit,
            window,
            0, // bridge check disabled
        ));
    }
    assert_eq!(reorder.len(), window as usize);
    // Far ahead beyond window must drop even with small buffer (W29 always-throttle).
    assert!(!insert_reorder_gap_aware(
        &mut reorder,
        next_needed + window + 1,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        0,
    ));
    // Gap height always admitted.
    assert!(insert_reorder_gap_aware(
        &mut reorder,
        next_needed,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        0,
    ));
    // Once gap present (and bridge not full), far ahead is admitted again.
    assert!(insert_reorder_gap_aware(
        &mut reorder,
        next_needed + window + 50,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        0,
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn r97_reserved_far_inserts_while_gap_missing() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    test_clear_lookahead_reserved();
    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let far = next_needed + 2048;
    publish_lookahead_reserved(vec![(far, far + 2047)]);
    assert!(
        insert_reorder_gap_aware(
            &mut reorder,
            far,
            Arc::clone(&block),
            Arc::clone(&w),
            next_needed,
            64,
            16,
            0,
        ),
        "reserved hole+LEAD must insert while gap_missing"
    );
    test_clear_lookahead_reserved();
    assert!(
        !insert_reorder_gap_aware(
            &mut reorder,
            far + 1,
            Arc::clone(&block),
            Arc::clone(&w),
            next_needed,
            64,
            16,
            0,
        ),
        "unreserved far must still drop"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r97_evict_skips_reserved_key() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    test_clear_lookahead_reserved();
    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let window = 16u64;
    let reserved = next_needed + 50;
    publish_lookahead_reserved(vec![(reserved, reserved)]);
    for h in (next_needed + window + 1)..=(next_needed + 50) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    let evicted = evict_reorder_gap_pressure(&mut reorder, next_needed, 64, window, 0);
    assert!(evicted > 0);
    assert!(
        reorder.contains_key(&reserved),
        "reserved key must survive evict"
    );
    test_clear_lookahead_reserved();
}

/// Phase 0b.2 / rbitcoin request-vs-receive: throttle *new* far-ahead admit; do not
/// clear already-buffered near-gap heights, and tip (`h == next_needed`) still enqueues.
/// See docs/RBITCOIN_VS_BLVM_IBD_ARCHITECTURE.md § Request-vs-receive.
#[serial_test::serial(ibd)]
#[test]
fn admit_throttle_preserves_already_buffered_near_gap() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let limit = 64usize;
    let window = 16u64;

    // Already-received / buffered near-gap (gap itself still missing → throttle on).
    let buffered: Vec<u64> = ((next_needed + 1)..=(next_needed + 8)).collect();
    for &h in &buffered {
        assert!(insert_reorder_gap_aware(
            &mut reorder,
            h,
            Arc::clone(&block),
            Arc::clone(&w),
            next_needed,
            limit,
            window,
            0,
        ));
    }
    assert_eq!(reorder.len(), buffered.len());

    // New far-ahead assign/admit refused under gap_missing throttle.
    assert!(!insert_reorder_gap_aware(
        &mut reorder,
        next_needed + window + 40,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        0,
    ));

    // Already-buffered heights must remain (throttle ≠ refuse already-received).
    for &h in &buffered {
        assert!(
            reorder.contains_key(&h),
            "throttle must not clear already-buffered h={h}"
        );
    }
    assert_eq!(reorder.len(), buffered.len());

    // Tip / gap height still enqueues while far-ahead is throttled.
    assert!(insert_reorder_gap_aware(
        &mut reorder,
        next_needed,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        0,
    ));
    assert!(reorder.contains_key(&next_needed));

    // Dispatch side: tip is never deferred even when WAN tip crawl + gap missing.
    assert!(
        !defer_bridge_ahead_dispatch(
            next_needed,
            next_needed,
            true, // gap_missing
            true, // next_expected_missing
            window,
            true, // wan_tip_crawl
            false,
            false,
        ),
        "tip height must still dispatch while far-ahead is deferred"
    );
    assert!(
        defer_bridge_ahead_dispatch(
            next_needed + 1,
            next_needed,
            true,
            true,
            window,
            true,
            false,
            false,
        ),
        "far-ahead deferred under tip-missing WAN crawl"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn insert_reorder_gap_aware_s2b_drops_when_bridge_full_even_if_gap_present() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let limit = 64usize;
    let window = 16u64;
    let bridge_max = 512usize;

    // Gap present in reorder.
    reorder.insert(next_needed, (Arc::clone(&block), Arc::clone(&w)));
    // Fill to half capacity with near-gap heights.
    for h in (next_needed + 1)..=(next_needed + 31) {
        assert!(insert_reorder_gap_aware(
            &mut reorder,
            h,
            Arc::clone(&block),
            Arc::clone(&w),
            next_needed,
            limit,
            window,
            bridge_max,
        ));
    }
    assert!(reorder.len() >= limit / 2);

    // Simulate bridge at cap (S2b).
    memory::BRIDGE_PENDING_COUNT.store(bridge_max as u64, Ordering::Relaxed);
    assert!(
        !insert_reorder_gap_aware(
            &mut reorder,
            next_needed + window + 1,
            Arc::clone(&block),
            Arc::clone(&w),
            next_needed,
            limit,
            window,
            bridge_max,
        ),
        "S2b: far-ahead must drop when bridge is full even if gap is present"
    );
    // Gap height still admitted.
    assert!(insert_reorder_gap_aware(
        &mut reorder,
        next_needed,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        bridge_max,
    ));
    // Near-window still admitted.
    assert!(insert_reorder_gap_aware(
        &mut reorder,
        next_needed + window,
        Arc::clone(&block),
        Arc::clone(&w),
        next_needed,
        limit,
        window,
        bridge_max,
    ));
    memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn emergency_drain_s2a_uses_coordinator_admit_limit() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    for h in 101..=120u64 {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    assert_eq!(reorder.len(), 20);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    tx.try_send((200u64, Arc::clone(&block), Arc::clone(&w)))
        .unwrap();

    let mut total = 0u64;
    // len=20 < half(32) of coordinator admit_limit=64 — far-ahead must admit.
    emergency_drain_block_rx_for_gap(
        &mut rx,
        &mut reorder,
        next_needed,
        16,
        64,
        &mut total,
        0,
        256,
    );
    assert!(
        reorder.contains_key(&200),
        "S2a: far-ahead should admit when reorder is below half of coordinator limit"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn evict_reorder_gap_pressure_prunes_stale_and_far_ahead() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let limit = 64usize;
    let window = 16u64;

    reorder.insert(90, (Arc::clone(&block), Arc::clone(&w)));
    for h in (next_needed + 1)..=(next_needed + 32) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    for h in (next_needed + window + 1)..=(next_needed + 50) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    assert!(reorder.len() >= limit / 2);
    assert!(reorder.contains_key(&90));
    assert!(!reorder.contains_key(&next_needed));

    let evicted = evict_reorder_gap_pressure(&mut reorder, next_needed, limit, window, 0);
    assert!(evicted > 0);
    assert!(
        !reorder.contains_key(&90),
        "stale heights below next_needed pruned"
    );
    assert!(
        !reorder.contains_key(&(next_needed + 50)),
        "far-ahead beyond window evicted"
    );
    assert!(
        reorder.contains_key(&(next_needed + window)),
        "near-window heights preserved"
    );
    assert!(reorder.len() < limit / 2 + window as usize + 1);
}

#[serial_test::serial(ibd)]
#[test]
fn evict_reorder_s2e_deeper_target_when_bridge_full() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let limit = 2000usize;
    let window = 256u64;
    // Gap present so W29 gap-missing eviction does not fire — isolate S2e bridge_full path.
    reorder.insert(next_needed, (Arc::clone(&block), Arc::clone(&w)));
    // Fill to the old pressure_target (half-64 = 936) with far-ahead heights.
    for h in (next_needed + window + 1)..(next_needed + window + 1 + 936) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    assert_eq!(reorder.len(), 937);
    // Without bridge_full: at pressure_target → no eviction (gap present).
    assert_eq!(
        evict_reorder_gap_pressure(&mut reorder, next_needed, limit, window, 0),
        0,
        "at half-64 with bridge empty + gap present: no-op"
    );
    // S2e: simulate bridge at cap → deeper target (half/4 = 500).
    memory::BRIDGE_PENDING_COUNT.store(512, Ordering::Relaxed);
    let evicted = evict_reorder_gap_pressure(&mut reorder, next_needed, limit, window, 512);
    memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    assert!(
        evicted >= 1,
        "S2e must evict when bridge_full even at old pressure_target (evicted={evicted})"
    );
    assert!(
        reorder.len() < 937,
        "reorder must shrink below 937 under bridge_full"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w29_evict_reorder_to_window_when_gap_missing() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let window = 64u64;
    // Tip missing; fill far ahead past window (live W28d signature).
    for h in (next_needed + 1)..=(next_needed + 270) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    assert_eq!(reorder.len(), 270);
    let mut total = 0usize;
    for _ in 0..16 {
        let n = evict_reorder_gap_pressure(&mut reorder, next_needed, 2000, window, 0);
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(total > 0, "W29 must evict far-ahead while gap_missing");
    let ceiling = next_needed + window;
    assert!(
        reorder.keys().next_back().copied().unwrap_or(0) <= ceiling
            || reorder.len() <= (window as usize) + 8,
        "reorder must shrink toward window (len={}, max={:?})",
        reorder.len(),
        reorder.keys().next_back()
    );
}

#[serial_test::serial(ibd)]
#[test]
fn evict_reorder_gap_pressure_noop_when_gap_present_and_bridge_empty() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    reorder.insert(next_needed, (Arc::clone(&block), Arc::clone(&w)));
    for h in 150..=200u64 {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    let before = reorder.len();
    let evicted = evict_reorder_gap_pressure(&mut reorder, next_needed, 64, 16, 0);
    assert_eq!(evicted, 0);
    assert_eq!(reorder.len(), before);
}

#[serial_test::serial(ibd)]
#[test]
fn defer_bridge_ahead_dispatch_blocks_far_ahead_when_gap_missing() {
    let next = 100u64;
    let window = 16u64;
    assert!(!defer_bridge_ahead_dispatch(
        next, next, true, false, window, false, false, false
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + window + 1,
        next,
        true,
        false,
        window,
        false,
        false,
        false
    ));
    assert!(!defer_bridge_ahead_dispatch(
        next + window + 1,
        next,
        false,
        false,
        window,
        false,
        false,
        false
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn defer_bridge_ahead_dispatch_tight_band_when_next_expected_missing() {
    let next = 100u64;
    let window = 256u64;
    // Gap height always allowed.
    assert!(!defer_bridge_ahead_dispatch(
        next, next, false, true, window, false, false, false
    ));
    // Inside tight band (≤64) still allowed.
    assert!(!defer_bridge_ahead_dispatch(
        next + 32,
        next,
        false,
        true,
        window,
        false,
        false,
        false
    ));
    // Past tight band deferred even if reorder has the gap.
    assert!(defer_bridge_ahead_dispatch(
        next + 65,
        next,
        false,
        true,
        window,
        false,
        false,
        false
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn defer_bridge_ahead_w17_wan_tip_crawl_defers_all_ahead() {
    let next = 685470u64;
    let window = 256u64;
    // Tip always allowed.
    assert!(!defer_bridge_ahead_dispatch(
        next, next, true, true, window, true, false, false
    ));
    // Tip missing from reorder+bridge → defer all ahead (W17 hole-fill guard).
    assert!(defer_bridge_ahead_dispatch(
        next + 1,
        next,
        true,
        true,
        window,
        true,
        false,
        false
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + 32,
        next,
        true,
        true,
        window,
        true,
        false,
        false
    ));
    // Tip present in reorder → allow contiguous band (W18), defer past band.
    assert!(!defer_bridge_ahead_dispatch(
        next + 32,
        next,
        false,
        false,
        window,
        true,
        false,
        false
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + 65,
        next,
        false,
        false,
        window,
        true,
        false,
        false
    ));
    // Local / non-WAN still allows near-ahead under prior L2 rules.
    assert!(!defer_bridge_ahead_dispatch(
        next + 32,
        next,
        false,
        true,
        window,
        false,
        false,
        false
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn defer_bridge_ahead_w57_never_hole_fill_when_tip_missing() {
    let next = 100u64;
    let window = 256u64;
    // W17/W57: gap + next_expected missing → defer ALL ahead (even feeder-starved).
    assert!(defer_bridge_ahead_dispatch(
        next + 32,
        next,
        true,
        true,
        window,
        true,
        false,
        false
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + 32,
        next,
        true,
        true,
        window,
        true,
        true,
        false
    ));
    // Tip present in reorder (gap_missing=false) — W18 band still allows near-ahead.
    assert!(!defer_bridge_ahead_dispatch(
        next + 32,
        next,
        false,
        true,
        window,
        true,
        true,
        false
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + 65,
        next,
        false,
        true,
        window,
        true,
        true,
        false
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn defer_bridge_ahead_w58_bulk_still_defers_when_tip_missing() {
    let next = 60_000u64;
    let window = 256u64;
    // W58: bulk + tip nowhere → W17 (no hole-fill). Old bulk path allowed tip+32.
    assert!(defer_bridge_ahead_dispatch(
        next + 32,
        next,
        true,
        true,
        window,
        true,
        false,
        true
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + 1,
        next,
        true,
        true,
        window,
        true,
        false,
        true
    ));
    // Tip itself still admitted.
    assert!(!defer_bridge_ahead_dispatch(
        next, next, true, true, window, true, false, true
    ));
    // Bulk + tip present in reorder (gap_missing=false): multi-peer tight band.
    assert!(!defer_bridge_ahead_dispatch(
        next + 32,
        next,
        false,
        true,
        window,
        true,
        false,
        true
    ));
    assert!(defer_bridge_ahead_dispatch(
        next + 65,
        next,
        false,
        true,
        window,
        true,
        false,
        true
    ));
}

/// L2b: W58 still defers reserved farm while apply is in the desert.
#[serial_test::serial(ibd)]
#[test]
fn r102_reserved_far_still_deferred_while_h_missing() {
    test_clear_lookahead_reserved();
    let next = 100u64;
    let farm_a = next + 2048;
    publish_lookahead_reserved(vec![(farm_a, farm_a + 2047)]);
    assert!(
        defer_bridge_ahead_dispatch(farm_a, next, true, true, 192, true, false, true),
        "H missing: reserved hole+LEAD must still W58"
    );
    assert!(
        defer_bridge_ahead_dispatch(farm_a + 1, next, true, true, 192, true, false, true),
        "H missing: rest of reserved stripe still W58"
    );
    test_clear_lookahead_reserved();
}

/// L2b: apply inside held stripe — emit the rest of that stripe (W58 would re-arm).
#[serial_test::serial(ibd)]
#[test]
fn r102_same_stripe_not_deferred_after_first_height_leaves() {
    test_clear_lookahead_reserved();
    let s = 10_241u64;
    let e = 12_288u64;
    publish_lookahead_reserved(vec![(s, e)]);
    // R-101: after s leaves reorder, gap_missing && next_expected_missing.
    assert!(
        !defer_bridge_ahead_dispatch(s + 1, s, true, true, 192, true, false, true),
        "same stripe after tip leaves reorder"
    );
    assert!(
        !defer_bridge_ahead_dispatch(e, s, true, true, 192, true, false, true),
        "same stripe end"
    );
    test_clear_lookahead_reserved();
}

/// L2b: farm B at +LEAD stays W58 while apply is in A.
#[serial_test::serial(ibd)]
#[test]
fn r102_other_reserved_stripe_still_deferred() {
    test_clear_lookahead_reserved();
    let a_s = 10_241u64;
    let a_e = 12_288u64;
    let b_s = a_s + 2048;
    let b_e = b_s + 2047;
    publish_lookahead_reserved(vec![(a_s, a_e), (b_s, b_e)]);
    assert!(
        !defer_bridge_ahead_dispatch(a_e, a_s, true, true, 192, true, false, true),
        "stripe A must emit"
    );
    assert!(
        defer_bridge_ahead_dispatch(b_s, a_s, true, true, 192, true, false, true),
        "stripe B at +LEAD must stay W58"
    );
    assert!(
        defer_bridge_ahead_dispatch(b_s + 64, a_s, true, true, 192, true, false, true),
        "stripe B interior must stay W58"
    );
    test_clear_lookahead_reserved();
}

/// L2b: unreserved far still W58 (HASH_FETCH unset).
#[serial_test::serial(ibd)]
#[test]
fn r102_unreserved_far_still_deferred() {
    test_clear_lookahead_reserved();
    let next = 100u64;
    assert!(
        defer_bridge_ahead_dispatch(next + 8192, next, true, true, 192, true, false, true),
        "unreserved hole+LEAD still W58"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn wan_bulk_catchup_threshold() {
    assert!(!wan_bulk_catchup(0, 60_000));
    assert!(!wan_bulk_catchup(60_100, 60_000)); // only 100 ahead
    assert!(wan_bulk_catchup(70_000, 60_000)); // ≥2048
    assert!(wan_bulk_catchup(900_000, 60_000));
}

#[serial_test::serial(ibd)]
#[test]
fn r305_wide_runway_default_off_clamps_tip_gap_at_2048() {
    unsafe {
        std::env::remove_var("BLVM_IBD_WIDE_RUNWAY");
        std::env::set_var("BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD", "8192");
        assert!(!wide_runway_enabled(), "WIDE_RUNWAY unset must stay off");
        assert_eq!(
            wan_bulk_tip_gap_ahead_cap(),
            2048,
            "R-305: without WIDE_RUNWAY, 8192 env still clamps at today's 2048"
        );
        std::env::remove_var("BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD");
        assert_eq!(
            wan_bulk_tip_gap_ahead_cap(),
            wan_tip_gap_ahead_cap(),
            "unset GAP env still follows tip-gap default"
        );
    }
}

#[serial_test::serial(ibd)]
#[test]
fn r305_wide_runway_on_allows_8192() {
    unsafe {
        std::env::set_var("BLVM_IBD_WIDE_RUNWAY", "1");
        std::env::set_var("BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD", "8192");
        assert!(wide_runway_enabled());
        assert_eq!(
            wan_bulk_tip_gap_ahead_cap(),
            8192,
            "R-305: WIDE_RUNWAY + GAP=8192 must match RUNWAY_SPAN"
        );
        std::env::remove_var("BLVM_IBD_WIDE_RUNWAY");
        std::env::remove_var("BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w76_wan_ahead_policy_feeder_starve_uses_tip_window_even_when_bulk() {
    // Mid-chain: headers at network tip ⇒ bulk=true always; feeder empty must not
    // keep the old 1024 bulk-gap window (live tip never in bridge @ ~350k).
    unsafe {
        std::env::remove_var("BLVM_IBD_WIDE_RUNWAY");
        std::env::remove_var("BLVM_IBD_WAN_BULK_TIP_GAP_AHEAD");
        std::env::remove_var("BLVM_IBD_TIP_ADMIT_TIGHT");
    }
    let (kind, cap) = wan_ahead_policy(true, true, true, 2);
    assert_eq!(kind, "wan_bulk_gap");
    assert_eq!(cap, wan_bulk_tip_gap_ahead_cap());
    assert_eq!(
        cap,
        wan_tip_gap_ahead_cap(),
        "W76 default bulk-gap == tip ahead"
    );
    let (kind2, cap2) = wan_ahead_policy(false, true, true, 2);
    assert_eq!(kind2, "wan_tip");
    assert_eq!(cap2, wan_bulk_tip_gap_ahead_cap());
    let (kind3, cap3) = wan_ahead_policy(true, false, false, 2);
    assert_eq!(kind3, "wan_bulk");
    assert_eq!(cap3, wan_bulk_ahead_cap());
}

#[serial_test::serial(ibd)]
#[test]
fn reorder_has_feeder_prefetch_band_detects_near_blocks() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next = 1000u64;
    assert!(!reorder_has_feeder_prefetch_band(&reorder, next, 16));
    reorder.insert(next + 8, (Arc::clone(&block), Arc::clone(&w)));
    assert!(reorder_has_feeder_prefetch_band(&reorder, next, 16));
    reorder.clear();
    reorder.insert(next + 20, (Arc::clone(&block), Arc::clone(&w)));
    assert!(!reorder_has_feeder_prefetch_band(&reorder, next, 16));
}

#[serial_test::serial(ibd)]
#[test]
fn evict_reorder_gap_pressure_runs_when_one_below_half() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 526_335u64;
    let limit = 2000usize;
    let window = 256u64;
    for h in (next_needed + 1)..=(next_needed + 999) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    assert_eq!(reorder.len(), 999);

    let evicted = evict_reorder_gap_pressure(&mut reorder, next_needed, limit, window, 512);
    assert!(
        evicted > 0,
        "must evict when reorder=999 and gap_missing under production limits"
    );
    assert!(
        reorder.len() < 999,
        "eviction must shrink below treadmill equilibrium, got {}",
        reorder.len()
    );
}

#[serial_test::serial(ibd)]
#[test]
fn evict_reorder_gap_pressure_batch_caps_at_32_per_tick() {
    use blvm_protocol::{Block, BlockHeader};
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let next_needed = 100u64;
    let limit = 128usize;
    let window = 8u64;
    for h in (next_needed + window + 1)..=(next_needed + 200) {
        reorder.insert(h, (Arc::clone(&block), Arc::clone(&w)));
    }
    let before = reorder.len();
    let evicted = evict_reorder_gap_pressure(&mut reorder, next_needed, limit, window, 0);
    assert_eq!(
        evicted, 32,
        "S2d: batch eviction capped at 32 per coordinator tick"
    );
    assert_eq!(reorder.len(), before - 32);
    assert!(reorder.len() >= limit / 2);
}

#[serial_test::serial(ibd)]
#[test]
fn w54_tip_handoff_ignores_feeder_depth_when_tip_stranded() {
    use blvm_protocol::{Block, BlockHeader};
    use rustc_hash::FxHashSet;
    use std::sync::Arc;

    let block = Arc::new(Block {
        header: BlockHeader::default(),
        transactions: Default::default(),
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut reorder: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let mut dispatched = FxHashSet::default();
    let next_needed = 428_344u64;
    reorder.insert(next_needed, (Arc::clone(&block), Arc::clone(&w)));

    // Pre-W54: feeder_len > 16 returned None and left tip stranded under soft-resume.
    let out = prepare_coordinator_tip_handoff(
        next_needed,
        false,
        383,
        false,
        &mut reorder,
        &mut dispatched,
        None,
        256,
        512,
        true,
        false,
    );
    assert!(
        out.is_some(),
        "W54: stranded tip must hand off with feeder=383"
    );
    assert!(!reorder.contains_key(&next_needed));
    assert!(dispatched.contains(&next_needed));

    reorder.insert(next_needed, (Arc::clone(&block), Arc::clone(&w)));
    let blocked = prepare_coordinator_tip_handoff(
        next_needed,
        false,
        0,
        false,
        &mut reorder,
        &mut dispatched,
        None,
        256,
        512,
        true,
        true, // already in feeder
    );
    assert!(
        blocked.is_none(),
        "must not re-handoff tip already in feeder"
    );
    assert!(reorder.contains_key(&next_needed));
}

/// R-289 slow-peer rotation clock. Eviction today only fires on hard failure, so the
/// peer draw taken at connect time survives the whole run: R-287 and R-288 are the
/// SAME binary `a7729c9a` and scored dump 3378 vs 907 (3.7x).
#[test]
fn r289_rotate_seeds_clock_before_it_judges_a_cold_mesh() {
    assert!(
        !should_rotate_slow_peer(1_000_000, 0, 30),
        "last_ms=0 is a cold mesh with no CRAWL window yet — seed the clock, \
         do not evict the first peer that happens to be unscored"
    );
    assert!(
        !should_rotate_slow_peer(1_029_999, 1_000_000, 30),
        "29.999s — one rotation per interval"
    );
    assert!(should_rotate_slow_peer(1_030_000, 1_000_000, 30));
    assert!(
        !should_rotate_slow_peer(9_999_999, 1_000_000, 0),
        "BLVM_IBD_PEER_ROTATE_SECS=0 is the off switch (R-273 DNA baseline arm)"
    );
}

/// R-289 evicted 13/13 at recv=0.00 mbps. Those peers were never assigned
/// (R-280 peers_conn 45.0 vs peers_inflight 13.6). Ranking the byte store
/// rotates the bench. R-290 scores only peers with ≥1 assignment in the window.
#[serial_test::serial(ibd)]
#[test]
fn r290_rotate_evicts_worst_seated_not_the_bench() {
    download::test_reset_rotate_state();
    // Seed window: three seated + one bench that already has historical bytes
    // (the R-289 0.00 mbps shape — delivered once, then never asked again).
    download::test_note_assigned("1.1.1.1:8333", 16); // sticky
    download::test_note_download_block_bytes("1.1.1.1:8333", 800_000);
    download::test_note_assigned("3.3.3.3:8333", 16); // will be slow seated
    download::test_note_download_block_bytes("3.3.3.3:8333", 800_000);
    download::test_note_assigned("4.4.4.4:8333", 16); // fast seated
    download::test_note_download_block_bytes("4.4.4.4:8333", 800_000);
    download::test_note_download_block_bytes("2.2.2.2:8333", 50_000); // bench, never assigned
    assert!(
        download::download_rotate_slowest("1.1.1.1:8333", 2).is_none(),
        "first call seeds the window; nobody is judged on a cold mesh"
    );

    // Window 2: sticky + slow + fast get more work. Bench gets ZERO assignments
    // and ZERO new bytes — the R-289 victim shape.
    download::test_note_assigned("1.1.1.1:8333", 16);
    download::test_note_download_block_bytes("1.1.1.1:8333", 800_000);
    download::test_note_assigned("3.3.3.3:8333", 16);
    download::test_note_download_block_bytes("3.3.3.3:8333", 10_000); // poor yield
    download::test_note_assigned("4.4.4.4:8333", 16);
    download::test_note_download_block_bytes("4.4.4.4:8333", 800_000);
    download::test_rotate_backdate_secs(2);

    let v =
        download::download_rotate_slowest("1.1.1.1:8333", 2).expect("three seated ≥ min_scored=2");
    assert_eq!(
        v.peer, "3.3.3.3:8333",
        "R-289 evicted 13/13 at recv=0.00 — those peers were never assigned \
         (R-280 conn 45 inflight 13.6). Victim must be the worst SEATED peer \
         (3.3.3.3 yield 10000/16), not the bench peer 2.2.2.2 (assigned=0) \
         and not sticky 1.1.1.1"
    );
    assert_eq!(
        v.seated, 3,
        "sticky+slow+fast were assigned this window; bench must not count as seated"
    );
    assert!(
        v.bench >= 1,
        "bench= peers with 0 assignment this window (2.2.2.2); got bench={}",
        v.bench
    );
    assert!(
        v.assigned >= 48,
        "assigned= total blocks given this window (3×16); got {}",
        v.assigned
    );
    assert!(
        v.recv_mbps > 0.0,
        "seated slow still delivered some bytes — recv>0 is the R-290 gate vs R-289's all-zero"
    );
}

/// min_scored now counts SEATED peers. Default 8: a 13-seat roster (R-280 inflight
/// 13.6) can meet it; 16 could not without counting the bench.
#[serial_test::serial(ibd)]
#[test]
fn r290_min_scored_counts_seated_not_connected() {
    download::test_reset_rotate_state();
    unsafe {
        std::env::remove_var("BLVM_IBD_PEER_ROTATE_MIN_SCORED");
    }
    assert_eq!(
        ibd_peer_rotate_min_scored(),
        8,
        "default 8: R-280 inflight 13.6; 16 was the connected-mesh floor that \
         forced scoring the bench (R-289 scored 40–43 against min 16)"
    );
    for i in 0..3u16 {
        let p = format!("10.0.0.{i}:8333");
        download::test_note_assigned(&p, 16);
        download::test_note_download_block_bytes(&p, 100_000);
    }
    // 30 bench peers with historical bytes, zero assignments — R-289 would have
    // scored them and met min_scored=16.
    for i in 0..30u16 {
        download::test_note_download_block_bytes(&format!("11.0.0.{i}:8333"), 1_000);
    }
    assert!(download::download_rotate_slowest("-", 8).is_none()); // seed
    for i in 0..3u16 {
        let p = format!("10.0.0.{i}:8333");
        download::test_note_assigned(&p, 16);
        download::test_note_download_block_bytes(&p, 100_000);
    }
    download::test_rotate_backdate_secs(2);
    assert!(
        download::download_rotate_slowest("-", 8).is_none(),
        "3 seated < min_scored=8 — do not evict out of a thin roster, even if \
         30 bench peers would have padded a connected-mesh count to 33"
    );
}

/// R-291: default depth 1 is current DNA (one stripe per non-sticky). Env 8 is
/// same-peer sequential pipelining, clamped to Core's 16. Sticky/tip path is
/// not this knob (max_in_flight_for / TOP_PEER / sole_tip).
#[serial_test::serial(ibd)]
#[test]
fn r291_peer_depth_default_is_one_and_env_raises() {
    unsafe {
        std::env::remove_var("BLVM_IBD_PEER_DEPTH");
    }
    assert_eq!(
        ibd_peer_depth(),
        1,
        "unset BLVM_IBD_PEER_DEPTH must be 1 — R-288 DNA, a true control vs depth=8"
    );
    unsafe {
        std::env::set_var("BLVM_IBD_PEER_DEPTH", "8");
    }
    assert_eq!(
        ibd_peer_depth(),
        8,
        "R-280 GetData→body 1274ms; depth 1 is a >1s bubble after every stripe. 8 is the treatment."
    );
    unsafe {
        std::env::set_var("BLVM_IBD_PEER_DEPTH", "99");
    }
    assert_eq!(
        ibd_peer_depth(),
        16,
        "clamp to Core MAX_BLOCKS_IN_TRANSIT_PER_PEER=16, not unbounded ahead"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_PEER_DEPTH");
    }
}

/// R-291 pin is a different env from BLVM_IBD_PEERS (LAN/archive tip-now).
/// PIN skips DNS entirely so A/B shares one peer set.
#[serial_test::serial(ibd)]
#[test]
fn r291_pin_peers_skips_archive_dns_seed() {
    unsafe {
        std::env::remove_var("BLVM_IBD_PEERS");
        std::env::remove_var("BLVM_IBD_PIN_PEERS");
    }
    assert!(
        !skip_ibd_archive_dns_seed(),
        "no pin → DNS seeds still run (fresh lottery every dest)"
    );
    unsafe {
        std::env::set_var("BLVM_IBD_PIN_PEERS", "1.2.3.4:8333,5.6.7.8:8333");
    }
    assert!(
        skip_ibd_archive_dns_seed(),
        "BLVM_IBD_PIN_PEERS must skip DNS or the A/B pair is two different draws"
    );
    assert_eq!(crate::network::ibd_pin_peers().len(), 2);
    unsafe {
        std::env::remove_var("BLVM_IBD_PIN_PEERS");
    }
}

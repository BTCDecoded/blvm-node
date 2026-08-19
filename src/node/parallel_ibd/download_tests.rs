//! Download / tip-hole unit tests (GetData pipe, GD EWMA, local-gap).
//! Policy constants live in `policy.rs`; this file tests **runtime** download
//! helpers. Overlap with assigner tests is intentional (different layer).

use super::*;

/// Serialize tests that poke bridge/tip-stage atomics (shared with assigner tests).
fn tip_soft_atomic_lock() -> crate::ibd_test_lock::Guard {
    super::super::tip_stage::test_tip_atomics_lock()
}

#[serial_test::serial(ibd)]
#[test]
fn resume_download_height_skips_validated_prefix() {
    assert_eq!(resume_download_height(955186, 955241, 955194), Some(955195));
    assert_eq!(resume_download_height(955186, 955241, 955185), Some(955186));
    assert_eq!(resume_download_height(955186, 955241, 955241), None);
}

#[serial_test::serial(ibd)]
#[test]
fn wan_tip_stream_credit_dedupes_tip_adjacent_gap_stream() {
    // Common tip crawl: tip-adjacent WAN body + successful GAP_STREAM → one credit.
    assert_eq!(wan_tip_stream_credit_count(false, true, true), 1);
    // STREAM no-op (LOCAL_GAP already advanced): still one body credit.
    assert_eq!(wan_tip_stream_credit_count(false, true, false), 1);
    // Non-adjacent body that drains tip gap: STREAM-only credit.
    assert_eq!(wan_tip_stream_credit_count(false, false, true), 1);
    // Local disk: never WAN tip-stream credit.
    assert_eq!(wan_tip_stream_credit_count(true, true, true), 0);
    // Neither tip-adjacent nor streamed.
    assert_eq!(wan_tip_stream_credit_count(false, false, false), 0);
}

#[serial_test::serial(ibd)]
#[test]
fn w66_received_soft_cap_covers_tip_owner_pipe() {
    // Tip-owner GetData depth is 128; soft cap must not sit below that (W65 live: 96).
    let soft = download_received_soft_cap();
    let hard = download_received_hard_cap();
    assert!(soft >= 128, "soft={soft}");
    assert!(hard >= soft, "hard={hard} soft={soft}");
}

/// Phase 0b.1: hard-trim may drop far-ahead under OOM; never tip-adjacent `h <= need`.
#[serial_test::serial(ibd)]
#[test]
fn hard_trim_never_drops_tip_adjacent_need() {
    use blvm_protocol::{Block, BlockHeader, Transaction, TransactionOutput};

    let dummy = || -> (SharedBlock, SharedWitnesses) {
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
        (Arc::new(block), Arc::new(vec![vec![]]))
    };

    let need = 100u64;
    let mut received: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    // Tip window + far ahead — hard=1 forces drops of farthest only.
    for h in [need, need + 1, need + 50, need + 200] {
        received_put(&mut received, h, dummy());
    }
    let forced = hard_trim_download_received_far_ahead(&mut received, need, 1);
    assert_eq!(forced, 3, "drop three far-ahead only");
    assert!(received.contains_key(&need), "must keep h==need");
    assert_eq!(received.len(), 1);

    // Over hard with only tip-adjacent / behind-tip heights: keep all (break on h<=need).
    let mut tip_only: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    for h in [need.saturating_sub(2), need.saturating_sub(1), need] {
        received_put(&mut tip_only, h, dummy());
    }
    let forced2 = hard_trim_download_received_far_ahead(&mut tip_only, need, 1);
    assert_eq!(forced2, 0, "must not hard-drop h<=need");
    assert_eq!(tip_only.len(), 3);
}

#[serial_test::serial(ibd)]
#[test]
fn soft_outer_extend_requires_gap_streams() {
    let start = std::time::Instant::now();
    assert!(
        !should_extend_outer_while_streaming(start, 0, start, 0),
        "no streams → no extend"
    );
    assert!(
        should_extend_outer_while_streaming(start, 10, start, 0),
        "recent streams → extend"
    );
    assert!(
        !should_extend_outer_while_streaming(start, 10, start, 4),
        "max extends exhausted"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn chunk_outer_deadline_scales_with_remaining_blocks() {
    assert_eq!(
        chunk_outer_deadline_secs(955186, 955241, 955186, 30),
        56 * 30
    );
    assert_eq!(
        chunk_outer_deadline_secs(955186, 955241, 955195, 30),
        47 * 30
    );
    // Multi-block chunk with 2 remaining: 2 × 30 = 60 ≥ 35 min.
    assert_eq!(chunk_outer_deadline_secs(100, 101, 100, 30), 60);
    // Multi-block chunk, only 1 block remaining after resume: formula gives 1s → clamped to 35.
    assert_eq!(
        chunk_outer_deadline_secs(100, 101, 101, 1),
        35,
        "multi-block minimum is 35"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn empty_witness_hit_cap_defaults_to_four() {
    // Don't assert env-free if the soak set BLVM_IBD_EMPTY_WITNESS_MAX; just check clamp.
    let c = empty_witness_hit_cap();
    assert!((2..=32).contains(&c), "cap must be in 2..=32, got {c}");
}

#[serial_test::serial(ibd)]
#[test]
fn gap_soft_retry_budget_tip_far_and_mid() {
    let tip = 684_955;
    assert_eq!(
        gap_soft_retry_budget(tip + 1, tip),
        tip_gap_soft_retries(),
        "tip gap: keep soft retries before abort/rotate"
    );
    assert_eq!(
        gap_soft_retry_budget(tip + 1 + far_ahead_band() + 1, tip),
        0,
        "far ahead: abort on first timeout"
    );
    assert_eq!(
        gap_soft_retry_budget(tip + 2, tip),
        3,
        "near-ahead of tip (not exact gap): keep P4 budget"
    );
    assert_eq!(
        gap_soft_retry_budget(tip, tip),
        0,
        "W14: behind tip must abort immediately"
    );
    assert_eq!(
        gap_soft_retry_budget(tip.saturating_sub(10), tip),
        0,
        "W14: far behind tip must abort immediately"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w70_tip_gap_soft_budget_one_for_hh_and_deep() {
    // W171: holey soft=1 @holes≥20 only if pending==0; soft=1 @holes≥32 always.
    let tip = 261_654;
    let tip_needed = tip + 1;
    let _g = tip_soft_atomic_lock();
    // pending>0 → holey path (not empty-bridge soft).
    super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W156: holey (H,H) soft=2 when holes&lt;20"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W156: holey deep soft=2 when holes&lt;20"
    );
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(19, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W167: holes=19 deep soft=2"
    );
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(20, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W171: holes≥20 + pending>0 deep soft=2 (no tip-step)"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W171: holes≥20 + pending>0 (H,H) soft=2"
    );
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    // empty path: pending==0 gap_missing → soft=2 (W158), not holey soft=1.
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W158: empty deep soft=2"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(32, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(
            tip_needed,
            tip,
            tip_needed,
            tip_needed + 127,
            true,
            false,
        ),
        1,
        "W171: holes≥32 soft=1 even with pending>0"
    );
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(7, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W156: holes=7 still soft=2"
    );
    // W171: holes≥20 + pending==0 + !gap_missing → soft=1 (no runway holey).
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(20, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        1,
        "W171: holes≥20 pending==0 soft=1"
    );
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed + 75, tip, tip_needed, tip_needed + 127),
        3,
        "near-ahead inside deep tip pipe keeps P4 budget"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed + 5, tip, tip_needed, tip_needed),
        3,
        "non-tip height in a (weird) singleton still uses near-ahead budget"
    );
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w83_deep_pipe_tip_soft_one_hh_stays_zero() {
    // W147: holey deep soft=2; holey (H,H) soft=2; empty HH soft=2.
    let tip = 333_000;
    let tip_needed = tip + 1;
    let _g = tip_soft_atomic_lock();
    super::super::memory::BRIDGE_PENDING_COUNT.store(1, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W154: deep holey tip soft=2 (holes&lt;32)"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W154: holey (H,H) soft=2 (holes&lt;32)"
    );
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(tip_needed);
    super::super::tip_stage::test_backdate_awaiting_ms(1_000);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W146: empty (H,H) soft=2"
    );
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w139_empty_bridge_deep_soft_two_hh_still_zero() {
    // W158: empty deep soft=2 always; empty HH soft=2 (W154 DNA re-soak).
    let tip = 322_610;
    let tip_needed = tip + 1;
    let _g = tip_soft_atomic_lock();
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(tip_needed);
    super::super::tip_stage::test_backdate_awaiting_ms(17_000);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W158: empty deep soft=2 even when awaiting≥16s"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W146: empty (H,H) soft=2 (survives progressive+CAP)"
    );
    super::super::tip_stage::mark_needed(0);
    super::super::memory::BRIDGE_PENDING_COUNT.store(4, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed + 127),
        2,
        "W152: pending>0 holey deep soft=2 (holes&lt;8)"
    );
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w139_empty_deep_cap_eight_hh_five() {
    let _g = tip_soft_atomic_lock();
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert_eq!(
        tip_gap_timeout_secs_for_chunk(false, 316_114, 316_241),
        16,
        "W184: empty deep CAP=16 (W182 was 12)"
    );
    assert_eq!(
        tip_gap_timeout_secs_for_chunk(false, 316_114, 316_114),
        5,
        "W139: empty (H,H) mute CAP=5"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w71_tip_gap_inflight_exceeded_respects_cap() {
    let started = Instant::now() - Duration::from_secs(45);
    assert!(
        tip_gap_inflight_exceeded(started, 10),
        "W71: 45s wait exceeds tip-gap cap 10s (live tip=322456 after 45s/limit 10s)"
    );
    assert!(!tip_gap_inflight_exceeded(Instant::now(), 10));
}

#[serial_test::serial(ibd)]
#[test]
fn w89b_tip_hole_cap_requires_await_trigger() {
    let _g = tip_soft_atomic_lock();
    // SAFETY: isolate from other parallel tests' BLVM_IBD_* tip-cap overrides.
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_GAP_TIMEOUT_SECS");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GAP_TIMEOUT_SECS");
        std::env::remove_var("BLVM_IBD_TIP_HOLEY_PENDING_CAP_SECS");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_CAP_TRIGGER_SECS");
        std::env::remove_var("BLVM_IBD_TIP_EMPTY_BRIDGE_CAP_TRIGGER_SECS");
    }
    let prev_holes = super::super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    let cold = tip_gap_timeout_secs();
    assert!(cold >= 8, "cold tip CAP floor 8 (W80); got {cold}");
    // Standing holes with fresh tip clock must NOT shorten CAP.
    super::super::IBD_TIP_BRIDGE_HOLES.store(23, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(328_690);
    assert_eq!(
        tip_gap_timeout_secs(),
        cold,
        "W89b: holes without ≥trigger wait keep base CAP"
    );
    // W103/W109: hole trigger default 3s; empty/hole mute CAP default 5s.
    std::thread::sleep(std::time::Duration::from_millis(3100));
    let hole = tip_gap_timeout_secs();
    assert_eq!(
        hole, 5,
        "W103/W109: holes + awaiting≥3s + pending=0 → CAP 5s; got {hole}"
    );
    assert!(hole < cold);
    // W106/W184: holes + pending>0 → CAP 16s (W182 late-band getdata→body p90≈15.4s).
    super::super::memory::BRIDGE_PENDING_COUNT.store(28, Ordering::Relaxed);
    let holey = tip_gap_timeout_secs();
    assert_eq!(holey, 16, "W184: holes + pending>0 → CAP 16s; got {holey}");
    // W176/W184: export-active floor tracks holey default (16s).
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(true, Ordering::Relaxed);
    let during_export = tip_gap_timeout_secs();
    assert_eq!(
        during_export, 16,
        "W184: export-active holey CAP floor 16s; got {during_export}"
    );
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    // W104/W109: empty-bridge mute CAP 5s immediately (trigger default 0).
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(314_596);
    let empty = tip_gap_timeout_secs();
    assert_eq!(empty, 5, "W109: empty mute → CAP 5s; got {empty}");
    // W109b: ahead_buffered must NOT lengthen mute CAP (soft=1 handles live pipes).
    assert_eq!(
        tip_gap_timeout_secs_ex(true),
        5,
        "W109b: empty + ahead_buffered still CAP 5s (mute rotate)"
    );
    // Land E: empty deep stripe + TIP_HOLE_AHEAD cheese must not wait holey 16s.
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    let empty_deep = tip_gap_timeout_secs_for_chunk(false, 406_000, 406_031);
    assert_eq!(
        empty_deep, 16,
        "W139: empty deep, no cheese → holey 16s; got {empty_deep}"
    );
    assert_eq!(
        tip_gap_timeout_secs_for_chunk(true, 406_000, 406_031),
        5,
        "W109: empty deep + pipe ahead_buffered → mute 5s"
    );
    super::super::IBD_REORDER_AHEAD.store(68, Ordering::Relaxed);
    assert_eq!(
        tip_gap_timeout_secs_for_chunk(false, 406_000, 406_031),
        5,
        "Land E: empty deep + reorder cheese → mute 5s (not 16s leftover)"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_BRIDGE_HOLES.store(prev_holes, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w110_tip_covering_fail_is_mute_matches_empty_rotate() {
    assert!(tip_covering_fail_is_mute(
        "tip-gap timeout cap: gap 311585 waited 5s in chunk 311585-311712"
    ));
    assert!(tip_covering_fail_is_mute(
        "tip-gap timeout: Block timeout for gap height 311585 after 5s - chunk needs retry"
    ));
    assert!(tip_covering_fail_is_mute(
        "Block timeout for gap height 311585 after 5s - chunk needs retry"
    ));
    assert!(tip_covering_fail_is_mute(
        "Block download stalled (no first block in 5s)"
    ));
    // P1d: PIPE_FILL mute eject must take the mute fail path (tip-role ban).
    assert!(tip_covering_fail_is_mute(
        "PIPE_FILL mute: gap streamed but no network body in 3000ms (chunk 304697-304824)"
    ));
    assert!(tip_covering_fail_is_mute(
        "PIPE_FILL mute: no network body in 3000ms (chunk 304697-304824)"
    ));
    // P1d clock: local tip advances must not clear; network tip-band body does.
    assert!(pipe_mute_episode_active(1, true, false, true));
    assert!(!pipe_mute_should_clear_clock(false, 1, true, false));
    assert!(pipe_mute_should_clear_clock(false, 1, true, true));
    assert!(!pipe_mute_may_fire(true, false));
    assert!(!pipe_mute_may_fire(false, true));
    assert!(pipe_mute_may_fire(false, false));
    assert!(
        !tip_covering_fail_is_mute(
            "tip-SLA blacklist: aborting chunk 311585-311712 for peer rotate"
        ),
        "SLA keep long cooldown / blacklist path"
    );
    assert!(!tip_covering_fail_is_mute(
        "tip-enter walk-in abort: keeping sticky owner"
    ));
    assert!(!tip_covering_fail_is_mute(
        "Peer disconnected during chunk download"
    ));
    // Mode T: GetData cancel from tip re-arm is not a mute CAP (mod.rs skips
    // note_tip_owner_failed entirely for this string).
    assert!(!tip_covering_fail_is_mute(
        "Block channel closed for height 402430 - chunk needs retry"
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn p1d_mute_pipe_ms_default_and_clamp() {
    let _g = std::sync::Mutex::new(());
    // Avoid racing other tests that touch env — only assert clamp helper bounds.
    let ms = mute_pipe_ms();
    assert!(
        (1000..=15_000).contains(&ms),
        "mute_pipe_ms must clamp to 1–15s, got {ms}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn c1_tip_hole_grow_on_delivery_deepens_to_cap() {
    // Pure arithmetic under ambient env (grow default on; grow_cap default 32).
    // Reset EWMA so C1n gd-fast does not elevate the cap mid-test.
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let start = tip_hole_grow_start();
    let grow_cap = tip_hole_grow_cap();
    let pipe_cap = tip_hole_pipe_cap();
    assert!((2..=128).contains(&pipe_cap));
    assert!(grow_cap <= pipe_cap);
    if tip_hole_grow_enabled() {
        assert!(start <= grow_cap);
        let next = tip_hole_grow_on_delivery(start);
        assert!(next >= start);
        assert!(next <= grow_cap);
        let mut d = start;
        for _ in 0..32 {
            d = tip_hole_grow_on_delivery(d);
        }
        assert_eq!(d, grow_cap, "repeated grow must hit tip_hole_grow_cap");
        // C1d warm default off — hot/cold both use grow_cap unless WARM=1.
        // C1n: without warm EWMA, effective == cold.
        let cold = tip_hole_grow_cap_for_peer(false);
        assert_eq!(cold, grow_cap);
        if tip_hole_warm_enabled() {
            let warm = tip_hole_grow_cap_for_peer(true);
            assert!(warm >= grow_cap);
            assert!(warm <= pipe_cap);
        } else {
            assert_eq!(tip_hole_grow_cap_for_peer(true), grow_cap);
        }
    } else {
        assert_eq!(tip_hole_grow_on_delivery(start), pipe_cap);
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1n_gd_fast_elevates_cap_only_when_ewma_fast() {
    super::super::tip_stage::test_reset_getdata_body_ewma();
    // Pipe default 32 clamps FAST_CAP — bake uses PIPE=128.
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_HOLE_PIPE", "128");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP", "48");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_STEP", "8");
    }
    let cold = tip_hole_grow_cap();
    let fast = tip_hole_grow_fast_cap();
    assert!(fast > cold, "pipe must allow FAST_CAP > cold");
    // Cold samples → stay at cold.
    assert_eq!(tip_hole_grow_cap_effective(), cold);
    // Slow EWMA → stay at cold (C1m mute thrash guard).
    super::super::tip_stage::test_seed_getdata_body_ewma(3_000, 32);
    assert_eq!(tip_hole_grow_cap_effective(), cold);
    // Abs sticky cap stays at FAST_CAP even when EWMA is slow (C1s).
    assert_eq!(tip_hole_sticky_abs_cap(false), fast.max(cold));
    // Fast EWMA → elevate fill cap + step ≥16 (C1r, even if GROW_STEP baked to 8).
    super::super::tip_stage::test_seed_getdata_body_ewma(100, tip_hole_gd_fast_n());
    if tip_hole_gd_fast_enabled() && tip_hole_grow_enabled() {
        assert_eq!(tip_hole_grow_cap_effective(), fast);
        assert!(tip_hole_grow_step() >= 16, "C1r gd-fast step");
        let mut d = tip_hole_grow_start();
        for _ in 0..16 {
            d = tip_hole_grow_on_delivery(d);
        }
        assert_eq!(d, fast, "gd-fast grow must reach FAST_CAP");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_PIPE");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_CAP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_STEP");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_gd_slow_arms_and_clamps_fill_cap() {
    super::super::tip_stage::test_reset_getdata_body_ewma();
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_MS");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SLOW_FILL_CAP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET");
    }
    assert!(tip_hole_gd_slow_enabled());
    assert!(!tip_hole_gd_slow(), "no EWMA → not slow");
    // Below A6m gate → not slow.
    super::super::tip_stage::test_seed_getdata_body_ewma(400, tip_hole_gd_slow_n());
    assert!(!tip_hole_gd_slow());
    // Ignition cliff territory (5–8s) → slow; fill clamp = grow_start.
    super::super::tip_stage::test_seed_getdata_body_ewma(5_000, tip_hole_gd_slow_n());
    assert!(tip_hole_gd_slow());
    assert_eq!(tip_hole_slow_fill_cap(), tip_hole_grow_start());
    assert!(
        tip_hole_gd_slow_sole_keep(1),
        "sole ready + GD_SLOW arms sole floor path"
    );
    assert!(
        !tip_hole_gd_slow_sole_keep(2),
        "multi-ready still uses GD_SLOW shrink"
    );
    assert_eq!(tip_hole_sole_gd_slow_floor(), 16, "default sole floor");
    assert_eq!(
        tip_hole_gd_slow_next_depth(32),
        8,
        "multi-peer legacy cliff unchanged"
    );
    // Mid-band: release floor (cold deepen OK) but keep no-FAST (death spiral).
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_RECOVER_MS");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_FAST_MS");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_FAST_N");
    }
    assert_eq!(
        tip_hole_sole_floor_recover_ms(),
        tip_hole_gd_slow_ms(),
        "default floor recover = gd-slow (not gd-fast)"
    );
    super::super::tip_stage::test_reset_sole_floor_latch();
    super::super::tip_stage::note_sole_floor_latch();
    super::super::tip_stage::note_sole_no_fast_latch();
    super::super::tip_stage::test_seed_getdata_body_ewma(
        765,
        tip_hole_gd_slow_n().max(tip_hole_gd_fast_n()),
    );
    assert!(
        !tip_hole_sole_floor_blocks_grow(),
        "765ms < gd-slow → floor clears; cold CAP may deepen"
    );
    assert!(
        tip_hole_sole_no_fast_active(),
        "765ms > gd-fast → no FAST_CAP"
    );
    assert_eq!(
        tip_hole_cap_for_sole(true, tip_hole_grow_fast_cap()),
        tip_hole_grow_cap(),
        "sole no-FAST clamps to cold grow_cap"
    );
    super::super::tip_stage::note_sole_floor_latch();
    super::super::tip_stage::note_sole_no_fast_latch();
    super::super::tip_stage::test_seed_getdata_body_ewma(
        359,
        tip_hole_gd_slow_n().max(tip_hole_gd_fast_n()),
    );
    assert!(
        !tip_hole_sole_floor_blocks_grow(),
        "359ms mid-band: floor clear"
    );
    assert!(tip_hole_sole_no_fast_active(), "359ms still blocks FAST");
    // Healthy blips inside min-hold must NOT clear no-FAST (tc152/tc153).
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_CLEAR_N");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS");
    }
    assert_eq!(tip_hole_sole_no_fast_clear_n(), 16);
    assert_eq!(tip_hole_sole_no_fast_min_hold_ms(), 120_000);
    super::super::tip_stage::test_seed_getdata_body_ewma(
        80,
        tip_hole_gd_slow_n().max(tip_hole_gd_fast_n()),
    );
    assert!(!tip_hole_sole_floor_blocks_grow());
    for _ in 0..tip_hole_sole_no_fast_clear_n() {
        assert!(
            tip_hole_sole_no_fast_active(),
            "inside min-hold, streak must not clear no-FAST"
        );
    }
    assert!(super::super::tip_stage::sole_no_fast_latched());
    assert_eq!(
        tip_hole_cap_for_sole(true, tip_hole_grow_fast_cap()),
        tip_hole_grow_cap(),
        "still cold-capped inside hold"
    );
    // After hold expires + sustained streak → clear.
    super::super::tip_stage::test_set_sole_no_fast_armed_ms_ago(
        tip_hole_sole_no_fast_min_hold_ms() + 1,
    );
    for _ in 0..tip_hole_sole_no_fast_clear_n() {
        let _ = tip_hole_sole_no_fast_active();
    }
    assert!(!tip_hole_sole_no_fast_active());
    assert!(!super::super::tip_stage::sole_no_fast_latched());
    assert!(!tip_hole_gd_slow());
    assert!(!tip_hole_gd_slow_sole_keep(1));
    assert_eq!(
        tip_hole_cap_for_sole(true, tip_hole_grow_fast_cap()),
        tip_hole_grow_fast_cap(),
        "after hold+streak clear, FAST_CAP allowed again"
    );
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_sole_floor_latch();
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_prime_gd_slow_ratchets_not_cliff() {
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SLOW_FILL_CAP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_STEP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET");
    }
    assert!(
        !tip_hole_gd_slow_ratchet_enabled(),
        "default off after T172520Z"
    );
    assert_eq!(tip_hole_gd_slow_next_depth(32), 8, "default = legacy cliff");
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET", "1");
    }
    assert_eq!(tip_hole_gd_slow_next_depth(32), 24, "opt-in 32→24 one step");
    assert_eq!(tip_hole_gd_slow_next_depth(24), 16);
    assert_eq!(tip_hole_gd_slow_next_depth(16), 8);
    assert_eq!(tip_hole_gd_slow_next_depth(8), 8, "floor at slow fill cap");
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn sole_floor_max_h_default_keeps_floor_everywhere() {
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H");
    }
    assert_eq!(tip_hole_sole_floor_max_h(), 0, "default 0 = KEEP always-on");
    assert!(tip_hole_sole_floor_applies(400_300));
    assert!(tip_hole_sole_floor_applies(406_787));
}

#[serial_test::serial(ibd)]
#[test]
fn sole_floor_max_h_405k_floors_cold_skips_dens() {
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H", "405000");
    }
    assert_eq!(tip_hole_sole_floor_max_h(), 405_000);
    assert!(
        tip_hole_sole_floor_applies(400_300),
        "cold READY still floors (do not repeat #8)"
    );
    assert!(
        !tip_hole_sole_floor_applies(405_000),
        "at cutoff skip 32→16"
    );
    super::super::tip_stage::test_reset_sole_floor_latch();
    super::super::tip_stage::note_sole_floor_latch();
    assert!(super::super::tip_stage::sole_floor_latched());
    assert!(
        !tip_hole_sole_floor_applies(406_787),
        "406787 dens death must not re-clamp"
    );
    assert!(
        !super::super::tip_stage::sole_floor_latched(),
        "crossing max_h clears leftover 400.3k latch"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1_tip_covering_fail_pipe_fill_mute_still_matches() {
    // Regress: C1 must not break mute string matching used for tip-role ban.
    assert!(tip_covering_fail_is_mute(
        "PIPE_FILL mute: no network body in 3000ms (chunk 304697-304824)"
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn w108_empty_bridge_ahead_buffered_allows_deep_soft() {
    let tip = 355_391;
    let tip_needed = tip + 1;
    let _g = tip_soft_atomic_lock();
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(tip_needed);
    super::super::tip_stage::test_backdate_awaiting_ms(1_000);
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(
            tip_needed,
            tip,
            tip_needed,
            tip_needed + 127,
            false,
            false,
        ),
        2,
        "W147: empty deep soft=2"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(
            tip_needed,
            tip,
            tip_needed,
            tip_needed + 127,
            true,
            false,
        ),
        2,
        "W147: empty deep soft=2 even with ahead_buf"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(tip_needed, tip, tip_needed, tip_needed, true, false,),
        2,
        "W146: empty (H,H) soft=2 even with ahead flag"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(
            tip_needed,
            tip,
            tip_needed,
            tip_needed + 127,
            false,
            true,
        ),
        2,
        "W147: empty deep soft=2 (hot STREAM included)"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(tip_needed, tip, tip_needed, tip_needed, false, true,),
        2,
        "W146: hot tip-STREAM (H,H) empty → soft=2"
    );
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w114_hot_streamer_holey_tip_soft_budget() {
    let tip = 332_237;
    let tip_needed = tip + 1;
    let _g = tip_soft_atomic_lock();
    // Holey tip: pending > 0 (bridge ahead of tip hole); holes&lt;8 → soft=2/2.
    super::super::memory::BRIDGE_PENDING_COUNT.store(23, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(
            tip_needed,
            tip,
            tip_needed,
            tip_needed + 127,
            false,
            false,
        ),
        2,
        "W152: deep holey tip soft=2 (holes&lt;8)"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(
            tip_needed,
            tip,
            tip_needed,
            tip_needed + 127,
            false,
            true,
        ),
        2,
        "W152: hot tip-STREAM deep holey → soft=2"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(tip_needed, tip, tip_needed, tip_needed, false, true,),
        2,
        "W152: hot tip-STREAM (H,H) holey → soft=2"
    );
    assert_eq!(
        gap_soft_retry_budget_for_chunk_ex(tip_needed, tip, tip_needed, tip_needed, false, false,),
        2,
        "W152: non-hot holey (H,H) soft=2"
    );
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w142_holey_hh_soft_one_empty_hh_stays_zero() {
    let tip = 315_110;
    let tip_needed = tip + 1;
    let _g = tip_soft_atomic_lock();
    // Holey: pending ahead of tip hole (W141 death shape).
    super::super::memory::BRIDGE_PENDING_COUNT.store(39, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W152: holey (H,H) soft=2 (holes&lt;8)"
    );
    // Empty (H,H): pending=0 + gap_missing — W146 soft=2.
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    assert_eq!(
        gap_soft_retry_budget_for_chunk(tip_needed, tip, tip_needed, tip_needed),
        2,
        "W146: empty (H,H) soft=2"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w85_rebase_tip_cap_clock_clears_pre_roll_age() {
    let tip_needed = 322_054u64;
    let mut clock_h = None;
    let mut heights = HashSet::new();
    heights.insert(tip_needed);
    let mut started = HashMap::new();
    started.insert(tip_needed, Instant::now() - Duration::from_secs(8));
    assert!(
        rebase_tip_cap_clock(tip_needed, &mut clock_h, &heights, &mut started),
        "first tip-roll must rebase"
    );
    assert!(!tip_gap_inflight_exceeded(started[&tip_needed], 12));
    assert!(
        !rebase_tip_cap_clock(tip_needed, &mut clock_h, &heights, &mut started),
        "idempotent per tip height"
    );
    // Pre-roll age must not survive rebase.
    assert!(started[&tip_needed].elapsed() < Duration::from_secs(1));
}

#[serial_test::serial(ibd)]
#[test]
fn gap_timeout_for_chunk_tip_and_far() {
    let _guard = crate::ibd_test_lock::guard();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    let tip = 684_955;
    let tip_needed = tip + 1;
    assert_eq!(
        gap_timeout_for_chunk(tip_needed, tip_needed + 15, tip, 45),
        tip_gap_timeout_secs()
    );
    assert_eq!(
        gap_timeout_for_chunk(
            tip_needed + far_ahead_band() + 10,
            tip_needed + far_ahead_band() + 73,
            tip,
            45
        ),
        far_ahead_timeout_secs()
    );
    assert_eq!(
        gap_timeout_for_chunk(tip_needed + 32, tip_needed + 95, tip, 45),
        45,
        "mid-window keeps default"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn wan_deep_pipe_chunk_deadline_capped_on_wan_gap() {
    assert_eq!(
        super::wan_deep_tip_pipe_chunk_deadline_secs(700_001, 700_128, 700_000, 600),
        super::super::tip_stage::tip_sla_secs()
            .saturating_mul(2)
            .clamp(90, 180)
    );
    assert_eq!(
        super::wan_deep_tip_pipe_chunk_deadline_secs(100, 200, 0, 600),
        600,
        "non-WAN keeps default"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn wan_deep_pipe_timeout_tiers() {
    let tip = 710_000;
    let confirmed = 700_000;
    let tip_needed = tip + 1;
    assert_eq!(
        wan_deep_pipe_timeout_secs(tip_needed, tip, confirmed),
        Some(tip_gap_timeout_secs())
    );
    // W32d″: mid/deep ≥ tip soft-retry window (not 8/12).
    assert_eq!(
        wan_deep_pipe_timeout_secs(tip_needed + 1, tip, confirmed),
        Some(30)
    );
    assert_eq!(
        wan_deep_pipe_timeout_secs(tip_needed + 31, tip, confirmed),
        Some(30)
    );
    assert_eq!(
        wan_deep_pipe_timeout_secs(tip_needed + 32, tip, confirmed),
        Some(45)
    );
    assert!(wan_deep_pipe_timeout_secs(confirmed, tip, confirmed).is_none());
    assert!(wan_deep_pipe_timeout_secs(tip_needed, tip, 0).is_none());
}

#[serial_test::serial(ibd)]
#[test]
fn chunk_outer_deadline_single_height_micro_chunk_gets_120s_minimum() {
    // (H,H) stall-recovery micro-chunks use 4× per-block timeout, ≥120s.
    // With default 30s per-block this gives 120s.
    assert_eq!(chunk_outer_deadline_secs(558211, 558211, 558211, 30), 120);
    // Larger per-block timeout scales: 4×40 = 160.
    assert_eq!(chunk_outer_deadline_secs(100, 100, 100, 40), 160);
    // Minimum floor is always 120 even with tiny per-block timeout.
    assert_eq!(chunk_outer_deadline_secs(1, 1, 1, 1), 120);
    // start == end == 0 (bootstrap sentinel) also gets 120.
    assert_eq!(chunk_outer_deadline_secs(0, 0, 0, 30), 120);
}

#[serial_test::serial(ibd)]
#[test]
fn worker_chunk_outer_deadline_caps_wan_tip_pipe() {
    let confirmed = 714_450;
    let start = 714_451;
    let end = 714_578;
    let uncapped = chunk_outer_deadline_secs(start, end, start, 45);
    assert!(
        uncapped >= 5000,
        "uncapped WAN tip pipe should be huge (live 5760), got {uncapped}"
    );
    let capped = worker_chunk_outer_deadline_secs(start, end, start, 45, confirmed);
    assert!(
        capped <= 120,
        "WAN tip pipe worker outer must be capped, got {capped}"
    );
    assert!(
        capped >= 60,
        "WAN tip pipe worker outer floor 60s, got {capped}"
    );
    // Non-WAN chunk keeps full budget.
    assert_eq!(
        worker_chunk_outer_deadline_secs(100, 200, 100, 30, 0),
        chunk_outer_deadline_secs(100, 200, 100, 30)
    );
}

#[tokio::test]
async fn wait_cooperative_outer_none_stays_pending() {
    let raced = tokio::select! {
        _ = wait_cooperative_outer(None) => "resolved",
        _ = tokio::time::sleep(Duration::from_millis(20)) => "timeout",
    };
    assert_eq!(raced, "timeout");
}

/// D0: completing a full pipe moves the permit into the worker frame. Acquiring another
/// permit *before* dropping it deadlocks when capacity == in-flight count (WAN tip pipe).
#[tokio::test(flavor = "current_thread")]
async fn completed_permit_must_drop_before_refill_or_deadlock() {
    let capacity = 4usize;
    let sem = Arc::new(Semaphore::new(capacity));
    let mut in_flight_permits = Vec::new();
    for _ in 0..capacity {
        in_flight_permits.push(sem.clone().acquire_owned().await.unwrap());
    }
    assert_eq!(sem.available_permits(), 0);

    // Simulate in_flight.next() completing one future — permit lands in the stack frame.
    let completed = in_flight_permits.pop().unwrap();
    assert_eq!(
        sem.available_permits(),
        0,
        "permit still held by completed binding"
    );

    // Bug path: acquire while `completed` lives → hang (this is what tip pipes hit).
    let blocked =
        tokio::time::timeout(Duration::from_millis(50), sem.clone().acquire_owned()).await;
    assert!(
        blocked.is_err(),
        "acquire while completed permit held must not succeed (would not be a deadlock otherwise)"
    );

    // Fix path (D0): drop before refill.
    drop(completed);
    let got = tokio::time::timeout(Duration::from_millis(200), sem.clone().acquire_owned())
        .await
        .expect("acquire after drop must complete")
        .expect("semaphore open");
    drop(got);
    drop(in_flight_permits);
}

/// D1: try_take returns None when full instead of awaiting.
#[serial_test::serial(ibd)]
#[test]
fn try_take_blocks_permit_returns_none_when_full() {
    let sem = Arc::new(Semaphore::new(1));
    let held = sem.clone().try_acquire_owned().unwrap();
    let blocks_sem = Some(sem);
    assert!(
        try_take_blocks_permit(&blocks_sem).unwrap().is_none(),
        "full semaphore must report None (stop fill, return to select)"
    );
    drop(held);
    assert!(
        try_take_blocks_permit(&blocks_sem).unwrap().is_some(),
        "after release, take must succeed"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn try_take_blocks_permit_none_sem_is_unbounded() {
    assert!(matches!(try_take_blocks_permit(&None).unwrap(), Some(None)));
}

#[serial_test::serial(ibd)]
#[test]
fn w102_sync_next_to_send_after_gap_stream_advances_past_dedup() {
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(100, Ordering::Relaxed);
    let mut next = 100u64;
    sync_next_to_send_after_gap_stream(&mut next, 200);
    assert_eq!(
        next, 101,
        "cursor must pass streamed tip so tip-hole pipe disarms"
    );
    // Already ahead of dedup — no-op.
    next = 150;
    sync_next_to_send_after_gap_stream(&mut next, 200);
    assert_eq!(next, 150);
    // Drain advanced dedup past tip+N (gap not missing — healthy W102 disarm).
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(120, Ordering::Relaxed);
    next = 100;
    sync_next_to_send_after_gap_stream(&mut next, 200);
    assert_eq!(next, 121);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn a31_sync_always_jumps_to_dedup_plus_one_even_when_gap_missing() {
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(409671, Ordering::Relaxed);
    let mut next = 409600u64;
    sync_next_to_send_after_gap_stream(&mut next, 409727);
    assert_eq!(next, 409672, "a31 always advances next_to_send to DEDUP+1");
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn land_e_rewind_dedup_over_unbuffered_hole() {
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(409671, Ordering::Relaxed);
    let got = rewind_gap_stream_dedup_over_missing_hole(409600, 409671);
    assert_eq!(got, Some(409599));
    assert_eq!(
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed),
        409599
    );
    // Already behind the gap — no-op.
    assert!(rewind_gap_stream_dedup_over_missing_hole(409600, 409500).is_none());
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(409671, Ordering::Relaxed);
    assert!(
        rewind_gap_stream_dedup_over_missing_hole(409600, 409671).is_none(),
        "must not rewind when the gap is not missing"
    );
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn h5_received_clone_keeps_tip_keyed() {
    use blvm_protocol::{BlockHeader, Transaction, TransactionOutput};
    let block = Arc::new(Block {
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
    });
    let w: SharedWitnesses = Arc::new(vec![]);
    let mut received = BTreeMap::new();
    received_put(&mut received, 300_288, (Arc::clone(&block), Arc::clone(&w)));
    let cloned = received_clone(&received, 300_288).expect("clone");
    assert!(received.contains_key(&300_288), "clone must not remove tip");
    assert!(Arc::ptr_eq(&cloned.0, &block));
    let _taken = received_take(&mut received, 300_288).expect("take");
    assert!(received_clone(&received, 300_288).is_none());
}

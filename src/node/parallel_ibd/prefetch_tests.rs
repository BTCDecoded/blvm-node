use super::*;
use blvm_protocol::{Block, BlockHeader, Hash, UtxoSet};
use crossbeam_channel::unbounded;

fn dummy_ready(height: u64) -> ReadyItem {
    let block = Arc::new(Block {
        header: BlockHeader {
            version: 1,
            prev_block_hash: [0u8; 32],
            merkle_root: [height as u8; 32],
            timestamp: 1,
            bits: 0x0f00ffff,
            nonce: 0,
        },
        transactions: vec![].into(),
    });
    (
        height,
        block,
        Arc::new(Vec::new()),
        Vec::new(),
        Arc::new(FxHashMap::default()),
        Vec::<Hash>::new(),
        Arc::new(UtxoSet::default()),
    )
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_emits_in_height_order() {
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    bridge.worker_complete(11, dummy_ready(11), 0);
    assert!(rx.try_recv().is_err());
    bridge.worker_complete(10, dummy_ready(10), 0);
    assert_eq!(rx.recv().unwrap().0, 10);
    assert_eq!(rx.recv().unwrap().0, 11);
    bridge.worker_complete(12, dummy_ready(12), 0);
    assert_eq!(rx.recv().unwrap().0, 12);
}

#[serial_test::serial(ibd)]
#[test]
fn worker_complete_drops_duplicate_below_cursor() {
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    bridge.worker_complete(10, dummy_ready(10), 0);
    assert_eq!(rx.recv().unwrap().0, 10);
    assert_eq!(bridge.next_expected(), Some(11));
    // Late duplicate of 10 must not strand in pending or rewind cursor.
    bridge.worker_complete(10, dummy_ready(10), 0);
    assert!(rx.try_recv().is_err());
    assert_eq!(bridge.next_expected(), Some(11));
    assert!(!bridge.pending_contains(10));
}

#[serial_test::serial(ibd)]
#[test]
fn r376_below_cursor_bodies_flush_when_tip_arrives() {
    let _lock = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    let feeder = super::super::feeder::new_feeder_state();
    bridge.attach_feeder(Arc::clone(&feeder));
    // Resume10 14:19Z: cursor ~700 ahead, feeder full of those heights,
    // validation tip not among them. Bodies between the tip and the cursor
    // must survive until the tip rewinds the cursor.
    bridge.coordinator_will_send_height(110);
    for h in 101..106 {
        assert!(
            bridge
                .try_emit_in_order_to_feeder(h, dummy_ready(h), &feeder, 99)
                .is_none()
        );
        assert!(bridge.pending_contains(h), "parked {h}");
    }
    assert_eq!(bridge.next_expected(), Some(110));
    assert!(feeder.0.lock().0.get(101).is_none());
    // Already validated: still dropped.
    assert!(
        bridge
            .try_emit_in_order_to_feeder(99, dummy_ready(99), &feeder, 99)
            .is_none()
    );
    assert!(!bridge.pending_contains(99));
    assert!(
        bridge
            .try_emit_in_order_to_feeder(100, dummy_ready(100), &feeder, 99)
            .is_none()
    );
    let guard = feeder.0.lock();
    for h in 100..106 {
        assert!(guard.0.get(h).is_some(), "feeder has {h}");
    }
    drop(guard);
    assert_eq!(bridge.next_expected(), Some(106));
    assert!(!bridge.pending_contains(101));
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn rewind_cursor_allows_reemit_of_lost_tip() {
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    bridge.worker_complete(10, dummy_ready(10), 0);
    let _ = rx.recv();
    bridge.worker_complete(11, dummy_ready(11), 0);
    let _ = rx.recv();
    assert_eq!(bridge.next_expected(), Some(12));
    assert!(bridge.rewind_cursor_to(11));
    assert_eq!(bridge.next_expected(), Some(11));
    bridge.worker_complete(11, dummy_ready(11), 0);
    assert_eq!(rx.recv().unwrap().0, 11);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_may_accept_respects_pending_cap() {
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    // Fill pending with ahead heights (gap at 10).
    bridge.worker_complete(11, dummy_ready(11), 0);
    bridge.worker_complete(12, dummy_ready(12), 0);
    assert_eq!(bridge.pending_len(), 2);
    assert!(
        !bridge.may_accept_height(13, 2),
        "at cap: refuse ahead height"
    );
    assert!(
        bridge.may_accept_height(10, 2),
        "at cap: still accept gap height to drain"
    );
    assert!(bridge.may_accept_height(13, 3), "under cap: accept ahead");
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_try_flush_emits_when_gap_already_pending() {
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    // Gap height buffered without a subsequent worker_complete to trigger flush.
    {
        let mut g = bridge.inner.lock().unwrap();
        g.pending.insert(10, dummy_ready(10));
        g.pending.insert(11, dummy_ready(11));
        sync_bridge_pending_count(&g);
    }
    assert!(bridge.pending_contains(10));
    assert_eq!(bridge.try_flush(), 2);
    assert_eq!(rx.recv().unwrap().0, 10);
    assert_eq!(rx.recv().unwrap().0, 11);
    assert_eq!(bridge.pending_len(), 0);
    assert_eq!(bridge.next_expected(), Some(12));
}

#[serial_test::serial(ibd)]
#[test]
fn try_emit_direct_to_feeder_even_when_pending_has_ahead() {
    // W26: tip must not take the ready-channel hop just because ahead blocks are pending.
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    {
        let mut g = bridge.inner.lock().unwrap();
        g.pending.insert(11, dummy_ready(11));
        g.pending.insert(12, dummy_ready(12));
        sync_bridge_pending_count(&g);
    }
    let feeder = super::super::feeder::new_feeder_state();
    let leftover = bridge.try_emit_in_order_to_feeder(10, dummy_ready(10), &feeder, 9);
    assert!(leftover.is_none(), "tip must be direct-emitted");
    assert!(
        feeder.0.lock().0.get(10).is_some(),
        "tip must land in feeder buffer"
    );
    // W56: contiguous ahead drained directly into feeder (not ready channel).
    assert!(
        feeder.0.lock().0.get(11).is_some() && feeder.0.lock().0.get(12).is_some(),
        "contiguous pending must land in feeder with tip"
    );
    assert!(
        rx.try_recv().is_err(),
        "must not advance cursor via ready-channel hop"
    );
    assert_eq!(bridge.pending_len(), 0);
    assert_eq!(bridge.next_expected(), Some(13));
}

#[serial_test::serial(ibd)]
#[test]
fn try_emit_skips_reemit_when_tip_already_taken() {
    // W40: vh atomic lags retire; tip_stage advances on feeder take — no REEMIT storm.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    let _ = bridge.try_emit_in_order_to_feeder(
        10,
        dummy_ready(10),
        &super::super::feeder::new_feeder_state(),
        9,
    );
    assert_eq!(bridge.next_expected(), Some(11));
    // W59: take is latched under feeder lock *before* finish_validated / HEIGHT roll.
    super::super::tip_stage::mark_needed(10);
    super::super::tip_stage::mark_getdata(10);
    super::super::tip_stage::mark_body(10);
    super::super::tip_stage::mark_feeder(10);
    super::super::tip_stage::mark_taken_from_feeder(10);
    assert!(
        super::super::tip_stage::tip_taken_by_validation(10),
        "taken latch must suppress REEMIT/REWIND before finish_validated"
    );
    super::super::tip_stage::finish_validated(10);
    assert!(super::super::tip_stage::tip_taken_by_validation(10));
    let feeder = super::super::feeder::new_feeder_state();
    let leftover = bridge.try_emit_in_order_to_feeder(10, dummy_ready(10), &feeder, 9);
    assert!(leftover.is_none());
    assert!(
        feeder.0.lock().0.get(10).is_none(),
        "must not re-insert tip already taken by validation"
    );
    assert_eq!(bridge.next_expected(), Some(11));
}

#[serial_test::serial(ibd)]
#[test]
fn fast_forward_cursor_when_validation_ahead() {
    // W26b: bridge behind validation — advance cursor and drop obsolete pending.
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    {
        let mut g = bridge.inner.lock().unwrap();
        g.pending.insert(11, dummy_ready(11));
        g.pending.insert(66, dummy_ready(66));
        sync_bridge_pending_count(&g);
    }
    assert!(bridge.fast_forward_cursor_to(66));
    assert!(!bridge.pending_contains(11), "obsolete pending dropped");
    // flush emits 66 if it was pending → cursor becomes 67
    assert_eq!(bridge.next_expected(), Some(67));
    assert_eq!(bridge.pending_len(), 0);
    let _ = rx.try_recv(); // 66 may already have been flushed
}

#[serial_test::serial(ibd)]
#[test]
fn repair_missing_cursor_hole_flushes_stranded_tip() {
    // Live 698202: bridge_next=698199 missing, tip 698202 in pending.
    let (tx, rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(199);
    {
        let mut g = bridge.inner.lock().unwrap();
        g.pending.insert(200, dummy_ready(200));
        g.pending.insert(202, dummy_ready(202));
        sync_bridge_pending_count(&g);
    }
    assert!(!bridge.pending_contains(199));
    assert!(bridge.repair_missing_cursor_hole(202));
    assert_eq!(rx.recv().unwrap().0, 202);
    assert_eq!(bridge.next_expected(), Some(203));
    assert!(!bridge.pending_contains(200), "below-tip pending dropped");
}

#[serial_test::serial(ibd)]
#[test]
fn repair_missing_cursor_hole_skips_when_tip_not_pending() {
    // Local-replay race: hole at cursor but tip not yet in pending — do not FF.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(100);
    {
        let mut g = bridge.inner.lock().unwrap();
        g.pending.insert(101, dummy_ready(101)); // only below tip
        g.pending.insert(200, dummy_ready(200)); // ahead prefetch — must not trigger FF
        sync_bridge_pending_count(&g);
    }
    assert!(!bridge.repair_missing_cursor_hole(105));
    assert_eq!(bridge.next_expected(), Some(100));
    assert!(bridge.pending_contains(101));
    assert!(bridge.pending_contains(200));
}

#[serial_test::serial(ibd)]
#[test]
fn flush_try_send_full_channel_leaves_pending_and_returns() {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    // Emit 10 into the single channel slot.
    bridge.worker_complete(10, dummy_ready(10), 0);
    // Channel full — next in-order complete must not block; leave in pending.
    bridge.worker_complete(11, dummy_ready(11), 0);
    assert!(
        bridge.pending_contains(11),
        "full channel must park tip in pending, not block coordinator"
    );
    assert_eq!(bridge.next_expected(), Some(11));
    // Drain channel then flush.
    assert_eq!(rx.try_recv().unwrap().0, 10);
    assert!(bridge.try_flush() >= 1);
    assert_eq!(rx.try_recv().unwrap().0, 11);
    assert_eq!(bridge.next_expected(), Some(12));
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_evicts_far_ahead_when_gap_missing() {
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    // Fill pending at cap with far-ahead heights (gap at 10 missing).
    for h in 11..=13 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    assert_eq!(bridge.pending_len(), 3);
    assert_eq!(
        bridge.evict_far_ahead_pending(10, 1, true, 3),
        2,
        "evict h=12,13 above ceiling=11"
    );
    assert_eq!(bridge.pending_len(), 1);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_evicts_when_next_expected_missing_from_pending() {
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    // IBD_STALL pattern: bridge_next=10, pending full of ahead, gap not in pending.
    // gap_missing=false (near-ahead may be in reorder) but next_expected absent from pending.
    for h in 11..=13 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    assert_eq!(bridge.pending_len(), 3);
    assert!(!bridge.pending_contains(10));
    assert_eq!(
        bridge.evict_far_ahead_pending(10, 1, false, 3),
        2,
        "evict far-ahead when next_expected missing from pending even if gap_missing=false"
    );
    assert_eq!(bridge.pending_len(), 1);
    // After putting next_expected into pending, no further eviction with gap_missing=false
    // when under cap.
    bridge.worker_complete(10, dummy_ready(10), 0);
    // pending may have flushed 10+11; refill ahead to cap without gap_missing path.
    let (tx2, _rx2) = unbounded();
    let bridge2 = OrderedReadyBridge::new(tx2);
    bridge2.coordinator_will_send_height(20);
    bridge2.worker_complete(20, dummy_ready(20), 0); // in-order: flushes, pending empty
    bridge2.worker_complete(21, dummy_ready(21), 0);
    bridge2.worker_complete(22, dummy_ready(22), 0);
    // next_expected=23 missing; pending=2 < min_pending=max(64,3/4).min(3)=3 → no eviction
    assert_eq!(bridge2.evict_far_ahead_pending(23, 1, false, 3), 0);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_b1_evicts_under_cap_when_next_expected_missing() {
    // Live soak: bridge_pending=64–450, never hit 512 — old gate never fired.
    // B1: next_expected missing → min_pending = max(64, pending_max/4).
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(100);
    // Fill 64 ahead heights (under pending_max=256).
    for h in 101..=164 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    assert_eq!(bridge.pending_len(), 64);
    assert!(!bridge.pending_contains(100));
    let evicted = bridge.evict_far_ahead_pending(100, 1, false, 256);
    assert!(
        evicted >= 1,
        "B1 must evict far-ahead under cap when next_expected missing (evicted={evicted})"
    );
    assert!(
        bridge.pending_len() < 64,
        "pending should shrink after under-cap eviction"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_b1b_peel_disabled_by_default() {
    // L2: inside-window peel is opt-in. Far-ceiling (tight keep) still runs for heights
    // above next+tight_keep; this fixture keeps all pending ≤ ceiling so only peel
    // would have fired — and peel must not.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(100);
    // window=256 → tight_keep=64 → ceiling=164. Fill 101..=164 (all ≤ ceiling).
    for h in 101..=164 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    assert_eq!(bridge.pending_len(), 64);
    assert!(!bridge.pending_contains(100));
    let evicted = bridge.evict_far_ahead_pending(100, 256, false, 256);
    assert_eq!(
        evicted, 0,
        "default must not peel inside tight keep band (evicted={evicted})"
    );
    assert_eq!(bridge.pending_len(), 64);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_no_evict_when_under_cap() {
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(10);
    bridge.worker_complete(11, dummy_ready(11), 0);
    assert_eq!(
        bridge.evict_far_ahead_pending(10, 1, true, 3),
        0,
        "under min_pending: no eviction"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_w17_wan_default_preserves_tip_pipe() {
    let _guard = crate::ibd_test_lock::guard();
    // SAFETY: test-only; peel opt-in must not leak from parallel env tests.
    unsafe { std::env::remove_var("BLVM_IBD_WAN_B1B_PEEL") };
    // 2026-07-14: default WAN peel/tight_keep=16 wiped pending→0 every tip tick.
    // Near-ahead inside next+128 must be kept while tip is missing.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(100);
    for h in 101..=164 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    assert_eq!(bridge.pending_len(), 64);
    let evicted = bridge.evict_far_ahead_pending_ex(100, 256, true, 256, true);
    assert_eq!(
        evicted, 0,
        "default WAN must not peel/wipe tip-pipe band (evicted={evicted})"
    );
    assert_eq!(bridge.pending_len(), 64);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_w17_wan_peel_opt_in() {
    // Opt-in peel still available for experiments.
    // SAFETY: test-only env mutation; serial within this module.
    unsafe { std::env::set_var("BLVM_IBD_WAN_B1B_PEEL", "1") };
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(100);
    for h in 101..=164 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    let evicted = bridge.evict_far_ahead_pending_ex(100, 256, true, 256, true);
    unsafe { std::env::remove_var("BLVM_IBD_WAN_B1B_PEEL") };
    assert!(
        evicted > 0,
        "WAN peel with BLVM_IBD_WAN_B1B_PEEL=1 must evict (evicted={evicted})"
    );
    assert!(bridge.pending_len() < 64);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_w18_purges_stale_below_floor() {
    // Live W17: bridge_min=640001 while tip=6859xx — dead pending never flushed.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(200);
    // Insert stale heights below next_expected via pending map.
    {
        let mut g = bridge.inner.lock().unwrap();
        g.pending.insert(50, dummy_ready(50));
        g.pending.insert(51, dummy_ready(51));
        g.pending.insert(201, dummy_ready(201));
        sync_bridge_pending_count(&g);
    }
    assert_eq!(bridge.pending_len(), 3);
    // Tip healthy — still purge stale below floor.
    let evicted = bridge.evict_far_ahead_pending_ex(200, 256, false, 512, true);
    assert!(
        evicted >= 2,
        "must purge h=50,51 below floor=200 (evicted={evicted})"
    );
    assert!(!bridge.pending_contains(50));
    assert!(!bridge.pending_contains(51));
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_w19_under_min_only_evicts_beyond_far_ceiling() {
    // 2026-07-14: under-min + tight_keep=16 deleted tip-pipe (h=120..139 with tip=100).
    // Under min_pending, only heights beyond far_ceiling (next+window) may go.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(100);
    // Inside tip-pipe band (next+128) — must keep.
    for h in 120..=139 {
        bridge.worker_complete(h, dummy_ready(h), 0);
    }
    assert_eq!(bridge.pending_len(), 20);
    let kept = bridge.evict_far_ahead_pending_ex(100, 256, false, 512, true);
    assert_eq!(
        kept, 0,
        "must not wipe tip-pipe under min_pending (evicted={kept})"
    );
    assert_eq!(bridge.pending_len(), 20);

    // Beyond far_ceiling=356 — may evict even under min.
    let (tx2, _rx2) = unbounded();
    let bridge2 = OrderedReadyBridge::new(tx2);
    bridge2.coordinator_will_send_height(100);
    for h in 400..=419 {
        bridge2.worker_complete(h, dummy_ready(h), 0);
    }
    let evicted = bridge2.evict_far_ahead_pending_ex(100, 256, false, 512, true);
    assert!(
        evicted > 0,
        "under-min may still drop heights beyond far_ceiling (evicted={evicted})"
    );
    assert!(bridge2.pending_len() < 20);
}

#[serial_test::serial(ibd)]
#[test]
fn ordered_ready_bridge_wan_no_pending_zero_thrash() {
    // Live signature: one ahead ReadyItem arrives, eviction leaves pending=0.
    let (tx, _rx) = unbounded();
    let bridge = OrderedReadyBridge::new(tx);
    bridge.coordinator_will_send_height(392138);
    bridge.worker_complete(392140, dummy_ready(392140), 0);
    assert_eq!(bridge.pending_len(), 1);
    let evicted = bridge.evict_far_ahead_pending_ex(392138, 1024, true, 512, true);
    assert_eq!(evicted, 0, "must not discard lone tip-pipe ReadyItem");
    assert_eq!(bridge.pending_len(), 1);
}

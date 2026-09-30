//! Validation-loop unit tests: pipeline depth, pressure, engine-append gates.
//! Retire/flush batch cases live in `validation_loop_retire_flush_tests.rs`.

use super::*;

fn fresh_cap(initial: usize) -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(initial))
}
fn fresh_last_adapt_zero() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

#[serial_test::serial(ibd)]
#[test]
fn classify_binder_supply_vs_engine() {
    assert_eq!(
        classify_ibd_binder(0, 12, 0, 0, Some(90), PressureLevel::None, false),
        "SUPPLY_TIP_HOLE_STAGED",
        "R-302: feeder=0 holes>0 contig=0 await_ms=0 is staged (in our bridge), not absent"
    );
    assert_eq!(
        classify_ibd_binder(0, 0, 0, 250, None, PressureLevel::None, false),
        "SUPPLY_TIP_HOLE_ABSENT",
        "R-302: await_ms>=200 is H has not arrived"
    );
    assert_eq!(
        classify_ibd_binder(0, 0, 0, 0, None, PressureLevel::None, false),
        "SUPPLY_EMPTY_TIP"
    );
    assert_eq!(
        classify_ibd_binder(0, 0, 8, 0, None, PressureLevel::None, false),
        "SUPPLY_FEEDER_STARVE",
        "no gd sample → still classic starve"
    );
    assert_eq!(
        classify_ibd_binder(0, 0, 66, 0, Some(29), PressureLevel::None, false),
        "PIPE_DRAINED",
        "H3 C3: feeder=0 + healthy gd + contig ≠ supply starve"
    );
    assert_eq!(
        classify_ibd_binder(64, 0, 16, 0, Some(80), PressureLevel::None, false),
        "ENGINE_OR_SCRIPTS"
    );
    assert_eq!(
        classify_ibd_binder(64, 0, 16, 0, None, PressureLevel::Emergency, false),
        "ENGINE_PRESSURE"
    );
    assert_eq!(
        classify_ibd_binder(4, 0, 0, 0, Some(400), PressureLevel::None, false),
        "SUPPLY_GD_SLOW"
    );
}

/// Nominal cap is always positive; adaptation runs at every pressure level.
#[serial_test::serial(ibd)]
#[test]
fn adapt_always_runs_with_positive_nominal() {
    let cap = fresh_cap(5_000_000);
    let last = fresh_last_adapt_zero();
    adapt_max_pending_ops_tick(&cap, 5_000_000, PressureLevel::Emergency, 5_000_000, &last);
    assert!(cap.load(Ordering::Relaxed) < 5_000_000);
}

/// Emergency must aggressively shrink the cap (but respect floors).
#[serial_test::serial(ibd)]
#[test]
fn adapt_emergency_halves_cap() {
    let cap = fresh_cap(8_000_000);
    let last = fresh_last_adapt_zero();
    adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Emergency, 8_000_000, &last);
    let new = cap.load(Ordering::Relaxed);
    assert!(new < 8_000_000, "Emergency must shrink");
    assert!(new >= 100_000, "Emergency must respect 100k floor");
}

/// Critical multiplies by 0.75; floor `nominal/8` keeps it from collapsing.
///
/// Append diagnostics are process-global atomics — keep these cases in one test so
/// parallel `cargo test` filters cannot race the throttle window.
#[serial_test::serial(ibd)]
#[test]
fn pipeline_depth_pressure_and_engine_append_throttle() {
    use crate::storage::ibd_engine::memory_age::{
        bump_append_stats_detailed_for_test, reset_append_diagnostics_for_test,
        set_append_window_baseline_for_test,
    };

    reset_append_diagnostics_for_test();
    let _tip = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::memory::test_seed_ibd_rss_anon_mb(0);
    assert_eq!(pipeline_depth_for_pressure(PressureLevel::None, 32), 32);
    assert_eq!(pipeline_depth_for_pressure(PressureLevel::Emergency, 32), 8);
    assert_eq!(pipeline_depth_for_pressure(PressureLevel::Critical, 32), 16);
    assert_eq!(pipeline_depth_for_pressure(PressureLevel::Elevated, 32), 24);

    // Peak Land E: tip-crawl healthy holds Critical at Elevated depth (24). No C2v2 cap.
    super::super::tip_stage::publish_wan_body_tip(100);
    super::super::tip_stage::mark_needed(200);
    super::super::tip_stage::test_seed_getdata_body_ewma(40, 32);
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
        24,
        "Elevated must not soft-cap (C2 tip30 regress)"
    );
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Critical, 32),
        24,
        "peak: raw Critical + tip-crawl healthy holds at Elevated depth"
    );
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Emergency, 32),
        8,
        "Emergency depth unchanged (real reclaim)"
    );
    assert_eq!(
        engine_pressure_poll_interval(PressureLevel::Critical),
        16,
        "tip-crawl healthy supply must poll Critical like Elevated"
    );

    // Peak: Elevated is always 3/4. r28 small-anon hold is off.
    super::memory::test_seed_ibd_rss_anon_mb(8377);
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
        24,
        "peak: Elevated stays 24 at anon 8G (r28 hold off)"
    );
    assert_eq!(
        engine_pressure_poll_interval(PressureLevel::Elevated),
        16,
        "must not remap poll — that is r26"
    );
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Critical, 32),
        24,
        "must not lift Critical off Land E 24"
    );
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Emergency, 32),
        8,
        "must not hold Emergency"
    );
    super::memory::test_seed_ibd_rss_anon_mb(17174);
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
        24,
        "real Elevated anon 17G stays depth 24"
    );
    super::memory::test_seed_ibd_rss_anon_mb(8377);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::publish_wan_body_tip(100);
    super::super::tip_stage::mark_needed(200);
    assert_eq!(
        pipeline_depth_for_pressure(PressureLevel::Elevated, 32),
        24,
        "unhealthy supply still Elevated 24"
    );
    super::memory::test_seed_ibd_rss_anon_mb(0);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();

    // 33% total slow but 0% contention — must keep full depth.
    reset_append_diagnostics_for_test();
    bump_append_stats_detailed_for_test(70_000, 35_000, 0);
    assert_eq!(pipeline_depth_for_engine_append(16), 16);

    // Pure contention spike → collapse depth.
    reset_append_diagnostics_for_test();
    bump_append_stats_detailed_for_test(90_000, 140_600, 140_600);
    set_append_window_baseline_for_test(90_000, 140_600);
    bump_append_stats_detailed_for_test(0, 256, 256);
    assert_eq!(pipeline_depth_for_engine_append(32), 1);
}

#[serial_test::serial(ibd)]
#[test]
fn engine_pressure_poll_interval_tightens_with_pressure() {
    assert_eq!(engine_pressure_poll_interval(PressureLevel::None), 32);
    assert_eq!(engine_pressure_poll_interval(PressureLevel::Emergency), 1);
    assert_eq!(engine_pressure_poll_interval(PressureLevel::Critical), 4);
}

/// Critical multiplies by 0.75; floor `nominal/8` keeps it from collapsing.
#[serial_test::serial(ibd)]
#[test]
fn adapt_critical_multiplies_by_three_quarters() {
    let cap = fresh_cap(8_000_000);
    let last = fresh_last_adapt_zero();
    adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Critical, 8_000_000, &last);
    let new = cap.load(Ordering::Relaxed);
    assert!(new < 8_000_000);
    assert!(
        new >= 8_000_000 / 8,
        "Critical must respect nominal/8 floor"
    );
}

/// Elevated is a hold — cap unchanged.
#[serial_test::serial(ibd)]
#[test]
fn adapt_elevated_is_hold() {
    let cap = fresh_cap(8_000_000);
    let last = fresh_last_adapt_zero();
    adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Elevated, 8_000_000, &last);
    assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
}

/// `None` + low pending → grow by ~10%, capped at `1.1 × nominal` (integer ×11/10).
#[serial_test::serial(ibd)]
#[test]
fn adapt_none_grows_when_drain_keeps_up() {
    let nominal = 8_000_000;
    let cap = fresh_cap(nominal);
    let last = fresh_last_adapt_zero();
    adapt_max_pending_ops_tick(&cap, nominal, PressureLevel::None, 100_000, &last);
    let new = cap.load(Ordering::Relaxed);
    assert!(new > nominal, "None + drain-ahead must grow cap");
    let ceiling = nominal.saturating_mul(11).saturating_div(10);
    assert!(
        new <= ceiling,
        "Must respect 1.1× nominal ceiling (got {new}, ceiling {ceiling})"
    );
}

/// `None` + high pending → hold (no point growing if validator is racing ahead).
#[serial_test::serial(ibd)]
#[test]
fn adapt_none_holds_when_pending_full() {
    let cap = fresh_cap(8_000_000);
    let last = fresh_last_adapt_zero();
    adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::None, 7_000_000, &last);
    assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
}

/// Throttle: if `last_adapt_ms` is recent, the call is a no-op.
#[serial_test::serial(ibd)]
#[test]
fn adapt_throttle_skips_recent_calls() {
    let cap = fresh_cap(8_000_000);
    let now_ms = crate::utils::time::current_timestamp_millis();
    let last = Arc::new(AtomicU64::new(now_ms));
    adapt_max_pending_ops_tick(&cap, 8_000_000, PressureLevel::Emergency, 8_000_000, &last);
    assert_eq!(cap.load(Ordering::Relaxed), 8_000_000);
}

/// Repeated Emergency ticks must converge to the floor at max(nominal/2, 1_000_000).
/// The previous policy used nominal/16 which caused workers to spin at <1 BPS.
/// Now Emergency uses gentle 10% trim with floor = nominal/2 to keep workers moving.
#[serial_test::serial(ibd)]
#[test]
fn adapt_emergency_respects_floor_under_repeat() {
    let nominal = 8_000_000;
    let cap = fresh_cap(nominal);
    for _ in 0..50 {
        let last = fresh_last_adapt_zero();
        adapt_max_pending_ops_tick(&cap, nominal, PressureLevel::Emergency, nominal, &last);
    }
    let final_cap = cap.load(Ordering::Relaxed);
    // Floor = max(nominal/2, 1_000_000) — must never go below this
    let expected_floor = (nominal / 2).max(1_000_000);
    assert!(
        final_cap >= expected_floor,
        "must respect floor {expected_floor} (got {final_cap})",
    );
    // Must be at most nominal (never grow under emergency pressure)
    assert!(
        final_cap <= nominal,
        "must not exceed nominal (got {final_cap} for nominal {nominal})",
    );
}

/// Regression: holding `utxo_flush_handles` across `join()` wedged IBD shutdown when a
/// RocksDB commit was slow — other paths could not drain or enqueue flushes.
#[serial_test::serial(ibd)]
#[test]
fn join_all_utxo_flush_handles_releases_mutex_before_join() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let started_join = Arc::clone(&started);
    let release_join = Arc::clone(&release);
    let slow = thread::spawn(move || {
        started_join.store(true, Ordering::Release);
        while !release_join.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(blvm_muhash::MuHash3072::new())
    });

    let utxo_flush_handles = Arc::new(Mutex::new(VecDeque::new()));
    utxo_flush_handles.lock().push_back(slow);

    let handles_for_join = Arc::clone(&utxo_flush_handles);
    let joiner = thread::spawn(move || join_all_utxo_flush_handles(&handles_for_join, "test"));

    let wait_start = Instant::now();
    while !started.load(Ordering::Acquire) {
        assert!(
            wait_start.elapsed() < Duration::from_secs(2),
            "slow flush worker did not start"
        );
        thread::sleep(Duration::from_millis(1));
    }

    let lock_start = Instant::now();
    {
        let _guard = utxo_flush_handles.lock();
    }
    assert!(
        lock_start.elapsed() < Duration::from_millis(500),
        "utxo_flush_handles mutex still held during join"
    );

    release.store(true, Ordering::Release);
    joiner.join().expect("join thread").expect("join flushes");
    assert!(utxo_flush_handles.lock().is_empty());
}

/// R-361: the dropper frees the block's last references off-thread; disabled → inline drop.
#[test]
fn r361_deferred_dropper_frees_last_refs() {
    use blvm_consensus::{Block, BlockHeader};
    let mk = || {
        Arc::new(Block {
            header: BlockHeader {
                version: 4,
                ..Default::default()
            },
            transactions: Vec::new().into(),
        })
    };
    let block = mk();
    let weak = Arc::downgrade(&block);
    let mut dropper = DeferredDropper::spawn();
    assert!(dropper.send(DeferredDrop {
        block,
        witnesses: Arc::new(Vec::new()),
        undo_log: Some(blvm_consensus::reorganization::BlockUndoLog::new()),
    }));
    dropper.close_and_join();
    assert!(weak.upgrade().is_none(), "dropper must have freed the block");
    // A second close is a no-op; a send after close reports false and drops inline.
    dropper.close_and_join();
    let block2 = mk();
    let weak2 = Arc::downgrade(&block2);
    assert!(!dropper.send(DeferredDrop {
        block: block2,
        witnesses: Arc::new(Vec::new()),
        undo_log: None,
    }));
    assert!(weak2.upgrade().is_none());

    let off = DeferredDropper::disabled();
    let block3 = mk();
    let weak3 = Arc::downgrade(&block3);
    assert!(!off.send(DeferredDrop {
        block: block3,
        witnesses: Arc::new(Vec::new()),
        undo_log: None,
    }));
    assert!(weak3.upgrade().is_none(), "disabled dropper frees inline");
}

/// R-362: the tip syncer runs the sync off-thread, coalesces a burst of requests into fewer
/// syncs, never loses the last one, and reports `false` (caller flushes inline) when inline or closed.
#[test]
fn r362_tip_syncer_coalesces_and_never_drops_the_last_tip() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let calls = Arc::new(AtomicU64::new(0));
    let max_seen = Arc::new(AtomicU64::new(0));
    let (c, m) = (calls.clone(), max_seen.clone());
    let mut syncer = TipSyncer::spawn_with(move |h| {
        std::thread::sleep(std::time::Duration::from_millis(5));
        c.fetch_add(1, Ordering::SeqCst);
        m.fetch_max(h, Ordering::SeqCst);
        Ok(())
    });
    // Burst of 200 requests while each sync takes 5 ms: every request must be accepted (a
    // queued sync covers it), and the thread must run far fewer than 200 syncs.
    for h in 1..=200u64 {
        assert!(syncer.request(h * 1000), "request {} must be covered", h);
    }
    // The last request must be covered even though the queue was full: the syncer folds the
    // final queued item, and the final sync runs after this point in program order.
    syncer.close_and_join();
    let n = calls.load(Ordering::SeqCst);
    assert!(n >= 1, "at least one sync ran");
    assert!(n < 200, "requests must coalesce (ran {n})");
    assert!(max_seen.load(Ordering::SeqCst) >= 1000, "a real tip was synced");
    // Closed → caller must flush inline.
    assert!(!syncer.request(201_000));
    syncer.close_and_join();

    // Inline mode → caller flushes inline.
    let inline = TipSyncer::inline();
    assert!(!inline.request(1000));

    // Failing sync is logged, not fatal.
    let mut failing = TipSyncer::spawn_with(|_h| Err(anyhow::anyhow!("disk gone")));
    assert!(failing.request(5000));
    failing.close_and_join();
}

/// R-360: the append thread must consume prep-pool output strictly in height order, once each,
/// and reject a height it has already passed.
#[test]
fn r360_in_order_jobs_reassembles_prep_pool_output() {
    let mut q: InOrderJobs<&'static str> = InOrderJobs::new(100);
    assert!(q.pop_ready().is_none());
    q.push(102, "c").unwrap();
    q.push(101, "b").unwrap();
    assert!(q.pop_ready().is_none(), "100 not yet arrived");
    assert_eq!(q.pending_len(), 2);
    q.push(100, "a").unwrap();
    let mut out = Vec::new();
    while let Some(j) = q.pop_ready() {
        out.push(j);
    }
    assert_eq!(out, vec!["a", "b", "c"]);
    assert_eq!(q.next_height(), 103);
    assert_eq!(q.pending_len(), 0);
    assert_eq!(q.push(102, "dup"), Err((103, "dup")));
    q.push(104, "e").unwrap();
    assert!(q.pop_ready().is_none(), "103 missing blocks 104");
    q.push(103, "d").unwrap();
    assert_eq!(q.pop_ready(), Some("d"));
    assert_eq!(q.pop_ready(), Some("e"));
}

/// R-360: the append-thread prep must produce exactly what the orchestrator's dispatch built
/// (txids and, below assume-valid, the output cache); above assume-valid no cache.
#[test]
fn r360_engine_prep_deferred_matches_dispatch_prep() {
    use blvm_consensus::{Block, BlockHeader, Transaction, TransactionOutput};
    let tx = |value: i64| Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![],
        outputs: blvm_protocol::tx_outputs![
            TransactionOutput {
                value,
                script_pubkey: vec![0x51],
            },
            TransactionOutput {
                value: value / 2,
                script_pubkey: vec![0x52],
            }
        ],
        lock_time: 0,
    };
    let block = Block {
        header: BlockHeader {
            version: 4,
            timestamp: 1_600_000_000,
            ..Default::default()
        },
        transactions: vec![tx(50_0000_0000), tx(25_0000_0000), tx(7)].into(),
    };
    let mut expect_ids = Vec::new();
    crate::storage::disk_utxo::compute_tx_ids_only(&block, &mut expect_ids);
    assert_eq!(expect_ids.len(), 3);
    let expect_cache = blvm_consensus::utxo_overlay::build_block_output_utxo_cache(
        &block,
        expect_ids.as_slice(),
        1_000,
    );

    // Below assume-valid: ids filled, cache built and equal to the dispatch-side build.
    let mut ids = Vec::new();
    let cache = engine_prep_deferred(&block, 1_000, 912_683, &mut ids).expect("cache below AV");
    assert_eq!(ids, expect_ids);
    assert_eq!(cache.len(), expect_cache.len());
    assert_eq!(cache.len(), 6);
    for (op, u) in expect_cache.iter() {
        let got = cache.get(op).expect("outpoint present");
        assert_eq!(got.value, u.value);
        assert_eq!(got.script_pubkey, u.script_pubkey);
    }

    // At/above assume-valid: ids still filled, no cache (matches dispatch `h < AV` gate).
    let mut ids_hi = Vec::new();
    assert!(engine_prep_deferred(&block, 912_683, 912_683, &mut ids_hi).is_none());
    assert_eq!(ids_hi, expect_ids);

    // Pre-filled ids are kept, not recomputed.
    let mut pre = expect_ids.clone();
    pre.reverse();
    let _ = engine_prep_deferred(&block, 1_000, 912_683, &mut pre);
    assert_ne!(pre, expect_ids);
}

#[serial_test::serial(ibd)]
#[test]
fn join_all_utxo_flush_handles_empty_queue_is_noop() {
    let utxo_flush_handles = Arc::new(Mutex::new(VecDeque::new()));
    let combined = join_all_utxo_flush_handles(&utxo_flush_handles, "test").expect("empty join");
    assert_eq!(
        combined.finalize(),
        blvm_muhash::MuHash3072::new().finalize()
    );
}

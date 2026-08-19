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
        "SUPPLY_TIP_HOLE"
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

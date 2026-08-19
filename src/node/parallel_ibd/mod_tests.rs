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
fn checkpoint_export_exits_on_validation_height_even_if_ckpt_lagging() {
    // Live hang: cl=957000, interval-aligned ckpt stuck at 880000, end=957804.
    assert!(checkpoint_export_thread_should_exit(
        957804, 957000, 957804, 880000
    ));
    assert!(checkpoint_export_thread_should_exit(957804, 0, 957804, 0));
    assert!(!checkpoint_export_thread_should_exit(
        957000, 957000, 957804, 880000
    ));
}

#[serial_test::serial(ibd)]
#[test]
fn checkpoint_export_exits_on_contiguous_length_or_ckpt() {
    assert!(checkpoint_export_thread_should_exit(0, 957804, 957804, 0));
    assert!(checkpoint_export_thread_should_exit(
        0, 957000, 957804, 957804
    ));
    assert!(!checkpoint_export_thread_should_exit(
        0, 957000, 957804, 880000
    ));
    assert!(checkpoint_export_thread_should_exit(0, 0, 0, 0)); // end_h<=0
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
fn w76_wan_ahead_policy_feeder_starve_uses_tip_window_even_when_bulk() {
    // Mid-chain: headers at network tip ⇒ bulk=true always; feeder empty must not
    // keep the old 1024 bulk-gap window (live tip never in bridge @ ~350k).
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

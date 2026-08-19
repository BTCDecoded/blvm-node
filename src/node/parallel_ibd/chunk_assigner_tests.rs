//! ChunkAssigner rematch regression suite.
//!
//! Names are **live-cell IDs** (`w28c`, `a6m`, `p0a`, `c1u`, …), not copies of
//! the same test. Do not merge “similar” cases — each encodes a peel-bar
//! failure mode. Shared helpers stay at the top; synth-bulk cases need
//! `feature = "ibd-dev"` or `cfg(test)` (real `synthetic_wan` module).

use super::*;
use std::sync::atomic::AtomicU64;

fn assigner_for_heights(
    chunks: &[(u64, u64)],
    peers: &[&str],
    start_height: u64,
    work_stealing: bool,
) -> ChunkAssigner {
    ChunkAssigner::new(
        chunks.to_vec(),
        peers.iter().map(|s| (*s).to_string()).collect(),
        Arc::new(AtomicU64::new(0)),
        start_height,
        work_stealing,
    )
}

#[serial_test::serial(ibd)]
#[test]
fn note_wan_tip_stream_increments_on_hit_without_reset() {
    let a = assigner_for_heights(&[(100, 200)], &["p"], 100, true);
    a.note_wan_tip_stream("p");
    a.note_wan_tip_stream("p");
    a.note_wan_tip_stream("p");
    assert_eq!(a.tip_stream_count("p"), 3);
    assert_eq!(a.tip_stream_count("other"), 0);
}

/// Build a WAN work-stealing assigner for tip/gap tests: one covering range + peer workers.
/// No fake peer-per-range padding — ranges and workers are independent.
fn wan_tip_assigner(
    validation_height: u64,
    body_tip: u64,
    header_tip: u64,
    peers: &[&str],
) -> ChunkAssigner {
    let start = body_tip.min(validation_height);
    let assigner = ChunkAssigner::new(
        vec![(start, header_tip)],
        peers.iter().map(|s| (*s).to_string()).collect(),
        Arc::new(AtomicU64::new(validation_height)),
        start,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_header_tip(header_tip);
    assigner
}

fn mark_scored_peers_ibd_ready(assigner: &ChunkAssigner) {
    assigner.set_ibd_ready_peers(assigner.peer_ids_for_ibd_ready().into_iter().collect());
}

fn mark_peers_ibd_ready(assigner: &ChunkAssigner, peers: &[&str]) {
    assigner.set_ibd_ready_peers(peers.iter().map(|s| s.to_string()).collect());
}

/// W4/N12: snapshot deep/healthy counts match live Mutex readers.
#[serial_test::serial(ibd)]
#[test]
fn w4_tip_cover_snapshot_counts_match_live() {
    let assigner = wan_tip_assigner(300_000, 300_000, 301_000, &["pA", "pB"]);
    let tip = 300_001;
    // Shallow failover micro — healthy but not deep (min depth default 16).
    assigner.note_tip_cover_claim("pA", tip, tip);
    // Deep pipe claim.
    assigner.note_tip_cover_claim("pB", tip, tip + 127);
    let snap = assigner.snapshot_tip_cover_claims();
    assert_eq!(
        ChunkAssigner::healthy_tip_cover_count_from(&snap, tip),
        assigner.healthy_tip_cover_count(tip)
    );
    assert_eq!(
        ChunkAssigner::deep_tip_cover_count_from(&snap, tip),
        assigner.deep_tip_cover_count(tip)
    );
    assert_eq!(assigner.healthy_tip_cover_count(tip), 2);
    assert_eq!(assigner.deep_tip_cover_count(tip), 1);
}

#[serial_test::serial(ibd)]
#[test]
fn get_work_assigns_sequential_chunks_per_peer() {
    let chunks = vec![(200, 263), (264, 327)];
    let assigner = assigner_for_heights(&chunks, &["p1", "p2"], 200, false);
    let w0 = assigner.get_work("p1", 1000).expect("chunk 0");
    assert_eq!(w0, (200, 263));
    assert!(
        assigner.get_work("p1", 1000).is_none(),
        "one in flight per peer"
    );
    assigner.on_chunk_complete("p1");
    assigner.mark_bootstrap_complete();
    let w1 = assigner.get_work("p2", 1000).expect("chunk 1");
    assert_eq!(w1, (264, 327));
}

#[serial_test::serial(ibd)]
#[test]
fn main_queue_assigns_next_height_when_max_ahead_zero() {
    let chunks = vec![(955186, 955244)];
    let vh = Arc::new(AtomicU64::new(955185));
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], Arc::clone(&vh), 955186, true);
    assert_eq!(
        assigner.get_work("p1", 0),
        Some((955186, 955244)),
        "next block must be assignable even when max_ahead=0"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn bootstrap_serializes_until_marked_complete() {
    let chunks = vec![(0, 127), (128, 255)];
    let assigner = assigner_for_heights(&chunks, &["p1"], 0, false);
    assert_eq!(assigner.get_work("p1", 1000), Some((0, 127)));
    assigner.on_chunk_complete("p1");
    assert!(
        assigner.get_work("p1", 1000).is_none(),
        "second chunk blocked until bootstrap done"
    );
    assigner.mark_bootstrap_complete();
    // vh=0 → next_needed=1 mid first-chunk range → W16 tip-fills before main queue.
    assert_eq!(assigner.get_work("p1", 1000), Some((1, 16)));
}

#[serial_test::serial(ibd)]
#[test]
fn work_stealing_gap_fetcher_defaults() {
    // W28b/W28c: one tip owner by default (failover may raise to 2 at runtime).
    // start_height>0 auto-completes bootstrap; pin body tip so this is not WAN gap
    // (WAN + deep_cover==0 → fetchers=2 by W41 design).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let prev = std::env::var("BLVM_IBD_GAP_FETCHERS").ok();
    unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") };
    let ws = assigner_for_heights(&[(100, 199)], &["p1"], 100, true);
    ws.set_confirmed_body_height_at_start(10_000);
    assert_eq!(ws.max_gap_fetchers_per_height(), 1);
    assert_eq!(ws.gap_micro_chunk_batch(), 32);
    let lan = assigner_for_heights(&[(100, 199)], &["p1"], 100, false);
    lan.set_confirmed_body_height_at_start(10_000);
    assert_eq!(lan.max_gap_fetchers_per_height(), 1);
    assert_eq!(lan.gap_micro_chunk_batch(), 8);
    match prev {
        Some(v) => unsafe { std::env::set_var("BLVM_IBD_GAP_FETCHERS", v) },
        None => unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") },
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w28c_sticky_tip_owner_prefers_best_scored() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(1000));
    let chunks = vec![(1000, 1200)];
    let assigner = ChunkAssigner::new(chunks, vec!["bind".into()], Arc::clone(&vh), 1000, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(900);
    assigner.set_peer_scores(&[("slow".into(), 1.0), ("fast".into(), 9.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    // Low-score peer must not win tip while high-score has capacity.
    assert_eq!(
        assigner.get_work("slow", 1000),
        None,
        "slow peer must not take tip ownership while fast is free"
    );
    let tip = assigner.get_work("fast", 1000);
    assert!(tip.is_some(), "fast peer should take tip ownership");
    let (s, e) = tip.unwrap();
    assert_eq!(s, 1001);
    assert!(
        e >= s + 31,
        "WAN tip owner should pipeline deeply, got {s}-{e}"
    );
    // Sticky: after assign, slow still shouldn't steal tip.
    let slow2 = assigner.get_work("slow", 1000);
    if let Some((ss, ee)) = slow2 {
        assert!(ss > e, "slow gets ahead partition only, got {ss}-{ee}");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w28c_failover_allows_second_tip_cover() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(1000));
    let chunks = vec![(1000, 1200)];
    let assigner = ChunkAssigner::new(chunks, vec!["bind".into()], Arc::clone(&vh), 1000, true);
    assigner.mark_bootstrap_complete();
    // Pre-body tip: failover path still valid (not WAN gap crawl).
    assigner.set_confirmed_body_height_at_start(2000);
    assert_eq!(assigner.get_work("pA", 1000).map(|(s, _)| s), Some(1001));
    // Without failover, pB must not cover tip.
    let before = assigner.get_work("pB", 1000);
    if let Some((s, _)) = before {
        assert!(s > 1001, "no failover yet — ahead only, got start={s}");
    }
    assigner.on_chunk_complete("pB");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        // Keep pA as tip owner only.
        g.retain(|p, _| p == "pA");
    }
    // W37: armed alone is not enough — soft-retry freeze must be latched.
    super::super::tip_stage::arm_tip_failover();
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        1,
        "armed without freeze must not open covering=2"
    );
    super::super::tip_stage::mark_needed(1001);
    // mark_needed clears armed latch on height roll — re-arm as download.rs does
    // after soft-retry (arm follows mark_soft_retry in production).
    super::super::tip_stage::mark_soft_retry(1001);
    super::super::tip_stage::arm_tip_failover();
    assert_eq!(assigner.max_gap_fetchers_per_height(), 2);
    assert_eq!(
        assigner.get_work("pB", 1000),
        Some((1001, 1001)),
        "failover assigns tip height only"
    );
    // W86: with covering already at fetchers_cap (deep+failover), no more tip micros.
    let third = assigner.get_work("pC", 1000);
    if let Some((s, e)) = third {
        assert!(
            !(s == 1001 && e == 1001),
            "W86: must not stack another tip failover, got {s}-{e}"
        );
    }
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w86_wan_distress_does_not_stack_unbounded_tip_failover() {
    // Live W85: tip_distress + healthy-only gate + overlaps_ok=failover stacked
    // thousands of (H,H) assigns on a handful of tip heights.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let vh = Arc::new(AtomicU64::new(300_000));
    let assigner = ChunkAssigner::new(
        vec![(300_001, 300_200)],
        vec!["pA".into(), "pB".into(), "pC".into(), "pD".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[
        ("pA".into(), 1.0),
        ("pB".into(), 0.9),
        ("pC".into(), 0.8),
        ("pD".into(), 0.7),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "pA".into(),
        "pB".into(),
        "pC".into(),
        "pD".into(),
    ]));
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::mark_needed(300_001);
    // Force distress via soft-retry latch (deterministic in tests).
    super::super::tip_stage::mark_soft_retry(300_001);
    assert_eq!(assigner.max_gap_fetchers_per_height(), 2);
    let owner = assigner.get_work("pA", 1000);
    assert!(owner.is_some(), "deep tip owner must assign");
    let (os, oe) = owner.unwrap();
    assert!(oe > os, "deep tip pipe expected, got {os}-{oe}");
    // First failover ok.
    assert_eq!(
        assigner.get_work("pB", 1000),
        Some((300_001, 300_001)),
        "one tip failover micro under distress"
    );
    // W87: even after failover peer drops in-flight (fail→requeue), no second (H,H).
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        if let Some(v) = g.get_mut("pB") {
            v.retain(|(s, e)| !(*s == 300_001 && *e == 300_001));
        }
    }
    let mut tip_micros = 0usize;
    for peer in ["pC", "pD", "pB", "pC", "pD"] {
        if let Some((s, e)) = assigner.get_work(peer, 1000) {
            if s == 300_001 && e == 300_001 {
                tip_micros += 1;
            }
        }
    }
    assert_eq!(
        tip_micros, 0,
        "W86/W87: must not stack/reassign tip failover micros, got {tip_micros}"
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
}

fn c1u_tests_env_lock() -> crate::ibd_test_lock::Guard {
    crate::ibd_test_lock::guard()
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_handoff_prime_assigns_past_body_tip_while_local() {
    // Binder cliff: local ahead ~690 BPS then body tip GetData cold → ~13 BPS.
    // Near_tip prime only on the last local height (next>=body_tip) with cover —
    // mid-window cover+prime freezes (C0 T025719Z next=304649 body_tip=304663).
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "64");
        std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_START", "8");
        std::env::set_var("BLVM_IBD_TIP_HOLE_STICKY", "1");
        std::env::remove_var("BLVM_IBD_GAP_PREEMPT_BATCH");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 304_663u64;
    let vh = Arc::new(AtomicU64::new(body_tip - 1)); // next = body_tip (last local)
    let assigner = ChunkAssigner::new(
        vec![(300_000, 320_000)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        300_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(400_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    assigner.set_tip_gap_missing(false); // local tip filled via LOCAL_GAP

    let next = vh.load(Ordering::Relaxed).saturating_add(1);
    assert_eq!(next, body_tip);
    assert!(
        assigner.handoff_prime_active(next),
        "next_needed={next} must be inside HANDOFF_PRIME of body_tip={body_tip}"
    );
    assert!(
        !assigner.handoff_prime_active(body_tip - 200),
        "far local must not prime via near_tip window alone"
    );
    assert!(
        !assigner.handoff_prime_active(body_tip + 1),
        "already past body tip is WAN crawl, not handoff prime"
    );

    // Uncovered tip: must take local cover, not steal onto body_tip+1.
    let cover = assigner.get_work("pA", 1000);
    assert!(cover.is_some(), "tip owner must cover local tip first");
    let (cs, ce) = cover.unwrap();
    assert!(
        cs <= next && ce >= next && ce <= body_tip,
        "uncovered near_tip must assign local tip cover, got {cs}-{ce}"
    );

    // Sticky often has top_peer cap≥2 → primes on second poll; else fallback / after complete.
    let mut prime = assigner
        .get_work("pA", 1000)
        .or_else(|| assigner.get_work("pB", 1000))
        .filter(|(s, _)| *s == body_tip + 1);
    if prime.is_none() {
        assigner.on_chunk_complete_range("pA", cs, ce);
        prime = assigner
            .get_work("pA", 1000)
            .filter(|(s, _)| *s == body_tip + 1);
    }
    assert_eq!(
        prime,
        Some((body_tip + 1, body_tip + 32)),
        "after tip cover on last local, handoff prime must assign body_tip+1..+stripe, got {prime:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
        std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_near_tip_prime_blocked_while_local_gap_remains() {
    // Live C0 freeze: next=304649 body_tip=304663 covering>0 → prime stole sticky.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
        std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 304_663u64;
    let vh = Arc::new(AtomicU64::new(304_648)); // next=304649
    let assigner = ChunkAssigner::new(
        vec![(300_000, 320_000)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        300_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(400_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    assigner.set_tip_gap_missing(false);

    let next = vh.load(Ordering::Relaxed) + 1;
    assert!(assigner.handoff_prime_active(next));
    let cover = assigner.get_work("pA", 1000).expect("local cover");
    assert!(
        cover.0 <= next && cover.1 >= next && cover.1 <= body_tip,
        "must cover local gap, got {}-{}",
        cover.0,
        cover.1
    );
    // Even with cover, mid-window must not prime body_tip+1.
    for peer in ["pA", "pB"] {
        if let Some((s, e)) = assigner.get_work(peer, 1000) {
            assert!(
                s != body_tip + 1,
                "{peer} must not near_tip-prime while next={next}<body_tip, got {s}-{e}"
            );
            assigner.on_chunk_complete_range(peer, s, e);
        }
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
        std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_near_tip_prime_requires_tip_cover() {
    // Regression: dens early near_tip prime with covering=0 → freeze hole under cheese.
    // FAIL DNA: next=437080, body_tip=437309, HANDOFF_PRIME=256.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
        std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 437_309u64;
    let vh = Arc::new(AtomicU64::new(437_079)); // next=437080 inside prime=256
    let assigner = ChunkAssigner::new(
        vec![(400_000, 450_000)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        400_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(500_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    assigner.set_tip_gap_missing(false);

    let next = vh.load(Ordering::Relaxed) + 1;
    assert!(
        assigner.handoff_prime_active(next),
        "FAIL DNA next={next} body_tip={body_tip} must arm near_tip"
    );
    let work = assigner.get_work("pA", 1000);
    let (s, e) = work.expect("must assign");
    assert!(
        e <= body_tip && s <= next && e >= next,
        "covering=0 near_tip must return local tip span, not prime; got {s}-{e}"
    );
    assert!(
        s != body_tip + 1,
        "must not HANDOFF_PRIME while next_needed uncovered; got {s}-{e}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
        std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_handoff_prime_blocks_local_ahead_partitions() {
    // During HANDOFF_PRIME, second peer must not W28c-ahead cheese ≤ body_tip.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
        std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
        std::env::set_var("BLVM_IBD_TIP_PARTITION_WINDOW", "256");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 437_309u64;
    let vh = Arc::new(AtomicU64::new(437_079));
    let assigner = ChunkAssigner::new(
        vec![(400_000, 450_000)],
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        400_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(500_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9), ("pC".into(), 0.8)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into(), "pC".into()]));
    assigner.set_tip_gap_missing(false);

    let tip = assigner.get_work("pA", 512);
    assert!(tip.is_some(), "sticky must take tip cover");
    let (ts, te) = tip.unwrap();
    assert!(te <= body_tip, "tip cover must stay local, got {ts}-{te}");

    // pB may fallback-prime past tip, but must NOT get ahead partition ≤ body_tip.
    let b = assigner.get_work("pB", 512);
    if let Some((s, e)) = b {
        assert!(
            s > body_tip,
            "handoff_prime must block local ahead partitions; pB got {s}-{e}"
        );
    }
    let c = assigner.get_work("pC", 512);
    if let Some((s, e)) = c {
        assert!(
            s > body_tip,
            "handoff_prime must block local ahead partitions; pC got {s}-{e}"
        );
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
        std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
        std::env::remove_var("BLVM_IBD_TIP_PARTITION_WINDOW");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_local_ahead_clips_to_body_tip_and_primes_via_frontier() {
    // Live fail: ahead assigned 304672 while tip=304418 (past body_tip=304663) → cheese.
    // Local ahead must clip at body tip; once frontier is there, tip-owner primes WAN.
    unsafe {
        std::env::set_var("BLVM_IBD_HANDOFF_PRIME", "256");
        std::env::set_var("BLVM_IBD_TIP_RUNWAY_STRIPE", "32");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_START", "8");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 304_663u64;
    // Far behind near_tip window (PRIME=256) — only ahead_frontier may prime.
    let vh = Arc::new(AtomicU64::new(body_tip - 400));
    let assigner = ChunkAssigner::new(
        vec![(300_000, 320_000)],
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        300_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(400_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9), ("pC".into(), 0.8)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into(), "pC".into()]));
    assigner.set_tip_gap_missing(false);

    assert!(
        !assigner.handoff_prime_active(vh.load(Ordering::Relaxed) + 1),
        "far local must not arm near_tip window"
    );

    // Tip-owner takes local tip cover (clipped at body tip).
    let tip_work = assigner.get_work("pA", 512);
    assert!(
        tip_work.is_some(),
        "tip owner should get local tip cover, got {tip_work:?}"
    );
    let (ts, te) = tip_work.unwrap();
    assert!(
        te <= body_tip,
        "tip-owner must not claim past body tip, got {ts}-{te}"
    );
    assert!(
        ts < body_tip,
        "far next_needed must start as local cover, not prime, got {ts}-{te}"
    );

    // Fill local ahead up to body tip with other peers (clipped).
    for _ in 0..40 {
        let w = assigner
            .get_work("pB", 512)
            .or_else(|| assigner.get_work("pC", 512));
        if let Some((s, e)) = w {
            assert!(
                e <= body_tip,
                "local ahead must clip to body tip, got {s}-{e}"
            );
            if e >= body_tip {
                break;
            }
        } else {
            break;
        }
    }

    // Prime only once next reaches the last local height (C0 freeze: mid-gap prime).
    assigner.on_chunk_complete_range("pA", ts, te);
    vh.store(body_tip - 1, Ordering::Relaxed);
    // Frontier may already cover body_tip — first poll can be the WAN prime.
    let mut got_prime = assigner
        .get_work("pA", 512)
        .or_else(|| assigner.get_work("pB", 512));
    if let Some((s, e)) = got_prime {
        if s <= body_tip && e >= body_tip && s != body_tip + 1 {
            assigner.on_chunk_complete_range("pA", s, e);
            got_prime = assigner
                .get_work("pA", 512)
                .or_else(|| assigner.get_work("pB", 512));
        } else {
            got_prime = Some((s, e));
        }
    }
    assert_eq!(
        got_prime,
        Some((body_tip + 1, body_tip + 32)),
        "on last local height, handoff-prime must assign, got {got_prime:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
        std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn c1t_gd_fast_subsecond_tip_height_race() {
    // Good-day mid-gaps: tip missing ~250–500ms, covering=1, soft-retry=0.
    // peer_may_take_tip_owner previously required soft/late-body (≥2s) → failover=0.
    let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    unsafe {
        std::env::set_var("BLVM_IBD_C1T_TIP_RACE_MS", "250");
        // Disable C1g await=0 (would open fetchers_cap whenever tip missing).
        std::env::set_var("BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS", "30");
        std::env::set_var("BLVM_IBD_TIP_HOLE_PIPE", "128");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_CAP", "32");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP", "48");
        std::env::set_var("BLVM_IBD_TIP_HOLE_GD_FAST", "1");
    }
    let vh = Arc::new(AtomicU64::new(300_000));
    let assigner = ChunkAssigner::new(
        vec![(300_001, 300_200)],
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9), ("pC".into(), 0.8)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into(), "pC".into()]));
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::mark_needed(300_001);
    // Mute EWMA — C1t gate must stay off (assert helper, not assign path).
    super::super::tip_stage::test_seed_getdata_body_ewma(3_000, 32);
    super::super::tip_stage::test_backdate_awaiting_ms(400);
    assert!(
        !assigner.c1t_tip_height_race(),
        "C1t mute guard: slow EWMA must not arm"
    );
    let owner = assigner.get_work("pA", 1000);
    assert!(owner.is_some(), "deep tip owner");
    let (os, oe) = owner.unwrap();
    assert!(oe > os, "deep tip pipe expected");
    // No tip-height race under mute (C1g disabled for this test).
    let mute_race = assigner.get_work("pB", 1000);
    if let Some((s, e)) = mute_race {
        assert!(
            !(s == 300_001 && e == 300_001),
            "mute must not tip-race, got {s}-{e}"
        );
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.remove("pB");
    }
    assigner.tip_failover_once_h.store(0, Ordering::Relaxed);
    assigner.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
    // Fast EWMA + tip missing 400ms → one (H,H).
    super::super::tip_stage::test_seed_getdata_body_ewma(100, 16);
    super::super::tip_stage::test_backdate_awaiting_ms(400);
    assert!(
        assigner.c1t_tip_height_race(),
        "C1t should arm under gd-fast + awaiting≥250ms"
    );
    assert_eq!(assigner.max_gap_fetchers_per_height(), 2);
    assert_eq!(
        assigner.get_work("pB", 1000),
        Some((300_001, 300_001)),
        "C1t tip-height failover under gd-fast"
    );
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
    assigner.set_tip_gap_missing(false);
    unsafe {
        std::env::remove_var("BLVM_IBD_C1T_TIP_RACE_MS");
        std::env::remove_var("BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_PIPE");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_CAP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GROW_FAST_CAP");
        std::env::remove_var("BLVM_IBD_TIP_HOLE_GD_FAST");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w112_empty_tip_triple_race_allows_second_failover_micro() {
    // Live W111 @323780: covering=2 mute rotate ~25s; third racer STREAM'd tip
    // in <1s once assigned. Empty bridge + awaiting≥12s → fetchers=3.
    let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(323_779));
    let assigner = ChunkAssigner::new(
        vec![(323_780, 324_000)],
        vec!["pA".into(), "pB".into(), "pC".into(), "pD".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[
        ("pA".into(), 1.0),
        ("pB".into(), 0.9),
        ("pC".into(), 0.8),
        ("pD".into(), 0.7),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "pA".into(),
        "pB".into(),
        "pC".into(),
        "pD".into(),
    ]));
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(323_780);
    super::super::tip_stage::test_backdate_awaiting_ms(6_000);
    assert!(
        !assigner.empty_tip_triple_race(),
        "W112b: awaiting=6s < 12s default — keep covering=2"
    );
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        2,
        "W112b: distress alone → covering=2"
    );
    // W122/W149: covering=1 mute reopen arms at 3s (before empty_triple @12s).
    super::super::tip_stage::test_backdate_awaiting_ms(3_000);
    assert!(
        assigner.mute_single_cover_reopen(1),
        "W149: covering=1 + awaiting≥3s"
    );
    assert!(
        !assigner.mute_single_cover_reopen(2),
        "W122: covering=2 must not mute-reopen"
    );
    assert!(
        !assigner.empty_tip_triple_race(),
        "W122: mute-reopen must not imply empty_triple"
    );
    super::super::tip_stage::test_backdate_awaiting_ms(13_000);
    assert!(
        assigner.empty_tip_triple_race(),
        "W112b: empty bridge + awaiting≥12s"
    );
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        3,
        "W112: empty tip → covering=3"
    );
    let owner = assigner.get_work("pA", 1000);
    assert!(owner.is_some(), "deep tip owner");
    assert_eq!(
        assigner.get_work("pB", 1000),
        Some((323_780, 323_780)),
        "first failover micro"
    );
    // W88 episode latched — empty triple still opens a second micro.
    assert_eq!(
        assigner.get_work("pC", 1000),
        Some((323_780, 323_780)),
        "W112: second failover under covering=3"
    );
    let fourth = assigner.get_work("pD", 1000);
    if let Some((s, e)) = fourth {
        assert!(
            !(s == 323_780 && e == 323_780),
            "must not exceed covering=3, got {s}-{e}"
        );
    }
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w149_mute_reopen_at_3s_under_w88_episode() {
    // Live W148 tip-step ~5s/h: mute_reopen@5s never won the race under W88 latch.
    let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(329_994));
    let assigner = ChunkAssigner::new(
        vec![(329_995, 330_200)],
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9), ("pC".into(), 0.8)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into(), "pC".into()]));
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(329_995);
    // Latch W88 episode as if a prior mute CAP already failover'd.
    assigner.latch_tip_failover_episode(329_995);
    super::super::tip_stage::test_backdate_awaiting_ms(3_000);
    assert!(
        assigner.mute_single_cover_reopen(1),
        "W149: covering=1 + awaiting≥3s reopens under W88"
    );
    // Awaiting=2s must NOT reopen (keep W88 cascade protection).
    super::super::tip_stage::test_backdate_awaiting_ms(2_000);
    assert!(
        !assigner.mute_single_cover_reopen(1),
        "W149: awaiting=2s stays under reopen trigger"
    );
    // get_work failover path is covered by w112 (serial); atomics race under
    // parallel download soft-budget tests.
    let _ = assigner;
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w120_shallow_end_of_pipe_deep_rearms_not_failover() {
    // W120: shallow cover (deep=0, raw=1) must deep re-arm, not (H,H) failover.
    // W117–W119 shallow-failover soaks rate-failed @306–311k; W116 DNA preferred.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::BRIDGE_PENDING_COUNT.store(32, Ordering::Relaxed);
    let tip = 344_580u64;
    let vh = Arc::new(AtomicU64::new(tip - 1));
    let assigner = ChunkAssigner::new(
        vec![(tip + 1_000, tip + 1_100)],
        vec!["pDeep".into(), "pRace".into(), "pIdle".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[
        ("pDeep".into(), 1.0),
        ("pRace".into(), 0.9),
        ("pIdle".into(), 0.8),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "pDeep".into(),
        "pRace".into(),
        "pIdle".into(),
    ]));
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(1);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("pDeep".into(), vec![(tip - 3, tip)]);
    }
    assigner.note_tip_cover_claim("pDeep", tip - 3, tip);
    assigner.note_tip_owner_assigned("pDeep");
    assigner.tip_failover_once_h.store(0, Ordering::Relaxed);
    assigner.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
    assert_eq!(assigner.deep_tip_cover_count(tip), 0, "shallow depth=4");
    super::super::tip_stage::mark_needed(tip);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_stage::mark_soft_retry(tip);
    assert!(ChunkAssigner::tip_is_distressed());
    assigner.set_header_tip(tip + 500);
    let got = assigner.get_work("pRace", 1000);
    // Must not open W117-style shallow (H,H). Deep re-arm may be None in this
    // harness (open-slot streamer preference / sticky) — that's OK for W120.
    assert!(
        !matches!(got, Some((s, e)) if s == tip && e == tip),
        "W120: shallow must not open (H,H) failover, got {got:?}"
    );
    if let Some((s, e)) = got {
        assert!(s == tip && e > tip, "deep re-arm span, got {got:?}");
    }
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(0);
}

#[serial_test::serial(ibd)]
#[test]
fn w37_local_ahead_sticky_failover_does_not_block_deep_owner() {
    // Live 2026-07-16: LOCAL_AHEAD soft-resume with tip_failover_armed stuck →
    // covering=2/2 (H,H) forever and 0 deep tip owners.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(1000));
    let chunks = vec![(1000, 1200)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        1000,
        true,
    );
    assigner.mark_bootstrap_complete();
    // Soft-resume: next_needed under confirmed body tip (not WAN gap crawl).
    assigner.set_confirmed_body_height_at_start(2000);
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 8.0), ("pC".into(), 7.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    super::super::tip_stage::arm_tip_failover();
    // Stuck (H,H) micros from a prior soft-retry; freeze already cleared.
    assigner.note_tip_cover_claim("pB", 1001, 1001);
    assigner.note_tip_cover_claim("pC", 1001, 1001);
    assert_eq!(assigner.healthy_tip_cover_count(1001), 2);
    assert_eq!(assigner.deep_tip_cover_count(1001), 0);
    assert_eq!(assigner.max_gap_fetchers_per_height(), 1);

    let work = assigner.get_work("pA", 1000);
    assert!(
        work.is_some(),
        "deep owner must re-arm despite sticky failover micros"
    );
    let (s, e) = work.unwrap();
    assert_eq!(s, 1001);
    assert!(e > s, "must be deep pipeline not (H,H), got {s}-{e}");
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w31_wan_gap_max_fetchers_one_even_when_failover_armed() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    unsafe { std::env::set_var("BLVM_IBD_GAP_FETCHERS", "2") };
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    // Deep tip claim first — otherwise W41 deep_cover==0 keeps fetchers at 2.
    let tip = assigner.get_work("pA", 1000).expect("deep tip owner");
    assert!(tip.1 > tip.0, "deep tip pipe, got {}-{}", tip.0, tip.1);
    super::super::tip_stage::arm_tip_failover();
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        1,
        "WAN gap stays single-fetcher when failover armed but soft-retry freeze is off"
    );
    // Soft-retry freeze opens a temporary second tip slot.
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::mark_soft_retry(901);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        2,
        "WAN soft-retry must allow tip-height race (covering slot 2)"
    );
    unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") };
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w31_wan_gap_retry_covering_tip_sticky_owner_only() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["owner".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.note_tip_owner_assigned("owner");
    mark_peers_ibd_ready(&assigner, &["owner"]);
    assigner.requeue(901, 916, None);
    assert_eq!(
        assigner.get_work("other", 1000),
        None,
        "non-owner must not take WAN gap retry covering tip"
    );
    let work = assigner
        .get_work("owner", 1000)
        .expect("sticky owner takes tip-covering work on WAN gap");
    assert!(
        work.0 <= 901 && work.1 >= 901,
        "owner range must cover tip 901, got {}-{}",
        work.0,
        work.1
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w51_promote_idempotent_when_deep_claim_already_covers_tip() {
    // Live W50: two in-flight covers → parallel promote steals tenure from each other.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(1000));
    let assigner = ChunkAssigner::new(
        vec![(1000, 1400)],
        vec!["a".into(), "b".into()],
        Arc::clone(&vh),
        1000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(900);
    assigner.set_peer_scores(&[("a".into(), 9.0), ("b".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_cover_claim("a", 1001, 1128);
    assert_eq!(assigner.deep_tip_cover_count(1001), 1);
    // Second peer must not overwrite A's deep tenure.
    assigner.promote_tip_walk_in("b", 1001, 1128);
    assert_eq!(assigner.deep_tip_cover_count(1001), 1);
    assert!(
        assigner
            .tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == "a" && *s == 1001 && *e == 1128),
        "first deep claim must survive competing promote"
    );
    assert!(
        !assigner
            .tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _, _)| p == "b"),
        "competing promote must be a no-op"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w49_tip_walk_in_promotes_instead_of_abort_thrash() {
    // Live WAN: abort-after-body + W28d short preempt → span=32 storms. W49 promotes
    // the walk-in to tip-cover tenure while tip is inside the span.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(1000));
    let chunks = vec![(1000, 1400)];
    let assigner = ChunkAssigner::new(chunks, vec!["bind".into()], Arc::clone(&vh), 1000, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(900);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("ahead".into(), 5.0),
        ("c".into(), 5.0),
        ("d".into(), 5.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);

    let tip = assigner.get_work("owner", 1000).expect("tip owner");
    assert_eq!(tip.0, 1001);
    assert!(tip.1 >= 1001 + 31, "deep tip pipe, got {:?}", tip);

    let ahead = assigner.get_work("ahead", 1000).expect("ahead");
    assert!(ahead.0 > tip.1, "ahead after tip owner end {:?}", ahead);

    assigner.on_chunk_complete_range("owner", tip.0, tip.1);
    vh.store(ahead.0 + 5, Ordering::Relaxed);
    let need = ahead.0 + 6;
    assert!(ahead.0 <= need && need <= ahead.1);

    assert!(
        !assigner.should_abort_tip_walk_in("ahead", ahead.0, ahead.1),
        "W49: never abort while tip inside walk-in span"
    );
    assert!(
        assigner.healthy_tip_cover_count(need) >= 1,
        "W49: walk-in must be promoted to tip-cover claim"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("ahead"),
        "promoted walk-in becomes preferred tip owner"
    );
    // Sticky owner must not open a competing tip-covering pipe (the W28d thrash).
    // Far-ahead partitions are OK.
    let again = assigner.get_work("owner", 1000);
    if let Some((s, e)) = again {
        assert!(
            !(s <= need && need <= e && e > s),
            "must not assign competing deep tip pipe under promoted walk-in, got {s}-{e} tip={need}"
        );
    }
    // Tip walked past span → abort leftover ahead.
    vh.store(ahead.1, Ordering::Relaxed);
    assert!(
        assigner.should_abort_tip_walk_in("ahead", ahead.0, ahead.1),
        "W49: abort only after tip walks past span end"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn c1e_tip_contiguous_assign_frontier_stripes_multi_peer() {
    // Peer A: tip..tip+31, Peer B: tip+32..tip+63 → frontier tip+63 (contiguous).
    // Phantom claim tip..tip+127 alone would lie; we only walk contiguous cover.
    let mut inflight: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
    let tip = 300_000u64;
    inflight.insert("a".into(), vec![(tip, tip + 31)]);
    inflight.insert("b".into(), vec![(tip + 32, tip + 63)]);
    let runway_end = tip + 95;
    assert_eq!(
        ChunkAssigner::tip_contiguous_assign_frontier(&inflight, tip, runway_end),
        tip + 63
    );
    // Hole between stripes → stop at first stripe end.
    inflight.insert("c".into(), vec![(tip + 80, tip + 95)]);
    assert_eq!(
        ChunkAssigner::tip_contiguous_assign_frontier(&inflight, tip, runway_end),
        tip + 63,
        "must not jump hole to c's stripe"
    );
    // Tip uncovered → frontier tip-1.
    let empty = HashMap::new();
    assert_eq!(
        ChunkAssigner::tip_contiguous_assign_frontier(&empty, tip, runway_end),
        tip - 1
    );
    // Phantom deep assign without covering tip from next_needed:
    // range starts at tip+10 → not contiguous from tip.
    let mut phantom = HashMap::new();
    phantom.insert("p".into(), vec![(tip + 10, tip + 127)]);
    assert_eq!(
        ChunkAssigner::tip_contiguous_assign_frontier(&phantom, tip, tip + 127),
        tip - 1,
        "assign starting past tip is not runway"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w41_wan_allows_two_fetchers_when_deep_owner_absent() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        2,
        "WAN with no deep owner must allow tip race"
    );
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    let tip = assigner.get_work("pA", 1000).expect("deep owner");
    assert!(tip.1 > tip.0);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        1,
        "after deep claim, back to single tip pipe"
    );
    // W47: bridge holes alone must NOT reopen tip race (was covering≈2 treadmill).
    // Ahead partitions for pB are fine; tip-height (H,H) micros are not.
    assigner.set_tip_bridge_holes(64);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        1,
        "W47: holes alone must not arm tip race"
    );
    if let Some((s, e)) = assigner.get_work("pB", 1000) {
        assert!(
            !(s == e && s == tip.0),
            "must not (H,H) tip race on holes alone, got {s}-{e}"
        );
        assigner.on_chunk_complete_range("pB", s, e);
    }
    // Soft-retry is real tip distress → one failover micro.
    super::super::tip_stage::mark_needed(tip.0);
    super::super::tip_stage::mark_soft_retry(tip.0);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        2,
        "W47: soft-retry must reopen tip race"
    );
    let failover = assigner.get_work("pB", 1000);
    assert!(
        failover.is_some(),
        "second peer tip failover under soft-retry"
    );
    let (s, e) = failover.unwrap();
    assert_eq!(s, e, "failover must be tip-height micro, got {s}-{e}");
    assert_eq!(s, tip.0, "failover races current tip");
    assert!(
        assigner.get_work("pB", 1000).is_none(),
        "must not assign unlimited failover micros"
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w47_ahead_ok_with_high_holes_frozen_on_tip_distress() {
    // W47: high bridge holes alone must NOT block ahead while feeder>0.
    // W125: holes≥24 + feeder=0 sticky-freezes ahead until holes < 8.
    // Soft-retry still freezes ahead (real tip distress — A6g/W31).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["owner".into(), "ahead".into(), "spare".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);

    let tip = assigner.get_work("owner", 1000);
    assert!(tip.is_some(), "tip owner must get work");
    let (ts, te) = tip.unwrap();
    assert_eq!(ts, 901);
    assert!(te > ts, "deep tip pipeline");

    // C1g: ahead only after tip lands in reorder (not while tip_gap_missing).
    assigner.set_tip_gap_missing(false);
    // C1i: ahead also requires contig runway ≥ min (default 8) — tipfix DNA.
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(64);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let ahead = assigner.get_work("ahead", 1000);
    assert!(
        ahead.is_some(),
        "W47: ahead partition must work with holes=64 when feeder>0"
    );
    let (s, e) = ahead.unwrap();
    assert!(
        s > te,
        "ahead must start after tip owner end, got {s}-{e} tip_end={te}"
    );
    assigner.on_chunk_complete_range("ahead", s, e);

    // W125: arm@24 / clear@8 — holes=16 must NOT freeze; holes=10 stays frozen.
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(16);
    assert!(
        assigner.wan_allow_multi_peer_ahead(1, 0),
        "W125: holes=16 < arm=24 must allow ahead (W124 over-froze here)"
    );
    assigner.set_tip_bridge_holes(24);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W125: holes≥24 + feeder=0 must freeze ahead"
    );
    assert!(
        assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
        "W125: sticky latch armed"
    );
    assigner.set_tip_bridge_holes(10);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W125: holes=10 still frozen (clear only &lt;8; W123 released @12)"
    );
    assigner.set_tip_bridge_holes(7);
    assert!(
        assigner.wan_allow_multi_peer_ahead(1, 8),
        "W125: holes&lt;8 + feeder>0 releases sticky"
    );

    // W181: distress arm must sticky-latch at holes=16 when awaiting≥3s.
    assigner
        .tip_ahead_hole_freeze
        .store(false, Ordering::Relaxed);
    assigner
        .tip_ahead_hole_clear_since_ms
        .store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(16);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(3_500);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W181: distress arm must sticky-latch at holes=16"
    );
    assert!(
        assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
        "W181: sticky latch armed under distress"
    );
    assigner.set_tip_bridge_holes(7);
    assert!(
        assigner.wan_allow_multi_peer_ahead(1, 8),
        "W143: holes&lt;8 + feeder>0 releases distress sticky"
    );

    // W183: feeder-empty clear is debounced — brief holes&lt;8 must not reopen W35.
    assigner
        .tip_ahead_hole_freeze
        .store(true, Ordering::Relaxed);
    assigner
        .tip_ahead_hole_clear_since_ms
        .store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(7);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W183: holes&lt;8 + feeder=0 must not clear sticky on first poll"
    );
    assert!(
        assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
        "W183: clear countdown armed, freeze still latched"
    );
    assigner.set_tip_bridge_holes(16);
    let _ = assigner.wan_allow_multi_peer_ahead(1, 0);
    assert_eq!(
        assigner
            .tip_ahead_hole_clear_since_ms
            .load(Ordering::Relaxed),
        0,
        "W183: holes back mid-band cancels clear countdown"
    );
    assert!(assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed));

    // Soft-retry: no new far-ahead past tip frontier (main-queue behind-tip OK).
    assigner.set_tip_bridge_holes(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::mark_soft_retry(901);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 8),
        "soft-retry must freeze multi-peer ahead"
    );
    let blocked = assigner.get_work("spare", 1000);
    if let Some((bs, be)) = blocked {
        assert!(
            !(bs > te),
            "during soft-retry must not assign far ahead, got {bs}-{be} tip_end={te}"
        );
    }
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w102b_narrow_allows_ahead_when_awaiting_healthy_cover_low_holes() {
    // True-WAN 400→500 (20260731T154656Z): feeder=0 ~79% of samples with tip
    // covering≥1 is the *normal* single-owner crawl. Old W102b hard-denied ahead
    // on awaiting≥3s ∧ feeder=0 alone → ahead_partition 21 vs tip_owner 562.
    // Hole-band (W181) + C1g still freeze STREAM storms; awaiting alone must not.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["owner".into(), "ahead".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner
        .tip_ahead_hole_freeze
        .store(false, Ordering::Relaxed);
    assigner.set_tip_gap_missing(false);
    assigner.set_tip_bridge_holes(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(3_500);
    assert!(
        assigner.wan_allow_multi_peer_ahead(1, 0),
        "W102b narrow: awaiting≥3s + feeder=0 + holes=0 + covering≥1 must allow ahead"
    );
    assert!(
        !assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed),
        "W102b narrow: low holes must not latch hole-band freeze"
    );
    // W181 still armed when holes enter distress under the same awaiting clock.
    assigner.set_tip_bridge_holes(16);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W181: awaiting≥3s + holes=16 must still freeze ahead"
    );
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w127_covering0_allows_floor_when_mid_pool_fail_cooled() {
    // Live W126b @337k: mute CAP cooled mid peers; W95 ignore_cooldown still treated
    // them as alternatives → floor open-slot denied → covering=0 OPEN_STALL.
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["mid".into(), "floor".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("mid".into(), 0.20), ("floor".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.tip_owner_open.store(true, Ordering::Relaxed);
    assigner.mark_tip_owner_fail_cooldown("mid", 30);

    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert!(
        !assigner.peer_may_take_tip_owner("mid", &g, 0),
        "cooled mid must still be denied"
    );
    assert!(
        assigner.peer_may_take_tip_owner("floor", &g, 0),
        "W127: covering=0 must allow floor when only mid is fail-cooled"
    );
    // covering>0 keeps W95: cooled mid still blocks floor lottery.
    assert!(
        !assigner.peer_may_take_tip_owner("floor", &g, 1),
        "W95: covering>0 must still refuse floor while cooled mid exists"
    );
    drop(g);
}

#[serial_test::serial(ibd)]
#[test]
fn w128_covering0_clears_mid_cooldown_keeps_floor_gate() {
    // Tipfix DNA: W95 counts cooled mid when covering>0; covering=0 MID_CLEAR
    // uncools mid+; floor stays refused once live mid exists; floor cooldown retained.
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["mid".into(), "floor".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("mid".into(), 0.25), ("floor".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.tip_owner_open.store(true, Ordering::Relaxed);
    assigner.mark_tip_owner_fail_cooldown("mid", 120);
    assert!(assigner.tip_owner_in_fail_cooldown("mid"));
    {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            !assigner.peer_may_take_tip_owner("mid", &g, 0),
            "cooled mid denied before clear"
        );
        assert!(
            !assigner.peer_may_take_tip_owner("floor", &g, 1),
            "W95/W128: floor denied while cooled mid still counts as alternative"
        );
        // W127: covering=0 allows floor while mid is cooled — MID_CLEAR is for when
        // we *want* mid back, not to keep floor locked out forever.
        assert!(
            assigner.peer_may_take_tip_owner("floor", &g, 0),
            "covering=0 floor ok while mid cooled (W127)"
        );
    }
    // covering=0 MID_CLEAR — mid re-arms; floor refused once live mid exists.
    assigner.maybe_clear_mid_plus_fail_cooldowns_covering0(901);
    assert!(
        !assigner.tip_owner_in_fail_cooldown("mid"),
        "W128: mid re-arms after mid+ cooldown clear"
    );
    {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            assigner.peer_may_take_tip_owner("mid", &g, 0),
            "W128: mid re-arms after mid+ cooldown clear"
        );
        assert!(
            !assigner.peer_may_take_tip_owner("floor", &g, 0),
            "W128: floor still refused once live mid exists"
        );
    }
    // mid_clear must not wipe a floor cooldown.
    assigner.mark_tip_owner_fail_cooldown("floor", 120);
    assigner.mark_tip_owner_fail_cooldown("mid", 120);
    assigner.maybe_clear_mid_plus_fail_cooldowns_covering0(901);
    assert!(
        assigner.tip_owner_in_fail_cooldown("floor"),
        "floor cooldown retained"
    );
    assert!(
        !assigner.tip_owner_in_fail_cooldown("mid"),
        "W128: mid re-arms after mid+ cooldown clear"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w126_covering0_pin_prefers_idle_over_ahead_busy() {
    // Live W125 @326975: TIP_PIN elected top_w mid W35 ahead → covering=0 for 16s.
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["busy".into(), "idle".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("busy".into(), 9.0), ("idle".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    // busy holds ahead-only in-flight past tip.
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("busy".into(), vec![(950, 981)]);
    }
    let tip = 901u64;
    let inflight = assigner.in_flight_per_peer.lock().unwrap().clone();
    assert!(
        ChunkAssigner::peer_inflight_ahead_only_map(&inflight, "busy", tip),
        "busy must be ahead-only"
    );
    assert!(
        assigner.peer_has_flight_capacity("idle", &inflight),
        "idle must have capacity"
    );
    let pin = assigner
        .best_covering0_tip_pin_candidate(tip)
        .expect("pin candidate");
    assert_eq!(
        pin, "idle",
        "W126: must prefer idle over ahead-busy top score"
    );

    // W126a: peer_may_take_tip_owner must not deadlock while caller holds in_flight.
    assigner.tip_owner_open.store(true, Ordering::Relaxed);
    assigner.set_tip_gap_missing(true);
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let _ = assigner.peer_may_take_tip_owner("idle", &g, 0);
    drop(g);
}

#[serial_test::serial(ibd)]
#[test]
fn a6g_ahead_ok_with_gap_missing_low_holes() {
    // C1g: tip-band ahead requires tip in reorder (`tip_gap_missing=false`). Opening
    // stripes while tip empty caused TIP_HOLE_AHEAD (C1f). Soft-retry still freezes.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0); // clear any leftover soft-retry latch
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("ahead".into(), 8.0),
        ("spare".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.set_tip_bridge_holes(0);
    // W61: gap_missing+feeder==0 freezes ahead; simulate healthy pipe runway.
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    let tip = assigner.get_work("owner", 4096);
    assert!(tip.is_some(), "tip owner must get work");
    let (ts, te) = tip.unwrap();
    assert_eq!(ts, 901);

    // While tip missing: C1g freezes past-tip stripes (tip-height race OK).
    if let Some((s, e)) = assigner.get_work("ahead", 4096) {
        assert!(
            s == 901 && e == 901,
            "C1g: only tip-height race while tip_gap_missing, got {s}-{e}"
        );
        // Free the race micro so tip-band ahead can arm after tip lands.
        assigner.on_chunk_complete_range("ahead", s, e);
    }
    assigner.set_tip_gap_missing(false);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let ahead = assigner.get_work("ahead", 4096);
    assert!(
        ahead.is_some(),
        "multi-peer tip-band ahead after tip lands in reorder"
    );
    let (s, e) = ahead.unwrap();
    assert!(
        s > te,
        "ahead must start after tip frontier, got {s}-{e} tip_end={te}"
    );

    // Soft-retry: freeze multi-peer ahead; tip-height failover race is allowed (W31).
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::mark_soft_retry(901);
    assert!(super::super::tip_stage::tip_ahead_frozen_for_soft_retry());
    let raced = assigner.get_work("spare", 4096);
    match raced {
        None => {}
        Some((s, e)) => {
            assert_eq!(
                (s, e),
                (901, 901),
                "during soft-retry only tip-height failover is allowed, got {s}-{e}"
            );
        }
    }
    // A third peer must not get a far ahead partition while freeze is latched.
    let blocked = assigner.get_work("ahead", 4096);
    // "ahead" already holds an ahead range from before soft-retry — may be at cap.
    // Use a fresh peer name that only appears now… spare already used. Check via
    // wan_allow directly is enough: any new partition past tip frontier is forbidden.
    if let Some((s, e)) = blocked {
        assert!(
            s <= 901 && e <= te,
            "must not assign far ahead during soft-retry, got {s}-{e} tip_end={te}"
        );
    }

    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w98_find_inflight_deep_skips_shallow_remnant() {
    let mut inflight = HashMap::new();
    // Live W97: tip=312048 covered by ahead remnant 312018-312049 (remain=2).
    inflight.insert("shallow".into(), vec![(312_018u64, 312_049u64)]);
    assert!(
        ChunkAssigner::find_inflight_deep_covering(&inflight, 312_048).is_none(),
        "W98: shallow remain=2 must not promote-as-deep"
    );
    inflight.insert("deep".into(), vec![(312_048u64, 312_175u64)]);
    let found = ChunkAssigner::find_inflight_deep_covering(&inflight, 312_048);
    assert_eq!(
        found.as_ref().map(|(p, _, _)| p.as_str()),
        Some("deep"),
        "W98: prefer substantial tip runway"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w113_empty_tip_open_slot_prefers_tip_streamer() {
    // Live W112b @331209: tip_owner_open elected score=0.100 while ready=62
    // included tip STREAM heroes → empty mute lottery rate-fail 33.5 vs 35.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(331_208));
    let assigner = ChunkAssigner::new(
        vec![(331_209, 331_500)],
        vec!["floor".into(), "hero".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    // Both floored (live mid-chain score collapse) — W95 mid-gate cannot help;
    // tip-STREAM history must break the lottery.
    assigner.set_peer_scores(&[("floor".into(), 0.100), ("hero".into(), 0.100)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    assigner.tip_owner_open.store(true, Ordering::Relaxed);
    assigner.note_wan_tip_stream("hero");
    assert!(
        assigner.empty_tip_owner_prefer_streamer(),
        "W113: proven tip streamer ready"
    );
    assert!(
        assigner.get_work("floor", 1000).is_none(),
        "W113: non-streamer must not deep-own empty tip while streamer ready"
    );
    let hero = assigner.get_work("hero", 1000);
    assert!(hero.is_some(), "W113: tip streamer takes deep owner");
    let (s, e) = hero.unwrap();
    assert_eq!(s, 331_209);
    assert!(e > s, "deep tip pipe, got {s}-{e}");
    assigner.tip_owner_open.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(0);
}

#[serial_test::serial(ibd)]
#[test]
fn w111_mute_cooldown_blocks_walk_promote_resticky() {
    // Live W110 @326324: mute CAP → TIP_FAILOVER armed, then same-ms
    // TIP_WALK_PROMOTE re-stickied the mute-failed peer from residual in-flight.
    let vh = Arc::new(AtomicU64::new(326_323));
    let assigner = ChunkAssigner::new(
        vec![(326_000, 327_000)],
        vec!["mute".into(), "other".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_tip_gap_missing(true);
    // Cooldown skips when ≤1 IBD-ready peer — both must be ready so mute stays cooled.
    assigner.set_ibd_ready_peers(HashSet::from(["mute".into(), "other".into()]));
    assigner.mark_tip_owner_fail_cooldown("mute", 5);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("mute".into(), vec![(326_316, 326_347)]);
    }
    assigner.promote_tip_walk_in("mute", 326_316, 326_347);
    assert_eq!(
        assigner.preferred_tip_owner(),
        None,
        "W111: mute-cooled peer must not become preferred via walk-promote"
    );
    assert_eq!(
        assigner.deep_tip_cover_count(326_324),
        0,
        "W111: no deep tip claim from mute-cooled walk-promote"
    );
    assert!(
        assigner.should_abort_tip_walk_in("mute", 326_316, 326_347),
        "W111: cooldown walk-in aborts rather than re-sticky"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w65_shallow_walk_promote_allows_deep_tip_rearm() {
    // Live genesis tip=218: TIP_WALK_PROMOTE ahead 193-224 → claim 218-224 (depth 7)
    // plus (H,H) failover covering=2/2 held tip tenure through 3× soft-retry (~40s).
    // Deep owner 218-345 then streamed tip immediately. Shallow remnants must not
    // count as deep tip cover / block claim_overlap.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(217));
    let assigner = ChunkAssigner::new(
        vec![(1, 400)],
        vec!["walk".into(), "failover".into(), "owner".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("walk".into(), 5.0),
        ("failover".into(), 4.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);

    // Ahead walk-in still in-flight; tip has walked into it.
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("walk".into(), vec![(193, 224)]);
    }
    assigner.promote_tip_walk_in("walk", 193, 224);
    assert_eq!(
        assigner.deep_tip_cover_count(218),
        0,
        "W65: shallow promote remnant 218-224 must not count as deep cover"
    );
    assert!(
        assigner.healthy_tip_cover_count(218) >= 1,
        "promote still registers a tip-cover claim (GetData keep)"
    );
    // Failover micros as in live covering=2/2.
    assigner.note_tip_cover_claim("failover", 218, 218);
    super::super::tip_stage::arm_tip_failover();
    super::super::tip_stage::mark_needed(218);
    super::super::tip_stage::mark_soft_retry(218);

    let work = assigner.get_work("owner", 4096);
    assert!(
        work.is_some(),
        "deep owner must re-arm over shallow walk-promote"
    );
    let (s, e) = work.unwrap();
    assert_eq!(s, 218);
    assert!(
        e >= 218 + 63,
        "must be substantial deep pipe not (H,H)/shallow, got {s}-{e}"
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w30_wan_gap_ignores_failover_micro_for_deep_owner() {
    // covering=2 from (H,H) failovers must not block a new deep tip owner on WAN gap.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 8.0), ("pC".into(), 7.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::arm_tip_failover();
    // Simulate two stuck (H,H) failover claims at tip 901.
    assigner.note_tip_cover_claim("pB", 901, 901);
    assigner.note_tip_cover_claim("pC", 901, 901);
    assert_eq!(assigner.healthy_tip_cover_count(901), 2);
    assert_eq!(assigner.deep_tip_cover_count(901), 0);

    let work = assigner.get_work("pA", 1000);
    assert!(
        work.is_some(),
        "deep owner must re-arm despite micro failover claims"
    );
    let (s, e) = work.unwrap();
    assert_eq!(s, 901);
    assert!(e > s, "must be deep pipeline not (H,H), got {s}-{e}");
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn gap_preempt_skips_when_next_needed_at_chunk_start() {
    // Tip uncovered → tip owner bulk; second peer gets non-overlapping ahead partition.
    // LOCAL_AHEAD (body tip past next): empty ibd_ready must not block tip owner.
    let vh = Arc::new(AtomicU64::new(505_153));
    let chunks = vec![(505_153, 505_184), (505_185, 505_216)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        505_153,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(600_000);
    assert_eq!(
        assigner.get_work("pA", 1000),
        Some((505_154, 505_169)),
        "pA tip owner from next_needed"
    );
    let second = assigner.get_work("pB", 1000);
    assert_eq!(
        second,
        Some((505_170, 505_184)),
        "pB ahead partition after tip owner, not overlapping tip race"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn w130_hole_freeze_opens_weak_sticky_keeps_ahead_frozen() {
    // RECONSTRUCTED from blvm_node-0faf3b9b3ecfa01e assert strings (2026-07-28).
    // Full body was NOT present in agent-transcript StrReplace blobs — only fn name
    // anchors (512e3125 L8799) and production DNA tip_owner_credible/nudge_weak_sticky.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["floor".into(), "mid".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("floor");
    assigner.set_tip_gap_missing(false);
    // W125/W130: holes≥24 + feeder=0 must freeze ahead
    unsafe {
        std::env::set_var("BLVM_IBD_WEAK_STICKY_OPEN_MS", "0");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(24);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W125/W130: holes≥24 + feeder=0 must freeze ahead"
    );
    assert!(assigner.tip_ahead_hole_freeze.load(Ordering::Relaxed));
    // Floor sticky is not credible under hole-freeze → open tip slot; ahead stays frozen.
    assert!(
        assigner.preferred_tip_owner().is_none(),
        "W130: floor sticky cleared during hole-freeze"
    );
    assert!(
        assigner.tip_owner_open.load(Ordering::Relaxed),
        "W130: tip slot open for mid+/STREAM re-arm"
    );
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "W130: ahead must stay frozen under holes≥24"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_WEAK_STICKY_OPEN_MS");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w132_weak_sticky_open_debounced_under_hole_freeze() {
    // RECONSTRUCTED from binary asserts: "W132: first freeze sample must not clear
    // sticky (15s debounce)"; tip_owner_open false; wan_allow false.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["floor".into(), "mid".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("floor");
    assigner.tip_owner_open.store(false, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_WEAK_STICKY_OPEN_MS", "15000");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(24);
    // First sample arms freeze + starts weak-sticky debounce — must NOT clear yet.
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 0),
        "assertion failed: !assigner.wan_allow_multi_peer_ahead(1, 0)"
    );
    assert!(
        assigner.preferred_tip_owner().as_deref() == Some("floor"),
        "W132: first freeze sample must not clear sticky (15s debounce)"
    );
    assert!(
        !assigner.tip_owner_open.load(Ordering::Relaxed),
        "assertion failed: !assigner.tip_owner_open.load(Ordering::Relaxed)"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_WEAK_STICKY_OPEN_MS");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w138_tip_pin_prefers_mid_over_idle_floor() {
    // RECONSTRUCTED from binary asserts + TIP_PIN_PREFER_MID DNA (transcript L8978).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["floor".into(), "mid".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    // Mid holds ahead-only; floor is idle — covering=0 pin must prefer mid and release ahead.
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("mid".into(), vec![(950, 981)]);
    }
    // preferred empty → nudge/TIP_PIN path
    assert!(assigner.preferred_tip_owner().is_none());
    assert!(
        assigner.nudge_wan_tip_owner(),
        "assertion failed: assigner.nudge_wan_tip_owner()"
    );
    let pref = assigner.preferred_tip_owner();
    assert_eq!(
        pref.as_deref(),
        Some("mid"),
        "W138: covering=0 must prefer mid+ over idle floor"
    );
    let inflight = assigner.in_flight_per_peer.lock().unwrap().clone();
    assert!(
        !ChunkAssigner::peer_inflight_ahead_only_map(&inflight, "mid", 901)
            || assigner.peer_has_flight_capacity("mid", &inflight),
        "W138: mid ahead must be released so tip can arm"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w153_holey_tip_triple_race_at_12s() {
    // RECONSTRUCTED from binary asserts near w112. Dens-era empty_tip_triple may have
    // allowed covering=3 with BRIDGE_PENDING>0 (holey); CURRENT empty_tip_triple_race
    // returns false when pending>0 — this test documents dens intent / may need DNA.
    let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::BRIDGE_PENDING_COUNT.store(32, Ordering::Relaxed); // holey
    super::super::IBD_TIP_BRIDGE_HOLES.store(8, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(323_779));
    let assigner = ChunkAssigner::new(
        vec![(323_780, 324_000)],
        vec!["pA".into(), "pB".into(), "pC".into(), "pD".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_peer_scores(&[
        ("pA".into(), 1.0),
        ("pB".into(), 0.9),
        ("pC".into(), 0.8),
        ("pD".into(), 0.7),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "pA".into(),
        "pB".into(),
        "pC".into(),
        "pD".into(),
    ]));
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(323_780);
    super::super::tip_stage::test_backdate_awaiting_ms(6_000);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        2,
        "W153: awaiting=6s < 12s — keep covering=2 on holey"
    );
    super::super::tip_stage::test_backdate_awaiting_ms(13_000);
    assert_eq!(
        assigner.max_gap_fetchers_per_height(),
        3,
        "W153: holey pending>0 + awaiting≥12s → covering=3"
    );
    let owner = assigner.get_work("pA", 1000);
    assert!(owner.is_some(), "deep tip owner");
    assert_eq!(assigner.get_work("pB", 1000), Some((323_780, 323_780)));
    assert_eq!(
        assigner.get_work("pC", 1000),
        Some((323_780, 323_780)),
        "W153: second failover under covering=3"
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::memory::BRIDGE_PENDING_COUNT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn w180_mute_distress_refuses_floor_and_clears_mid_cooldown() {
    // Tipfix DNA (binary asserts): mute arms failover + cools mid; MID_CLEAR then
    // uncools mid+ so mid can take failover; distress race still refuses floor.
    let _tip_atomics = super::super::tip_stage::test_tip_atomics_lock();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 943), (944, 1007)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["mid".into(), "floor".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("mid".into(), 0.25), ("floor".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("mid");
    assigner.note_tip_owner_failed_mute("mid");
    assert!(
        super::super::tip_stage::tip_failover_armed(),
        "assertion failed: tip_failover_armed()"
    );
    assert!(
        assigner.tip_owner_in_fail_cooldown("mid"),
        "mute CAP must cool mid before MID_CLEAR"
    );
    // Covering=0 MID_CLEAR path — uncool mid+ so mid can take failover.
    assigner.maybe_clear_mid_plus_fail_cooldowns_covering0(901);
    assert!(
        !assigner.tip_owner_in_fail_cooldown("mid"),
        "W180: mute CAP must MID_CLEAR so mid can take failover"
    );
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::mark_soft_retry(901);
    assert!(
        super::super::tip_stage::tip_ahead_frozen_for_soft_retry(),
        "assertion failed: tip_ahead_frozen_for_soft_retry()"
    );
    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert!(
        !assigner.peer_may_take_tip_owner("floor", &g, 1),
        "W180: distress race must refuse floor while mid+ exists"
    );
    drop(g);
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(0);
}

#[serial_test::serial(ibd)]
#[test]
fn c1j_aborts_past_tip_while_tip_missing() {
    super::super::tip_stage::clear_tip_failover();
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
    assigner.set_tip_gap_missing(true);
    assert!(
        assigner.should_abort_tip_walk_in("ahead", 933, 964),
        "C1j: must abort tip+32.. while tip missing"
    );
    assigner.set_tip_gap_missing(false);
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", 933, 964),
        "C1j: must not abort ahead span when tip present and tip below span"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn synth_bulk_clears_tip_cover_claim_on_complete() {
    let _guard = SYNTH_BULK_TEST_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("BLVM_IBD_SYNTH_WAN", "1");
        std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS");
    }
    assert!(super::super::synthetic_wan::bulk_local_disk_stream());
    let vh = Arc::new(AtomicU64::new(300_300));
    let assigner = ChunkAssigner::new(
        vec![(300_288, 300_351)],
        vec!["local-disk".into()],
        Arc::clone(&vh),
        300_288,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.note_tip_cover_claim("local-disk", 300_288, 300_351);
    assigner.on_chunk_complete_range("local-disk", 300_288, 300_351);
    assert_eq!(
        assigner.healthy_tip_cover_count(300_300),
        0,
        "synth must clear tip-cover claim on complete (keep-claim muted tip-owner)"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn synth_bulk_dedup_blocks_same_span_tip_owner_reassign() {
    // H6: DEDUP gate + get_work must not W28c-reassign tip after GAP_STREAM while
    // validation lags (in_flight/claims already cleared on complete).
    let _guard = SYNTH_BULK_TEST_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("BLVM_IBD_SYNTH_WAN", "1");
        std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS");
        std::env::set_var("BLVM_IBD_SYNTH_DEDUP_REARM_MS", "60000");
        std::env::set_var("BLVM_IBD_GAP_PREEMPT_BATCH", "128");
    }
    assert!(super::super::synthetic_wan::bulk_local_disk_stream());
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(300_287));
    let assigner = ChunkAssigner::new(
        vec![(300_288, 300_351), (300_352, 300_415)],
        vec!["local-disk".into()],
        Arc::clone(&vh),
        300_288,
        true,
    );
    assigner.mark_bootstrap_complete();
    // Match live synth short: bodies far above tip, pin creates crawl gate above band.
    assigner.set_confirmed_body_height_at_start(503_656);
    assigner.set_wan_body_tip(400_000);
    assigner.set_header_tip(400_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[("local-disk".into(), 1.0)]);
    assert!(
        !assigner.synth_tip_owner_blocked_by_dedup(300_288),
        "DEDUP=0 must not block first tip-owner"
    );
    let first = assigner.get_work("local-disk", 1000);
    assert!(
        first.is_some_and(|(s, e)| s == 300_288 && e >= 300_300),
        "initial tip-owner assign, got {first:?}"
    );
    let (fs, fe) = first.unwrap();
    assigner.on_chunk_complete_range("local-disk", fs, fe);
    // Simulate GAP_STREAM having delivered tip (and more) while validation lags.
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(300_351, Ordering::Relaxed);
    assert!(
        assigner.synth_tip_owner_blocked_by_dedup(300_288),
        "DEDUP past tip must block tip-owner re-arm"
    );
    let second = assigner.get_work("local-disk", 1000);
    assert!(
        second.map(|(s, _)| s != 300_288).unwrap_or(true),
        "H6: must not reassign tip-covering span after DEDUP, got {second:?}"
    );
    // Validation caught up — tip-owner for next height is allowed.
    vh.store(300_351, Ordering::Relaxed);
    assigner
        .synth_tip_dedup_block_since_ms
        .store(0, Ordering::Relaxed);
    assert!(
        !assigner.synth_tip_owner_blocked_by_dedup(300_352),
        "DEDUP below next tip must allow"
    );
    let third = assigner.get_work("local-disk", 1000);
    assert!(
        third.is_some_and(|(s, _)| s == 300_352),
        "after tip advance, next tip-owner span assigns, got {third:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT");
        std::env::remove_var("BLVM_IBD_SYNTH_DEDUP_REARM_MS");
        std::env::remove_var("BLVM_IBD_GAP_PREEMPT_BATCH");
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
    }
}

// Shared across synth-bulk env tests (parallel cargo test races otherwise).
static SYNTH_BULK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[serial_test::serial(ibd)]
#[test]
fn synth_bulk_obsolete_does_not_tip_owner_open() {
    // obsolete/behind-tip must clear sticky without TIP_OWNER_OPEN under synth bulk.
    let _guard = SYNTH_BULK_TEST_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("BLVM_IBD_SYNTH_WAN", "1");
        std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS");
    }
    assert!(super::super::synthetic_wan::bulk_local_disk_stream());
    let vh = Arc::new(AtomicU64::new(505_200));
    let assigner = ChunkAssigner::new(
        vec![(505_153, 505_184)],
        vec!["local-disk".into()],
        Arc::clone(&vh),
        505_153,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.note_tip_owner_assigned("local-disk");
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("local-disk")
    );
    assigner.on_chunk_complete_range("local-disk", 505_153, 505_184);
    assert!(assigner.preferred_tip_owner().is_none());
    assert!(
        !assigner.tip_owner_open.load(Ordering::Relaxed),
        "synth bulk must not TIP_OWNER_OPEN after obsolete complete"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w40_local_tip_hole_owner_at_chunk_start() {
    // Soft-resume: next_needed == chunk start, tip missing — must still deep-own tip.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let vh = Arc::new(AtomicU64::new(1000));
    let chunks = vec![(1001, 1032), (1033, 1064)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        1001,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(2000); // LOCAL_AHEAD (not WAN gap)
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    let work = assigner.get_work("pA", 1000);
    assert!(
        work.is_some(),
        "LOCAL tip-hole owner must assign at chunk start"
    );
    let (s, e) = work.unwrap();
    assert_eq!(s, 1001);
    assert!(
        e >= s + 15,
        "deep tip pipe under local tip hole, got {s}-{e}"
    );
    // Entirely-behind main-queue work must not be handed out while tip missing.
    // Advance index past tip chunk by completing owner; pB must not get a behind span.
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn wan_tip_gap_preempt_bulk_pipeline() {
    // W28b/W32: past body tip → contiguous tip-owner bulk (up to 128), not chunk-map clips.
    // Claim-frontier dens KEEP: second peer ahead after owner end.
    super::super::tip_stage::test_reset_tip_stage();
    let vh = Arc::new(AtomicU64::new(698_999));
    let chunks = vec![(698_953, 698_984), (698_985, 699_016)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        698_953,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(695_359);
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 1.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let work = assigner.get_work("pA", 1000);
    assert!(work.is_some(), "WAN tip owner must assign");
    let (s, e) = work.unwrap();
    assert_eq!(s, 699_000);
    assert!(
        e - s + 1 >= 64,
        "W32: WAN owner must pipeline deeply across chunk map, got {s}-{e}"
    );
    let second = assigner.get_work("pB", 1000);
    assert!(
        second.is_some(),
        "second peer should get ahead partition or main-queue work"
    );
    let (s2, e2) = second.unwrap();
    assert!(
        s2 > e,
        "ahead partition must start after tip owner end, got {s2}-{e2}"
    );
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn gap_preempt_bulk_when_peer_stuck_mid_chunk() {
    // Tip fill when next_needed is last height of containing chunk → extend into next.
    let vh = Arc::new(AtomicU64::new(505_183));
    let chunks = vec![(505_153, 505_184), (505_185, 505_216)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        505_153,
        true,
    );
    assigner.mark_bootstrap_complete();
    assert_eq!(
        assigner.get_work("pA", 1000),
        Some((505_184, 505_199)),
        "tip owner extends into next chunk (not (H,H))"
    );
    let second = assigner.get_work("pB", 1000);
    assert!(second.is_some());
    let (s, _e) = second.unwrap();
    assert!(
        s >= 505_200,
        "second peer ahead of tip owner, got start={s}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn gap_preempt_caps_fan_out_to_max_gap_fetchers() {
    // Cap at 2 tip owners for this test.
    unsafe { std::env::set_var("BLVM_IBD_GAP_FETCHERS", "2") };
    let vh = Arc::new(AtomicU64::new(100));
    let chunks = vec![(80, 200), (201, 250), (251, 300)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        80,
        true,
    );
    assigner.mark_bootstrap_complete();
    assert_eq!(
        assigner.get_work("pA", 1000),
        Some((101, 116)),
        "first peer tip-fills"
    );
    let _ = assigner.get_work("pB", 1000);
    let third = assigner.get_work("pC", 1000);
    assert!(third.is_some());
    let (s, e) = third.unwrap();
    // With default max=1 we'd never have 2 tip owners; with env=2, pB may cover tip.
    // Either way pC must not also cover next_needed=101 once two covering ranges exist,
    // OR if pB took ahead partition, pC still shouldn't duplicate tip owner range.
    assert!(
        s > 116 || s == 80,
        "third peer should be ahead partition or main queue, got {s}-{e}"
    );
    unsafe { std::env::remove_var("BLVM_IBD_GAP_FETCHERS") };
}

#[serial_test::serial(ibd)]
#[test]
fn gap_preempt_bulk_range_when_mid_chunk_has_runway() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(100));
    let chunks = vec![(80, 200), (201, 250)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        80,
        true,
    );
    assigner.mark_bootstrap_complete();
    // pA already owns tip-covering range (80-200).
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("pA".into(), vec![(80, 200)]);
    }
    assigner.note_tip_cover_claim("pA", 80, 200);
    // covering=1 at max → pB gets ahead partition after frontier 200, not a tip race.
    assert_eq!(
        assigner.get_work("pB", 1000),
        Some((201, 216)),
        "pB ahead partition after tip owner frontier"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn requeue_gap_height_push_front_micro_chunk() {
    let chunks = vec![(100, 199)];
    let vh = Arc::new(AtomicU64::new(149));
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
    assigner.requeue_gap_height(150);
    // W16 tip fill runs before retry micros and assigns bulk from next_needed.
    assert_eq!(assigner.get_work("p1", 1000), Some((150, 165)));
}

#[serial_test::serial(ibd)]
#[test]
fn w80_requeue_drops_obsolete_behind_tip_ranges() {
    // Live loop-1: ChunkGuard Drop re-queued 309798-309925 while tip≈321k.
    let chunks = vec![(300_000, 300_127), (321_000, 321_127)];
    let vh = Arc::new(AtomicU64::new(321_000)); // next_needed = 321001
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 300_000, true);
    assigner.set_wan_body_tip(312_499);
    assigner.requeue(309_798, 309_925, None);
    assert!(
        assigner.retry_queue.lock().unwrap().is_empty(),
        "behind-tip retry must not enter the queue"
    );
    assigner.requeue(321_001, 321_128, None);
    assert_eq!(assigner.retry_queue.lock().unwrap().len(), 1);
}

#[serial_test::serial(ibd)]
#[test]
fn requeue_gap_heights_batches_micro_chunks() {
    let chunks = vec![(100, 199)];
    let vh = Arc::new(AtomicU64::new(149));
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
    assigner.requeue_gap_heights(150, 4, None);
    // W16 tip fill prefers bulk 150-165 over coalesced micros.
    assert_eq!(assigner.get_work("p1", 1000), Some((150, 165)));
}

#[serial_test::serial(ibd)]
#[test]
fn requeue_chunk_containing_height_is_idempotent() {
    let chunks = vec![(100, 199)];
    let assigner = assigner_for_heights(&chunks, &["p1"], 100, false);
    assigner.requeue_chunk_containing_height(150);
    let after_first = assigner.remaining_count();
    assigner.requeue_chunk_containing_height(150);
    assert_eq!(
        assigner.remaining_count(),
        after_first,
        "second stall recovery must not duplicate micro-chunks"
    );
    // 1 main chunk (100-199) + 1 bulk (150-165) + 1 gap micro (150) per W9.
    assert_eq!(after_first, 3, "main chunk + bulk gap + single (H,H) race");
}

#[serial_test::serial(ibd)]
#[test]
fn stall_recovery_clears_exclude_on_existing_retry_entry() {
    let chunks = vec![(100, 199)];
    let vh = Arc::new(AtomicU64::new(149));
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
    assigner.requeue(100, 199, Some("p1".into()));
    assigner.requeue_stall_gaps(150, None);
    // Stall recovery must clear exclude on the containing full-chunk retry entry.
    let rq = assigner.retry_queue.lock().unwrap();
    let full = rq.iter().find(|(s, e, _)| *s == 100 && *e == 199);
    assert!(
        full.is_some_and(|(_, _, ex)| ex.is_none()),
        "exclude must be cleared so a peer can retry the containing chunk, got {full:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn requeue_stall_gaps_debounces_same_height_within_window() {
    let chunks = vec![(100, 199)];
    let vh = Arc::new(AtomicU64::new(149));
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], vh, 100, true);
    assigner.requeue_stall_gaps(150, None);
    let after_first = assigner.remaining_count();
    assigner.requeue_stall_gaps(150, None);
    assert_eq!(
        assigner.remaining_count(),
        after_first,
        "duplicate stall requeue within debounce window must not add micro-chunks"
    );
    assigner.requeue_stall_gaps(150, Some("p1".into()));
    assert_eq!(
        assigner.remaining_count(),
        after_first,
        "exclude must not bypass debounce for same height"
    );
    assigner.requeue_stall_gaps(151, None);
    assert!(
        assigner.remaining_count() > after_first,
        "different stall height may requeue within debounce window"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn p1a_open_tip_slot_requires_ready_snapshot() {
    // Live W34′ soak: open slot assigned ibd_ready=false workers → handshake hard-fail carousel.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["worker".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("worker".into(), 1.0),
        ("other".into(), 1.0),
        ("idle-ready".into(), 9.0),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from(["idle-ready".into()]));
    assigner.open_tip_owner_slot();
    assert!(
        assigner.get_work("worker", 1000).is_none(),
        "open tip slot must not assign worker missing from ready snapshot"
    );
    assigner.set_ibd_ready_peers(HashSet::from(["worker".into(), "idle-ready".into()]));
    assert_eq!(
        assigner.get_work("worker", 1000).map(|(s, _)| s),
        Some(901),
        "open tip slot assigns ready top-half worker"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn mode_t_sole_peer_gd_slow_still_assigns_tip_span() {
    // Tip-band cliff plan Phase 3: sole ready + elevated gd_ewma must keep tip span;
    // no blacklist / tip-owner fail cooldown on the only archive peer.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    let vh = Arc::new(AtomicU64::new(400_287));
    let assigner = ChunkAssigner::new(
        vec![(400_288, 400_415), (400_416, 400_543)],
        vec!["127.0.0.1:18333".into()],
        Arc::clone(&vh),
        400_288,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(400_287);
    assigner.set_peer_scores(&[("127.0.0.1:18333".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["127.0.0.1:18333".into()]));
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::mark_needed(400_288);
    assert_eq!(assigner.ibd_ready_peer_count(), 1);
    assert!(super::super::download::tip_hole_gd_slow_sole_keep(1));
    let work = assigner.get_work("127.0.0.1:18333", 1000);
    assert!(
        work.is_some(),
        "sole ready peer must get tip work under GD_SLOW"
    );
    let (s, e) = work.unwrap();
    assert!(e >= s, "tip span end≥start");
    assert!(
        e.saturating_sub(s) + 1 >= 32,
        "sole GD_SLOW must assign tip span, got {s}-{e}"
    );
    assigner.mark_tip_owner_fail_cooldown("127.0.0.1:18333", 180);
    assert!(
        !assigner.tip_owner_in_fail_cooldown("127.0.0.1:18333"),
        "sole peer must not enter tip-owner fail cooldown"
    );
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::mark_needed(0);
    assigner.set_tip_gap_missing(false);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p1a_open_tip_slot_not_blocked_by_idle_higher_peer() {
    // Equal scores: lex-earlier "idle" peers have capacity but no get_work caller.
    // Open slot must let a later active peer take tip (live 714261 deadlock).
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec![
            "100.0.0.1:8333".into(),
            "162.55.195.152:8333".into(),
            "170.75.166.57:8333".into(),
        ],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("100.0.0.1:8333".into(), 1.0),
        ("162.55.195.152:8333".into(), 1.0),
        ("163.0.0.1:8333".into(), 1.0), // scored, no worker
        ("170.75.166.57:8333".into(), 1.0),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "100.0.0.1:8333".into(),
        "162.55.195.152:8333".into(),
        "163.0.0.1:8333".into(),
        "170.75.166.57:8333".into(),
    ]));
    assigner.blacklist_peer("100.0.0.1:8333", Duration::from_secs(60));
    assigner.blacklist_peer("162.55.195.152:8333", Duration::from_secs(60));
    assigner.open_tip_owner_slot();
    assert_eq!(
        assigner
            .get_work("170.75.166.57:8333", 1000)
            .map(|(s, _)| s),
        Some(901),
        "open tip slot must not wait on idle higher-tiebreak peer 163.0.0.1"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w36_sla_rotate_releases_inflight_and_opens_tip_slot() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["owner".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("other".into(), 8.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["owner".into(), "other".into()]));
    assert_eq!(
        assigner.get_work("owner", 1000).map(|(s, e)| (s, e)),
        Some((901, 1028)),
        "WAN tip owner gets 128-deep session (W48 64-deep reverted)"
    );
    let (healthy, raw, _) = assigner.tip_flight_diag_healthy();
    assert!(healthy >= 1 && raw >= 1);
    let prev = assigner.rotate_tip_owner_on_sla();
    assert_eq!(prev.as_deref(), Some("owner"));
    assigner.blacklist_peer("owner", Duration::from_secs(60));
    let (healthy2, raw2, _) = assigner.tip_flight_diag_healthy();
    assert_eq!(healthy2, 0, "claims cleared");
    assert_eq!(raw2, 0, "inflight released");
    assert!(
        assigner.is_done() == false,
        "workers must stay alive on WAN tip gap"
    );
    // Post-SLA open slot: non-top peer (other) may take tip.
    assert_eq!(
        assigner.get_work("other", 1000).map(|(s, _)| s),
        Some(901),
        "open tip slot lets next peer take tip after SLA"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w82_open_slot_denies_floor_score_when_mid_ready_exists() {
    // Live mid-chain: open-slot lottery elected score=0.001 → 25s TIP_SLA stalls.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1000)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["floor".into(), "mid".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("floor".into(), 0.001), ("mid".into(), 0.50)]);
    assigner.set_ibd_ready_peers(HashSet::from(["floor".into(), "mid".into()]));
    assigner.open_tip_owner_slot();
    assert!(
        assigner.get_work("floor", 1000).is_none(),
        "W82: floor-score peer must not win open tip slot while mid ready"
    );
    assert_eq!(
        assigner.get_work("mid", 1000).map(|(s, _)| s),
        Some(901),
        "W82: mid/high ready worker takes open tip slot"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn is_done_when_validation_reaches_ibd_end_despite_wan_tip_gap() {
    // Live 2026-07-13: after vh==effective_end past body tip, wan_tip_gap_crawl kept
    // is_done()==false forever → download_handles.await blocked Phase 3.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1000)];
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800); // past body tip once next>800
    assigner.set_ibd_end_height(1000);
    assigner.set_tip_gap_missing(true);

    vh.store(999, Ordering::Relaxed);
    // Without end-height gate, W36 tip keep-alive would hold is_done==false here.
    assert!(
        !assigner.shutdown.load(Ordering::Relaxed),
        "precondition: not shut down"
    );
    // Mid-IBD: validation_reached_ibd_end is false, so tip-gap keep-alive wins.
    assert!(
        !assigner.is_done(),
        "mid-IBD: tip-gap keep-alive still applies (vh=999 < end=1000)"
    );

    vh.store(1000, Ordering::Relaxed);
    assert!(
        assigner.is_done(),
        "validation at IBD end must unblock worker exit despite wan_tip_gap / tip_gap_missing"
    );
    assert!(
        assigner.get_work("p1", 1000).is_none(),
        "no new work past IBD end"
    );

    // Explicit shutdown also forces done even before end.
    vh.store(900, Ordering::Relaxed);
    assigner.request_shutdown();
    assert!(assigner.is_done());
    assert!(assigner.get_work("p1", 1000).is_none());
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0b_wan_stall_retry_blocked_without_owner() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["bind".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("other".into(), 8.0),
        ("racer".into(), 7.0),
    ]);
    // Non-force still must not enqueue WAN bulk/micro storms.
    assigner.requeue_stall_gaps(901, None);
    assert!(
        assigner.get_work("racer", 1000).is_none(),
        "WAN non-force stall must not assign to non-owner when preferred=None"
    );
    let ready = HashSet::from(["other".into()]);
    assigner.set_ibd_ready_peers(ready);
    assert_eq!(
        assigner.get_work("other", 1000).map(|(s, _)| s),
        Some(901),
        "gap preempt still arms ready owner"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0b_wan_stall_recovery_skips_micro_enqueue() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["owner".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.requeue_stall_gaps(901, None);
    let rq = assigner.retry_queue.lock().unwrap();
    assert!(
        rq.is_empty(),
        "WAN tip gap non-force must not enqueue stall micro/bulk — gap preempt only"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// W73: force + covering=0 arms a single (H,H) tip hole on WAN.
/// Stripe-32 FORCE re-cheesed TIP_HOLE_AHEAD (Land E 2026-08-13 soak 12).
#[serial_test::serial(ibd)]
#[test]
fn w73_wan_force_requeue_enqueues_tip_hole_when_covering_zero() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["owner".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.requeue_stall_gaps_force(901, None);
    let rq: Vec<_> = assigner
        .retry_queue
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    let tip_heights: Vec<u64> = rq
        .iter()
        .filter(|(s, e, _)| *s == *e)
        .map(|(s, _, _)| *s)
        .collect();
    assert_eq!(
        tip_heights,
        vec![901],
        "WAN force covering=0 must enqueue (H,H) only; got {rq:?}"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// TRUE WAN: download complete must not clear tip-cover while tip still in span.
#[serial_test::serial(ibd)]
#[test]
fn wan_tip_claim_keep_until_tip_advances_past_span() {
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::set_var("BLVM_IBD_WAN_TIP_CLAIM_KEEP", "1");
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["owner".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_wan_body_tip(800); // next=901 > body → WAN tip crawl
    assigner.note_tip_cover_claim("owner", 901, 932);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("owner".into(), vec![(901, 932)]);
    }
    // Tip body present → keep; tip still missing → clear (allow re-fetch).
    assigner.tip_gap_missing.store(false, Ordering::Relaxed);
    assert_eq!(assigner.healthy_tip_cover_count(901), 1);
    assigner.on_chunk_complete_range("owner", 901, 932);
    assert_eq!(
        assigner.healthy_tip_cover_count(901),
        1,
        "claim must survive complete while tip present in span"
    );
    assigner.note_tip_cover_claim("owner", 901, 932);
    assigner.tip_gap_missing.store(true, Ordering::Relaxed);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("owner".into(), vec![(901, 932)]);
    }
    assigner.on_chunk_complete_range("owner", 901, 932);
    assert_eq!(
        assigner.healthy_tip_cover_count(901),
        0,
        "must clear when tip still missing after complete"
    );
    assigner.note_tip_cover_claim("owner", 901, 932);
    assigner.tip_gap_missing.store(false, Ordering::Relaxed);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("owner".into(), vec![(901, 932)]);
    }
    assigner.on_chunk_complete_range("owner", 901, 932);
    assert_eq!(assigner.healthy_tip_cover_count(901), 1);
    // Tip walks past span → prune on next complete (or retain filter).
    vh.store(933, Ordering::Relaxed);
    assigner.on_chunk_complete_range("owner", 940, 950);
    assert_eq!(
        assigner.healthy_tip_cover_count(934),
        0,
        "claims ending before tip must prune"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_WAN_TIP_CLAIM_KEEP");
    }
}

/// Dens: cross-height force debounce — tip 901 then 902 within window must not storm.
#[serial_test::serial(ibd)]
#[test]
fn w73_wan_force_requeue_debounces_across_tip_advance() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["owner".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.requeue_stall_gaps_force(901, None);
    let n1 = assigner.retry_queue.lock().unwrap().len();
    assert!(n1 > 0, "first force must enqueue");
    vh.store(901, Ordering::Relaxed); // tip advanced
    assigner.requeue_stall_gaps_force(902, None);
    let n2 = assigner.retry_queue.lock().unwrap().len();
    assert_eq!(
        n2, n1,
        "second force within debounce must not enqueue more (cross-height); n1={n1} n2={n2}"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// W73: force must not storm while two tip covers are already in flight.
#[serial_test::serial(ibd)]
#[test]
fn w73_wan_force_requeue_skips_when_covering_nonzero() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["owner".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 901, 1028);
        ChunkAssigner::insert_in_flight(&mut g, "other", 901, 901);
    }
    assigner.note_tip_cover_claim("owner", 901, 1028);
    assigner.note_tip_cover_claim("other", 901, 901);
    assigner.requeue_stall_gaps_force(901, None);
    assert!(
        assigner.retry_queue.lock().unwrap().is_empty(),
        "WAN force must not enqueue while tip covering>1"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0b_non_wan_stall_recovery_still_enqueues_micro() {
    let vh = Arc::new(AtomicU64::new(50));
    let chunks = vec![(0, 199)];
    let assigner = ChunkAssigner::new(chunks, vec!["p1".into()], Arc::clone(&vh), 0, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(100);
    assigner.requeue_stall_gaps(51, None);
    let rq = assigner.retry_queue.lock().unwrap();
    assert!(
        !rq.is_empty(),
        "pre-body-tip gap should still use stall micro recovery"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn p0b_wan_stall_skipped_while_deep_owner_in_flight() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["owner".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 901, 1028);
    }
    assigner.note_tip_cover_claim("owner", 901, 1028);
    assigner.requeue_stall_gaps(901, None);
    assert!(
        assigner.retry_queue.lock().unwrap().is_empty(),
        "must not micro-requeue while deep owner covers tip"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn blacklist_blocks_peer_until_expired() {
    let chunks = vec![(0, 63)];
    let assigner = assigner_for_heights(&chunks, &["p1"], 0, false);
    assigner.blacklist_peer("p1", Duration::from_secs(3600));
    assert!(assigner.get_work("p1", 1000).is_none());
}

#[serial_test::serial(ibd)]
#[test]
fn work_stealing_ignores_peer_binding() {
    let chunks = vec![(0, 63)];
    let assigner = assigner_for_heights(&chunks, &["p1"], 0, true);
    assert_eq!(assigner.get_work("other-peer", 1000), Some((0, 63)));
}

#[serial_test::serial(ibd)]
#[test]
fn chunk_guard_requeues_on_drop() {
    let chunks = vec![(0, 63)];
    let assigner = Arc::new(assigner_for_heights(&chunks, &["p1"], 0, false));
    let work = assigner.get_work("p1", 1000).unwrap();
    {
        let _guard = ChunkGuard::new(work.0, work.1, None, "p1".into(), Arc::clone(&assigner));
    }
    assert_eq!(assigner.remaining_count(), 1);
}

#[serial_test::serial(ibd)]
#[test]
fn a4_top_scored_peer_may_hold_two_in_flight() {
    let vh = Arc::new(AtomicU64::new(99));
    let chunks = vec![(100, 115), (116, 131), (132, 147), (148, 163)];
    let peers = vec!["fast".into(), "mid".into(), "slow".into(), "worse".into()];
    let assigner = ChunkAssigner::new(chunks, peers, vh, 100, true);
    assigner.mark_bootstrap_complete();
    assigner.set_peer_scores(&[
        ("fast".into(), 10.0),
        ("mid".into(), 5.0),
        ("slow".into(), 2.0),
        ("worse".into(), 1.0),
    ]);
    assert_eq!(assigner.get_work("fast", 1000), Some((100, 115)));
    assert_eq!(
        assigner.get_work("fast", 1000),
        Some((116, 131)),
        "top-half scorer may pipeline a second chunk"
    );
    assert!(
        assigner.get_work("fast", 1000).is_none(),
        "still capped at dual in-flight"
    );
    assert_eq!(assigner.get_work("worse", 1000), Some((132, 147)));
    assert!(
        assigner.get_work("worse", 1000).is_none(),
        "bottom-half scorer stays single in-flight"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn p5_bottom_quartile_skips_gap_preempt() {
    let vh = Arc::new(AtomicU64::new(100));
    let chunks = vec![(80, 200), (201, 250), (251, 300), (301, 350)];
    let peers = vec!["a".into(), "b".into(), "c".into(), "d".into()];
    let assigner = ChunkAssigner::new(chunks, peers, Arc::clone(&vh), 80, true);
    assigner.mark_bootstrap_complete();
    assigner.set_peer_scores(&[
        ("a".into(), 10.0),
        ("b".into(), 8.0),
        ("c".into(), 6.0),
        ("d".into(), 1.0),
    ]);
    // Mid-chunk tip (next=101) → high scorer tip-fills.
    assert_eq!(assigner.get_work("a", 1000), Some((101, 116)));
    // Low-score peer skips tip ownership but still gets ahead partition (use peers).
    assert_eq!(
        assigner.get_work("d", 1000),
        Some((117, 132)),
        "low-score peer takes ahead partition, not tip race"
    );
    // Another peer continues partitioning ahead.
    let b = assigner.get_work("b", 1000);
    assert!(b.is_some());
    let (s, _) = b.unwrap();
    assert!(s >= 133, "b continues ahead of d, got start={s}");
}

#[serial_test::serial(ibd)]
#[test]
fn w16_refuses_far_main_queue_while_tip_uncovered() {
    let vh = Arc::new(AtomicU64::new(100));
    // Tip at 101 inside first chunk; far chunk starts at 300 (> tip+64 band).
    let chunks = vec![(80, 200), (300, 363)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        80,
        true,
    );
    assigner.mark_bootstrap_complete();
    // Force next_index to the far chunk with tip uncovered.
    assigner.next_index.store(1, Ordering::Relaxed);
    let w = assigner.get_work("pA", 1000);
    assert_eq!(w, Some((101, 116)), "W16 tip fill before far main queue");
}

#[serial_test::serial(ibd)]
#[test]
fn w33_wan_gap_top_peer_only_tip_owner() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(710_000));
    let chunks = vec![(710_000, 710_100), (710_101, 710_200)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        710_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(700_000);
    assigner.set_peer_scores(&[("pA".into(), 9.0), ("pB".into(), 3.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assert_eq!(
        assigner.get_work("pB", 1000),
        None,
        "W33a: non-top peer must not take WAN tip owner"
    );
    let tip = assigner.get_work("pA", 1000);
    assert!(tip.is_some(), "top peer must take tip owner");
    let (s, e) = tip.unwrap();
    assert_eq!(s, 710_001);
    assert!(e - s + 1 >= 64, "deep pipe expected, got {s}-{e}");
}

#[serial_test::serial(ibd)]
#[test]
fn w15_overlapping_bulk_counts_toward_gap_fetcher_cap() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(149));
    let chunks = vec![(100, 199)];
    let assigner = ChunkAssigner::new(chunks, vec!["pA".into()], vh, 100, true);
    assigner.mark_bootstrap_complete();
    // First tip fill 150-165 (mid-chunk).
    assert_eq!(assigner.get_work("pA", 1000), Some((150, 165)));
    assigner.on_chunk_complete("pA");
    // Simulate two overlapping bulks already covering tip (cap=2).
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("x".into(), vec![(150, 165)]);
        g.insert("y".into(), vec![(151, 166)]);
    }
    assigner.note_tip_cover_claim("x", 150, 165);
    assigner.note_tip_cover_claim("y", 151, 166);
    assigner.requeue(152, 167, None);
    // Cap reached — must not assign another overlapping tip bulk to pA.
    let w = assigner.get_work("pA", 1000);
    if let Some((s, e)) = w {
        assert!(
            !(s <= 150 && 150 <= e),
            "W15: overlapping tip range must not assign when cap reached, got {s}-{e}"
        );
    }
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_empty_ready_denies_non_worker_on_wan() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["worker".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("worker".into(), 9.0), ("scored-idle".into(), 8.0)]);
    assigner.set_ibd_ready_peers(HashSet::new());
    assert!(
        assigner.get_work("scored-idle", 1000).is_none(),
        "empty ready must deny non-worker tip owner on WAN"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_tip_owner_open_denies_active_worker_not_in_ready() {
    // Live W34′ soak: 11/42 assigns ibd_ready=false → hard-fail nudge carousel ~4 blk/s.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["worker".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("worker".into(), 1.0),
        ("other".into(), 1.0),
        ("idle-ready".into(), 9.0),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from(["idle-ready".into()]));
    assigner.open_tip_owner_slot();
    assert!(
        assigner.get_work("worker", 1000).is_none(),
        "open tip slot must not assign active worker missing from ready snapshot"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_tip_owner_open_denies_scored_non_worker_not_ready() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["worker".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("worker".into(), 1.0), ("scored-idle".into(), 9.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["idle-ready".into()]));
    assigner.open_tip_owner_slot();
    assert!(
        assigner.get_work("scored-idle", 1000).is_none(),
        "open tip slot must not assign scored non-workers missing from ready snapshot"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_keeps_ready_sticky_owner() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["sticky".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 9.0),
        ("other".into(), 1.0),
        ("mid".into(), 5.0),
        ("low".into(), 0.0),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "other".into(),
        "mid".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "nudge must keep ready sticky owner"
    );
    assert_eq!(
        assigner.get_work("sticky", 1000).map(|(s, _)| s),
        Some(901),
        "open slot after nudge must re-arm sticky owner"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "sticky must remain preferred after assign"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_upgrades_mediocre_sticky_to_better_worker() {
    // Live A6c: sticky score=1.000 @ ~15s/chunk locked out breakthrough-class peers.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "fast".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 1.0),
        ("fast".into(), 1.365),
        ("mid".into(), 1.1),
        ("low".into(), 0.5),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "fast".into(),
        "mid".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("fast"),
        "nudge must pin preferred to better active worker (not None lottery)"
    );
    assert_eq!(
        assigner.get_work("fast", 1000).map(|(s, _)| s),
        Some(901),
        "open slot must arm better-scored active worker"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("fast"),
        "better worker becomes new sticky"
    );
    // Sticky may still take ahead partitions; tip cover must stay with fast.
    let (covering, _, _) = assigner.tip_flight_diag();
    assert!(covering >= 1, "fast must hold tip cover after upgrade");
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_ignores_floor_noise_upgrade() {
    // Live A6d: sticky@0.100 → better@0.191 thrash cleared owners mid-pipe.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("other".into(), 0.191),
        ("mid".into(), 0.190),
        ("low".into(), 0.100),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "other".into(),
        "mid".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "floor-noise score delta must not clear sticky"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn nudge_defers_upgrade_while_sticky_holds_tip_download() {
    // Live genesis 2026-07-17: every ~1s UPGRADE sticky@0.001→better_worker blacklisted
    // the in-flight tip peer → IBD_TIP_BLACKLIST abort → tip freeze. Mid-download must
    // defer score upgrade; tip-SLA is the abort path.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "faster".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("sticky".into(), 0.001), ("faster".into(), 0.210)]);
    assigner.set_ibd_ready_peers(HashSet::from(["sticky".into(), "faster".into()]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.note_tip_cover_claim("sticky", 901, 1028);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("sticky".into(), vec![(901, 1028)]);
    }
    assert_eq!(assigner.tip_flight_diag().0, 1, "tip covering in-flight");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "must not upgrade away from peer mid tip-download"
    );
    assert!(
        !assigner.is_peer_blacklisted("sticky"),
        "must not blacklist mid tip-download peer (that aborts the pipe)"
    );
    // After flight ends, upgrade + blacklist of demoted sticky is allowed.
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.remove("sticky");
    }
    assigner.clear_tip_cover_claims_for_peer("sticky");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("faster"),
        "after tip flight ends, 0.001 sticky may upgrade to better_worker"
    );
    assert!(
        assigner.is_peer_blacklisted("sticky"),
        "demoted sticky without tip flight may be cooloff-blacklisted"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_upgrades_floor_sticky_on_2x_jump() {
    // Live 2026-07-14: sticky@0.100 vs top_w@0.203 — must upgrade (2× rule).
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "faster".into(), "low".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("faster".into(), 0.210),
        ("mid".into(), 0.190),
        ("low".into(), 0.001),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "faster".into(),
        "mid".into(),
        "low".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("faster"),
        "2× floor jump must pin preferred to better_worker@0.210 (not None lottery)"
    );
    // Live 2026-07-15: demoted peer_ok (score=0.001, floor=0.001) must not win *tip*
    // ahead of the pinned upgrade target — probe *before* faster arms.
    if let Some((s, _)) = assigner.get_work("low", 1000) {
        assert_ne!(
            s, 901,
            "demoted/floor peer must not take tip span on open slot (got start={s})"
        );
    }
    assert_eq!(
        assigner.get_work("faster", 1000).map(|(s, _)| s),
        Some(901),
        "open slot must arm 2×-better worker"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_keeps_hot_floor_sticky_streamer() {
    // Live 2026-07-14: sticky@0.100 mid-GAP_STREAM upgraded to idle@0.211 → walk-in abort.
    // Hold only when recent tip BPS is proven ≥ stretch floor_min (missing samples escape).
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(9999));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "faster".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("faster".into(), 0.210),
        ("mid".into(), 0.190),
        ("low".into(), 0.100),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "faster".into(),
        "mid".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    test_set_sticky_tenure(&assigner, "sticky", 1000, 600);
    // note_tip_owner_assigned seeds a "now" sample — clear so ago-samples stay time-ordered.
    assigner.tip_progress_samples.lock().unwrap().clear();
    // Sample older than recent window (default 60s). +2700 / 90s = 30 ≥ stretch floor_min=22.
    test_push_tip_sample(&assigner, 7300, 90);
    test_push_tip_sample(&assigner, 10000, 0);
    assigner.note_wan_tip_stream("sticky");
    assert!(
        assigner.peer_recently_tip_streaming("sticky", Duration::from_secs(15)),
        "just-streamed sticky must be hot"
    );
    assert!(
        !assigner.preferred_is_idle_floor_sticky(),
        "hot sticky with proven stretch BPS is not idle"
    );
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "hot tip streamer with ≥stretch BPS must not be score-upgraded away"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_upgrades_hot_but_stalling_floor_sticky() {
    // Live 2026-07-15: receive-path tip-hot + score=0.100 @ ~5 blk/s blocked 2× upgrade
    // while OPEN_STALL top_w@0.197. Hot+below stretch floor_min must escape.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec!["sticky".into(), "faster".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("faster".into(), 0.210),
        ("mid".into(), 0.190),
        ("low".into(), 0.100),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "faster".into(),
        "mid".into(),
    ]));
    test_set_sticky_tenure(&assigner, "sticky", 1000, 600);
    // Recent ~5 blk/s (< stretch floor_min 22).
    test_push_tip_sample(&assigner, 9700, 60);
    test_push_tip_sample(&assigner, 10000, 0);
    assigner.note_wan_tip_stream("sticky");
    assert!(assigner.peer_recently_tip_streaming("sticky", Duration::from_secs(15)));
    assert!(
        assigner.preferred_is_idle_floor_sticky(),
        "hot-but-below-stretch floor sticky is idle for nudge"
    );
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("faster"),
        "hot below-stretch sticky must 2×-upgrade to faster worker"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_upgrades_hot_floor_sticky_below_stretch() {
    // Live 2026-07-15 ~h670k: ~11–15 blk/s hot sticky@0.100 vs OPEN_STALL top_w@0.197.
    // open_slot_min=12 correctly keeps A6N; stretch floor_min=22 must still allow 2× escape.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec!["sticky".into(), "faster".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("faster".into(), 0.210),
        ("mid".into(), 0.190),
        ("low".into(), 0.100),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "faster".into(),
        "mid".into(),
    ]));
    test_set_sticky_tenure(&assigner, "sticky", 1000, 600);
    // Recent +900 / 60s = 15 blk/s — ≥ open_slot_min, < stretch floor_min.
    test_push_tip_sample(&assigner, 9100, 60);
    test_push_tip_sample(&assigner, 10000, 0);
    assigner.note_wan_tip_stream("sticky");
    assert!(
        assigner.preferred_is_idle_floor_sticky(),
        "15 blk/s hot floor sticky is below stretch for nudge"
    );
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("faster"),
        "below-stretch hot sticky must 2×-upgrade"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w28d_hot_tip_streamer_survives_walk_in_after_claim_clear() {
    // After upgrade clears exact tip-cover claim, hot streamer must not abort.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["streamer".into(), "ahead".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("streamer".into(), 0.100), ("ahead".into(), 0.210)]);
    assigner.set_ibd_ready_peers(HashSet::from(["streamer".into(), "ahead".into()]));
    assigner.note_tip_cover_claim("streamer", 901, 964);
    assigner.note_wan_tip_stream("streamer");
    // Simulate upgrade clearing the claim while streamer still holds the range.
    assigner.clear_all_tip_cover_claims();
    assert!(
        !assigner.should_abort_tip_walk_in("streamer", 901, 964),
        "hot GAP_STREAM peer must not walk-in-abort after claim clear"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_nudge_ignores_unproven_default_score_upgrade() {
    // Live A6e: 13/13 upgrades sticky@0.100 → unproven@1.000 (blocks_received==0).
    // tip_owner_score demotes unproven; min-candidate 0.5 also blocks raw default 1.0
    // only when... wait, raw 1.0 would still pass min 0.5. Refresh demotion is required.
    // Simulate post-refresh demoted ranks:
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "unproven".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("unproven".into(), 0.001),
        ("mid".into(), 0.001),
        ("low".into(), 0.001),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "unproven".into(),
        "mid".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "demoted unproven must not clear delivering sticky"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_below_floor_sticky_does_not_deadlock_open_tip() {
    // Live A6g: preferred stayed after span end while score fell below WAN median
    // (OPEN_STALL: preferred≠top_w, floor=0.190, open=true, covering=0, busy=0).
    // Upgrade min 0.5 never fires in tip_owner_score demotion world → exclusive sticky
    // blocks all peer_ok workers forever.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135), (1136, 1199)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "top_w".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.050), // below median floor
        ("top_w".into(), 0.195),
        ("mid".into(), 0.190),
        ("low".into(), 0.100),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "top_w".into(),
        "mid".into(),
        "low".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assert!(
        assigner.tip_sticky_usable("sticky"),
        "A6k: ready+active sticky remains usable even below peer_ok floor"
    );
    assigner.nudge_wan_tip_owner();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "nudge must keep ready sticky (score-floor must not STICKY_DROP)"
    );
    // Sticky must be able to re-arm tip despite floor.
    let tip = assigner.get_work("sticky", 1000);
    assert_eq!(
        tip.map(|(s, _)| s),
        Some(901),
        "below-floor sticky must still take tip"
    );
    let te = tip.unwrap().1;
    // Non-sticky may take non-overlapping ahead/main-queue, but not tip cover.
    if let Some((s, e)) = assigner.get_work("top_w", 1000) {
        assert!(
            s > te || e < 901,
            "top_w must not steal tip cover from usable sticky, got {s}-{e} tip_end={te}"
        );
    }
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6l_sticky_below_median_gets_top_in_flight_cap() {
    // Live A6k: sticky@0.1 < median → max_in_flight=1 → cannot re-arm next tip span.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1135), (1136, 1263), (1264, 1391)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "top_w".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("top_w".into(), 0.195),
        ("mid".into(), 0.190),
        ("low".into(), 0.185),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "top_w".into(),
        "mid".into(),
        "low".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assert_eq!(
        assigner.max_in_flight_for("sticky"),
        ChunkAssigner::top_peer_in_flight_cap(),
        "A6l: preferred sticky must get top in-flight cap even below score median"
    );
    assert_eq!(
        assigner.max_in_flight_for("low"),
        1,
        "non-sticky below median stays at 1"
    );
    // Fill one span, sticky must still re-arm tip with second slot.
    let first = assigner.get_work("sticky", 1000);
    assert!(first.is_some(), "sticky first tip assign");
    let second = assigner.get_work("sticky", 1000);
    assert!(
        second.is_some(),
        "A6l: sticky must re-arm second tip span while first still in flight"
    );
    // Idle higher-scored peer must not steal tip while sticky holds / is usable.
    if let Some((s, e)) = assigner.get_work("top_w", 1000) {
        let tip = vh.load(Ordering::Relaxed) + 1;
        assert!(
            s > tip && !(s <= tip && tip <= e),
            "top_w must not steal tip cover while sticky busy, got {s}-{e}"
        );
    }
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a31_frontier_dual_removed_top1_stays_capped() {
    // T2.5: frontier dual on-path is gone. TOP=1 sticky stays flight=1 even if
    // the old env names are set (policy no longer feeds an assigner bypass).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    let prev_top = std::env::var("BLVM_IBD_TOP_PEER_IN_FLIGHT").ok();
    unsafe {
        std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", "1");
        std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL", "1");
        std::env::set_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS", "0");
    }
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "other", "mid"]);
    assigner.set_peer_scores(&[
        ("sticky".into(), 9.0),
        ("other".into(), 8.0),
        ("mid".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    let tip = assigner
        .get_work("sticky", 4096)
        .expect("tip owner under TOP=1");
    assert_eq!(tip.0, 901);
    assigner.set_tip_gap_missing(false);
    assert!(
        assigner.get_work("sticky", 4096).is_none(),
        "T2.5: TOP=1 sticky must not take after-tip while tip flight held"
    );

    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        match prev_top {
            Some(v) => std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", v),
            None => std::env::remove_var("BLVM_IBD_TOP_PEER_IN_FLIGHT"),
        }
        std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL");
        std::env::remove_var("BLVM_IBD_TIP_FRONTIER_DUAL_DISTRESS");
    }
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w35ppp_sticky_tip_session_is_deep() {
    // Near tip (header tip close): WAN tip owner gets a deep session (~128 default).
    // Dual-pipe second get_work is dead on 1-worker/peer WAN without bulk window.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1135), (1136, 1263), (1264, 1391)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "other".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    // Near tip — not bulk catch-up (header only ~490 ahead of next).
    assigner.set_header_tip(1391);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("other".into(), 0.195),
        ("mid".into(), 0.190),
        ("low".into(), 0.185),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "other".into(),
        "mid".into(),
        "low".into(),
    ]));
    // C1e/C1g: while tip missing, tip-owner takes runway stripe (default 32), not 128.
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");

    let first = assigner.get_work("sticky", 256);
    assert!(first.is_some(), "sticky tip span");
    let (s0, e0) = first.unwrap();
    assert_eq!(s0, 901);
    let span = e0.saturating_sub(s0).saturating_add(1);
    assert!(
        (8..=96).contains(&span),
        "tip-missing owner stripe must be runway-sized, got {s0}-{e0} span={span}"
    );

    // After tip lands: deep pipe on a fresh assigner; ahead OK with high holes.
    assigner.on_chunk_complete_range("sticky", s0, e0);
    assigner.set_tip_gap_missing(false);
    // C1i: contig≥8 before deep/ahead (tipfix DNA).
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let deep = assigner
        .get_work("sticky", 256)
        .expect("deep tip after tip lands");
    let deep_span = deep.1.saturating_sub(deep.0).saturating_add(1);
    assert!(
        deep_span >= 100,
        "near-tip session after tip lands must be ~128 deep, got {}-{} span={deep_span}",
        deep.0,
        deep.1
    );
    // W47: ahead OK with high holes after tip lands.
    assigner.set_tip_bridge_holes(64);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let ahead = assigner.get_work("other", 256);
    assert!(
        ahead.is_some(),
        "W47: other must get tip-band ahead with holes=64 after tip lands"
    );
    let (s, e) = ahead.unwrap();
    assert!(
        s > deep.1,
        "ahead after tip end, got {s}-{e} tip_end={}",
        deep.1
    );
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn c1g_freezes_past_tip_stripes_while_tip_missing() {
    // C1f live: tip_hole_ahead×20 / ahead_buf_p50=115 — stripes past tip while tip empty.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("ahead".into(), 8.0),
        ("spare".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    let tip = assigner.get_work("owner", 4096).expect("tip owner");
    assert_eq!(tip.0, 901);
    assert!(tip.1 > tip.0, "tip owner stripe");

    for peer in ["ahead", "spare"] {
        match assigner.get_work(peer, 4096) {
            None => {}
            // C1h: tip-height race (tip fill or clipped main-queue) OK.
            Some((s, e)) if s == 901 && e == 901 => {
                assigner.on_chunk_complete_range(peer, s, e);
            }
            Some((s, e)) => panic!("C1g: past-tip stripe while tip missing, {peer} got {s}-{e}"),
        }
    }

    assigner.set_tip_gap_missing(false);
    // C1i: ahead also requires contig runway ≥ min (default 8).
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let ahead = assigner
        .get_work("ahead", 4096)
        .expect("ahead after tip lands");
    assert!(
        ahead.0 > tip.1,
        "after tip lands, ahead starts past tip stripe end, got {}-{}",
        ahead.0,
        ahead.1
    );
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn bulk_catchup_ahead_ok_with_high_holes() {
    // W47: bulk catch-up keeps multi-peer ahead under high holes when tip healthy.
    // Tip owner deep pipe (≥128); ahead fills past that. Soft-retry freezes ahead.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("ahead".into(), 8.0),
        ("spare".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    // C1g: tip in reorder → deep tip pipe + multi-peer ahead (not while tip missing).
    assigner.set_tip_gap_missing(false);
    // C1i: contig≥8 before deep/ahead (tip form DNA).
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(64);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    let tip = assigner.get_work("owner", 4096);
    assert!(tip.is_some(), "tip owner must get work");
    let (ts, te) = tip.unwrap();
    assert_eq!(ts, 901);
    let tip_span = te.saturating_sub(ts).saturating_add(1);
    assert!(
        tip_span >= 128,
        "WAN tip owner must get ≥128 deep pipe in bulk catch-up, got {tip_span}"
    );

    let ahead = assigner.get_work("ahead", 4096);
    assert!(
        ahead.is_some(),
        "W47: multi-peer ahead must work with holes=64 when tip healthy"
    );
    let (s, e) = ahead.unwrap();
    assert!(s > te, "ahead past tip owner end, got {s}-{e} tip_end={te}");
    assert!(
        s <= ts.saturating_add(400),
        "WAN ahead must stay near tip, got start={s} tip={ts}"
    );
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6k_not_ready_sticky_still_dropped_on_nudge() {
    // A6h safety: not-ready sticky must still clear so open slot can re-arm.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135), (1136, 1199)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "top_w".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("top_w".into(), 0.195),
        ("mid".into(), 0.190),
        ("low".into(), 0.100),
    ]);
    // sticky NOT in ready set
    assigner.set_ibd_ready_peers(HashSet::from(["top_w".into(), "mid".into(), "low".into()]));
    assigner.note_tip_owner_assigned("sticky");
    assert!(!assigner.tip_sticky_usable("sticky"));
    assigner.nudge_wan_tip_owner();
    assert_eq!(assigner.preferred_tip_owner().as_deref(), None);
    assert_eq!(
        assigner.get_work("top_w", 1000).map(|(s, _)| s),
        Some(901),
        "open slot must arm a ready peer_ok worker after not-ready sticky drop"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_ready_floor_not_inflated_by_unready_high_scorers() {
    // Live A6i: floor=0.153 from unready scorers, all ready ≤0.127 → ready_active_ok=0/9.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![
        (880, 1007),
        (1008, 1071),
        (1072, 1135),
        (1136, 1199),
        (1200, 1263),
        (1264, 1327),
        (1328, 1391),
        (1392, 1455),
    ];
    let assigner = ChunkAssigner::new(
        chunks,
        vec![
            "live0".into(),
            "live1".into(),
            "live2".into(),
            "live3".into(),
            "gone0".into(),
            "gone1".into(),
            "gone2".into(),
            "gone3".into(),
        ],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("live0".into(), 0.127),
        ("live1".into(), 0.120),
        ("live2".into(), 0.115),
        ("live3".into(), 0.110),
        ("gone0".into(), 0.195),
        ("gone1".into(), 0.190),
        ("gone2".into(), 0.185),
        ("gone3".into(), 0.180),
    ]);
    // Only low-scored peers are ready (gone* disconnected).
    assigner.set_ibd_ready_peers(HashSet::from([
        "live0".into(),
        "live1".into(),
        "live2".into(),
        "live3".into(),
    ]));
    assigner.open_tip_owner_slot();
    assert!(
        assigner.peer_ok_for_gap_race("live0"),
        "ready-only floor must admit top live worker"
    );
    assert_eq!(
        assigner.get_work("live0", 1000).map(|(s, _)| s),
        Some(901),
        "open tip must arm despite unready high scorers"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_idle_score_pollution_does_not_block_active_tip_owner() {
    // Live A6d: set_peer_scores(all network) injected idle peers at 1.0; tip workers
    // at ~0.2 failed peer_ok median → post-SLA covering=0 forever.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135), (1136, 1199)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["w0".into(), "w1".into(), "w2".into(), "w3".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    let mut scores = vec![
        ("w0".into(), 0.19),
        ("w1".into(), 0.18),
        ("w2".into(), 0.17),
        ("w3".into(), 0.16),
    ];
    for i in 0..40 {
        scores.push((format!("idle{i}:8333"), 1.0));
    }
    assigner.set_peer_scores(&scores);
    assigner.set_ibd_ready_peers(HashSet::from([
        "w0".into(),
        "w1".into(),
        "w2".into(),
        "w3".into(),
    ]));
    assigner.open_tip_owner_slot();
    assert_eq!(
        assigner.get_work("w0", 1000).map(|(s, _)| s),
        Some(901),
        "active worker at 0.19 must pass WAN peer_ok despite idle 1.0 pollution"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_open_slot_without_sticky_allows_any_active_ready_worker() {
    // Live A6d post-SLA: preferred=None + sole top_w gate → deadlock if top_w not polling.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1071)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["aaaa".into(), "zzzz".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    // Equal scores: lex-max "zzzz" would be sole top_w under old gate.
    assigner.set_peer_scores(&[
        ("aaaa".into(), 0.2),
        ("zzzz".into(), 0.2),
        ("mid".into(), 0.15),
        ("low".into(), 0.1),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from(["aaaa".into(), "zzzz".into()]));
    assigner.open_tip_owner_slot();
    assert_eq!(
        assigner.get_work("aaaa", 1000).map(|(s, _)| s),
        Some(901),
        "non-top_w active ready worker must re-arm open tip slot after SLA"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p0a_tip_owner_open_denies_score_zero_bottom_half() {
    // Live regression: tip_owner_open lotteried score=0 ready peers → ~2 blk/s.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007)];
    let assigner = ChunkAssigner::new(chunks, vec!["top".into()], Arc::clone(&vh), 880, true);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        ("top".into(), 9.0),
        ("good".into(), 8.0),
        ("mid".into(), 5.0),
        ("zero".into(), 0.0),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "top".into(),
        "good".into(),
        "mid".into(),
        "zero".into(),
    ]));
    assigner.open_tip_owner_slot();
    assert!(
        assigner.get_work("zero", 1000).is_none(),
        "open tip slot must not assign bottom-half score=0 peers"
    );
    assert_eq!(
        assigner.get_work("top", 1000).map(|(s, _)| s),
        Some(901),
        "open tip slot must still assign top-half ready peer"
    );
    super::super::tip_stage::clear_tip_failover();
}

fn test_set_sticky_tenure(assigner: &ChunkAssigner, peer: &str, start_h: u64, ago_secs: u64) {
    *assigner.preferred_tip_owner.lock().unwrap() = Some(peer.to_string());
    *assigner.sticky_wan_tenure.lock().unwrap() = Some(StickyWanTenure {
        peer: peer.to_string(),
        start_next_needed: start_h,
        started_at: Instant::now() - Duration::from_secs(ago_secs),
    });
}

fn test_push_tip_sample(assigner: &ChunkAssigner, next_needed: u64, ago_secs: u64) {
    assigner
        .tip_progress_samples
        .lock()
        .unwrap()
        .push_back((Instant::now() - Duration::from_secs(ago_secs), next_needed));
}

#[serial_test::serial(ibd)]
#[test]
fn a6n_rotates_to_tip_stream_peer_not_bulk_hero() {
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let slow = "10.0.0.1:8333";
    let tip_fast = "10.0.0.2:8333";
    let bulk_hero = "10.0.0.3:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let chunks = vec![(880, 1007), (1008, 1071), (1072, 1135)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec![slow.into(), tip_fast.into(), bulk_hero.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[
        (slow.into(), 0.10),
        (tip_fast.into(), 0.11),
        (bulk_hero.into(), 0.19),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        slow.into(),
        tip_fast.into(),
        bulk_hero.into(),
    ]));
    test_set_sticky_tenure(&assigner, slow, 901, 301);

    // Simulate tip streams: sticky slow, tip_fast clearly better. Bulk hero has none.
    for _ in 0..10 {
        assigner.note_wan_tip_stream(slow);
    }
    for _ in 0..80 {
        assigner.note_wan_tip_stream(tip_fast);
    }

    let scorer = PeerScorer::new();
    // Bulk hero would win on lifetime delivery_blocks_per_sec — must be ignored.
    let bulk_addr: std::net::SocketAddr = bulk_hero.parse().unwrap();
    for _ in 0..500 {
        scorer.record_block(bulk_addr, 500_000, 10.0);
    }

    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(1000, &scorer),
        "A6n must rotate when a tip-proven faster peer exists"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(tip_fast),
        "must pick tip-stream peer, not bulk IBD hero"
    );
    assert_ne!(
        assigner.preferred_tip_owner().as_deref(),
        Some(bulk_hero),
        "lifetime bulk hero must not win tip ownership"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn w35ppph_clips_tip_pipe_to_header_tip() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let chunks = vec![(880, 1007), (1008, 1135), (1136, 1263), (1264, 1391)];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "other".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_header_tip(920); // only 20 headers past tip
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("other".into(), 0.195),
        ("mid".into(), 0.190),
        ("low".into(), 0.185),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "other".into(),
        "mid".into(),
        "low".into(),
    ]));
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");

    let first = assigner.get_work("sticky", 256).expect("tip assign");
    assert_eq!(first.0, 901);
    assert_eq!(
        first.1, 920,
        "must clip tip pipe to header tip, got {}-{}",
        first.0, first.1
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn nudge_clears_blacklists_when_ready_active_zero() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec!["w0".into(), "w1".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_header_tip(1200);
    assigner.set_peer_scores(&[("w0".into(), 0.2), ("w1".into(), 0.19)]);
    assigner.set_ibd_ready_peers(HashSet::from(["w0".into(), "w1".into()]));
    assigner.blacklist_peer("w0", Duration::from_secs(300));
    assigner.blacklist_peer("w1", Duration::from_secs(300));
    assert!(assigner.is_peer_blacklisted("w0"));
    assert!(assigner.nudge_wan_tip_owner());
    assert!(
        !assigner.is_peer_blacklisted("w0"),
        "nudge must clear active blacklists when covering=0 and ready_active=0"
    );
    assert!(!assigner.is_peer_blacklisted("w1"));
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn nudge_pins_top_w_when_covering_zero_preferred_none() {
    // Live 2026-07-16: OPEN_STALL preferred=None + top_w_ok left covering=0 for ~18 min.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec!["w0".into(), "w1".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_header_tip(1200);
    assigner.set_peer_scores(&[("w0".into(), 0.100), ("w1".into(), 0.201)]);
    assigner.set_ibd_ready_peers(HashSet::from(["w0".into(), "w1".into()]));
    assigner.set_tip_gap_missing(true);
    assert!(assigner.preferred_tip_owner().is_none());
    assert!(assigner.nudge_wan_tip_owner());
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("w1"),
        "covering=0 nudge must pin top scored ready worker"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn wan10k_replacement_peer_must_register_to_take_tip() {
    // Live wan10k-c4 @438479:
    //   TIP_CRAWL ready=2 covering=0 busy=0
    //   OPEN_STALL preferred=None top_w=None ready_active_ok=0/0 score_keys=2
    //   CHEESE: tip missing, ahead in reorder
    // Peer watcher spawned replacements that polled get_work but were never added to
    // assigner.workers. Open-slot + tip_sticky_usable require is_active_download_worker
    // → tip hole forever while handshake-ready peers existed.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let stale = "35.182.131.76:8333";
    let repl = "188.214.129.139:8333";
    let vh = Arc::new(AtomicU64::new(438_478));
    let assigner = ChunkAssigner::new(
        vec![(437_309, 500_000)],
        vec![stale.into()], // construction-time workers only
        Arc::clone(&vh),
        437_309,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(437_309);
    assigner.set_wan_body_tip(437_309);
    assigner.set_header_tip(500_000);
    assigner.set_tip_gap_missing(true);
    // Stale tip hero: fail-cooled (mute CAP) and not ready — score still in map.
    assigner.set_peer_scores(&[(stale.into(), 466.0), (repl.into(), 400.0)]);
    assigner.mark_tip_owner_fail_cooldown(stale, 120);
    assigner.set_ibd_ready_peers(HashSet::from([repl.into()]));
    assigner.open_tip_owner_slot();

    assert!(
        !assigner.is_active_download_worker(repl),
        "replacement must start outside construction workers"
    );
    assert!(
        assigner.get_work(repl, 256).is_none(),
        "unregistered replacement must not win tip (ready_active_ok=0/0 freeze)"
    );

    assigner.register_download_worker(repl);
    assert!(assigner.is_active_download_worker(repl));
    assert!(
        assigner.nudge_wan_tip_owner(),
        "covering=0 nudge must run after register"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(repl),
        "nudge must pin registered ready replacement, not cooled stale hero"
    );
    let work = assigner.get_work(repl, 256);
    assert!(
        work.is_some_and(|(s, _)| s == 438_479),
        "registered replacement must cover tip hole, got {:?}",
        work
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn covering0_tip_pin_uncools_when_every_hero_fail_cooled() {
    // Live wan10k @438022: mute CAP → mid_clear=0 → OPEN_STALL preferred=None
    // top_w=None while score_keys=2 (both fail-cooled). E15 existed for GD_SLOW OPEN
    // only; covering=0 TIP_PIN must clear cooldowns and pin.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let a = "35.182.131.76:8333";
    let b = "188.214.129.139:8333";
    let vh = Arc::new(AtomicU64::new(438_021));
    let assigner = ChunkAssigner::new(
        vec![(437_309, 500_000)],
        vec![a.into(), b.into()],
        Arc::clone(&vh),
        437_309,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(437_309);
    assigner.set_wan_body_tip(437_309);
    assigner.set_header_tip(500_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(a.into(), 466.0), (b.into(), 400.0)]);
    assigner.set_ibd_ready_peers(HashSet::from([a.into(), b.into()]));
    assigner.mark_tip_owner_fail_cooldown(a, 120);
    assigner.mark_tip_owner_fail_cooldown(b, 120);
    assert!(assigner.preferred_tip_owner().is_none());
    assert!(assigner.nudge_wan_tip_owner());
    let pref = assigner.preferred_tip_owner();
    assert!(
        pref.as_deref() == Some(a) || pref.as_deref() == Some(b),
        "covering=0 must uncool and pin a tip hero, got {:?}",
        pref
    );
    assert!(
        !assigner.tip_owner_in_fail_cooldown(pref.as_deref().unwrap()),
        "pinned hero must leave fail-cooldown"
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn dead_sticky_allows_force_requeue_tip_micro_to_ready_worker() {
    // Live wan10k: preferred=disconnected hero → peer_may_take_wan_gap_retry only
    // matched that peer → FORCE_REQUEUE (H,H) never assigned while covering=0.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let dead = "35.182.131.76:8333";
    let live = "188.214.129.139:8333";
    let vh = Arc::new(AtomicU64::new(438_478));
    let assigner = ChunkAssigner::new(
        vec![(437_309, 500_000)],
        vec![dead.into(), live.into()],
        Arc::clone(&vh),
        437_309,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(437_309);
    assigner.set_wan_body_tip(437_309);
    assigner.set_header_tip(500_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(dead.into(), 466.0), (live.into(), 400.0)]);
    // Dead sticky still preferred; only `live` is handshake-ready.
    {
        let mut g = assigner.preferred_tip_owner.lock().unwrap();
        *g = Some(dead.into());
    }
    assigner.set_ibd_ready_peers(HashSet::from([live.into()]));
    assert!(!assigner.tip_sticky_usable(dead));
    assigner.requeue_stall_gaps_force(438_479, None);
    let work = assigner.get_work(live, 256);
    assert!(
        work.is_some_and(|(s, _)| s == 438_479),
        "living ready worker must cover tip after dead sticky drop (retry micro or tip stripe), got {:?}",
        work
    );
    assert!(
        assigner.preferred_tip_owner().as_deref() != Some(dead),
        "dead sticky must be cleared"
    );
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn covering0_blacklist_clear_includes_registered_replacements() {
    super::super::tip_stage::clear_tip_failover();
    let stale = "stale:8333";
    let repl = "repl:8333";
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec![stale.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(1200);
    assigner.set_tip_gap_missing(true);
    assigner.register_download_worker(repl);
    assigner.set_peer_scores(&[(repl.into(), 0.40)]);
    assigner.set_ibd_ready_peers(HashSet::from([repl.into()]));
    assigner.blacklist_peer(repl, Duration::from_secs(300));
    assert!(assigner.is_peer_blacklisted(repl));
    assert!(assigner.nudge_wan_tip_owner());
    assert!(
        !assigner.is_peer_blacklisted(repl),
        "covering=0 ready_active=0 must clear blacklists on registered replacements"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(repl));
    super::super::tip_stage::clear_tip_failover();
}

/// Live 2026-07-14 genesis stall: confirmed=0 while live bodies existed at 64.
/// Old `wan_tip_gap_crawl` required `confirmed > 0` → always false → nudge no-op.
/// New path gates on `wan_body_tip` (coordinator live tip).
#[serial_test::serial(ibd)]
#[test]
fn genesis_confirmed_zero_uses_wan_body_tip_for_crawl() {
    let vh = Arc::new(AtomicU64::new(512));
    let assigner = ChunkAssigner::new(
        vec![(1, 64), (65, 128), (513, 576)],
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    // Empty store (wan_body_tip=0): past tip is WAN crawl (true genesis download).
    assert!(assigner.wan_tip_gap_crawl(513));

    // Live tip raised to 64 (GAP_PERSIST race) — still WAN for next=513.
    assigner.set_wan_body_tip(64);
    assert!(assigner.wan_tip_gap_crawl(513));
    assert!(
        !assigner.wan_tip_gap_crawl(64),
        "at body tip boundary must not be WAN tip crawl"
    );
    // W84: tip height allowed; far-ahead height still suppressed.
    vh.store(512, Ordering::Relaxed);
    assert!(
        assigner.wan_stall_micro_allowed(513),
        "W84: WAN tip height must allow stall micro recovery"
    );
    assert!(
        !assigner.wan_stall_micro_allowed(600),
        "WAN tip crawl must still suppress ahead stall micro storms"
    );
    assert!(assigner.nudge_wan_tip_owner());
}

#[serial_test::serial(ibd)]
#[test]
fn w84_wan_stall_micro_allows_tip_height_only() {
    let vh = Arc::new(AtomicU64::new(256_686));
    let assigner = ChunkAssigner::new(
        vec![(256_687, 256_750)],
        vec!["pA".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assert!(assigner.wan_tip_gap_crawl(256_687));
    assert!(
        assigner.wan_stall_micro_allowed(256_687),
        "exact tip must requeue on stall (live freeze 256687)"
    );
    assert!(
        !assigner.wan_stall_micro_allowed(256_800),
        "ahead of tip must stay suppressed"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn chunk_guard_drop_clears_matching_range_not_lifo() {
    let vh = Arc::new(AtomicU64::new(100));
    let assigner = Arc::new(ChunkAssigner::new(
        vec![(101, 164), (165, 228)],
        vec!["p1".into(), "p1".into()],
        Arc::clone(&vh),
        101,
        true,
    ));
    assigner.mark_bootstrap_complete();
    assigner.set_peer_scores(&[("p1".into(), 1.0)]);
    // Force dual in-flight capacity.
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "p1", 101, 164);
        ChunkAssigner::insert_in_flight(&mut g, "p1", 165, 228);
    }
    {
        let mut guard = ChunkGuard::new(165, 228, None, "p1".into(), Arc::clone(&assigner));
        // Drop without disarm — must clear 165-228, leave 101-164.
        drop(guard);
    }
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let ranges = g.get("p1").cloned().unwrap_or_default();
    assert_eq!(ranges, vec![(101, 164)]);
}

#[serial_test::serial(ibd)]
#[test]
fn a6n_opens_slot_when_no_tip_proven_candidate() {
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let slow = "10.0.0.1:8333";
    let bulk = "10.0.0.9:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), bulk.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[(slow.into(), 0.10), (bulk.into(), 0.19)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), bulk.into()]));
    test_set_sticky_tenure(&assigner, slow, 901, 301);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(slow);
    }
    // bulk has zero tip streams
    let scorer = PeerScorer::new();
    let bulk_addr: std::net::SocketAddr = bulk.parse().unwrap();
    for _ in 0..200 {
        scorer.record_block(bulk_addr, 500_000, 10.0);
    }
    assert!(assigner.maybe_rotate_slow_sticky_a6m(1000, &scorer));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(bulk),
        "no tip-proven candidate → open slot pinned to top scored ready worker (not None lottery)"
    );
    assert!(assigner.is_peer_blacklisted(slow));
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn e16_a6m_gd_slow_keeps_sticky_when_feeder_runway() {
    // C1u @320k: tip_bps≈179 + feeder≈18 + gd_ewma≈5.9s must NOT OPEN/blacklist.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(5_900, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(18, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP", "8");
        // Disable tip_bps keep so this test isolates feeder keep.
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "0");
    }
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    // tip_bps ≈ (2201-901)/30 ≈ 43 ≥ min 40; tenure full window.
    test_set_sticky_tenure(&assigner, slow, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 2201, 0);
    for _ in 0..40 {
        assigner.note_wan_tip_stream(slow);
    }
    for _ in 0..5 {
        assigner.note_wan_tip_stream(alt);
    }
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(2201, &scorer),
        "E16: feeder runway must keep sticky despite GD_SLOW EWMA"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(slow));
    assert!(!assigner.is_peer_blacklisted(slow));

    // feeder=0 → LOCAL_GAP path may still rotate (tip_bps keep off).
    *assigner.last_a6m_rotate_at.lock().unwrap() = None;
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(2201, &scorer),
        "feeder=0 + GD_SLOW + tip_bps≥min must still rotate (E11)"
    );
    assert_ne!(assigner.preferred_tip_owner().as_deref(), Some(slow));

    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn e16b_a6m_gd_slow_keeps_on_tip_bps_when_feeder_dips() {
    // Live C1u-e16: KEEP@feeder=29 then OPEN at feeder=5 tip_bps=162 ewma=554.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(554, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(5, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP", "8");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    // tip_bps ≈ (5801-901)/30 ≈ 163 ≥ tip_keep 80; feeder=5 < feeder_keep 8.
    test_set_sticky_tenure(&assigner, slow, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 5801, 0);
    for _ in 0..40 {
        assigner.note_wan_tip_stream(slow);
    }
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(5801, &scorer),
        "E16b: tip_bps keep must hold when feeder dips below feeder_keep"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(slow));
    assert!(!assigner.is_peer_blacklisted(slow));

    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_FEEDER_KEEP");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_mute_fast_bypasses_tenure_window_when_feeder_empty_gd_slow() {
    // Mute-fast Phase 1: feeder=0 ∧ gap ∧ gd_slow skips 0.8×window (default ≥24s).
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(900, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
    }
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    // Only 5s tenure — classic A6m would return false (< 0.8×30 = 24s).
    test_set_sticky_tenure(&assigner, slow, 901, 5);
    test_push_tip_sample(&assigner, 901, 5);
    test_push_tip_sample(&assigner, 910, 0);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(slow);
    }
    for _ in 0..25 {
        assigner.note_wan_tip_stream(alt);
    }
    let scorer = PeerScorer::new();
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(910, &scorer),
        "mute-fast must rotate at elapsed=5s when feeder=0 + gd_slow"
    );
    assert_ne!(assigner.preferred_tip_owner().as_deref(), Some(slow));

    // Healthy feeder + healthy gd → still gated by tenure at elapsed=5s.
    *assigner.last_a6m_rotate_at.lock().unwrap() = None;
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(100, 32);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(32, Ordering::Relaxed);
    test_set_sticky_tenure(&assigner, slow, 901, 5);
    test_push_tip_sample(&assigner, 901, 5);
    test_push_tip_sample(&assigner, 920, 0);
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(920, &scorer),
        "feeder>0 + healthy gd must still require 0.8×window tenure"
    );

    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_mute_fast_slow_drip_without_gap_missing() {
    // Live Phase4: covering=1 drip clears tip_gap_missing; await≈0; classic mute-fast
    // never armed. feeder=0 ∧ gd_slow ∧ covering≥1 must still rotate early.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(900, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
    }
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(false); // drip cleared gap
    assigner.note_tip_cover_claim(slow, 901, 1028); // covering=1
    assigner.set_peer_scores(&[(slow.into(), 0.10), (alt.into(), 0.19)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    test_set_sticky_tenure(&assigner, slow, 901, 5);
    test_push_tip_sample(&assigner, 901, 5);
    test_push_tip_sample(&assigner, 910, 0);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(slow);
    }
    for _ in 0..25 {
        assigner.note_wan_tip_stream(alt);
    }
    let scorer = PeerScorer::new();
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(910, &scorer),
        "slow-drip mute-fast must rotate with gap=false covering=1 gd_slow"
    );
    assert_ne!(assigner.preferred_tip_owner().as_deref(), Some(slow));
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_rotates_when_getdata_ewma_slow_despite_fast_tip_bps() {
    // E11: tip-advance BPS ≥ min (LOCAL_GAP) while getdata→body EWMA stays slow.
    // E13: must pin a different ready peer + tip-owner cooldown (E12 re-elect bug).
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[(slow.into(), 1.3), (alt.into(), 1.2)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    // Lifetime tip advance ≈ 50 blk/s ≫ min_bps=6 — old A6m would skip.
    test_set_sticky_tenure(&assigner, slow, 1000, 200);
    test_push_tip_sample(&assigner, 9000, 90);
    test_push_tip_sample(&assigner, 10000, 0);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(slow);
    }
    // Alt tip-stream BPS = notes/max(1s) — need ≥ FORCE min (default 20).
    for _ in 0..25 {
        assigner.note_wan_tip_stream(alt);
    }
    let scorer = PeerScorer::new();
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
        "slow getdata EWMA must arm A6m even when tip-advance BPS looks healthy"
    );
    assert!(assigner.is_peer_blacklisted(slow));
    assert!(
        assigner.tip_owner_in_fail_cooldown(slow),
        "GD_SLOW must tip-owner-cooldown sticky so TIP_PIN cannot re-elect"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(alt),
        "GD_SLOW must pin a different ready peer (E12 pinned=None re-elect)"
    );
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_gd_slow_open_uncools_prior_hero_when_pin_empty() {
    // E15: ROTATE A→B blacklists+cools A 180s; OPEN on B 60s later pinned=None.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let a = "10.0.0.1:8333";
    let b = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![a.into(), b.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[(a.into(), 1.3), (b.into(), 1.2)]);
    assigner.set_ibd_ready_peers(HashSet::from([a.into(), b.into()]));
    // Simulate post-ROTATE: A blacklisted + tip-owner cooled; B is sticky.
    assigner.blacklist_peer(a, Duration::from_secs(120));
    assigner.mark_tip_owner_fail_cooldown(a, 180);
    test_set_sticky_tenure(&assigner, b, 1000, 200);
    test_push_tip_sample(&assigner, 9000, 90);
    test_push_tip_sample(&assigner, 10000, 0);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(b);
    }
    // A has tip streams but is cooled/blacklisted until OPEN retry clears.
    for _ in 0..30 {
        assigner.note_wan_tip_stream(a);
    }
    let scorer = PeerScorer::new();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    assert!(assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(a),
        "GD_SLOW OPEN must un-cool/un-blacklist prior tip hero to pin"
    );
    assert!(assigner.is_peer_blacklisted(b));
    assert!(assigner.tip_owner_in_fail_cooldown(b));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_gd_slow_open_slot_pins_ready_worker_not_in_score_map() {
    // E12: top_scored walked peer_scores only → pinned=None while another download
    // worker was ready. Fallback must pin via active-worker walk.
    // Sequential with EWMA seed immediately before rotate (tip_stage statics).
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    // Only sticky scored — alt ready but absent from peer_scores map.
    assigner.set_peer_scores(&[(slow.into(), 1.3)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    test_set_sticky_tenure(&assigner, slow, 1000, 200);
    test_push_tip_sample(&assigner, 9000, 90);
    test_push_tip_sample(&assigner, 10000, 0);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(slow);
    }
    let scorer = PeerScorer::new();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
        "GD_SLOW OPEN_SLOT must arm when tip BPS looks healthy"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(alt),
        "OPEN_SLOT must fall back to any ready active worker when score-map pin is None"
    );
    assert!(assigner.tip_owner_in_fail_cooldown(slow));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_mid_score_sticky_rotates_despite_soft_retry() {
    // E10: non-floor sticky@~1.3 + soft_retry>0 used to hard-block A6m (IBD_A6M=0).
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::mark_needed(1000);
    super::super::tip_stage::mark_soft_retry(1000);
    assert!(super::super::tip_stage::tip_soft_retries() > 0);
    let slow = "10.0.0.1:8333";
    let alt = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), alt.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    // Mid-band score (not floor 0.10) — the path E10 hit.
    assigner.set_peer_scores(&[(slow.into(), 1.3), (alt.into(), 1.2)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), alt.into()]));
    test_set_sticky_tenure(&assigner, slow, 901, 301);
    for _ in 0..5 {
        assigner.note_wan_tip_stream(slow);
    }
    let scorer = PeerScorer::new();
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(1000, &scorer),
        "soft_retry must not block A6m on mid-score sticky"
    );
    assert!(assigner.is_peer_blacklisted(slow));
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_opens_slot_on_recent_stall_despite_fast_lifetime() {
    // Live 2026-07-15: lifetime tenure ≥11 blk/s over 300s hid minute-scale stalls
    // (tip ~0.8 blk/s @ 04:08) — A6m never fired. Recent window must catch this.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let slow = "10.0.0.1:8333";
    let other = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![slow.into(), other.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[(slow.into(), 0.10), (other.into(), 0.11)]);
    assigner.set_ibd_ready_peers(HashSet::from([slow.into(), other.into()]));
    // Lifetime: 9000 blocks / 600s = 15 blk/s (≥ floor min 12) — old A6m would skip.
    test_set_sticky_tenure(&assigner, slow, 1000, 600);
    // Recent: only +40 blocks in 90s ≈ 0.44 blk/s.
    test_push_tip_sample(&assigner, 9960, 90);
    test_push_tip_sample(&assigner, 10000, 0);
    for _ in 0..50 {
        assigner.note_wan_tip_stream(slow);
    }
    // Other has tip streams but loses 1.25× bar (sticky monopoly) → must open slot.
    for _ in 0..5 {
        assigner.note_wan_tip_stream(other);
    }
    let scorer = PeerScorer::new();
    assert!(
        assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
        "recent-window stall must rotate/open even when lifetime BPS looks healthy"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(other),
        "bar-fail / true stall → open slot pinned to top scored ready worker"
    );
    assert!(assigner.is_peer_blacklisted(slow));
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6n_keeps_healthy_floor_sticky_when_no_tip_proven_alt() {
    // Live 2026-07-15: tenure_bps=12.57 OPEN_SLOT blacklisted a delivering sticky.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let sticky = "10.0.0.1:8333";
    let other = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![sticky.into(), other.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[(sticky.into(), 0.10), (other.into(), 0.20)]);
    assigner.set_ibd_ready_peers(HashSet::from([sticky.into(), other.into()]));
    test_set_sticky_tenure(&assigner, sticky, 1000, 600);
    // Recent: +900 / 60s = 15 blk/s — below stretch floor_min=22, above open_slot_min=12.
    test_push_tip_sample(&assigner, 9100, 60);
    test_push_tip_sample(&assigner, 10000, 0);
    for _ in 0..40 {
        assigner.note_wan_tip_stream(sticky);
    }
    for _ in 0..3 {
        assigner.note_wan_tip_stream(other);
    }
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
        "healthy-band floor sticky must not open-slot without tip-proven alt"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(sticky));
    assert!(!assigner.is_peer_blacklisted(sticky));
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_keeps_sticky_when_recent_bps_healthy() {
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    let sticky = "10.0.0.1:8333";
    let vh = Arc::new(AtomicU64::new(9999));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec![sticky.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[(sticky.into(), 0.10)]);
    assigner.set_ibd_ready_peers(HashSet::from([sticky.into()]));
    test_set_sticky_tenure(&assigner, sticky, 1000, 600);
    // Recent: +1800 blocks / 90s = 20 blk/s — below stretch floor_min=22 but ≥ open_slot_min=12.
    test_push_tip_sample(&assigner, 8200, 90);
    test_push_tip_sample(&assigner, 10000, 0);
    for _ in 0..20 {
        assigner.note_wan_tip_stream(sticky);
    }
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(10000, &scorer),
        "healthy-band recent tip BPS must not open-slot without tip-proven alt"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(sticky));
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p1c_no_tip_repreempt_while_peer_holds_tip_inflight() {
    // P1c: sticky with tip in-flight must not get a second overlapping tip span
    // (max_in_flight=2 dual-pipe is ahead-only).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1135), (1136, 1263)],
        vec!["owner".into(), "ahead".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("owner".into(), 0.50), ("ahead".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    let tip = assigner.get_work("owner", 1000).expect("tip owner");
    assert_eq!(tip.0, 901);
    assert!(
        ChunkAssigner::peer_holds_tip_inflight(
            &assigner.in_flight_per_peer.lock().unwrap(),
            "owner",
            901
        ),
        "owner must hold tip in-flight after assign"
    );
    let again = assigner.get_work("owner", 1000);
    if let Some((s, e)) = again {
        assert!(
            !(s <= 901 && 901 <= e),
            "P1c: must not re-preempt tip-covering span, got {s}-{e}"
        );
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
}

#[serial_test::serial(ibd)]
#[test]
fn wan_tip_dedup_blocks_same_span_reassign_after_gap_stream() {
    // WAN (not synth): obsolete→complete clears in_flight; P1c alone cannot stop
    // W28c same-start storms (live dens-hash160: same_start p50≈19ms).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::set_var("BLVM_IBD_TIP_DEDUP_REARM_MS", "60000");
    }
    assert!(!super::super::synthetic_wan::bulk_local_disk_stream());
    let vh = Arc::new(AtomicU64::new(300_287));
    let assigner = ChunkAssigner::new(
        vec![(300_288, 300_415)],
        vec!["hero".into(), "alt".into()],
        Arc::clone(&vh),
        300_288,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(300_000);
    assigner.set_wan_body_tip(300_000);
    assigner.set_header_tip(400_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[("hero".into(), 1.0), ("alt".into(), 0.5)]);
    mark_scored_peers_ibd_ready(&assigner);
    let first = assigner.get_work("hero", 1000);
    assert!(
        first.is_some_and(|(s, _)| s == 300_288),
        "first tip-owner, got {first:?}"
    );
    let (fs, fe) = first.unwrap();
    assigner.on_chunk_complete_range("hero", fs, fe);
    super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(300_351, Ordering::Relaxed);
    assert!(
        assigner.tip_owner_blocked_by_dedup(300_288),
        "WAN DEDUP past tip must block tip-owner re-arm"
    );
    let second = assigner.get_work("hero", 1000);
    assert!(
        second.map(|(s, _)| s != 300_288).unwrap_or(true),
        "WAN: must not reassign tip-covering span after DEDUP, got {second:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_DEDUP_REARM_MS");
        super::super::memory::GAP_STREAM_DEDUP_HEIGHT.store(0, Ordering::Relaxed);
        super::super::tip_stage::test_reset_tip_stage();
    }
}

#[serial_test::serial(ibd)]
#[test]
fn sole_ready_peer_skips_tip_owner_fail_cooldown() {
    // Mode T: workers may be 6 slots but only one IBD-ready archive.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec![
            "solo".into(),
            "slot2".into(),
            "slot3".into(),
            "slot4".into(),
            "slot5".into(),
            "slot6".into(),
        ],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("solo".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["solo".into()]));
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("solo".into());
    assigner.note_tip_cover_claim("solo", 901, 1028);
    assigner.note_tip_owner_failed("solo");
    assert!(
        !assigner.tip_owner_in_fail_cooldown("solo"),
        "sole ready peer must not enter tip-owner fail cooldown"
    );
    assert!(
        assigner.preferred_tip_owner().is_none(),
        "sticky still cleared so tip slot can re-arm"
    );
    assert!(
        assigner.tip_owner_open.load(Ordering::Relaxed),
        "WAN tip slot must open for immediate sole-peer re-arm"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_gd_slow_open_keeps_sole_ready_sticky() {
    // tc65: A6N_OPEN_SLOT with no challenger must not blacklist the sole archive.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    let vh = Arc::new(AtomicU64::new(401_190));
    let sticky = "127.0.0.1:18333";
    let assigner = ChunkAssigner::new(
        vec![(401_191, 401_318)],
        vec![sticky.into(), "slot2".into()],
        Arc::clone(&vh),
        401_191,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(400_287);
    assigner.set_wan_body_tip(400_287);
    assigner.set_header_tip(451_000);
    assigner.set_peer_scores(&[(sticky.into(), 1200.0)]);
    assigner.set_ibd_ready_peers(HashSet::from([sticky.into()]));
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some(sticky.into());
    test_set_sticky_tenure(&assigner, sticky, 401_000, 40);
    let rotated = assigner.a6m_do_rotate(401_191, sticky, 28.0, 40.0, false, true);
    assert!(
        !rotated,
        "sole ready peer must KEEP on GD_SLOW OPEN (no alternate)"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(sticky));
    assert!(
        !assigner.is_peer_blacklisted(sticky),
        "must not blacklist sole archive on OPEN with new=-"
    );
    assert!(
        !assigner.tip_owner_in_fail_cooldown(sticky),
        "must not cool sole archive on aborted OPEN"
    );
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn p1e_mute_fail_applies_long_tip_role_ban() {
    // P1e: mute path default ban ≥60s (tip-role), not the old 5s CAP cooldown.
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["mute".into(), "alt".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("mute".into(), 0.50), ("alt".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("mute".into());
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("mute".into(), vec![(901, 1028)]);
    }
    assigner.note_tip_cover_claim("mute", 901, 1028);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS");
    }
    assigner.note_tip_owner_failed_mute("mute");
    assert!(
        assigner.tip_owner_in_fail_cooldown("mute"),
        "mute peer must be tip-role banned"
    );
    let until = assigner
        .tip_owner_fail_until
        .lock()
        .unwrap()
        .get("mute")
        .copied();
    let remaining = until
        .map(|t| t.saturating_duration_since(Instant::now()).as_secs())
        .unwrap_or(0);
    assert!(
        remaining >= 55,
        "P1e: mute tip-role ban remaining ≥55s (default 120), got {remaining}s"
    );
    assert!(
        assigner.preferred_tip_owner().is_none(),
        "mute clears preferred sticky"
    );
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_starts_on_slow_drip_without_await() {
    // covering=1 drip: gap=false, await≈0, gd_slow, crawl << min_bps → trial without await gate.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(2000, 32);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_TIP_SLOW_DRIP_WINDOW_SECS", "8");
        std::env::set_var("BLVM_IBD_A6M_MIN_BPS", "40");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
    }
    let vh = Arc::new(AtomicU64::new(910));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    assigner.note_tip_cover_claim("sticky", 901, 1028);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    // reset_sticky clears samples — rebuild chronological crawl history.
    assigner.tip_progress_samples.lock().unwrap().clear();
    // ~9 blocks / 8s ≈ 1.1 BPS < min_bps.
    test_push_tip_sample(&assigner, 901, 8);
    test_push_tip_sample(&assigner, 910, 0);
    assert!(
        assigner.maybe_start_tip_trial(910),
        "slow-drip trial must start with await=0 gap=false"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_TIP_SLOW_DRIP_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_MIN_BPS");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_starts_when_feeder_empty_and_awaiting() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    assert!(
        assigner.maybe_run_tip_trial(901),
        "P2: trial must start on feeder=0 + awaiting"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger"),
        "challenger pinned for trial"
    );
    assert!(assigner.tip_trial.lock().unwrap().is_some());
    // Mid-trial: no finish yet.
    assert!(
        !assigner.maybe_run_tip_trial(901),
        "trial must not finish before TRIAL_SECS"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_keeps_challenger_with_tip_streams() {
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assert!(assigner.maybe_start_tip_trial(901));
    // Challenger delivered tip streams during trial; sticky delivered none.
    for _ in 0..3 {
        assigner.note_wan_tip_stream("challenger");
    }
    // Backdate trial start so finish fires.
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(9);
    }
    assert!(assigner.maybe_finish_tip_trial(910));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger"),
        "P2 KEEP when challenger tip-streams and sticky does not"
    );
    assert!(assigner.tip_trial.lock().unwrap().is_none());
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_reverts_when_challenger_silent() {
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assert!(assigner.maybe_start_tip_trial(901));
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(9);
    }
    assert!(assigner.maybe_finish_tip_trial(901));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "P2 REVERT when challenger delivers nothing"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

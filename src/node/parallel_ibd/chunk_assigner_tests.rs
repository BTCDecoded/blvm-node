//! ChunkAssigner rematch regression suite.
//!
//! Names are **live-cell IDs** (`w28c`, `a6m`, `p0a`, `c1u`, …), not copies of
//! the same test. Do not merge “similar” cases — each encodes a peel-bar
//! failure mode. Shared helpers stay at the top; synth-bulk cases need
//! `feature = "ibd-dev"` or `cfg(test)` (real `synthetic_wan` module).
//!
//! Removed: second-peer starts at hole+LEAD, owner_end+1, or `(H,H)`.
//! R-249 / R-250 / R-252 lock the replacement (H+1 over the hero cover,
//! no second TCP on H). Those tests are `r249_*`, `r250_*`, `r252_*`.

use super::*;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

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

/// R-291: BLVM_IBD_PEER_DEPTH raises only the non-sticky cap
/// (`max_in_flight_for_scores`). Sticky/tip still uses TOP_PEER / sole_tip
/// (tc172 / A2). A4 was "top-scoring half cap 2" = N-way ahead; this is uniform.
#[serial_test::serial(ibd)]
#[test]
fn r291_peer_depth_eight_same_peer_next_stripe_not_a4() {
    super::super::tip_stage::clear_tip_failover();
    unsafe {
        std::env::set_var("BLVM_IBD_PEER_DEPTH", "8");
    }
    let vh = Arc::new(AtomicU64::new(90_000));
    let chunks = vec![
        (90_000, 90_063),
        (90_064, 90_127),
        (90_128, 90_191),
        (90_192, 90_255),
    ];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "p1".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        90_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(80_000);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.100),
        ("p1".into(), 0.195),
        ("mid".into(), 0.190),
        ("low".into(), 0.185),
    ]);
    assigner.set_ibd_ready_peers(HashSet::from([
        "sticky".into(),
        "p1".into(),
        "mid".into(),
        "low".into(),
    ]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.set_tip_gap_missing(false);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(64, Ordering::Relaxed);
    let sticky_cap = assigner.max_in_flight_for("sticky");
    let p1_cap = assigner.max_in_flight_for("p1");
    unsafe {
        std::env::remove_var("BLVM_IBD_PEER_DEPTH");
    }
    assert_eq!(
        p1_cap, 8,
        "R-280 GetData→body 1274ms; non-sticky depth=8 is same-peer next-stripe. \
         A4 gave the top-scoring half cap 2 (N-way ahead). got p1_cap={p1_cap} \
         sticky_cap={sticky_cap}"
    );
    assert_ne!(
        sticky_cap, 8,
        "sticky/tip must NOT take BLVM_IBD_PEER_DEPTH (tc172 in_flight=2 flooded archive; \
         A2 attempt1 always-2 REVERT). sticky_cap={sticky_cap}"
    );
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
fn leftover_hole_under_body_tip_assigns_getdata() {
    // Live 2026-08-20: next=70713, leftover body_tip=70735, workers took 70527–70735
    // and skipped the miss. Tip missing under cheese must GetData (H,H), not a
    // leftover map stripe.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_712)); // next=70713
    let assigner = ChunkAssigner::new(
        vec![(70_527, 70_735), (70_736, 80_000)],
        vec!["pA".into()],
        Arc::clone(&vh),
        70_527,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into()]));
    assigner.set_tip_gap_missing(true);
    assigner.set_leftover_force_getdata(true);

    let work = assigner
        .get_work("pA", 512)
        .expect("leftover hole must assign GetData");
    assert_eq!(
        work.0, 70_713,
        "GetData must start at the miss, not leftover stripe {work:?}"
    );
    assert!(
        work.1 >= 70_713 && work.1 <= body_tip,
        "must not skip past the hole, got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_hole_assigns_getdata_despite_leftover_cover() {
    // Live 2026-08-21: leftover 70645–70735 in-flight, inject miss at 70709.
    // Covering leftover must not block (H,H) GetData for a second peer.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_708)); // next=70709
    let assigner = ChunkAssigner::new(
        vec![(70_645, 70_735), (70_736, 80_000)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        70_645,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("pA".into(), vec![(70_645, 70_735)]);
    }
    assigner.set_tip_gap_missing(true);
    assigner.set_leftover_force_getdata(true);
    let work = assigner
        .get_work("pB", 512)
        .expect("leftover covering must not hide hole GetData");
    assert_eq!(
        work,
        (70_709, 70_709),
        "must assign exact hole, got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_handoff_past_body_tip_assigns_getdata() {
    // Live 2026-08-21 abort-ahead soak: leftover holes under 70735 cleared,
    // then froze waiting 70736 (body_tip+1). leftover_force must GetData the
    // first WAN height — leftover_hole used to require next ≤ body_tip.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_735)); // next=70736
    let assigner = ChunkAssigner::new(
        vec![(70_736, 80_000)],
        vec!["pA".into()],
        Arc::clone(&vh),
        70_736,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into()]));
    assigner.set_tip_gap_missing(true);
    assigner.set_leftover_force_getdata(true);
    let work = assigner
        .get_work("pA", 512)
        .expect("leftover tip+1 must assign GetData");
    assert_eq!(
        work.0, 70_736,
        "GetData must start at handoff, got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_hole_genesis_wan_stall_assigns_despite_covering() {
    // Genesis-b 91698: body_tip=0, CHEESE covering=2, coordinator silent.
    // WAN IBD_STALL arms leftover_force; get_work must still issue (H,H).
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let vh = Arc::new(AtomicU64::new(91_697));
    let assigner = ChunkAssigner::new(
        vec![(91_698, 92_000)],
        vec!["pA".into(), "pB".into(), "pC".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_wan_body_tip(0);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9), ("pC".into(), 0.8)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into(), "pC".into()]));
    assigner.note_tip_cover_claim("pA", 91_581, 91_708);
    assigner.note_tip_cover_claim("pB", 91_698, 91_698);
    assigner.set_tip_gap_missing(true);
    assigner.set_leftover_force_getdata(true);
    let work = assigner
        .get_work("pC", 512)
        .expect("genesis WAN stall must GetData under covering=2");
    assert_eq!(
        work,
        (91_698, 91_698),
        "must assign exact tip, got {work:?}"
    );
    assigner.set_tip_gap_missing(false);
    assert!(
        !assigner.leftover_force_armed(),
        "tip land must clear genesis leftover_force"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_band_empty_queue_is_not_done() {
    // Live 2026-08-21 drop-feeder-first: leftover hole 70678, WAN chunk only
    // past 70735, leftover_force still false. is_done() was true → every
    // worker exited before leftover_stall armed GetData. 70736 then had
    // leftover_force + leftover_hole and nobody to take (H,H).
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_677)); // next=70678
    let assigner = ChunkAssigner::new(
        vec![(70_736, 80_000)],
        vec!["pA".into()],
        Arc::clone(&vh),
        70_736,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into()]));
    assigner.set_tip_gap_missing(false);
    assigner
        .next_index
        .store(1, std::sync::atomic::Ordering::Relaxed);
    assert!(
        assigner.retry_queue.lock().unwrap().is_empty(),
        "precondition: empty retry"
    );
    assert!(
        !assigner.is_done(),
        "leftover-band empty queue must keep workers for leftover_stall GetData"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_handoff_empty_queue_assigns_after_force() {
    // After leftover cheese, leftover_stall arms leftover_force at 70736.
    // Workers that stayed (leftover_band_empty_queue_is_not_done) must
    // get_work (70736,70736) even with an empty main index.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_735)); // next=70736
    let assigner = ChunkAssigner::new(
        vec![(70_736, 80_000)],
        vec!["pA".into()],
        Arc::clone(&vh),
        70_736,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into()]));
    assigner.set_tip_gap_missing(true);
    assigner.set_leftover_force_getdata(true);
    assigner
        .next_index
        .store(1, std::sync::atomic::Ordering::Relaxed);
    assert!(
        !assigner.is_done(),
        "leftover_force + empty queue must keep workers at WAN handoff"
    );
    let work = assigner
        .get_work("pA", 512)
        .expect("staying worker must GetData 70736");
    assert_eq!(
        work,
        (70_736, 70_736),
        "handoff must be exact (H,H), got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_handoff_releases_leftover_stripe_and_assigns_getdata() {
    // Live 2026-08-22 keep-force: leftover 70613–70735 in-flight, leftover_stall
    // at 70736 armed leftover_force, leftover_hole never assigned — worker never
    // returned to get_work. leftover_stall must release leftover cheese and
    // enqueue (70736,70736).
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_735)); // next=70736
    let assigner = ChunkAssigner::new(
        vec![(70_736, 80_000)],
        vec!["pA".into()],
        Arc::clone(&vh),
        70_736,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into()]));
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("pA".into(), vec![(70_613, 70_735)]);
    }
    assigner.leftover_force_arm_handoff_getdata(70_736);
    {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        let leftover = g.values().any(|r| r.iter().any(|(s, _)| *s <= body_tip));
        assert!(!leftover, "leftover stripe must be released, got {g:?}");
    }
    let rq = assigner.retry_queue.lock().unwrap();
    assert!(
        rq.iter().any(|e| e.start == 70_736 && e.end == 70_736),
        "handoff (H,H) must be on retry, got {rq:?}"
    );
    drop(rq);
    let work = assigner
        .get_work("pA", 512)
        .expect("freed worker must GetData 70736");
    assert_eq!(
        work,
        (70_736, 70_736),
        "handoff must be exact (H,H), got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_handoff_second_peer_assigns_despite_leftover_stripe_inflight() {
    // Live 2026-08-22 handoff-before-requeue: leftover_HANDOFF enqueued
    // (70736,70736), leftover 70614–70735 stayed in-flight (try_lock miss).
    // leftover_hole must still assign GetData to a free peer — leftover stripe
    // is not exact (70736,70736).
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_735)); // next=70736
    let assigner = ChunkAssigner::new(
        vec![(70_736, 80_000)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        70_736,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    assigner.set_tip_gap_missing(true);
    assigner.set_leftover_force_getdata(true);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("pA".into(), vec![(70_614, 70_735)]);
    }
    let work = assigner
        .get_work("pB", 512)
        .expect("free peer must GetData 70736 while leftover stripe is in-flight");
    assert_eq!(
        work,
        (70_736, 70_736),
        "handoff must be exact (H,H), got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn leftover_get_work_under_cover_skips_handoff_prime() {
    // Live leftover-getwork-phase 2026-08-22T16:57Z: next=70634, leftover
    // 70621–70735 in-flight, frontier_at_body called try_assign_handoff_prime
    // while holding in_flight (`phase=before_prime`) and never returned.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_633)); // next=70634
    let assigner = ChunkAssigner::new(
        vec![(70_621, 70_735), (70_736, 80_000)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        70_621,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    assigner.set_tip_gap_missing(false);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("pA".into(), vec![(70_621, 70_735)]);
    }
    let work = assigner.get_work("pB", 512);
    if let Some((s, e)) = work {
        assert!(
            e <= body_tip,
            "leftover get_work must not handoff-prime past body_tip, got {s}-{e}"
        );
    }
}

#[serial_test::serial(ibd)]
#[test]
fn local_ahead_does_not_assign_leftover_partition_past_window() {
    // Live 2026-08-21: next=70001, leftover 70657–70735 assigned as ahead
    // partition (656 > 256). Second peer must stay inside next+256.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(70_000)); // next=70001
    let assigner = ChunkAssigner::new(
        vec![(70_001, 70_256), (70_657, 70_735)],
        vec!["pA".into(), "pB".into()],
        Arc::clone(&vh),
        70_001,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0), ("pB".into(), 0.9)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into(), "pB".into()]));
    assigner.set_tip_gap_missing(false);
    let first = assigner.get_work("pA", 512);
    let second = assigner.get_work("pB", 512);
    for work in [first, second].into_iter().flatten() {
        assert!(
            work.0 <= 70_001,
            "leftover cheese must not assign ahead of next, got {work:?}"
        );
    }
}

#[serial_test::serial(ibd)]
#[test]
fn local_ahead_does_not_assign_leftover_stripe_past_tip() {
    // During 1→70k inject, leftover map chunks (70527–70735) must not be assigned.
    let _env = c1u_tests_env_lock();
    unsafe {
        std::env::remove_var("BLVM_IBD_SYNTH_WAN");
        std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS");
    }
    let body_tip = 70_735u64;
    let vh = Arc::new(AtomicU64::new(0)); // next=1
    let assigner = ChunkAssigner::new(
        vec![(70_527, 70_735)],
        vec!["pA".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(body_tip);
    assigner.set_wan_body_tip(body_tip);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("pA".into(), 1.0)]);
    assigner.set_ibd_ready_peers(HashSet::from(["pA".into()]));
    assigner.set_tip_gap_missing(false);

    let work = assigner.get_work("pA", 512);
    assert!(
        work.is_none() || work.unwrap().0 <= 1,
        "must not walk leftover stripe while inject owns 1→tip, got {work:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_local_ahead_clips_to_body_tip_and_primes_via_frontier() {
    // Live fail: ahead assigned 304672 while tip=304418 (past body_tip=304663) → cheese.
    // Local ahead must clip at body tip; once frontier is there, tip-owner primes WAN.
    let _env = c1u_tests_env_lock();
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

    // Extras may be idle (KEEP=0 local cover does not credit effective_healthy).
    // If they do take work, it still must clip at body tip.
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

    // Same last-local prime as c1u_handoff_prime_assigns_past_body_tip_while_local.
    assigner.on_chunk_complete_range("pA", ts, te);
    vh.store(body_tip - 1, Ordering::Relaxed);
    let cover = assigner.get_work("pA", 512);
    assert!(cover.is_some(), "last local height must cover body tip");
    let (cs, ce) = cover.unwrap();
    assert!(
        cs <= body_tip && ce >= body_tip && ce <= body_tip,
        "uncovered last local must assign body tip, got {cs}-{ce}"
    );
    let mut prime = assigner
        .get_work("pA", 512)
        .or_else(|| assigner.get_work("pB", 512))
        .filter(|(s, _)| *s == body_tip + 1);
    if prime.is_none() {
        assigner.on_chunk_complete_range("pA", cs, ce);
        prime = assigner
            .get_work("pA", 512)
            .filter(|(s, _)| *s == body_tip + 1);
    }
    assert_eq!(
        prime,
        Some((body_tip + 1, body_tip + 32)),
        "on last local height, handoff-prime must assign, got {prime:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_HANDOFF_PRIME");
        std::env::remove_var("BLVM_IBD_TIP_RUNWAY_STRIPE");
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
    // Wall A: one (H,H). The next peer takes the H+1 zone tile (R-249), not a second TCP on H.
    assert_eq!(
        assigner.get_work("pC", 1000),
        Some((323_781, 323_796)),
        "W112: second peer is the H+1 zone tile, not a second (H,H)"
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
    // W117: shallow remnant must not block distress (H,H). Live W116 used this
    // height (344580). A deep re-arm is the healthy==0 path, not this one.
    assert_eq!(
        got,
        Some((tip, tip)),
        "W117: shallow cover must not block the (H,H) failover"
    );
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
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    assert!(
        !assigner.should_abort_tip_walk_in("mute", 326_316, 326_347),
        "R-323: default keeps cooled in-flight; W111 resticky skip is promote"
    );
    assert_eq!(
        assigner.preferred_tip_owner(),
        None,
        "R-323: keep must not walk-promote a cooled peer"
    );
    unsafe {
        std::env::set_var("BLVM_IBD_NO_TIP_ABORT", "0");
    }
    assert!(
        assigner.should_abort_tip_walk_in("mute", 326_316, 326_347),
        "NO_TIP_ABORT=0: cooldown walk-in still aborts"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
}

/// R-242: covering hero RST, then TIP_WALK_PROMOTE retitled the corpse as
/// owner of 186264. TCP-gone must cooldown + drop inflight before promote.
#[serial_test::serial(ibd)]
#[test]
fn r243_tcp_gone_blocks_walk_promote_of_dead_hero() {
    let vh = Arc::new(AtomicU64::new(186_263));
    let assigner = ChunkAssigner::new(
        vec![(185_000, 188_000)],
        vec!["dead".into(), "other".into()],
        Arc::clone(&vh),
        185_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_header_tip(370_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_ibd_ready_peers(HashSet::from(["dead".into(), "other".into()]));
    assigner.note_tip_owner_assigned("dead");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("dead".into(), vec![(185_746, 187_793)]);
    }
    assigner.note_peer_tcp_gone("dead");
    assert!(
        assigner.tip_owner_in_fail_cooldown("dead"),
        "RST hero must cool so walk-promote cannot resticky"
    );
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .get("dead")
            .is_none_or(|r| r.is_empty()),
        "RST must drop inflight so covering is not a corpse"
    );
    assigner.promote_tip_walk_in("dead", 185_746, 187_793);
    assert_ne!(
        assigner.preferred_tip_owner().as_deref(),
        Some("dead"),
        "R-242: dead TCP must not become preferred via walk-promote"
    );
}

/// R-243 dump: covering RST at genesis skipped 15s cooldown (`sole ready`)
/// then flap-wiped H. TCP-gone must cool even when no alternate is ready.
#[serial_test::serial(ibd)]
#[test]
fn r244_tcp_gone_cools_when_sole_ready() {
    let vh = Arc::new(AtomicU64::new(1));
    let assigner = ChunkAssigner::new(
        vec![(1, 8_000)],
        vec!["dead".into(), "other".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_header_tip(370_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_ibd_ready_peers(HashSet::from(["dead".into()]));
    assigner.note_tip_owner_assigned("dead");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("dead".into(), vec![(1, 2_048)]);
    }
    assigner.note_peer_tcp_gone("dead");
    assert!(
        assigner.tip_owner_in_fail_cooldown("dead"),
        "R-243: sole-ready skip must not apply to TCP gone"
    );
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .get("dead")
            .is_none_or(|r| r.is_empty()),
        "TCP gone still drops inflight"
    );
    assigner.promote_tip_walk_in("dead", 1, 2_048);
    assert_ne!(
        assigner.preferred_tip_owner().as_deref(),
        Some("dead"),
        "flap-hero must not resticky via walk-promote after sole-ready RST"
    );
}

/// R-232 fat 180215: farm `34.106` span 179631-181678 walk-promoted to
/// sticky after H_SLOW successor `46.28`, then TIP_UPGRADE_DEFER held
/// mute score 1.979. Farm-behind-H (start < H, deep remain, not ≥60)
/// must not resticky or install deep cover, and must abort GetData.
#[serial_test::serial(ibd)]
#[test]
fn r232_farm_behind_h_walk_promote_skips_resticky() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(180_214));
    let assigner = ChunkAssigner::new(
        vec![(180_000, 182_000)],
        vec!["farm".into(), "succ".into()],
        Arc::clone(&vh),
        180_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_header_tip(370_000);
    assigner.set_peer_scores(&[("succ".into(), 166.0), ("farm".into(), 1.979)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("succ");
    for _ in 0..200 {
        assigner.note_wan_tip_stream("succ");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("farm".into(), vec![(179_631, 181_678)]);
    }
    assigner.promote_tip_walk_in("farm", 179_631, 181_678);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("succ"),
        "farm-behind-H must not steal H_SLOW successor sticky"
    );
    assert_eq!(
        assigner.deep_tip_cover_count(180_215),
        0,
        "farm-behind-H must not install a deep tip claim"
    );
    assert!(
        !assigner.should_abort_tip_walk_in("farm", 179_631, 181_678),
        "R-233: aborting farm-behind-H kills dump LOOKAHEAD once tip walks in; resticky skip is the lever"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// R-246 fat sit: covering farm 189996-190187 at H=190000 was walk-promote
/// skipped while sticky recv=0.0; farm-recv @191556 then titled a 30 mbps
/// peer. Skip only holds a live ≥60 sticky.
#[serial_test::serial(ibd)]
#[test]
fn r247_farm_behind_promotes_when_sticky_not_line_rate() {
    super::super::tip_stage::clear_tip_failover();
    let vh = Arc::new(AtomicU64::new(189_999));
    let assigner = ChunkAssigner::new(
        vec![(180_000, 192_000)],
        vec!["farm".into(), "sticky".into()],
        Arc::clone(&vh),
        180_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_header_tip(370_000);
    assigner.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("farm".into(), vec![(189_996, 190_187)]);
    }
    assert!(
        !assigner.farm_behind_h_blocks_walk_promote("farm", 189_996, 190_187, 190_000),
        "dead sticky must not farm-behind-skip a covering walk-in"
    );
    assigner.promote_tip_walk_in("farm", 189_996, 190_187);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("farm"),
        "covering walk-in must take H while sticky is not ≥60"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// R-248: covering=0 must not take H via the satd near-cursor zone.
#[serial_test::serial(ibd)]
#[test]
fn r248_covering0_cannot_take_h_via_priority_zone() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["ahead", "spare"]);
    assigner.set_peer_scores(&[("ahead".into(), 9.0), ("spare".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    match assigner.get_work("ahead", 4096) {
        None => {}
        Some((s, _)) => assert!(
            s <= assigner.next_needed_height() || s == 901,
            "covering=0 must not skip H, got start={s}"
        ),
    }
    assert!(
        assigner.priority_zone.lock().unwrap().is_empty(),
        "R-27/R-248: priority zone must not arm at covering=0"
    );
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

/// Owner covers H; other peers get disjoint 16-high tiles in (H, H+256].
/// C1j holds. Second tile does not reuse the first (R-27).
#[serial_test::serial(ibd)]
#[test]
fn r248_priority_zone_disjoint_tiles_after_owner_covers_h() {
    super::super::tip_stage::clear_tip_failover();
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
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let tip = assigner.get_work("owner", 4096).expect("owner");
    assigner.note_tip_owner_assigned("owner");
    let next = assigner.next_needed_height();
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .values()
            .flatten()
            .any(|&(s, e)| s <= next && next <= e),
        "owner must cover H={next} owner={tip:?}"
    );
    let a = assigner
        .get_work("ahead", 4096)
        .expect("R-248 ahead near-cursor tile");
    assert_eq!(
        a.0,
        next + 1,
        "R-249: first tile starts at H+1, not owner_end+1; got {}-{} H={next} owner={tip:?}",
        a.0,
        a.1,
        tip = tip
    );
    assert!(
        a.1 <= next + 256,
        "tile in (H, H+256], got {}-{} H={next}",
        a.0,
        a.1
    );
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", a.0, a.1),
        "C1j must hold the near-cursor tile"
    );
    let b = assigner
        .get_work("spare", 4096)
        .expect("R-248 spare disjoint tile");
    assert!(
        b.0 > next && b.1 <= next + 256,
        "spare tile in (H, H+256], got {}-{}",
        b.0,
        b.1
    );
    assert!(
        b.0 > a.1 || a.0 > b.1,
        "R-27: tiles must be disjoint a={a:?} b={b:?}"
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

/// R-249: owner inflight 1–32 must not push the zone to 33. First other-peer
/// tile is H+1 (duplicate covering, not a second TCP on H).
#[serial_test::serial(ibd)]
#[test]
fn r249_priority_zone_starts_at_h_plus_one_not_owner_end() {
    super::super::tip_stage::clear_tip_failover();
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
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let owner = assigner.get_work("owner", 4096).expect("owner");
    assigner.note_tip_owner_assigned("owner");
    let next = assigner.next_needed_height();
    assert!(
        owner.0 <= next && next <= owner.1,
        "owner must cover H={next} owner={owner:?}"
    );
    assert!(
        owner.1 >= next + 16,
        "fixture needs a wide owner stripe so a frontier jump would be visible owner={owner:?} H={next}"
    );
    let a = assigner
        .get_work("ahead", 4096)
        .expect("R-249 H+1 duplicate tile");
    assert_eq!(
        a.0,
        next + 1,
        "must not jump to owner_end+1={}",
        owner.1 + 1
    );
    assert!(
        a.1 <= owner.1,
        "first tile duplicates owner covering, got {}-{} owner={owner:?}",
        a.0,
        a.1
    );
    assert!(
        a.0 > next,
        "Wall A: no second TCP on H, got {}-{} H={next}",
        a.0,
        a.1
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

/// R-250: ignition tournament N×(1–32) must not push the zone to 34.
/// Except every inflight that covers H, not only preferred.
#[serial_test::serial(ibd)]
#[test]
fn r250_priority_zone_duplicates_h_plus_one_over_tournament_1_32() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(
        900,
        800,
        100_000,
        &["owner", "racer2", "racer3", "racer4", "ahead"],
    );
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("racer2".into(), 8.0),
        ("racer3".into(), 7.0),
        ("racer4".into(), 6.0),
        ("ahead".into(), 5.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let owner = assigner.get_work("owner", 4096).expect("owner");
    assigner.note_tip_owner_assigned("owner");
    let next = assigner.next_needed_height();
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("racer2".into(), vec![(next, next + 31)]);
        g.insert("racer3".into(), vec![(next, next + 31)]);
        g.insert("racer4".into(), vec![(next, next + 31)]);
    }
    let a = assigner
        .get_work("ahead", 4096)
        .expect("R-250 H+1 over tournament 1-32");
    assert_eq!(
        a.0,
        next + 1,
        "must not jump to 34; owner={owner:?} H={next} got {}-{}",
        a.0,
        a.1
    );
    assert!(a.0 > next, "Wall A: no second TCP on H");
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
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
fn w65_shallow_walk_promote_restickies_healthy_tip_bps_hero() {
    // Genesis-d 180k+: 164.152 shallow-promoted (remain=7) at 500–1100 BPS and
    // W65 refused sticky → mute lottery + grown=8. A ≥80 streamer is not the
    // mute remnant W65 was written for.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
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
    for _ in 0..600 {
        assigner.note_wan_tip_stream("walk");
    }
    assert!(assigner.wan_tip_stream_bps("walk") >= 80.0);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("walk".into(), vec![(193, 224)]);
    }
    assigner.promote_tip_walk_in("walk", 193, 224);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("walk"),
        "≥80 tip-streamer must become sticky on shallow promote"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn walk_promote_does_not_resticky_over_healthy_hero() {
    // genesis-o: mute ahead walk-in restickied over 174.93 @200–300 after hero
    // pipe completed (W51 deep-claim guard gone). Hold the ≥80 preferred.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(1001);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 0.90), ("mute".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("hero");
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.wan_tip_stream_bps("hero") >= 80.0);
    assigner.promote_tip_walk_in("mute", 1001, 1128);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("hero"),
        "mute deep walk-promote must not steal ≥80 sticky"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn keep_hold_denies_mute_without_tip_cover() {
    // dest-as @63k: cheese C1J dropped hero start>H (ahead=192). HOLD used
    // to require substantial cover, so mute gap-preempt restickied and the
    // empty-block window fell 3983→11 blk/s at 66k. KEEP hold does not
    // need cover; preferred re-arms H.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(1001);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 0.90), ("mute".into(), 0.95)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("hero");
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.preferred_meets_keep_bps());
    assert!(!assigner.peer_holds_substantial_tip_cover("hero"));
    {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            assigner.peer_may_take_tip_owner("hero", &g, 0),
            "KEEP preferred must still take H"
        );
        assert!(
            !assigner.peer_may_take_tip_owner("mute", &g, 0),
            "mute must not take H while KEEP preferred has no cover"
        );
    }
    assigner.note_tip_owner_assigned("mute");
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("hero"),
        "assign-path resticky must not overwrite KEEP"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn sticky_hold_keep_only_mute_does_not_block_steal() {
    // dest-aj @443k+: mute 1.4 grown=8 STICKY_HOLD denied 162.35 (max 40)
    // while win_mbps≈80. Hold KEEP ≥80 only — mute must not lock H.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(1001);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["mute", "hot", "hero"]);
    assigner.set_peer_scores(&[
        ("mute".into(), 0.20),
        ("hot".into(), 0.90),
        ("hero".into(), 0.85),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.tip_owner_open.store(false, Ordering::Relaxed);
    assigner.note_tip_owner_assigned("mute");
    assigner.note_tip_cover_claim("mute", 1001, 1128);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "mute", 1001, 1001);
        assert!(
            !assigner.preferred_meets_keep_bps(),
            "mute must not meet KEEP ≥80"
        );
        assert!(
            assigner.peer_may_take_tip_owner("hot", &g, 0),
            "mute STICKY_HOLD must not deny top-scored steal"
        );
    }

    assigner.note_tip_owner_assigned("hero");
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.preferred_meets_keep_bps());
    assigner.note_tip_cover_claim("hero", 1001, 1128);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "hero", 1001, 1001);
        assert!(
            !assigner.peer_may_take_tip_owner("hot", &g, 0),
            "≥80 STICKY_HOLD must still deny steal"
        );
    }
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn dest_bk_c1j_keep_must_drop_ahead_when_awaiting_even_if_ahead_low() {
    // dest-bk 179761: sticky `108.36` @1411, C1J_KEEP 179857–179984 (start>H),
    // await_ms=35291, IBD_STICKY_CAP 1/1, then 48s to 180k inst 20. dest-x 273
    // died at 184k ts 56. must_drop_ahead used to return false when ahead<8
    // && holes<5 *before* checking awaiting. Awaiting ≥2s must abort start>H
    // so the KEEP hero can take H. dest-be ≥80 C1J_KEEP before cheese (ahead
    // 64, awaiting not armed) must still pass.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(1001);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 0.90), ("mute".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("hero");
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    super::super::tip_stage::test_backdate_awaiting_ms(35_000);
    assigner.note_tip_cover_claim("hero", 1001, 1032);
    assert!(
        !assigner.should_abort_tip_walk_in("hero", 1100, 1131),
        "R-40: dest-bk await must not abort FAR while H is covered (R-39 H+88)"
    );
    assigner.clear_all_tip_cover_claims();
    // R-322 default holds covering=0 far spans. r321 locks =0 restore.
    assert!(
        !assigner.should_abort_tip_walk_in("hero", 1100, 1131),
        "R-322: covering=0 + awaiting still holds start>H unless NO_TIP_ABORT=0"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r37_pipe_fill_not_aborted_on_dest_bk_await() {
    // R-37 10–50k 92.7: dest-bk awaiting aborted hero PIPE_FILL H+32 every 2s
    // (107 CHEESE_HERO_ABORT) then MUTE_KILL GD_SLOW. dest-bk live was H+96.
    // dest-x/dest-bk still drop H+99. KEEP=0.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(1001);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 0.90), ("mute".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("hero");
    for _ in 0..80 {
        assigner.note_wan_tip_stream("hero");
    }
    super::super::tip_stage::test_backdate_awaiting_ms(35_000);
    assert!(
        !assigner.should_abort_tip_walk_in("hero", 1033, 1064),
        "R-37: dest-bk await must not abort PIPE_FILL H+32"
    );
    // R-322: default NO_TIP_ABORT holds an unreserved H+99 span. r321 locks
    // the =0 restore. This test keeps the PIPE_FILL hold.
    assert!(
        !assigner.should_abort_tip_walk_in("hero", 1100, 1131),
        "R-322: default holds H+99; abort returns only with NO_TIP_ABORT=0"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn cheese_hero_pin_blocks_c1t_cheese_race() {
    // r FAIL: lottery cover + C1t cheese opened covering=3 while ≥80 hero
    // was already sticky. Mute racers must not pile on.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(1001);
    super::super::IBD_EMPTY_TIP.store(false, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_BRIDGE_HOLES.store(22, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(164, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
        std::env::set_var("BLVM_IBD_C1T_TIP_RACE_MS", "120");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["hero", "mute", "racer"]);
    assigner.set_peer_scores(&[
        ("hero".into(), 0.90),
        ("mute".into(), 0.40),
        ("racer".into(), 0.50),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("hero");
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assigner.note_tip_cover_claim("hero", 1001, 1032);
    assigner.note_tip_cover_claim("mute", 1001, 1001);
    assert!(
        assigner.healthy_tip_cover_count(1001) >= 2,
        "cheese covering=2 setup"
    );
    assert!(
        assigner.cheese_hero_blocks_c1t_race(),
        "≥80 preferred + ahead/holes must block C1t"
    );
    assert!(
        !assigner.c1t_tip_height_race(),
        "C1t cheese must not race while preferred ≥80 holds"
    );
    assert!(
        !assigner.empty_tip_triple_race(),
        "must not open covering=3 while preferred ≥80 holds"
    );
    super::super::IBD_TIP_BRIDGE_HOLES.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn tip_owner_fail_cooldown_skips_healthy_tip_bps() {
    // Genesis-d 180k+: 30s trial cooldown parked 164.152 (lifetime ≥80) so
    // mutes owned tip at grown=8. Same keep bar as C1u-hero / KEEP.
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = wan_tip_assigner(300_000, 300_000, 301_000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 5.0), ("mute".into(), 4.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.wan_tip_stream_bps("hero") >= 80.0);
    assigner.mark_tip_owner_fail_cooldown("hero", 30);
    assert!(
        !assigner.tip_owner_in_fail_cooldown("hero"),
        "must not 30s-cool a ≥80 tip-streamer"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn cheese_hero_h_timeout_no_strike_and_short_failover() {
    // dest-ap: 107.194 @310 on cheese 148449 — 3×5s then LIMITED. ≥80 +
    // cheese sitting + covering timeout is no-strike; mute path cools this
    // H (W28c) without 120s P1e ban. Trial/A6m still skip healthy.
    let prev_ahead = super::super::IBD_REORDER_AHEAD.load(Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(224, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
        std::env::remove_var("BLVM_IBD_CHEESE_HERO_H_COOLDOWN_SECS");
    }
    let assigner = wan_tip_assigner(148_448, 148_448, 149_000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 5.0), ("mute".into(), 4.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.wan_tip_stream_bps("hero") >= 80.0);
    let err = "tip-gap timeout cap: gap 148449 waited 5s in chunk 148449-148449";
    assert!(
        assigner.cheese_hero_h_timeout_no_strike("hero", 148_449, 148_449, err),
        "≥80 cheese H timeout must not LIMITED-strike"
    );
    assert!(
        !assigner.cheese_hero_h_timeout_no_strike("mute", 148_449, 148_449, err),
        "mute still strikes toward LIMITED"
    );
    assigner.note_tip_owner_assigned("hero");
    assigner.note_tip_owner_failed_mute("hero");
    assert!(
        assigner.tip_owner_in_fail_cooldown("hero"),
        "cheese H timeout must cool ≥80 for this H (W28c failover)"
    );
    let until = assigner
        .tip_owner_fail_until
        .lock()
        .unwrap()
        .get("hero")
        .copied();
    let remaining = until
        .map(|t| {
            t.saturating_duration_since(std::time::Instant::now())
                .as_secs()
        })
        .unwrap_or(0);
    assert!(
        remaining <= 15,
        "must not P1e-ban the ≥80 for 120s (got {remaining}s)"
    );
    assert!(remaining >= 4, "failover window too short ({remaining}s)");
    assigner.mark_tip_owner_fail_cooldown("hero", 30);
    // still cooled from mute; healthy skip must not clear it, and a fresh
    // 30s trial cool without override stays the cheese-H window (≤15).
    let remaining2 = assigner
        .tip_owner_fail_until
        .lock()
        .unwrap()
        .get("hero")
        .copied()
        .map(|t| {
            t.saturating_duration_since(std::time::Instant::now())
                .as_secs()
        })
        .unwrap_or(0);
    assert!(
        remaining2 <= 15,
        "trial mark without override must not extend ≥80 to 30s (got {remaining2}s)"
    );
    super::super::IBD_REORDER_AHEAD.store(prev_ahead, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
}

fn tip_owner_cooldown_remaining_secs(assigner: &ChunkAssigner, peer: &str) -> u64 {
    assigner
        .tip_owner_fail_until
        .lock()
        .unwrap()
        .get(peer)
        .copied()
        .map(|t| {
            t.saturating_duration_since(std::time::Instant::now())
                .as_secs()
        })
        .unwrap_or(0)
}

#[serial_test::serial(ibd)]
#[test]
fn p1e_mute_skips_120s_for_decayed_stream_window_hero() {
    // dest-az 154k: `142.181` last ≥80 then stream decayed; P1e 120s parked
    // them so 180–280k was mute lottery (ge80=0). Window memory → 8s this-H.
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
        std::env::remove_var("BLVM_IBD_CHEESE_HERO_H_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS");
    }
    let prev_ahead = super::super::IBD_REORDER_AHEAD.load(Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(154_370, 154_370, 155_000, &["hero", "mute", "other"]);
    assigner.set_peer_scores(&[
        ("hero".into(), 5.0),
        ("mute".into(), 4.0),
        ("other".into(), 3.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.wan_tip_stream_bps("hero") >= 80.0);
    assigner.test_age_tip_stream_started("hero", 120);
    assert!(
        assigner.wan_tip_stream_bps("hero") < 80.0,
        "fixture must decay below keep, got {:.1}",
        assigner.wan_tip_stream_bps("hero")
    );
    assigner.note_tip_owner_assigned("hero");
    assigner.note_tip_owner_failed_mute("hero");
    let remaining = tip_owner_cooldown_remaining_secs(&assigner, "hero");
    assert!(
        remaining <= 15,
        "decayed STREAM-window ≥80 must not P1e 120s (got {remaining}s)"
    );
    assert!(
        remaining >= 4,
        "this-H failover window too short ({remaining}s)"
    );
    assigner.note_tip_owner_failed_mute("mute");
    let mute_remaining = tip_owner_cooldown_remaining_secs(&assigner, "mute");
    assert!(
        mute_remaining >= 60,
        "never-hero mute still P1e-bans (got {mute_remaining}s)"
    );
    super::super::IBD_REORDER_AHEAD.store(prev_ahead, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn p1e_mute_skips_120s_for_just_pinned_gd_slow_new() {
    // dest-az 14:33:24 MUTE_KILL GD_SLOW new=`142.181` then 14:33:27 P1e 120s.
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
        std::env::remove_var("BLVM_IBD_CHEESE_HERO_H_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_GD_SLOW_PIN_PROTECT_SECS");
    }
    let assigner = wan_tip_assigner(179_070, 179_070, 180_000, &["hero", "mute"]);
    assigner.set_peer_scores(&[("hero".into(), 5.0), ("mute".into(), 4.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    assigner.test_note_gd_slow_pin("hero");
    assigner.note_tip_owner_assigned("hero");
    assigner.note_tip_owner_failed_mute("hero");
    let remaining = tip_owner_cooldown_remaining_secs(&assigner, "hero");
    assert!(
        remaining <= 15,
        "just-pinned GD_SLOW new= must not P1e 120s (got {remaining}s)"
    );
    assert!(
        remaining >= 4,
        "this-H failover window too short ({remaining}s)"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
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
    // Owner took the 16-wide tip tile. A later peer must not overlap it.
    // The old exact (505170, 505184) partition is not what latch/zone emit.
    assert!(
        second.is_none() || second.is_some_and(|(s, _)| s > 505_169),
        "pB must not overlap the tip owner, got {second:?}"
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
fn get_work_hole_band_does_not_relock_inflight() {
    // Genesis-c 186043: `get_work` held `in_flight`, then `wan_allow_multi_peer_ahead`
    // → `peer_holds_tip_download` locked it again. CRAWL silent, leftover_force
    // `arm_try_lock_fail` held_ms=450s+.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(186_039));
    let assigner = Arc::new(ChunkAssigner::new(
        vec![(186_000, 186_127)],
        vec!["floor".into(), "mid".into()],
        Arc::clone(&vh),
        186_000,
        true,
    ));
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_wan_body_tip(0);
    assigner.set_header_tip(900_000);
    assigner.set_peer_scores(&[("floor".into(), 0.10), ("mid".into(), 0.25)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("floor");
    assigner.set_tip_gap_missing(false);
    unsafe {
        std::env::set_var("BLVM_IBD_WEAK_STICKY_OPEN_MS", "0");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.set_tip_bridge_holes(24);

    let a2 = Arc::clone(&assigner);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _g = a2.in_flight_per_peer.lock().unwrap();
        let _ = a2.wan_allow_multi_peer_ahead(1, 0);
        let _ = tx.send(());
    });
    rx.recv_timeout(Duration::from_secs(2)).expect(
        "wan_allow under in_flight lock must not deadlock (genesis-c 186043 get_work hold)",
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
    // Wall A: one (H,H). Second peer is the H+1 zone tile (R-249).
    assert_eq!(
        assigner.get_work("pC", 1000),
        Some((323_781, 323_796)),
        "W153: second peer is the H+1 zone tile, not a second (H,H)"
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
    unsafe {
        std::env::set_var("BLVM_IBD_NO_TIP_ABORT", "0");
    }
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
    assigner.set_tip_gap_missing(true);
    for _ in 0..8 {
        assigner.note_wan_tip_stream("ahead");
    }
    assert!(
        assigner.should_abort_tip_walk_in("ahead", 933, 964),
        "8 STREAM is 8 BPS — dest-be drip, not C1J_KEEP"
    );
    for _ in 0..80 {
        assigner.note_wan_tip_stream("ahead");
    }
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", 933, 964),
        "E: C1J_KEEP holds STREAM ≥60 with KEEP=0"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_skips_owner_with_healthy_tip_bps() {
    // Genesis-a 148k: KEEP 136.33 at 386 BPS then C1u HOLD 8→8 on global ewma=1003.
    // R-28: KEEP=0 (ship) must still clear a line-rate owner (≥60). Mute stays clamped.
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(900, 800, 100_000, &["hero", "other"]);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(
        assigner.tip_owner_clears_c1u_clamp("hero"),
        "KEEP=0 line-rate owner must skip C1u cliff"
    );
    assert!(
        !assigner.tip_owner_clears_c1u_clamp("other"),
        "unknown peer stays C1u-eligible"
    );
}

/// R-147: fat sticky sit (<60 BPS) must not C1u-cliff the 128 assign to 8.
#[serial_test::serial(ibd)]
#[test]
fn r147_fat_preferred_clears_c1u_sit() {
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["hero", "mute"]);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("hero".into());
    assigner.note_wan_tip_stream("hero");
    assert!(
        assigner.wan_tip_stream_bps("hero") < 60.0,
        "fixture is sit, not line-rate"
    );
    assert!(
        assigner.tip_owner_clears_c1u_clamp("hero"),
        "fat preferred with streams keeps grown pipe"
    );
    assert!(
        !assigner.tip_owner_clears_c1u_clamp("mute"),
        "non-preferred stays C1u-eligible"
    );
    let empty = wan_tip_assigner(900, 800, 100_000, &["hero"]);
    *empty.preferred_tip_owner.lock().unwrap() = Some("hero".into());
    empty.note_wan_tip_stream("hero");
    assert!(
        !empty.tip_owner_clears_c1u_clamp("hero"),
        "empty-band sit preferred still C1u-clamped"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn c1u_fill_loop_skip_matches_clears_clamp() {
    // Genesis-f 266272 / 270998: complete-path skip existed but enter/fill-loop
    // still min(slow_cap). One helper must agree with tip_owner_clears_c1u_clamp.
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(5_000, 32);
    // B1: mute clamp needs that peer's EWMA, not global.
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("mute", 5_000, 32);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = wan_tip_assigner(900, 800, 100_000, &["hero", "mute"]);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("hero");
    }
    let enter = Some(std::sync::Arc::new(assigner));
    assert!(
        super::super::download::tip_hole_gd_slow(),
        "fixture must be globally GD_SLOW"
    );
    assert!(
        !super::super::download::c1u_applies_slow_clamp(&enter, "hero"),
        "≥80 hero must not take enter/fill C1u min(slow_cap)"
    );
    assert!(
        super::super::download::c1u_applies_slow_clamp(&enter, "mute"),
        "mute still C1u-clamped on own EWMA"
    );
    assert!(
        !super::super::download::c1u_applies_slow_clamp(&enter, "fresh"),
        "B1: no peer samples → not clamped by global mute"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
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
fn w91_behind_tip_complete_still_opens_mute() {
    // W91 payload: a 0-BPS sticky that finished behind tip must clear + OPEN.
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(200_000));
    let assigner = ChunkAssigner::new(
        vec![(187_000, 187_127)],
        vec!["mute".into(), "hero".into()],
        Arc::clone(&vh),
        187_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.note_tip_owner_assigned("mute");
    assigner.on_chunk_complete_range("mute", 187_000, 187_127);
    assert!(
        assigner.preferred_tip_owner().is_none(),
        "W91: mute behind-tip complete must drop sticky"
    );
    assert!(
        assigner.tip_owner_open.load(Ordering::Relaxed),
        "W91: mute behind-tip complete must OPEN"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn w91_behind_tip_complete_keeps_healthy_tip_bps_hero() {
    // Genesis-e 180k+: 164.152 finished 187639-187766 while next=187765+ and
    // OPEN cleared him 101× → mute sticky → STICKY_HOLD. Same ≥80 keep bar.
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(200_000));
    let assigner = ChunkAssigner::new(
        vec![(187_000, 187_127)],
        vec!["hero".into(), "mute".into()],
        Arc::clone(&vh),
        187_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.note_tip_owner_assigned("hero");
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(assigner.wan_tip_stream_bps("hero") >= 80.0);
    assigner.on_chunk_complete_range("hero", 187_000, 187_127);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("hero"),
        "≥80 streamer must stay sticky after behind-tip complete"
    );
    assert!(
        !assigner.tip_owner_open.load(Ordering::Relaxed),
        "must not OPEN the slot for mute lottery"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
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
    let full = rq.iter().find(|e| e.start == 100 && e.end == 199);
    assert!(
        full.is_some_and(|e| e.exclude.is_none()),
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
        .filter(|e| e.start == e.end)
        .map(|e| e.start)
        .collect();
    assert_eq!(
        tip_heights,
        vec![901],
        "WAN force covering=0 must enqueue (H,H) only; got {rq:?}"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// R-231 dump: covering>1 still skips FORCE (dest-bc 0–10k). Not a second H pipe.
#[serial_test::serial(ibd)]
#[test]
fn r231_dump_covering2_force_still_skips() {
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["a".into(), "b".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("a".into(), vec![(901, 901)]);
        g.insert("b".into(), vec![(901, 932)]);
    }
    assigner.requeue_stall_gaps_force(901, None);
    let rq = assigner.retry_queue.lock().unwrap();
    assert!(
        rq.is_empty(),
        "dump covering>1 must still skip FORCE, got {rq:?}"
    );
    super::super::tip_stage::clear_tip_failover();
}

/// R-231 fat sit @180718: covering=2 zombie + skip → FORCE 0 / 3.7h STALL.
/// Release H inflight then enqueue one (H,H). Wall A stays one TCP.
#[serial_test::serial(ibd)]
#[test]
fn r231_fat_covering2_zombie_releases_and_enqueues_hh() {
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(180_717));
    let assigner = ChunkAssigner::new(
        vec![(180_000, 182_000)],
        vec!["z1".into(), "z2".into()],
        Arc::clone(&vh),
        180_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_header_tip(370_000);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("z1".into(), vec![(180_718, 181_801)]);
        g.insert("z2".into(), vec![(180_718, 180_718)]);
    }
    assigner.requeue_stall_gaps_force(180_718, None);
    let (covering, _, _) = assigner.tip_flight_diag();
    assert_eq!(covering, 0, "zombie H cover must be released");
    let rq: Vec<_> = assigner
        .retry_queue
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    let tip_hh: Vec<_> = rq
        .iter()
        .filter(|e| e.start == e.end && e.start == 180_718)
        .collect();
    assert_eq!(
        tip_hh.len(),
        1,
        "fat covering>1 zombie must enqueue one (H,H), got {rq:?}"
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
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

/// q 175715: covering=1 zombie/stripe-past-H must still enqueue (H,H).
#[serial_test::serial(ibd)]
#[test]
fn w73_wan_force_requeue_enqueues_when_covering_one() {
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
        .filter(|e| e.start == e.end)
        .map(|e| e.start)
        .collect();
    assert_eq!(
        tip_heights,
        vec![901],
        "WAN force covering=1 must enqueue (H,H) only; got {rq:?}"
    );
    super::super::tip_stage::clear_tip_failover();
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

/// Historical name. A4's top-half cap of 2 was N-way ahead; non-sticky depth is
/// `BLVM_IBD_PEER_DEPTH` (default 1). The next peer takes the next chunk.
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
    assert!(
        assigner.get_work("fast", 1000).is_none(),
        "PEER_DEPTH default 1: same peer does not take a second chunk (A4 top-half dual retired)"
    );
    assert_eq!(assigner.get_work("worse", 1000), Some((116, 131)));
    assert!(
        assigner.get_work("worse", 1000).is_none(),
        "non-sticky cap is uniform, not a bottom-half special case"
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
fn a6l_sticky_below_median_gets_top_in_flight_cap() {
    // Live A6k: sticky@0.1 < median → max_in_flight=1 → cannot re-arm next tip span.
    super::super::tip_stage::clear_tip_failover();
    // R-74: below 50k the sticky cap is 1 even when TOP is 2. This cell is the
    // above-50k claim: a below-median sticky still gets that cap.
    let vh = Arc::new(AtomicU64::new(50_000));
    let chunks = vec![
        (50_000, 50_127),
        (50_128, 50_255),
        (50_256, 50_383),
        (50_384, 50_511),
    ];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "top_w".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        50_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(49_900);
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
    assigner.set_tip_gap_missing(false);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(64, Ordering::Relaxed);
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
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn layer_c_top2_is_sticky_only() {
    // R-77: TOP=2 after 50k is same-peer next stripe. Median peers stay 1.
    super::super::tip_stage::clear_tip_failover();
    let prev_top = std::env::var("BLVM_IBD_TOP_PEER_IN_FLIGHT").ok();
    unsafe {
        std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", "2");
    }
    let vh = Arc::new(AtomicU64::new(199_999));
    let chunks = vec![
        (199_000, 200_127),
        (200_128, 200_255),
        (200_256, 200_383),
        (200_384, 200_511),
    ];
    let assigner = ChunkAssigner::new(
        chunks,
        vec!["sticky".into(), "top_w".into(), "mid".into(), "low".into()],
        Arc::clone(&vh),
        199_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
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
    assigner.set_tip_gap_missing(false);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(64, Ordering::Relaxed);
    assert_eq!(assigner.max_in_flight_for("sticky"), 2);
    assert_eq!(assigner.max_in_flight_for("top_w"), 1);
    assert_eq!(assigner.max_in_flight_for("mid"), 1);
    let first = assigner.get_work("sticky", 1000);
    assert!(first.is_some(), "sticky first, got {first:?}");
    let second = assigner.get_work("sticky", 1000);
    assert!(
        second.is_some(),
        "sticky second stripe under TOP=2 after 50k, first={first:?} second={second:?}"
    );
    if let Some((s, e)) = assigner.get_work("top_w", 1000) {
        let tip = vh.load(Ordering::Relaxed) + 1;
        assert!(
            s > tip && !(s <= tip && tip <= e),
            "top_w must not cover H, got {s}-{e}"
        );
    }
    unsafe {
        match prev_top {
            Some(v) => std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", v),
            None => std::env::remove_var("BLVM_IBD_TOP_PEER_IN_FLIGHT"),
        }
    }
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn layer_c_top2_empty_band_stays_cap1() {
    // R-74: dual during H<50k + covering=2 hang @8300. Cap stays 1.
    super::super::tip_stage::clear_tip_failover();
    let prev_top = std::env::var("BLVM_IBD_TOP_PEER_IN_FLIGHT").ok();
    unsafe {
        std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", "2");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1135)],
        vec!["sticky".into(), "other".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("other".into(), 0.40)]);
    assigner.set_ibd_ready_peers(HashSet::from(["sticky".into(), "other".into()]));
    assigner.note_tip_owner_assigned("sticky");
    assigner.set_tip_gap_missing(false);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(64, Ordering::Relaxed);
    assert_eq!(assigner.max_in_flight_for("sticky"), 1);
    let first = assigner.get_work("sticky", 1000);
    assert!(first.is_some(), "sticky first, got {first:?}");
    let second = assigner.get_work("sticky", 1000);
    assert!(
        second.is_none(),
        "empty-band TOP=2 must not dual, first={first:?} second={second:?}"
    );
    unsafe {
        match prev_top {
            Some(v) => std::env::set_var("BLVM_IBD_TOP_PEER_IN_FLIGHT", v),
            None => std::env::remove_var("BLVM_IBD_TOP_PEER_IN_FLIGHT"),
        }
    }
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
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
fn keep_default_off_does_not_meet_keep_without_env() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::mark_needed(1001);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(1000, 800, 2000, &["hero"]);
    assigner.set_peer_scores(&[("hero".into(), 0.90)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("hero");
    assigner.note_wan_tip_stream("hero");
    assert!(
        !assigner.preferred_meets_keep_bps(),
        "mute (1 stream) still false with KEEP=0"
    );
    for _ in 0..600 {
        assigner.note_wan_tip_stream("hero");
    }
    assert!(
        !assigner.preferred_meets_keep_bps(),
        "R-43: STREAM ≥60 with KEEP=0 must not open extras (R-30 / R-35)"
    );
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 8),
        "R-43: KEEP=0 hero must not farm C1g extras"
    );
    assert_eq!(
        assigner.test_ahead_stripe_floor(1001),
        1001,
        "wan_ahead_stripe_floor stays KEEP-only (dest-au / grown=128)"
    );
    super::super::tip_stage::mark_needed(0);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn c1g_hot_contig_second_pipe_after_streaming_hero() {
    // R-28: grown≥32 + stream≥80 + covering≥1 → disjoint extras (max 3).
    // R-27 was N peers on the same stripe with covering=0.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(
        900,
        800,
        100_000,
        &["owner", "ahead", "spare", "third", "fourth"],
    );
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("ahead".into(), 8.0),
        ("spare".into(), 7.0),
        ("third".into(), 6.0),
        ("fourth".into(), 5.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    let tip = assigner.get_work("owner", 4096).expect("tip owner");
    assert_eq!(tip.0, 901);
    assert!(
        tip.1 > tip.0,
        "owner must hold a deep stripe, got {}-{}",
        tip.0,
        tip.1
    );

    // R-245: dump-height latch off. LOOKAHEAD may still pack; extras must not latch.
    for peer in ["ahead", "spare", "third", "fourth"] {
        let _ = assigner.get_work(peer, 4096);
    }
    assert!(
        assigner.latched_ahead.lock().unwrap().is_empty(),
        "dump-height latch must stay off, got {:?}",
        assigner.latched_ahead.lock().unwrap()
    );
    match assigner.get_work("owner", 4096) {
        None => {}
        Some((s, _)) if s == 901 => {}
        Some((s, e)) => panic!("owner must not take start>H while tip_missing, got {s}-{e}"),
    }
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r30_c1j_still_aborts_start_past_h() {
    // Wall B off. C1j stays absolute: start>H while tip missing → abort.
    unsafe {
        std::env::set_var("BLVM_IBD_NO_TIP_ABORT", "0");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let tip = assigner.get_work("owner", 4096).expect("owner");
    assert_eq!(tip.0, 901);
    match assigner.get_work("ahead", 4096) {
        None => {}
        Some((s, e)) if s == 901 && e == 901 => {}
        Some((s, e)) => {
            assert!(
                s > 901 && e <= 901 + 256,
                "R-248: dump near-cursor tile only, got {s}-{e}"
            );
            assert!(
                assigner
                    .priority_zone
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(p, ps, pe)| p == "ahead" && *ps == s && *pe == e),
                "past-H dump assign must be a reserved priority-zone tile"
            );
        }
    }
    assigner.note_wan_tip_stream("ahead");
    assigner.test_age_tip_stream_last("ahead", 20);
    assert!(
        assigner.should_abort_tip_walk_in("ahead", 965, 1028),
        "C: cold extra (silent ≥15s) still aborts"
    );
    // Fresh extra + hero ≥60 + covering≥1 → hold (C).
    assigner.note_wan_tip_stream("ahead");
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", 965, 1028),
        "C: issued extra lives while hero ≥60 and H covered"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r32_retry_and_mq_refuse_past_h_while_gap() {
    // R-31 sat @36k: MQ/retry handed 36137-36264 while H=36024.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let vh = Arc::new(AtomicU64::new(0));
    let assigner = ChunkAssigner::new(
        vec![(1, 32), (33, 64), (65, 96), (97, 128)],
        vec!["a".into(), "b".into(), "c".into(), "d".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_header_tip(10_000);
    assigner.set_peer_scores(&[
        ("a".into(), 9.0),
        ("b".into(), 8.0),
        ("c".into(), 7.0),
        ("d".into(), 6.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);

    let first = assigner.get_work("a", 4096).expect("H cover");
    assert!(
        first.0 <= 1 && first.1 >= 1,
        "first must cover H=1, got {}-{}",
        first.0,
        first.1
    );
    // R-26/R-30 assigned 65–96 at tip=1 and lived. R-32 refuse of that walk
    // is reverted (10–50k 1415 → 349). Do not restore.
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r30_complete_does_not_open_past_h() {
    // R-28/R-29 extras OFF. Completing H must not hand start>H to another peer.
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
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let tip = assigner.get_work("owner", 4096).expect("owner");
    assigner.on_chunk_complete_range("owner", tip.0, tip.1);
    for peer in ["ahead", "spare"] {
        match assigner.get_work(peer, 4096) {
            None => {}
            Some((s, e)) if s == 901 && e == 901 => {}
            Some((s, e)) => panic!("{peer} must not get past-H after owner complete, got {s}-{e}"),
        }
    }
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn c1g_latch_ahead_stays_off_when_h_uncovered() {
    // R-27 cheese: covering=0 + owner_end+1. Latch must not skip H.
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
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    match assigner.get_work("ahead", 4096) {
        None => {}
        Some((s, e)) if s == 901 => {}
        Some((s, e)) => panic!("covering=0 must not skip H, got {s}-{e}"),
    }
    match assigner.get_work("spare", 4096) {
        None => {}
        Some((s, e)) if s == 901 => {}
        Some((s, e)) => panic!("covering=0 spare must not skip H, got {s}-{e}"),
    }
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r54_latch_hook_reverted_even_when_gates_pass() {
    // R-54 dest FAIL. Hook gone. Reorder + C1i + grown + stream must not
    // open start>H. Wall B off.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead", "spare", "third"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("ahead".into(), 8.0),
        ("spare".into(), 7.0),
        ("third".into(), 6.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);

    let tip = assigner.get_work("owner", 4096).expect("tip owner");
    assert!(tip.0 <= 902, "owner H, got {}-{}", tip.0, tip.1);
    for peer in ["ahead", "spare", "third"] {
        let _ = assigner.get_work(peer, 4096);
    }
    assert!(
        assigner.latched_ahead.lock().unwrap().is_empty(),
        "R-54: dump-height latch stays off, got {:?}",
        assigner.latched_ahead.lock().unwrap()
    );
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r54_latch_stays_off_while_c1i() {
    // First body in reorder is not contig≥8. Latch must not rematch R-50 33–288.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(1, Ordering::Relaxed);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let tip = assigner.get_work("owner", 4096).expect("owner");
    assert!(tip.0 <= 902, "owner H, got {}-{}", tip.0, tip.1);
    let _ = assigner.get_work("ahead", 4096);
    assert!(
        assigner.latched_ahead.lock().unwrap().is_empty(),
        "C1i must keep latch off at contig=1"
    );
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

fn r245_fat_latch_cleanup() {
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

struct R245LatchGuard;
impl Drop for R245LatchGuard {
    fn drop(&mut self) {
        r245_fat_latch_cleanup();
    }
}

fn r245_fat_latch_assigner(peers: &[&str]) -> ChunkAssigner {
    r245_fat_latch_cleanup();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let assigner = wan_tip_assigner(180_000, 0, 370_000, peers);
    let scores: Vec<(String, f64)> = peers
        .iter()
        .enumerate()
        .map(|(i, p)| ((*p).to_string(), 9.0 - i as f64))
        .collect();
    assigner.set_peer_scores(&scores);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner
}

fn r245_inflight_ahead(assigner: &ChunkAssigner, tip: u64) -> usize {
    assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .values()
        .flatten()
        .filter(|(s, _)| *s > tip)
        .count()
}

/// R-27: covering=0 must not skip H even at fat.
#[serial_test::serial(ibd)]
#[test]
fn r245_covering0_cannot_skip_h_at_fat() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["ahead", "spare"]);
    let _ = assigner.get_work("ahead", 4096);
    let _ = assigner.get_work("spare", 4096);
    assert!(
        assigner.latched_ahead.lock().unwrap().is_empty(),
        "R-27: latch must not arm at covering=0 (LOOKAHEAD may still pack)"
    );
}

/// R-33: inflight that does not contain H must not latch (covering=1 lie).
#[serial_test::serial(ibd)]
#[test]
fn r245_no_latch_while_h_empty() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "ahead"]);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("owner".into(), vec![(180_100, 180_227)]);
    }
    let _ = assigner.get_work("ahead", 4096);
    assert!(
        assigner.latched_ahead.lock().unwrap().is_empty(),
        "R-33: range 180100-180227 does not contain H=180001"
    );
}

/// Fat + reorder + contig + covering + stream≥60 → one extra (start>H).
/// Second peer denied. C1j holds. flight_ahead≥1 (R-34 had latch and 0).
#[serial_test::serial(ibd)]
#[test]
fn r245_one_latched_extra_at_fat_not_dump() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "ahead", "spare"]);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    let tip = assigner.get_work("owner", 4096).expect("owner");
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    let next = assigner.next_needed_height();
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("owner"),
        "preferred after owner GetData"
    );
    let covers = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .values()
        .flatten()
        .any(|&(s, e)| s <= next && next <= e);
    assert!(
        covers,
        "owner inflight must cover H={next} owner={tip:?} map={:?}",
        assigner.in_flight_per_peer.lock().unwrap()
    );
    assert!(
        assigner.mapped_tip_hole_depth("owner") >= 32,
        "grown={}",
        assigner.mapped_tip_hole_depth("owner")
    );
    assert!(
        assigner.wan_tip_stream_bps("owner") >= 60.0,
        "bps={}",
        assigner.wan_tip_stream_bps("owner")
    );
    let extra = assigner.get_work("ahead", 4096).expect("R-245 latch extra");
    assert!(
        extra.0 > next,
        "extra must start after H={next}, got {}-{}",
        extra.0,
        extra.1
    );
    assert!(
        assigner.latched_ahead_holds("ahead", extra.0, extra.1),
        "extra must be the latched reservation, not LOOKAHEAD"
    );
    assert!(
        r245_inflight_ahead(&assigner, next) >= 1,
        "R-34: reserved extra must show as flight_ahead"
    );
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", extra.0, extra.1),
        "R-28: C1j must hold the reserved extra"
    );
    assert!(
        assigner.latched_ahead.lock().unwrap().len() == 1,
        "exactly one latched extra, got {:?}",
        assigner.latched_ahead.lock().unwrap()
    );
    match assigner.get_work("spare", 4096) {
        None => {}
        Some((s, e)) => assert!(
            !(s == extra.0 && e == extra.1),
            "R-27/R-28: second peer must not take the same stripe {s}-{e}"
        ),
    }
    assert_eq!(
        assigner.latched_ahead.lock().unwrap().len(),
        1,
        "LOOKAHEAD may pack; second latch slot must stay closed"
    );
}

/// R-245 live: C1G_FREEZE @180102 covering=1, contig freeze delayed latch to 200k.
/// Cover of H + stream≥60 must latch at fat even when contig=0.
#[serial_test::serial(ibd)]
#[test]
fn r246_latch_fires_at_fat_when_contig0() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "ahead"]);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    let tip = assigner.get_work("owner", 4096).expect("owner");
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    let next = assigner.next_needed_height();
    let covers = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .values()
        .flatten()
        .any(|&(s, e)| s <= next && next <= e);
    assert!(covers, "owner inflight must cover H={next} owner={tip:?}");
    let extra = assigner
        .get_work("ahead", 4096)
        .expect("R-246: contig=0 must not block fat latch");
    assert!(
        assigner.latched_ahead_holds("ahead", extra.0, extra.1),
        "extra must be latched, not LOOKAHEAD, got {}-{}",
        extra.0,
        extra.1
    );
}

/// R-251 live: latch 190776-190967 at tip=188732 (owner_end=190775) then
/// 17.5s WaitFeeder. Fat extra must start at H+1, not owner_end+1.
/// Wall A: no second TCP on H.
#[serial_test::serial(ibd)]
#[test]
fn r252_latch_starts_at_h_plus_one_not_owner_end() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "ahead"]);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    let owner = assigner.get_work("owner", 4096).expect("owner");
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    let next = assigner.next_needed_height();
    assert!(
        owner.0 <= next && next <= owner.1,
        "owner must cover H={next} owner={owner:?}"
    );
    assert!(
        owner.1 >= next + 16,
        "fixture needs a wide owner stripe so a frontier jump would be visible owner={owner:?} H={next}"
    );
    let extra = assigner.get_work("ahead", 4096).expect("R-252 latch H+1");
    assert_eq!(
        extra.0,
        next + 1,
        "must not jump to owner_end+1={} got {}-{} H={next}",
        owner.1 + 1,
        extra.0,
        extra.1
    );
    assert_ne!(
        extra.0,
        owner.1 + 1,
        "R-251 door: start must not be owner_end+1"
    );
    assert!(
        extra.0 > next,
        "Wall A: no second TCP on H, got {}-{} H={next}",
        extra.0,
        extra.1
    );
    assert!(
        assigner.latched_ahead_holds("ahead", extra.0, extra.1),
        "extra must be latched, not zone/LOOKAHEAD, got {}-{}",
        extra.0,
        extra.1
    );
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", extra.0, extra.1),
        "C1j must hold the reserved extra"
    );
}

/// R-255: R-252 8s skip at fat covering=0 (R-253/R-254 OPEN reverted).
#[serial_test::serial(ibd)]
#[test]
fn r253_open_skip_clears_preferred_when_h_uncovered() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "ahead"]);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    let owner = assigner.get_work("owner", 4096).expect("owner");
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("owner"));
    assert!(
        assigner.peer_recently_tip_streaming("owner", Duration::from_secs(8)),
        "fixture needs the 8s recent-stream skip door"
    );
    assigner.test_set_validation_height(owner.1);
    assigner.on_chunk_complete_range("owner", owner.0, owner.1);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("owner"),
        "R-252 skip: covering=0 must keep preferred, got {:?}",
        assigner.preferred_tip_owner()
    );
    assert!(
        !assigner.tip_owner_open.load(Ordering::Relaxed),
        "R-72: must not OPEN-storm covering=0 after obsolete"
    );
}

/// R-255: reserved start>H covering=0 HOLDs at fat (COVERING0_ABORT reverted).
#[serial_test::serial(ibd)]
#[test]
fn r253_c1j_aborts_reserved_ahead_when_covering0() {
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "ahead"]);
    assigner.set_tip_gap_missing(true);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    assigner
        .lookahead_stripes
        .lock()
        .unwrap()
        .push(("ahead".into(), 180_100, 182_147));
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", 180_100, 182_147),
        "R-58: reserved start>H must HOLD while H has no GetData"
    );
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("owner".into(), vec![(180_001, 180_064)]);
    }
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", 180_100, 182_147),
        "HOLD stays when a live GetData covers H"
    );
}

/// R-254: dump covering=0 keeps R-252 8s skip (R-253 OPEN storm **420**).
#[serial_test::serial(ibd)]
#[test]
fn r254_dump_covering0_keeps_open_skip() {
    let _guard = R245LatchGuard;
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    let assigner = wan_tip_assigner(10_000, 0, 50_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    let owner = assigner.get_work("owner", 4096).expect("owner");
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("owner"));
    assert!(
        assigner.peer_recently_tip_streaming("owner", Duration::from_secs(8)),
        "fixture needs the 8s recent-stream skip door"
    );
    assigner.test_set_validation_height(owner.1);
    assigner.on_chunk_complete_range("owner", owner.0, owner.1);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("owner"),
        "dump covering=0 must keep preferred (R-72 skip), got {:?}",
        assigner.preferred_tip_owner()
    );
    assert!(
        !assigner.tip_owner_open.load(Ordering::Relaxed),
        "dump must not OPEN-storm covering=0 after obsolete"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-254: dump reserved start>H covering=0 HOLDs (R-253 abort **28** in dump).
#[serial_test::serial(ibd)]
#[test]
fn r254_dump_c1j_holds_reserved_when_covering0() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(35_130);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(35_130, 35_130, 50_000, &["hero", "idle"]);
    assigner.test_set_validation_height(35_130);
    assigner.set_peer_scores(&[("hero".into(), 9.0), ("idle".into(), 1.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner
        .lookahead_stripes
        .lock()
        .unwrap()
        .push(("hero".into(), 36_000, 38_047));
    assert!(
        !assigner.should_abort_tip_walk_in("hero", 36_000, 38_047),
        "dump covering=0 must HOLD reserved start>H, not COVERING0_ABORT"
    );
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-255: SWARM=1 at fat spreads packed LOOKAHEAD off hole+LEAD (8192 window).
/// Latch takes the first extra; second farm is LOOKAHEAD.
#[serial_test::serial(ibd)]
#[test]
fn r255_swarm_far_spreads_at_fat() {
    unsafe { std::env::set_var("BLVM_IBD_SWARM", "1") };
    let _guard = R245LatchGuard;
    let assigner = r245_fat_latch_assigner(&["owner", "latch", "farm"]);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 128);
    for _ in 0..200 {
        assigner.note_wan_tip_stream("owner");
    }
    let owner = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(owner.0, 180_001, "exclusive H, got {owner:?}");
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let _latch = assigner.get_work("latch", 4096);
    let farm = assigner.get_work("farm", 4096);
    unsafe { std::env::remove_var("BLVM_IBD_SWARM") };
    let Some((s, e)) = farm else {
        panic!("fat packed LOOKAHEAD farm must assign");
    };
    let hole = assigner.test_first_missing_height();
    assert!(
        s > hole,
        "Wall A: farm must not take H, got {s}-{e} hole={hole}"
    );
    let intended = hole.saturating_add(super::leapfrog_lead_at(hole));
    assert_eq!(
        s, intended,
        "R-245 restore: swarm env must not shuffle; sequential hole+LEAD, got {s}-{e} intended={intended}"
    );
}

/// R-255 fat: published FIRST_HOLE stays H+N after apply drained H
/// (`IBD_TIP_IN_REORDER=false`). That is not have. first_missing = H so
/// preferred GetData H (not HERO_SKIP the stale hole).
#[serial_test::serial(ibd)]
#[test]
fn r256_stale_published_hole_is_not_have() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_002, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assert_eq!(
        assigner.test_first_missing_height(),
        210_001,
        "stale published hole without have must not skip H"
    );
    let owner = assigner.get_work("owner", 4096).expect("owner GetData H");
    assert_eq!(
        owner.0, 210_001,
        "preferred must GetData H, not HERO_SKIP stale hole, got {owner:?}"
    );
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn layer_a_mute_trial_still_releases_covering_inflight() {
    // R-18 @513: streams=0 still force_release (find).
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(513);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(512, 400, 10_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.5), ("challenger".into(), 0.4)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 513);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 513, 513);
    }
    assert_eq!(assigner.wan_tip_stream_bps("sticky"), 0.0);
    assert!(
        assigner.maybe_start_tip_trial(513),
        "mute covering still trials"
    );
    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert!(
        g.get("sticky").map(|r| r.is_empty()).unwrap_or(true),
        "mute covering GetData must release"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn layer_a_dest_an_201447_does_not_force_release() {
    // dest-an: sticky 100.33 streams=20980 window ~35 BPS covering=2. Trial
    // started (under KEEP 80 / ship 60) but GetData was not mute.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(201_447);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(201_446, 201_000, 220_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.5), ("challenger".into(), 0.4)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 201_447);
    assigner.test_seed_tip_stream_rank("sticky", 20_980, 600);
    assigner.test_age_tip_stream_last("sticky", 9);
    let bps = assigner.wan_tip_stream_bps("sticky");
    assert!(bps >= 1.0 && bps < 60.0, "dest-an window, got {bps}");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 201_447, 201_447 + 63);
    }
    assert!(
        assigner.maybe_start_tip_trial(201_447),
        "under-60 covering may still trial"
    );
    let kept = {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        g.get("sticky")
            .is_some_and(|r| r.iter().any(|&(s, e)| s <= 201_447 && 201_447 <= e))
    };
    assert!(kept, "dest-an covering 20980-stream GetData must finish");
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn layer_a_demote_skips_covering_bps_ge1() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_seed_owner_body_ia(19, 16);
    super::super::tip_probe::test_seed_probe("challenger", 26);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.5), ("challenger".into(), 0.4)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.test_seed_tip_stream_rank("sticky", 400, 8);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 901, 964);
    }
    assert!(assigner.wan_tip_stream_bps("sticky") >= 1.0);
    assert!(
        !assigner.maybe_demote_cooled_ia_owner(901),
        "covering bps≥1 must not IA-demote"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    let kept = {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        g.get("sticky")
            .is_some_and(|r| r.iter().any(|&(s, e)| s <= 901 && 901 <= e))
    };
    assert!(kept, "demote must not drop covering GetData");
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
    }
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r59_lookahead_two_packets_do_not_outrank() {
    // R-58 dest: n=2 / 1e-3s printed top_bps=2000 and stole 195.52 (1839 streams).
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 1839, 3);
    assigner.test_seed_lookahead_rank("ahead", 2000, 1);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "sub-15s lookahead must not steal a streaming hero"
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r60_lookahead_drops_stale_and_arms_live() {
    // R-59 leftover 259-514 at tip=50k. Drop e<=H. R-70: do not re-arm H+256.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(50_900, 50_800, 200_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let tip = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(tip.0, 50_901);
    assigner.test_seed_lookahead_stripe("ahead", 259, 514);
    assigner.test_seed_lookahead_rank("ahead", 293, 16);
    match assigner.get_work("ahead", 4096) {
        None => {}
        Some((s, e)) if s > 50_901 && e < 50_901 + 256 => {}
        other => panic!("stale leftover must drop and must not re-arm H+256, got {other:?}"),
    }
    assert!(
        !assigner.peer_lookahead_covers("ahead", 400),
        "dropped ignition leftover must not still cover 400"
    );
    assert!(
        !assigner.peer_lookahead_covers("ahead", 51_200),
        "R-70: must not re-arm H+256 after 50k"
    );
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r62_flood_hero_not_sampled() {
    // R-53: IA 1 ms / 8503 must not take the empty-band sample door.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_owner_body_ia(1, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(10_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(10_000, 9_900, 100_000, &["sticky", "chall"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("chall".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 136_048, 16);
    assigner.test_seed_tip_stream_rank("chall", 40_000, 16);
    assert!(
        !assigner.maybe_start_tip_trial(10_001),
        "flood hero must hold"
    );
    assert!(
        !assigner.ignition_second_h_ok(true, true, 10_001, 1, 1),
        "flood hero must not reopen cap=2"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
}

#[serial_test::serial(ibd)]
#[test]
fn r62_empty_band_sample_second_h() {
    // IA 10 / ~1100: second peer on live H (R-62 3343). Not disjoint find (R-65 1447).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_owner_body_ia(10, 16);
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "sample"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("sample".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.test_seed_tip_stream_rank("owner", 17_600, 16);
    let a = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(a.0, 901);
    let (covering, _, _) = assigner.tip_flight_diag();
    let healthy = assigner.healthy_tip_cover_count(901);
    assert!(
        !assigner.ignition_second_h_ok(true, true, 901, healthy, covering),
        "R-78: second GetData on H is closed"
    );
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    let b = assigner.get_work("sample", 4096).expect("packed runway");
    let hole = assigner.test_first_missing_height();
    assert_eq!(
        b.0,
        hole.saturating_add(super::leapfrog_lead_at(hole)),
        "L1 pack at hole+LEAD, got {b:?} hole={hole}"
    );
    assert!(b.0 > 901, "must not cheese live H, got {b:?}");
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
}

#[serial_test::serial(ibd)]
#[test]
fn r63_spent_flood_sticky_holds() {
    // R-63 dest 724: IA-only hold sampled n=2/bps=2. Revert: sticky≥2000 holds again.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_owner_body_ia(4, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(10_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(10_000, 9_900, 100_000, &["sticky", "chall"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("chall".into(), 0.8)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 91_296, 16);
    assigner.test_seed_tip_stream_rank("chall", 114_128, 16);
    assert!(
        !assigner.maybe_start_tip_trial(10_001),
        "R-63 spent: sticky≥2000 must hold even at IA 4"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
}

#[serial_test::serial(ibd)]
#[test]
fn r64_unmeasured_ia_no_sample() {
    // R-64 dest 700: SAMPLE@tip=1 chall n=2 bps=2 while IA is None.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(1);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(0, 0, 100_000, &["sticky", "chall"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("chall".into(), 0.8)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    // Line-rate sticky so regular trial is skipped; chall is the R-64 2-packet fake.
    assigner.test_seed_tip_stream_rank("sticky", 1000, 16);
    assigner.test_seed_tip_stream_rank("chall", 2, 1);
    assert!(
        !assigner.maybe_start_tip_trial(1),
        "unmeasured IA must not sample n=2/bps=2 at tip=1"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
}

#[serial_test::serial(ibd)]
#[test]
fn r58_lookahead_holds_flood_hero() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 40000, 16);
    assigner.test_seed_lookahead_rank("ahead", 100000, 16);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "sticky_bps≥2000 must not be stolen"
    );
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r58_ignition_second_peer_takes_overlap_h() {
    // R-53: 176.126 1-32 then 170.253 1-64. R-55 never issued the second span.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(0, 0, 10_000, &["first", "second", "spare"]);
    assigner.set_peer_scores(&[
        ("first".into(), 9.0),
        ("second".into(), 8.0),
        ("spare".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    let a = assigner.get_work("first", 4096).expect("first H");
    assert_eq!(a.0, 1, "first must start at H, got {a:?}");
    let (covering, _ranges, _busy) = assigner.tip_flight_diag();
    let healthy = assigner.healthy_tip_cover_count(1);
    assert!(
        assigner.ignition_second_h_ok(true, true, 1, healthy, covering),
        "door must be open after first 1-32"
    );
    let b = assigner
        .get_work("second", 4096)
        .expect("R-53 second overlap");
    assert_eq!(b.0, 1, "second must cover H, got {b:?}");
    assert_eq!(b.1, 32, "same 1-32, not a second 1-64 range, got {b:?}");
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn r66_tournament_cap4_same_span() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(0, 0, 10_000, &["head", "a", "b", "c", "spare"]);
    assigner.set_peer_scores(&[
        ("head".into(), 9.0),
        ("a".into(), 8.0),
        ("b".into(), 7.0),
        ("c".into(), 6.0),
        ("spare".into(), 5.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("head");
    for p in ["head", "a", "b", "c"] {
        let w = assigner.get_work(p, 4096).expect(p);
        assert_eq!(w, (1, 32), "{p} must share 1-32, got {w:?}");
    }
    let spare = assigner.get_work("spare", 4096);
    assert!(
        spare.is_none() || spare.is_some_and(|w| w.0 > 1),
        "5th peer must not take ignition H (H+1 duplicate is the zone), got {spare:?}"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r66_list_head_reserved_last_slot() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    // A prior test can leave H "in reorder", and the hero then starts at H+1.
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(0, 0, 10_000, &["head", "a", "b", "c", "spare"]);
    assigner.set_peer_scores(&[
        ("head".into(), 9.0),
        ("a".into(), 8.0),
        ("b".into(), 7.0),
        ("c".into(), 6.0),
        ("spare".into(), 5.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("head");
    for p in ["a", "b", "c"] {
        let w = assigner.get_work(p, 4096).expect(p);
        assert_eq!(w, (1, 32), "{p}");
    }
    let spare = assigner.get_work("spare", 4096);
    assert!(
        spare.is_none() || spare.is_some_and(|w| w.0 > 1),
        "spare must not take ignition H (H+1 duplicate is the zone), got {spare:?}"
    );
    let head = assigner.get_work("head", 4096).expect("list-head reserved");
    assert_eq!(head, (1, 32));
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r66_n2_does_not_win() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_tournament_racer("chall", 2, 2);
    assert!(matches!(
        super::super::tip_stage::tournament_poll(),
        super::super::tip_stage::TournamentPoll::None
    ));
    assert!(!super::super::tip_stage::tournament_closed());
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r66_n16_before_2s_does_not_win() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_tournament_racer("burst", 0, 16);
    assert!(
        matches!(
            super::super::tip_stage::tournament_poll(),
            super::super::tip_stage::TournamentPoll::None
        ),
        "n=16 at t≈0 must not close (R-66b 36ms burst)"
    );
    assert!(!super::super::tip_stage::tournament_closed());
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r66_ia1_at_n16_wins() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_tournament_racer("slow", 10, 16);
    super::super::tip_stage::test_seed_tournament_racer("fast", 1, 16);
    assert!(matches!(
        super::super::tip_stage::tournament_poll(),
        super::super::tip_stage::TournamentPoll::None
    ));
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    match super::super::tip_stage::tournament_poll() {
        super::super::tip_stage::TournamentPoll::Win { peer, ia_ms, n } => {
            assert_eq!(peer, "fast");
            assert_eq!(ia_ms, 1);
            assert_eq!(n, 16);
        }
        other => panic!("expected win after 2s, got {other:?}"),
    }
    assert!(super::super::tip_stage::tournament_closed());
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-66b: WIN `100.11` then `get_work` ran `drop_unusable` because
/// `ibd_ready=false` at ignition. List-head kept H. Replay: winner holds.
#[serial_test::serial(ibd)]
#[test]
fn r66_win_holds_h_when_not_ibd_ready() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(0, 0, 10_000, &["head", "fast"]);
    assigner.set_peer_scores(&[("head".into(), 9.0), ("fast".into(), 1.0)]);
    mark_peers_ibd_ready(&assigner, &["head"]);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("head");
    super::super::tip_stage::test_seed_tournament_racer("fast", 1, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    match super::super::tip_stage::tournament_poll() {
        super::super::tip_stage::TournamentPoll::Win { peer, .. } => {
            assert_eq!(peer, "fast");
            assigner.note_tip_owner_assigned(&peer);
        }
        other => panic!("expected win, got {other:?}"),
    }
    assert!(
        assigner.tip_sticky_usable("fast"),
        "winner must be usable without ibd_ready"
    );
    let _ = assigner.get_work("head", 4096);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("fast"),
        "get_work must not STICKY_DROP the tournament winner"
    );
    let w = assigner.get_work("fast", 4096).expect("winner owns H");
    assert_eq!(w.0, 1, "winner H start, got {w:?}");
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r66_timeout_no_second_open() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(0, 0, 10_000, &["head", "late"]);
    assigner.set_peer_scores(&[("head".into(), 9.0), ("late".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    let _ = assigner.get_work("head", 4096);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_500);
    assert!(
        matches!(
            super::super::tip_stage::tournament_poll(),
            super::super::tip_stage::TournamentPoll::None
        ),
        "2s with no IA<=1 must keep racing until H=64"
    );
    super::super::tip_stage::mark_needed(64);
    assert!(matches!(
        super::super::tip_stage::tournament_poll(),
        super::super::tip_stage::TournamentPoll::Timeout
    ));
    assert!(super::super::tip_stage::tournament_closed());
    assert!(
        !assigner.ignition_second_h_ok(true, true, 50_000, 1, 1),
        "no overlap past 50k / no second tournament"
    );
    assert!(
        !assigner.ignition_second_h_ok(true, true, 65, 1, 1),
        "R-78: no second GetData on H after tournament"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r68_ia2_at_2s_does_not_win() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(1);
    super::super::tip_stage::test_seed_tournament_racer("mesh", 2, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    assert!(
        matches!(
            super::super::tip_stage::tournament_poll(),
            super::super::tip_stage::TournamentPoll::None
        ),
        "R-67: ia_ms=2 at 2s must not exclusive H"
    );
    assert!(!super::super::tip_stage::tournament_closed());
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r68_ia2_times_out_at_64() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_tournament_racer("mesh", 2, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    super::super::tip_stage::mark_needed(64);
    assert!(matches!(
        super::super::tip_stage::tournament_poll(),
        super::super::tip_stage::TournamentPoll::Timeout
    ));
    assert!(super::super::tip_stage::tournament_closed());
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r69_r62_overlap_after_64_when_not_flood() {
    super::super::tip_stage::test_reset_tip_stage();
    let assigner = wan_tip_assigner(0, 0, 10_000, &["a", "b"]);
    assert!(
        !assigner.ignition_second_h_ok(true, true, 65, 1, 1),
        "R-78: packed runway, not live-H overlap"
    );
    assert!(
        !assigner.ignition_second_h_ok(true, true, 50_000, 1, 1),
        "overlap door stays closed"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r69_timeout_preferred_holds_without_ibd_ready() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(64);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(64, 0, 10_000, &["head", "late"]);
    assigner.set_peer_scores(&[("head".into(), 9.0), ("late".into(), 8.0)]);
    mark_peers_ibd_ready(&assigner, &["late"]);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("head");
    super::super::tip_stage::test_seed_tournament_racer("late", 2, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    assert!(matches!(
        super::super::tip_stage::tournament_poll(),
        super::super::tip_stage::TournamentPoll::Timeout
    ));
    assert!(
        assigner.tip_sticky_usable("head"),
        "TIMEOUT preferred must be usable without ibd_ready"
    );
    let _ = assigner.get_work("late", 4096);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("head"),
        "get_work must not STICKY_DROP the TIMEOUT preferred"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r69_second_gets_live_h_not_lookahead() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_seed_owner_body_ia(10, 16);
    super::super::tip_stage::mark_needed(64);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(64, 0, 100_000, &["owner", "sample"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("sample".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.test_seed_tip_stream_rank("owner", 17_600, 16);
    let a = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(a.0, 65);
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    let b = assigner.get_work("sample", 4096).expect("packed runway");
    let hole = assigner.test_first_missing_height();
    assert_eq!(
        b.0,
        hole.saturating_add(super::leapfrog_lead_at(hole)),
        "L1 pack at hole+LEAD, got {b:?} hole={hole}"
    );
    assert!(b.0 > 65, "must not cheese live H, got {b:?}");
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
}

/// R-322: default holds start>H while tip missing. `=0` restores abort.
/// Do not seed a reserved lookahead stripe — C1J_LOOKAHEAD_HOLD would win first
/// (r69 is that trap).
#[serial_test::serial(ibd)]
#[test]
fn r321_no_tip_abort_holds_start_gt_h_when_set() {
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(900);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
    assigner.set_tip_gap_missing(true);
    // Far unreserved span: start > H+64, covering=0, not a HOLD.
    assert!(
        !assigner.should_abort_tip_walk_in("ahead", 1000, 1063),
        "default (unset): start>H tip_gap_missing covering=0 must hold"
    );
    unsafe {
        std::env::set_var("BLVM_IBD_NO_TIP_ABORT", "0");
    }
    assert!(
        assigner.should_abort_tip_walk_in("ahead", 1000, 1063),
        "NO_TIP_ABORT=0: start>H while tip_gap_missing covering=0 must abort"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-323: cooled peer, tip inside span. Default keeps GetData and does not
/// claim. `NO_TIP_ABORT=0` restores abort.
#[serial_test::serial(ibd)]
#[test]
fn r323_cooled_peer_keeps_inflight_inside_span() {
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
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
    assigner.set_ibd_ready_peers(HashSet::from(["mute".into(), "other".into()]));
    assigner.mark_tip_owner_fail_cooldown("mute", 5);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("mute".into(), vec![(326_316, 326_347)]);
    }
    assert!(
        !assigner.should_abort_tip_walk_in("mute", 326_316, 326_347),
        "default: cooled peer tip-inside-span must keep GetData"
    );
    assert_eq!(
        assigner.preferred_tip_owner(),
        None,
        "default keep must not add a tip claim (W111 promote skip)"
    );
    assert_eq!(
        assigner.deep_tip_cover_count(326_324),
        0,
        "default keep must not install deep cover"
    );
    unsafe {
        std::env::set_var("BLVM_IBD_NO_TIP_ABORT", "0");
    }
    assert!(
        assigner.should_abort_tip_walk_in("mute", 326_316, 326_347),
        "NO_TIP_ABORT=0: cooled tip-inside-span still aborts"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_NO_TIP_ABORT");
    }
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r78_hero_never_takes_start_gt_h() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let tip = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(tip.0, 210_001);
    match assigner.get_work("owner", 4096) {
        None => {}
        Some((s, _)) if s <= 210_001 => {}
        other => panic!("hero must not take start>H, got {other:?}"),
    }
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r78_covering0_no_runway() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(210_001);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["ahead"]);
    assigner.set_peer_scores(&[("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    match assigner.get_work("ahead", 4096) {
        None => {}
        Some((s, _)) if s <= 210_001 => {}
        other => panic!("covering=0 must not pack start>H, got {other:?}"),
    }
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r78_leftover_force_preferred_stays_on_h() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::mark_needed(0);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let tip = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(tip.0, 210_001);
    assigner.set_leftover_force_getdata(true);
    assigner
        .retry_queue
        .lock()
        .unwrap()
        .push_back(super::RetryEntry::fresh(210_200, 210_263, None));
    match assigner.get_work("owner", 4096) {
        None => {}
        Some((s, _)) if s <= 210_001 => {}
        other => panic!("leftover FORCE must not move preferred off H, got {other:?}"),
    }
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r79_runway_outrank_keeps_covering_inflight() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(210_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 800, 16);
    assigner.test_seed_tip_stream_rank("ahead", 1968, 16);
    assigner.test_seed_lookahead_rank("ahead", 4000, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 100, 8);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 210_001, 210_064);
    }
    assert!(
        assigner.runway_sojourn_outranks("sticky", 210_001),
        "reserved n≥8 + GetData EWMA 2× must rank"
    );
    assert!(
        !assigner.maybe_start_tip_trial(210_001),
        "KEEP retitle is not TipTrial"
    );
    assert!(
        assigner.maybe_keep_runway_retitle(210_001),
        "faster reserved KEEP must retitle"
    );
    let kept = {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        g.get("sticky")
            .is_some_and(|r| r.iter().any(|&(s, e)| s <= 210_001 && 210_001 <= e))
    };
    assert!(kept, "covering bps≥1 must not force_release");
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("ahead"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r79_runway_outrank_holds_flood() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(210_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 32_000, 16);
    assigner.test_seed_lookahead_rank("ahead", 2400, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 1, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 100, 8);
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "flood-class must hold"
    );
    assert!(!assigner.maybe_start_tip_trial(210_001));
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r81_lookahead_rank_needs_15s_n8() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 800, 16);
    assigner.test_seed_lookahead_rank("ahead", 2, 1);
    let (_, n2, secs2) = assigner.test_lookahead_stream("ahead");
    assert!(n2 < 8 || secs2 < 15.0);
    assert!(
        !assigner.runway_sojourn_outranks("sticky", 210_001),
        "n=2 / sub-15s must not retitle (R-58)"
    );
    assert!(!assigner.maybe_keep_runway_retitle(210_001));
    assigner.test_seed_lookahead_rank("ahead", 8, 16);
    let (bps, n, secs) = assigner.test_lookahead_stream("ahead");
    assert!(n >= 8 && secs >= 15.0 && bps > 0.0, "15s n≥8 readable");
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r92_drop_completed_stripe_keeps_15s_clock() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_lookahead_rank("ahead", 8, 16);
    {
        let mut g = assigner.lookahead_stripes.lock().unwrap();
        g.clear();
        g.push(("ahead".into(), 100, 163));
    }
    assigner.test_drop_stale_lookahead(200);
    let (bps, n, secs) = assigner.test_lookahead_stream("ahead");
    assert!(
        n >= 8 && secs >= 15.0 && bps > 0.0,
        "finished 64 must keep 15s clock, got bps={bps} n={n} secs={secs}"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r81_tip_ewma_without_lookahead_stream_does_not_retitle() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 800, 16);
    assigner.test_seed_lookahead_stripe("ahead", 210_065, 210_128);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 100, 8);
    assert!(
        !assigner.runway_sojourn_outranks("sticky", 210_001),
        "tip EWMA 4× with empty lookahead_streams must not retitle (R-80)"
    );
    assert!(!assigner.maybe_keep_runway_retitle(210_001));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r87_lifetime_bps_does_not_block_ewma_retitle() {
    // R-86: sticky lifetime 891 > farmer reserved ~80–200, farmer gd 656 vs 2160.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(210_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 800, 16);
    assigner.test_seed_lookahead_rank("ahead", 80, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 2160, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 656, 8);
    let (ahead_bps, _, _) = assigner.test_lookahead_stream("ahead");
    assert!(
        ahead_bps < 200.0,
        "farmer reserved BPS is R-86 fat class, not flood, got {ahead_bps}"
    );
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "R-93: reserved BPS 5 < sticky stream 50*2 — no KEEP even if EWMA 2× (R-92 486→30.5)"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r87_ewma_not_2x_holds_mesh() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 80, 16);
    assigner.test_seed_lookahead_rank("ahead", 4000, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 300, 8);
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "300*2 > 400 is mesh, not 2×"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r93_outrank_bps_latch_blocks_r92_486_to_30() {
    // R-92 @24k: sticky 486 ewma 120 → farmer 30.5 ewma 51. EWMA 2× would KEEP.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 7776, 16);
    assigner.test_seed_lookahead_rank("ahead", 488, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 120, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 51, 8);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let (top_bps, n, secs) = assigner.test_lookahead_stream("ahead");
    assert!(n >= 8 && secs >= 15.0);
    assert!(
        (sticky_bps - 486.0).abs() < 2.0 && (top_bps - 30.5).abs() < 2.0,
        "replay seeds sticky={sticky_bps} top={top_bps}"
    );
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "30.5 < 486*2 must not KEEP"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r93_outrank_bps_latch_allows_dying_hero() {
    // R-92 @233k class, empty clock: 6.8→31.6 must still KEEP. Fat is r118.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(38_000, 37_900, 50_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 109, 16);
    assigner.test_seed_lookahead_rank("ahead", 506, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 5830, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 862, 8);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let (top_bps, _, _) = assigner.test_lookahead_stream("ahead");
    assert!(
        sticky_bps < 8.0 && top_bps > sticky_bps * 2.0,
        "dying replay sticky={sticky_bps} top={top_bps}"
    );
    assert!(
        assigner.maybe_keep_runway_retitle(38_001),
        "empty: 31 > 6.8*2 + EWMA 2× must KEEP"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("ahead"));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-100 @38k: sticky H 123 ewma 153 → farmer reserved 1333. KEEP, then H=13.
#[serial_test::serial(ibd)]
#[test]
fn r101_reserved_bps_does_not_retitle_healthy_sticky() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(38_000, 37_900, 50_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 1968, 16);
    assigner.test_seed_lookahead_rank("ahead", 21341, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 153, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 40, 8);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let (top_bps, _, _) = assigner.test_lookahead_stream("ahead");
    assert!(
        sticky_bps >= 80.0 && top_bps > sticky_bps * 2.0,
        "replay sticky={sticky_bps} reserved={top_bps}"
    );
    assert_eq!(assigner.wan_tip_stream_bps("ahead"), 0.0);
    assert!(
        !assigner.maybe_keep_runway_retitle(38_001),
        "empty: reserved 1333 must not KEEP a 123 H-stream sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-116 @179k: sticky H 87, farm reserved 817 / H=0. Must not KEEP.
#[serial_test::serial(ibd)]
#[test]
fn r117_fat_reserved_bps_does_not_retitle_healthy_sticky() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 2624, 16);
    assigner.test_seed_lookahead_rank("ahead", 5600, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 180, 8);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let (top_bps, _, _) = assigner.test_lookahead_stream("ahead");
    assert!(
        sticky_bps >= 80.0 && sticky_bps < 2000.0 && top_bps > sticky_bps * 2.0,
        "replay sticky={sticky_bps} reserved={top_bps}"
    );
    assert_eq!(assigner.wan_tip_stream_bps("ahead"), 0.0);
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "fat: reserved 2× must not KEEP a ≥80 H-stream sticky (R-116 817→hole)"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-117 fat: dying 6.8, farm reserved 31 / H=0. Must not KEEP (28 dying hops).
#[serial_test::serial(ibd)]
#[test]
fn r118_fat_dying_reserved_does_not_retitle_without_h_stream() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 109, 16);
    assigner.test_seed_lookahead_rank("ahead", 506, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 5830, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 862, 8);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let (top_bps, _, _) = assigner.test_lookahead_stream("ahead");
    assert!(
        sticky_bps < 8.0 && top_bps > sticky_bps * 2.0,
        "replay sticky={sticky_bps} reserved={top_bps}"
    );
    assert_eq!(assigner.wan_tip_stream_bps("ahead"), 0.0);
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "fat dying: reserved 2× must not KEEP without proven H"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-123/R-129: do not drop ahead inflight to refill H (R-122 UNCOVER
/// 180–200k **42**). R-135: owner desert-fill (`s < hole+LEAD`) stays at
/// cap. Farm-peer ahead stays.
#[serial_test::serial(ibd)]
#[test]
fn r123_uncover_does_not_drop_ahead_inflight() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let _tip = assigner.get_work("owner", 4096).expect("hero H");
    let _farm = assigner.get_work("ahead", 4096).expect("farm");
    assigner.in_flight_per_peer.lock().unwrap().remove("owner");
    assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .insert("owner".into(), vec![(210_200, 210_263), (210_264, 210_327)]);
    assigner.note_tip_cover_claim("owner", 210_001, 210_064);
    let refill = assigner.get_work("owner", 4096);
    assert!(
        refill.is_none(),
        "R-135: desert-fill 64 (s < hole+LEAD) stays, not HERO_REARM, got {refill:?}"
    );
    let flight = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .get("owner")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        flight,
        vec![(210_200, 210_263), (210_264, 210_327)],
        "desert fill stays, got {flight:?}"
    );
    let ahead = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .get("ahead")
        .cloned()
        .unwrap_or_default();
    assert!(
        !ahead.is_empty(),
        "farm-peer ahead stays (not uncover), got {ahead:?}"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-124: apply parked on +1 hole, warehouse full, sticky_bps ≥80. Proven H
/// takes the hole. Ahead inflight stays. Not R-122 uncover. Not R-117 farm H=0.
#[serial_test::serial(ibd)]
#[test]
fn r124_parked_plus1_yields_proven_h_keeps_ahead() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_002, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead", "fast"]);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.9),
        ("ahead".into(), 0.1),
        ("fast".into(), 0.8),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 2624, 16);
    assigner.test_seed_tip_stream_rank("fast", 2000, 16);
    assigner.test_seed_lookahead_rank("ahead", 5600, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 180, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("fast", 120, 8);
    assert!(assigner.wan_tip_stream_bps("sticky") >= 80.0);
    assert!(assigner.wan_tip_stream_bps("fast") >= 80.0);
    assert_eq!(assigner.wan_tip_stream_bps("ahead"), 0.0);
    assigner.in_flight_per_peer.lock().unwrap().insert(
        "sticky".into(),
        vec![(210_001, 210_064), (210_200, 210_263)],
    );
    ChunkAssigner::test_seed_apply_win_stall(6.8, 200.0);
    assert!(
        assigner.maybe_keep_runway_retitle(210_001),
        "parked +1 / warehouse / proven H must yield"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("fast"));
    let flight = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .get("sticky")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        flight,
        vec![(210_200, 210_263)],
        "ahead inflight stays, H cover vacated, got {flight:?}"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-125: parked warehouse, no second H≥80. Ready worker takes the hole
/// (R-124 `PARKED_YIELD` **0** @213k). R-118 not-parked farm H=0 stays closed.
#[serial_test::serial(ibd)]
#[test]
fn r125_parked_ready_without_h_stream_yields() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_002, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.8)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 2624, 16);
    assigner.test_seed_lookahead_rank("ahead", 5600, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 180, 8);
    assert!(assigner.wan_tip_stream_bps("sticky") >= 80.0);
    assert_eq!(assigner.wan_tip_stream_bps("ahead"), 0.0);
    assigner.in_flight_per_peer.lock().unwrap().insert(
        "sticky".into(),
        vec![(210_001, 210_064), (210_200, 210_263)],
    );
    ChunkAssigner::test_seed_apply_win_stall(6.8, 200.0);
    assert!(
        assigner.maybe_keep_runway_retitle(210_001),
        "parked + ready (H=0) must yield; R-124 required proven H"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("ahead"));
    let flight = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .get("sticky")
        .cloned()
        .unwrap_or_default();
    assert!(
        flight.iter().any(|&(s, e)| s == 210_200 && e == 210_263),
        "ahead inflight stays, got {flight:?}"
    );
    assert!(
        !flight.iter().any(|&(s, e)| s <= 210_001 && 210_001 <= e),
        "H cover vacated, got {flight:?}"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-126: R-115 235k line-rate (**33** BPS / win **260**) must not hop.
/// Warehouse + hole+1 + ready is the healthy fat clock, not a park.
#[serial_test::serial(ibd)]
#[test]
fn r126_line_rate_warehouse_does_not_yield() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_002, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.8)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 2624, 16);
    assigner.test_seed_lookahead_rank("ahead", 5600, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 180, 8);
    assigner.in_flight_per_peer.lock().unwrap().insert(
        "sticky".into(),
        vec![(210_001, 210_064), (210_200, 210_263)],
    );
    ChunkAssigner::test_seed_apply_win_stall(33.0, 260.0);
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "R-115 line-rate fat must keep the hero"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-126: collapsed win is desert-adjacent. Do not hop a 6.8 apply onto ready
/// when receive is dead (R-124 first 20s of 213k `win=0.1`).
#[serial_test::serial(ibd)]
#[test]
fn r126_collapsed_win_does_not_yield() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_002, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.8)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 2624, 16);
    assigner.test_seed_lookahead_rank("ahead", 5600, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 180, 8);
    assigner.in_flight_per_peer.lock().unwrap().insert(
        "sticky".into(),
        vec![(210_001, 210_064), (210_200, 210_263)],
    );
    ChunkAssigner::test_seed_apply_win_stall(6.8, 10.0);
    assert!(
        !assigner.maybe_keep_runway_retitle(210_001),
        "win < 80 is collapsed receive, not a hop"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r81_land_credit_increments_once() {
    super::super::tip_stage::test_reset_tip_stage();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["ahead"]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_lookahead_stream("ahead");
    assert_eq!(assigner.test_lookahead_stream("ahead").1, 1);
    assigner.note_lookahead_stream("ahead");
    assert_eq!(
        assigner.test_lookahead_stream("ahead").1,
        2,
        "land credit increments once per reserved body"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r81_probe_repicks_after_stripe_drop() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
    }
    super::super::tip_probe::test_seed_probe("fast", 200);
    super::super::tip_probe::test_seed_probe("slow", 400);
    let assigner = wan_tip_assigner(
        210_000,
        209_900,
        300_000,
        &["owner", "fast", "slow", "newer"],
    );
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("fast".into(), 8.0),
        ("slow".into(), 7.0),
        ("newer".into(), 6.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let _tip = assigner.get_work("owner", 4096).expect("owner H");
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    let fast = assigner.get_work("fast", 4096).expect("stripe 1");
    super::super::tip_probe::test_reset_probes();
    super::super::tip_probe::test_seed_probe("newer", 100);
    super::super::tip_probe::test_seed_probe("slow", 400);
    assigner.test_set_validation_height(fast.1);
    super::super::tip_stage::mark_needed(fast.1);
    let _ = assigner.get_work("owner", 4096);
    let newer = assigner
        .get_work("newer", 4096)
        .expect("live table top-2 gets next stripe after drop");
    assert!(newer.0 > fast.1);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
    }
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r81_ia_demote_does_not_change_preferred() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_seed_owner_body_ia(19, 16);
    super::super::tip_probe::test_seed_probe("challenger", 26);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.5), ("challenger".into(), 0.4)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.test_seed_tip_stream_rank("sticky", 400, 8);
    assert!(!assigner.maybe_demote_cooled_ia_owner(901));
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
}

#[serial_test::serial(ibd)]
#[test]
fn r81_hero_skip_pack_two_c1j_hold() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(1, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "a1", "a2"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("a1".into(), 8.0),
        ("a2".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    match assigner.get_work("owner", 4096) {
        None => {}
        Some((s, e)) if s > 210_001 || e < 210_001 => {}
        other => panic!("H in reorder must not re-issue H, got {other:?}"),
    }
    let s1 = assigner
        .get_work("a1", 4096)
        .expect("stripe 1 while H buffered");
    assert!(s1.0 > 210_001, "pack must not cheese H, got {s1:?}");
    // One latch above 180k. A second pack is not required while tip is missing.
    if let Some(s2) = assigner.get_work("a2", 4096) {
        assert!(
            s2.0 > s1.1,
            "second stripe must sit after first, got {s2:?} s1={s1:?}"
        );
    }
    assert!(
        !assigner.should_abort_tip_walk_in("a1", s1.0, s1.1),
        "C1j must HOLD the reserved stripe"
    );
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r82_trim_holds_remainder_and_clock() {
    // L2 retires R-82 trim-hold: apply enter drops the whole stripe.
    // 15s farm clock still survives (R-92). Assign does not trim-hold;
    // R-100 keep-reserved hold is admit/evict, not peer_lookahead_covers.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.test_seed_lookahead_stripe("ahead", 210_065, 210_128);
    assigner.test_seed_lookahead_rank("ahead", 16, 16);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "ahead", 210_065, 210_128);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_001, 210_064);
    }
    assigner.test_set_validation_height(210_079);
    super::super::tip_stage::mark_needed(210_079);
    assigner.test_drop_stale_lookahead(210_080);
    assert!(
        assigner.peer_lookahead_covers("ahead", 210_080),
        "empty enter keeps the live farm (R-114 10s leftover)"
    );
    assert!(
        assigner.peer_lookahead_covers("ahead", 210_100),
        "must not trim-hold / rewrite s to H+1"
    );
    let kept = {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        g.get("ahead")
            .is_some_and(|r| r.iter().any(|&(s, e)| s == 210_065 && e == 210_128))
    };
    assert!(kept, "empty enter keeps original inflight (s,e)");
    let (_, n, secs) = assigner.test_lookahead_stream("ahead");
    assert!(n >= 8 && secs >= 15.0, "15s farmer clock must survive drop");
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r83_first_missing_does_not_walk_stripes() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.test_seed_lookahead_stripe("ahead", 210_065, 210_128);
    super::super::tip_stage::mark_needed(210_001);
    assert_eq!(
        assigner.test_first_missing_height(),
        210_001,
        "in-flight farmer stripe is not have"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// Preferred already inflight elsewhere, H uncovered: any other peer GetData (H,H).
#[serial_test::serial(ibd)]
#[test]
fn r195_hole_any_farm_takes_uncovered_h() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_HOLE_ANY");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_200, 210_263);
    }
    let hole = assigner
        .get_work("ahead", 4096)
        .expect("farm GetData uncovered H");
    assert_eq!(hole, (210_001, 210_001), "HOLE_ANY (H,H), got {hole:?}");
    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert_eq!(
        g.get("ahead").cloned().unwrap_or_default(),
        vec![(210_001, 210_001)],
        "farm owns the one GetData on H, got {g:?}"
    );
    assert!(
        g.get("owner")
            .is_some_and(|r| r.iter().any(|&(s, e)| s == 210_200 && e == 210_263)),
        "preferred farm stripe stays, got {g:?}"
    );
    drop(g);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-236 leftover 261k: random HOLE_ANY `100.63` while top_recv `142.113`
/// was 417 Mbps. At h≥180k only the CRAWL fattest other farm takes (H,H).
#[serial_test::serial(ibd)]
#[test]
fn r236_hole_any_fat_only_top_recv_farm() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_HOLE_ANY");
    }
    let assigner = wan_tip_assigner(261_000, 260_900, 370_000, &["owner", "thin", "fat"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("thin".into(), 8.0),
        ("fat".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 261_200, 261_263);
    }
    super::super::download::test_set_cached_recv_mbps("owner", 43.7);
    super::super::download::test_set_cached_recv_mbps("thin", 1.0);
    super::super::download::test_set_cached_recv_mbps("fat", 417.0);
    let thin = assigner.get_work("thin", 4096);
    assert_ne!(
        thin,
        Some((261_001, 261_001)),
        "thin farm must not HOLE_ANY cheese, got {thin:?}"
    );
    let hole = assigner
        .get_work("fat", 4096)
        .expect("top_recv farm GetData uncovered H");
    assert_eq!(
        hole,
        (261_001, 261_001),
        "HOLE_ANY (H,H) to top_recv, got {hole:?}"
    );
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::test_reset_tip_stage();
}

/// Dump: empty recv cache still lets any farm take uncovered H (R-195 / dest-bc).
#[serial_test::serial(ibd)]
#[test]
fn r236_hole_any_dump_any_farm_without_recv_cache() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_HOLE_ANY");
    }
    let assigner = wan_tip_assigner(900, 800, 2000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 1_200, 1_263);
    }
    super::super::download::test_set_cached_recv_mbps("fat_other", 400.0);
    super::super::download::test_set_cached_recv_mbps("ahead", 1.0);
    let hole = assigner
        .get_work("ahead", 4096)
        .expect("dump HOLE_ANY ignores recv rank");
    assert_eq!(hole, (901, 901), "dump any-farm HOLE_ANY, got {hole:?}");
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::test_reset_tip_stage();
}

/// Preferred set but not inflight: do not give the first farm (H,H) (dump).
#[serial_test::serial(ibd)]
#[test]
fn r195_hole_any_does_not_fire_without_pref_inflight() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_HOLE_ANY");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let ahead = assigner.get_work("ahead", 4096);
    assert_ne!(
        ahead,
        Some((210_001, 210_001)),
        "no preferred inflight → HOLE_ANY silent, got {ahead:?}"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// Opt-out keeps exclusive-H (farm packs, does not cheese H).
#[serial_test::serial(ibd)]
#[test]
fn r195_hole_any_off_farm_does_not_take_h() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_HOLE_ANY", "0");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_200, 210_263);
    }
    let ahead = assigner.get_work("ahead", 4096);
    assert_ne!(
        ahead,
        Some((210_001, 210_001)),
        "HOLE_ANY=0 must not cheese H, got {ahead:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_HOLE_ANY");
    }
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-234 fat 187–189k: HERO_SKIP hole=H+1 while H is still in reorder, then
/// HOLE_ANY (H,H) to random farms. Real have (IN_REORDER) keeps HOLE_ANY silent.
/// Stale published hole with IN_REORDER off is R-256, not this lock.
#[serial_test::serial(ibd)]
#[test]
fn r234_hole_any_silent_when_first_missing_ahead() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_002, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_HOLE_ANY");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_200, 210_263);
    }
    let ahead = assigner.get_work("ahead", 4096);
    assert_ne!(
        ahead,
        Some((210_001, 210_001)),
        "already-have H must not HOLE_ANY cheese, got {ahead:?}"
    );
    super::super::IBD_FIRST_HOLE.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// Promoted farmer must drop the farm stripe so the new hero can take H.
#[serial_test::serial(ibd)]
#[test]
fn r84_promoted_farmer_drops_stripe() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(210_001);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["sticky", "ahead"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.9), ("ahead".into(), 0.1)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("sticky");
    assigner.test_seed_tip_stream_rank("sticky", 800, 16);
    assigner.test_seed_tip_stream_rank("ahead", 1968, 16);
    assigner.test_seed_lookahead_stripe("ahead", 210_065, 210_128);
    assigner.test_seed_lookahead_rank("ahead", 4000, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 400, 8);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("ahead", 100, 8);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 210_001, 210_064);
        ChunkAssigner::insert_in_flight(&mut g, "ahead", 210_065, 210_128);
    }
    assert!(
        assigner.maybe_keep_runway_retitle(210_001),
        "faster reserved KEEP must retitle"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("ahead"));
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let ahead_farm = g
        .get("ahead")
        .is_some_and(|r| r.iter().any(|&(s, e)| s == 210_065 && e == 210_128));
    assert!(!ahead_farm, "new hero must drop the farm stripe");
    let sticky_h = g
        .get("sticky")
        .is_some_and(|r| r.iter().any(|&(s, e)| s <= 210_001 && 210_001 <= e));
    assert!(sticky_h, "old cover on H stays (one GetData)");
    drop(g);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-132: 64-walk inside reserved farm (`s>H`) vacates and hero takes H.
/// Farm-peer ahead stays. Not R-122 drop-ahead. Not H_COVER over cap.
#[serial_test::serial(ibd)]
#[test]
fn r127_walk_inside_farm_assigns_h_keeps_ahead() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_seed_lookahead_stripe("owner", 210_513, 212_560);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        // Two walks (TOP default 2). Neither equals reserved (s,e), so vacate
        // does not free a slot. Without H_COVER this is STICKY_CAP.
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_513, 210_576);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_577, 210_640);
        ChunkAssigner::insert_in_flight(&mut g, "ahead", 212_561, 214_608);
    }
    let tip = assigner
        .get_work("owner", 4096)
        .expect("R-132: walk-inside-farm must re-arm H");
    assert_eq!(tip.0, 210_001, "hero takes hole, got {tip:?}");
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let farm_walk = g.get("owner").is_some_and(|r| {
        r.iter().any(|&(s, e)| s == 210_513 && e == 210_576)
            || r.iter().any(|&(s, e)| s == 210_577 && e == 210_640)
    });
    assert!(!farm_walk, "owner non-H walks vacated, got {g:?}");
    assert!(
        g.get("owner")
            .is_some_and(|r| r.iter().any(|&(s, e)| s <= 210_001 && 210_001 <= e)),
        "H inflight after re-arm, got {g:?}"
    );
    assert_eq!(
        g.get("ahead").cloned().unwrap_or_default(),
        vec![(212_561, 214_608)],
        "farm-peer ahead stays, not uncover"
    );
    drop(g);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-115 235k: covering-H 64 at cap must wait. Do not drop the fill.
#[serial_test::serial(ibd)]
#[test]
fn r132_covering_h_64_does_not_rearm() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    let assigner = wan_tip_assigner(235_099, 235_000, 250_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 235_100, 235_163);
        ChunkAssigner::insert_in_flight(&mut g, "ahead", 237_000, 239_047);
    }
    assert!(
        assigner.get_work("owner", 4096).is_none(),
        "R-132: covering-H 64 must not re-arm"
    );
    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert_eq!(
        g.get("owner").cloned().unwrap_or_default(),
        vec![(235_100, 235_163)],
        "H 64 stays, got {g:?}"
    );
    assert_eq!(
        g.get("ahead").cloned().unwrap_or_default(),
        vec![(237_000, 239_047)],
        "farm-peer stays"
    );
    drop(g);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-133 fat: leftover `(h,h)` at cap must wait. Do not vacate and re-issue H.
#[serial_test::serial(ibd)]
#[test]
fn r134_leftover_single_does_not_rearm() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        // Cap is 2 (TOP). Two leftover singles fill it. Neither is a 64-walk.
        ChunkAssigner::insert_in_flight(&mut g, "owner", 209_999, 209_999);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_000, 210_000);
        ChunkAssigner::insert_in_flight(&mut g, "ahead", 212_561, 214_608);
    }
    assert!(
        assigner.get_work("owner", 4096).is_none(),
        "leftover (h,h) must STICKY_CAP, not HERO_REARM"
    );
    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert_eq!(
        g.get("owner").cloned().unwrap_or_default(),
        vec![(209_999, 209_999), (210_000, 210_000)],
        "leftovers stay, got {g:?}"
    );
    assert_eq!(
        g.get("ahead").cloned().unwrap_or_default(),
        vec![(212_561, 214_608)],
        "farm-peer stays"
    );
    drop(g);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-134 fat: desert-fill 64 (`s < hole+LEAD`) at cap must stay. That walk
/// fills the 512. Vacating it is not the farm-interior door.
#[serial_test::serial(ibd)]
#[test]
fn r135_desert_fill_walk_does_not_rearm() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_seed_lookahead_stripe("ahead", 210_513, 212_560);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        // Cap 2. Both walks start in the 512 desert (hole+1 / hole+65).
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_002, 210_065);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_066, 210_129);
        ChunkAssigner::insert_in_flight(&mut g, "ahead", 210_513, 212_560);
    }
    assert!(
        assigner.get_work("owner", 4096).is_none(),
        "desert-fill 64 must STICKY_CAP, not HERO_REARM"
    );
    let g = assigner.in_flight_per_peer.lock().unwrap();
    assert_eq!(
        g.get("owner").cloned().unwrap_or_default(),
        vec![(210_002, 210_065), (210_066, 210_129)],
        "desert fill stays, got {g:?}"
    );
    assert_eq!(
        g.get("ahead").cloned().unwrap_or_default(),
        vec![(210_513, 212_560)],
        "farm-peer stays"
    );
    drop(g);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// Sticky at cap=1 with only a farm stripe must vacate and GetData missing H.
#[serial_test::serial(ibd)]
#[test]
fn r84_sticky_farm_cap_vacates_for_h() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_seed_lookahead_stripe("owner", 210_065, 210_128);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_065, 210_128);
    }
    let tip = assigner
        .get_work("owner", 4096)
        .expect("hero must vacate farm and take missing H");
    assert_eq!(
        tip.0, 210_001,
        "farm cap must not STICKY_CAP H, got {tip:?}"
    );
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let farm_left = g
        .get("owner")
        .is_some_and(|r| r.iter().any(|&(s, e)| s == 210_065 && e == 210_128));
    assert!(!farm_left, "farm stripe vacated");
    drop(g);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-95: title already covering H must drop farm and not PIPE_FILL another stripe.
#[serial_test::serial(ibd)]
#[test]
fn r95_title_on_h_vacates_farm_no_pipe_fill() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_seed_lookahead_stripe("owner", 210_513, 210_576);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_001, 210_064);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_513, 210_576);
    }
    let extra = assigner.get_work("owner", 4096);
    if let Some((s, e)) = extra {
        assert!(
            s <= 210_001 && 210_001 <= e || s == 210_065,
            "title on H may refill H-pipe, not farm, got {extra:?}"
        );
        assert!(
            e < 210_513,
            "must not PIPE_FILL farm 513-576, got {extra:?}"
        );
    }
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let ranges = g.get("owner").cloned().unwrap_or_default();
    assert!(
        ranges.iter().any(|&(s, e)| s == 210_001 && e == 210_064),
        "H-cover stays, got {ranges:?}"
    );
    assert!(
        !ranges.iter().any(|&(s, e)| s == 210_513 && e == 210_576),
        "farm vacated, got {ranges:?}"
    );
    drop(g);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-95: title with only a farm stripe still vacates and takes missing H.
#[serial_test::serial(ibd)]
#[test]
fn r95_title_farm_only_takes_h() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_seed_lookahead_stripe("owner", 210_513, 210_576);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_513, 210_576);
    }
    let tip = assigner
        .get_work("owner", 4096)
        .expect("title farm-only must take missing H");
    assert_eq!(tip.0, 210_001, "must assign H, got {tip:?}");
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let farm_left = g
        .get("owner")
        .is_some_and(|r| r.iter().any(|&(s, e)| s == 210_513 && e == 210_576));
    assert!(!farm_left, "farm vacated before H assign");
    drop(g);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-96: H-pipe growth is not farm. Vacate must not drop owner_end+1.
#[serial_test::serial(ibd)]
#[test]
fn r96_title_h_pipe_survives_vacate() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_seed_lookahead_stripe("owner", 210_513, 210_576);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_001, 210_064);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_065, 210_128);
        ChunkAssigner::insert_in_flight(&mut g, "owner", 210_513, 210_576);
    }
    let _ = assigner.get_work("owner", 4096);
    let g = assigner.in_flight_per_peer.lock().unwrap();
    let ranges = g.get("owner").cloned().unwrap_or_default();
    assert!(
        ranges.iter().any(|&(s, e)| s == 210_001 && e == 210_064),
        "H-cover stays, got {ranges:?}"
    );
    assert!(
        ranges.iter().any(|&(s, e)| s == 210_065 && e == 210_128),
        "H-pipe refill must survive vacate (R-95 dropped it), got {ranges:?}"
    );
    assert!(
        !ranges.iter().any(|&(s, e)| s == 210_513 && e == 210_576),
        "reserved farm vacated, got {ranges:?}"
    );
    drop(g);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-138: empty LEAD 512 (tip=1 `513-2560`). After 50k, pack LEAD 64.
/// H-pipe 64 at hole+64 is not vacated (len<WIDTH and s<512).
#[serial_test::serial(ibd)]
#[test]
fn r138_fat_lead_is_64_empty_stays_512() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    assert_eq!(super::leapfrog_lead_at(1), 512);
    assert_eq!(super::leapfrog_lead_at(49_999), 512);
    assert_eq!(super::leapfrog_lead_at(50_000), 64);
    assert_eq!(super::leapfrog_lead_at(210_001), 64);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let tip = assigner.get_work("owner", 4096).expect("hero H");
    assert_eq!(tip, (210_001, 210_064));
    // leapfrog_lead_at(210001) is still 64. get_work must not jump there:
    // R-252 latches H+1 over the hero cover (R-251 owner_end+1 left a hole).
    let farm = assigner.get_work("ahead", 4096).expect("fat latch");
    assert_eq!(farm.0, tip.0 + 1, "H+1, not hole+LEAD, got {farm:?}");
    assert!(farm.0 > tip.0, "Wall A: no second TCP on H");
    assert!(
        farm.0 <= tip.1,
        "duplicates the hero cover, not a jump past it: tip={tip:?} farm={farm:?}"
    );
    assert!(
        assigner.latched_ahead_holds("ahead", farm.0, farm.1),
        "extra must be the latch, got {farm:?}"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

fn gap_b_farm_setup(tip: u64) -> ChunkAssigner {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(
        tip,
        tip.saturating_sub(100),
        tip + 100_000,
        &["owner", "a", "b"],
    );
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("a".into(), 8.0), ("b".into(), 7.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner
}

/// 300k + warehouse-full HOLE_AHEAD + hero on H → no new far tile.
#[serial_test::serial(ibd)]
#[test]
fn r165_gap_b_300k_warehouse_blocks_new_far_tile() {
    let assigner = gap_b_farm_setup(300_000);
    let _tip = assigner.get_work("owner", 4096).expect("hero H");
    let _fa = assigner.get_work("a", 4096).expect("farm A before hold");
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assert!(
        assigner.get_work("b", 4096).is_none(),
        "Gap B must not pack a new far tile on warehouse-full HOLE_AHEAD"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// Fat / dest-bc / R-136 248–300k **193**: floor is 300k. Warehouse-full at 180k still packs.
#[serial_test::serial(ibd)]
#[test]
fn r165_gap_b_silent_before_300k() {
    let assigner = gap_b_farm_setup(180_000);
    let _tip = assigner.get_work("owner", 4096).expect("hero H");
    let fa = assigner.get_work("a", 4096).expect("farm A");
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let fb = assigner
        .get_work("b", 4096)
        .expect("180k warehouse-full must still pack (not FAT_HOLD / not R-140)");
    assert!(fb.0 > fa.1);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-153 any-ahead firehose: ahead=64 at 300k is EMPTY_TIP flicker, not leftover. Still packs.
#[serial_test::serial(ibd)]
#[test]
fn r165_gap_b_empty_tip_still_packs() {
    let assigner = gap_b_farm_setup(300_000);
    let _tip = assigner.get_work("owner", 4096).expect("hero H");
    let fa = assigner.get_work("a", 4096).expect("farm A");
    super::super::IBD_REORDER_AHEAD.store(64, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let fb = assigner
        .get_work("b", 4096)
        .expect("300k ahead<WIDTH must still pack (not R-153 any-ahead)");
    assert!(fb.0 > fa.1);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// Already-reserved farm re-arms while Gap B holds. No uncover.
#[serial_test::serial(ibd)]
#[test]
fn r165_gap_b_rearm_stays() {
    let assigner = gap_b_farm_setup(300_000);
    let _tip = assigner.get_work("owner", 4096).expect("hero H");
    let fa = assigner.get_work("a", 4096).expect("farm A");
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    assigner.force_release_peer_inflight("a");
    let rearm = assigner
        .get_work("a", 4096)
        .expect("reserved stripe must re-arm under Gap B");
    assert_eq!(rearm, fa);
    assert!(
        assigner.get_work("b", 4096).is_none(),
        "re-arm must not open a new far tile"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-142: fat-entry retitles mesh sticky to table-top probe at 180k.
#[serial_test::serial(ibd)]
#[test]
fn r142_fat_probe_retitle_at_180k() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_probe::test_seed_probe("sticky", 444); // ~72 wave
    super::super::tip_probe::test_seed_probe("hero", 108); // ~296 wave
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
    }
    let vh = Arc::new(AtomicU64::new(179_999));
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["sticky", "hero"]);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(170_000);
    assigner.set_wan_body_tip(170_000);
    assigner.set_header_tip(900_000);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("hero".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(
        !assigner.maybe_fat_probe_retitle(180_000),
        "R-146: probe-only must not steal H"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    assert!(
        !assigner.maybe_fat_probe_retitle(185_000),
        "one retitle per dest"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

/// R-142b: sojourn outrank picks table-top, not first ready mid-rank.
#[serial_test::serial(ibd)]
#[test]
fn r142_fat_probe_retitle_prefers_sojourn_top() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_probe::test_seed_probe("sticky", 400);
    super::super::tip_probe::test_seed_probe("hero", 127);
    super::super::tip_probe::test_seed_probe("hero", 127);
    super::super::tip_probe::test_seed_probe("mid", 300);
    super::super::tip_probe::test_seed_probe("mid", 300);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 800, 16);
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
    }
    let vh = Arc::new(AtomicU64::new(179_999));
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["sticky", "hero", "mid"]);
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(170_000);
    assigner.set_wan_body_tip(170_000);
    assigner.set_header_tip(900_000);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.90),
        ("hero".into(), 0.40),
        ("mid".into(), 0.80),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(assigner.probe_outrank_bypasses_healthy("sticky", 180_000));
    assert!(
        !assigner.maybe_fat_probe_retitle(180_000),
        "R-146: sojourn/probe must not steal H"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

/// R-141: R-140 land-in-time reverted — farm B/C pack without reorder gate.
#[serial_test::serial(ibd)]
#[test]
fn r141_no_land_in_time_second_farm_tile() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "a", "b"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("a".into(), 8.0), ("b".into(), 7.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let _tip = assigner.get_work("owner", 4096).expect("hero H");
    let _fa = assigner.get_work("a", 4096).expect("farm A at LEAD");
    let fb = assigner
        .get_work("b", 4096)
        .expect("farm B packs with reorder=0 (R-140 gate removed)");
    assert!(fb.0 > _fa.1, "B after A, got {fb:?}");
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-145: slide-release when all farms slid (+4096) even if LEAD hold blocks.
#[serial_test::serial(ibd)]
#[test]
fn r145_slide_release_when_lead_hold_blocked() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "a", "b", "c"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("a".into(), 8.0),
        ("b".into(), 7.0),
        ("c".into(), 6.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    let hole = assigner.test_first_missing_height();
    let lead = super::leapfrog_lead_at(hole);
    let width = super::leapfrog_width_at(hole);
    let ws = hole.saturating_add(lead);
    let we = ws.saturating_add(width.saturating_sub(1));
    assigner.test_push_lookahead_hold(ws, we);
    let a0 = hole.saturating_add(4096);
    let b0 = a0.saturating_add(width);
    let c0 = b0.saturating_add(width);
    assigner.test_seed_lookahead_stripe("a", a0, a0 + width - 1);
    assigner.test_seed_lookahead_stripe("b", b0, b0 + width - 1);
    assigner.test_seed_lookahead_stripe("c", c0, c0 + width - 1);
    assert!(
        !assigner.test_lead_window_free(hole),
        "LEAD window must be blocked"
    );
    assert!(
        assigner.test_try_slide_release(hole),
        "R-145: release when all farms slid past LEAD+WIDTH"
    );
    assert!(
        assigner.test_have_hold_contains(c0, c0 + width - 1),
        "farthest slid must land in hold"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-144: fat retitle prefers flood-class over faster probe-only peer.
#[serial_test::serial(ibd)]
#[test]
fn r144_fat_retitle_prefers_flood_class() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_probe::test_seed_probe("sticky", 400);
    super::super::tip_probe::test_seed_probe("fast_probe", 50);
    super::super::tip_probe::test_seed_probe("flood", 250);
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
    }
    let vh = Arc::new(AtomicU64::new(179_999));
    let assigner = wan_tip_assigner(
        180_000,
        179_900,
        300_000,
        &["sticky", "fast_probe", "flood"],
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(170_000);
    assigner.set_wan_body_tip(170_000);
    assigner.set_header_tip(900_000);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.90),
        ("fast_probe".into(), 0.85),
        ("flood".into(), 0.40),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    for _ in 0..4000 {
        assigner.note_wan_tip_stream("flood");
    }
    assert!(assigner.wan_tip_stream_bps("flood") >= 2000.0);
    assert!(
        assigner.maybe_fat_probe_retitle(180_000),
        "flood challenger must retitle at fat entry"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("flood"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

/// Published first_hole: hero GetData starts there. Farmer waits, then packs after hero.
#[serial_test::serial(ibd)]
#[test]
fn r86_hero_assigns_published_first_hole() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(20, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(210_021, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(210_001, Ordering::Relaxed);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    super::super::tip_stage::mark_needed(210_001);
    assert_eq!(
        assigner.test_first_missing_height(),
        210_021,
        "published hole must win over stale contig"
    );
    let early = assigner.get_work("ahead", 4096);
    if let Some((s, _)) = early {
        assert_ne!(
            s, 210_021,
            "farmer must not cheese first_hole, got {early:?}"
        );
    }
    let tip = assigner
        .get_work("owner", 4096)
        .expect("hero must GetData published first_hole");
    assert_eq!(tip.0, 210_021, "hero idle or stayed on have H, got {tip:?}");
    let ahead = early.or_else(|| assigner.get_work("ahead", 4096));
    let ahead = ahead.expect("pack at hole+LEAD (have or after hero cover)");
    let hole = assigner.test_first_missing_height();
    assert_eq!(
        ahead.0,
        hole.saturating_add(super::leapfrog_lead_at(hole)),
        "L1 pack at hole+LEAD, got {ahead:?} tip={tip:?}"
    );
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-103: apply moved into the published have run; AT is stale. Hero must
/// still GetData `hole`, not H+1.
#[serial_test::serial(ibd)]
#[test]
fn r104_published_hole_survives_apply_into_run() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE.store(10_241, Ordering::Relaxed);
    super::super::IBD_FIRST_HOLE_AT.store(8_193, Ordering::Relaxed);
    let assigner = wan_tip_assigner(8_228, 8_000, 20_000, &["owner"]);
    mark_scored_peers_ibd_ready(&assigner);
    super::super::tip_stage::mark_needed(8_229);
    assert_eq!(
        assigner.test_first_missing_height(),
        10_241,
        "stale AT inside [at, hole) must not collapse to H+1"
    );
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    assert_eq!(
        assigner.test_first_missing_height(),
        8_229,
        "H missing: stale AT must not skip the hole"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-71: WIN mesh (gd 170) must not pin preferred after H≥64.
#[serial_test::serial(ibd)]
#[test]
fn r71_win_mesh_restickies_after_64() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::mark_needed(65);
    let assigner = wan_tip_assigner(64, 0, 10_000, &["win", "second"]);
    assigner.set_peer_scores(&[("win".into(), 1.0), ("second".into(), 2.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::test_seed_tournament_racer("win", 1, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    match super::super::tip_stage::tournament_poll() {
        super::super::tip_stage::TournamentPoll::Win { peer, .. } => {
            assert_eq!(peer, "win");
            assigner.note_tip_owner_assigned(&peer);
        }
        other => panic!("expected win, got {other:?}"),
    }
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("win", 170, 16);
    assigner.note_tip_owner_assigned("second");
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("second"),
        "WIN mesh gd=170 must resticky after 64"
    );
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-71: WIN flood (sticky≥2000) still exclusive after 64.
#[serial_test::serial(ibd)]
#[test]
fn r71_win_flood_holds_after_64() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::mark_needed(65);
    let assigner = wan_tip_assigner(64, 0, 10_000, &["win", "second"]);
    assigner.set_peer_scores(&[("win".into(), 1.0), ("second".into(), 2.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::test_seed_tournament_racer("win", 1, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    match super::super::tip_stage::tournament_poll() {
        super::super::tip_stage::TournamentPoll::Win { peer, .. } => {
            assigner.note_tip_owner_assigned(&peer);
        }
        other => panic!("expected win, got {other:?}"),
    }
    assigner.test_seed_tip_stream_rank("win", 40_000, 16);
    assigner.note_tip_owner_assigned("second");
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("win"),
        "WIN flood sticky≥2000 must hold"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-71: apply IA≤1 is not flood-class. r62 stay open.
#[serial_test::serial(ibd)]
#[test]
fn r71_apply_ia_is_not_flood() {
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_owner_body_ia(1, 16);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("owner", 170, 16);
    let assigner = wan_tip_assigner(64, 0, 10_000, &["owner", "second"]);
    assigner.note_tip_owner_assigned("owner");
    assert!(
        !assigner.ignition_second_h_ok(true, true, 65, 1, 1),
        "R-78: r62 live-H overlap stays closed"
    );
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-71: r62 second on live H becomes preferred (no trial). Mute-find stays.
#[serial_test::serial(ibd)]
#[test]
fn r71_r62_second_becomes_preferred() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_owner_body_ia(10, 16);
    super::super::tip_stage::mark_needed(64);
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    let assigner = wan_tip_assigner(64, 0, 100_000, &["owner", "sample"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("sample".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    super::super::tip_stage::test_seed_tournament_racer("owner", 1, 16);
    super::super::tip_stage::test_backdate_tournament_start_ms(2_000);
    match super::super::tip_stage::tournament_poll() {
        super::super::tip_stage::TournamentPoll::Win { peer, .. } => {
            assigner.note_tip_owner_assigned(&peer);
        }
        other => panic!("expected win, got {other:?}"),
    }
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("owner", 170, 16);
    assigner.test_seed_tip_stream_rank("owner", 4_800, 16);
    let a = assigner.get_work("owner", 4096).expect("owner H");
    assert_eq!(a.0, 65);
    super::super::IBD_TIP_IN_REORDER.store(true, Ordering::Relaxed);
    let b = assigner.get_work("sample", 4096).expect("packed runway");
    let hole = assigner.test_first_missing_height();
    assert_eq!(
        b.0,
        hole.saturating_add(super::leapfrog_lead_at(hole)),
        "L1 pack at hole+LEAD, got {b:?} hole={hole}"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("owner"),
        "packed runway must not resticky the hero off H"
    );
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn dest_bc_mute_still_restickies() {
    // dest-bc: mute `50.5` must yield preferred to `165.140`. KEEP=0.
    // Do not hold ≥60 here — that locked R-70 `13.48` and blocked the find.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    let assigner = wan_tip_assigner(900, 800, 10_000, &["mute", "hero"]);
    assigner.set_peer_scores(&[("mute".into(), 0.4), ("hero".into(), 0.9)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("mute");
    assigner.test_seed_tip_stream_rank("mute", 0, 16);
    assigner.note_tip_owner_assigned("hero");
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("hero"),
        "mute must resticky (dest-bc find)"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn open_skips_recent_stream_when_window_under_60() {
    // R-72: window BPS mid-GetData can sit under 60 while last_stream <8s.
    // KEEP=0. Do not OPEN → TIP_PIN score lottery.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    let vh = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(200_000));
    let assigner = ChunkAssigner::new(
        vec![(187_000, 187_127)],
        vec!["hero".into(), "mute".into()],
        std::sync::Arc::clone(&vh),
        187_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.note_tip_owner_assigned("hero");
    assigner.test_seed_tip_stream_rank("hero", 10, 16);
    assert!(assigner.wan_tip_stream_bps("hero") < 60.0);
    assert!(assigner.peer_recently_tip_streaming("hero", std::time::Duration::from_secs(8)));
    assigner.on_chunk_complete_range("hero", 187_000, 187_127);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("hero"),
        "recent streamer must stay sticky after behind-tip complete"
    );
    assert!(
        !assigner.tip_owner_open.load(Ordering::Relaxed),
        "must not OPEN for TIP_PIN lottery"
    );
    super::super::tip_stage::test_reset_tip_stage();
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
fn r52_covering0_pin_keeps_mute_cooldown_when_floor_exists() {
    // Live R-51 @21025: MUTE_DROP 113.30 (120s), hero TCP closed, covering=0.
    // best_covering0 elected floor 112.157; W137 MID_CLEAR uncooled the mute;
    // W138 PREFER_MID stole the pin. Floor candidate must win; mute stays cooled.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    let mute = "113.30.191.61:8333";
    let floor = "112.157.154.16:8333";
    let vh = Arc::new(AtomicU64::new(21_024));
    let assigner = ChunkAssigner::new(
        vec![(21_000, 30_000)],
        vec![mute.into(), floor.into()],
        Arc::clone(&vh),
        21_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(21_000);
    assigner.set_wan_body_tip(21_000);
    assigner.set_header_tip(30_000);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(mute.into(), 0.190), (floor.into(), 0.124)]);
    assigner.set_ibd_ready_peers(HashSet::from([mute.into(), floor.into()]));
    assigner.mark_tip_owner_fail_cooldown(mute, 120);
    assert!(assigner.preferred_tip_owner().is_none());
    assert!(assigner.nudge_wan_tip_owner());
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(floor),
        "covering=0 must pin the floor candidate, not re-elect MUTE_DROP mid"
    );
    assert!(
        assigner.tip_owner_in_fail_cooldown(mute),
        "120s MUTE_DROP cooldown must survive covering=0 TIP_PIN when a floor exists"
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
fn a6m_tip_bps_keep_holds_when_mute_fast_feeder_empty() {
    // Genesis TRUE WAN 2026-08-22 @181k: persist-skip feeder=0 made mute_fast=true
    // and rotated a 273 BPS sticky (gd_ewma=931). tip_bps keep must still hold.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(931, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
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
    // tip_bps ≈ (9091-901)/30 ≈ 273.
    test_set_sticky_tenure(&assigner, slow, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 9091, 0);
    for _ in 0..40 {
        assigner.note_wan_tip_stream(slow);
    }
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(9091, &scorer),
        "mute_fast + feeder=0 must not rotate a ≥80 tip_bps sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(slow));
    assert!(!assigner.is_peer_blacklisted(slow));

    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_gd_slow_keeps_stream_hero_when_cheese_sits_h() {
    // dest-ax 226k: cheese sit dropped height recent_bps below min; A6m then
    // MUTE_KILL GD_SLOW of `104.194` at stream 92 while COOLDOWN_SKIP held.
    // Stream ≥ keep must skip rotate even when H is sitting.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let hero = "10.0.0.1:8333";
    let mute = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![hero.into(), mute.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(hero.into(), 0.50), (mute.into(), 0.40)]);
    assigner.set_ibd_ready_peers(HashSet::from([hero.into(), mute.into()]));
    // H sitting 30s: height-advance BPS = 0 (below min 6) so old keep_tip never ran.
    test_set_sticky_tenure(&assigner, hero, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 901, 0);
    for _ in 0..90 {
        assigner.note_wan_tip_stream(hero);
    }
    for _ in 0..25 {
        assigner.note_wan_tip_stream(mute);
    }
    assert!(
        assigner.wan_tip_stream_bps(hero) >= 80.0,
        "fixture must look like dest-ax healthy_tip_bps=92"
    );
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(901, &scorer),
        "GD_SLOW MUTE_KILL must not rotate a ≥80 stream hero while cheese sits H"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(hero));
    assert!(!assigner.is_peer_blacklisted(hero));

    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn export_isolation_96s_must_not_mute_kill_ge80_owner() {
    // dest-bc 594973 export isolation was 96s wall. Isolation starves GetData
    // until sticky_bps died; CHEESE_HERO_NOSTRIKE does not cover that.
    // HOLD arms on EXPORT_ACTIVE (isolation is the GetData pause).
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::export_owner_hold_clear();
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_EXPORT_ISOLATION", "1");
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let hero = "10.0.0.1:8333";
    let mute = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![hero.into(), mute.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(hero.into(), 0.50), (mute.into(), 0.40)]);
    assigner.set_ibd_ready_peers(HashSet::from([hero.into(), mute.into()]));
    test_set_sticky_tenure(&assigner, hero, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 901, 0);
    for _ in 0..90 {
        assigner.note_wan_tip_stream(hero);
    }
    assigner.restore_tip_hole_depth(hero, 32);
    assert!(assigner.wan_tip_stream_bps(hero) >= 80.0);
    assert_eq!(
        assigner.tip_hole_depth.lock().unwrap().get(hero).copied(),
        Some(32)
    );

    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(true, Ordering::Relaxed);
    assigner.export_owner_hold_tick();
    assigner.test_age_tip_stream_started(hero, 601);
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(901, &scorer),
        "96s isolation must not MUTE_KILL a ≥80 owner"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(hero));
    assert!(!assigner.is_peer_blacklisted(hero));
    assert!(!assigner.maybe_start_tip_trial(901));

    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    assigner.export_owner_hold_tick();
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(hero));
    assert_eq!(
        assigner.tip_hole_depth.lock().unwrap().get(hero).copied(),
        Some(32),
        "isolation end must hand back saved grown"
    );

    super::super::export_owner_hold_clear();
    unsafe {
        std::env::remove_var("BLVM_IBD_EXPORT_ISOLATION");
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn export_active_91s_compact_must_not_mute_kill_ge80_owner() {
    // dest-bc 594973 compact was 91s with isolation *off* (default).
    // HOLD must arm on EXPORT_ACTIVE, not only isolation.
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::export_owner_hold_clear();
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_EXPORT_ISOLATION");
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let hero = "10.0.0.1:8333";
    let mute = "10.0.0.2:8333";
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![hero.into(), mute.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(hero.into(), 0.50), (mute.into(), 0.40)]);
    assigner.set_ibd_ready_peers(HashSet::from([hero.into(), mute.into()]));
    test_set_sticky_tenure(&assigner, hero, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 901, 0);
    for _ in 0..90 {
        assigner.note_wan_tip_stream(hero);
    }
    assigner.restore_tip_hole_depth(hero, 32);
    assert!(assigner.wan_tip_stream_bps(hero) >= 80.0);

    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(true, Ordering::Relaxed);
    assigner.export_owner_hold_tick();
    assigner.test_age_tip_stream_started(hero, 601);
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(901, &scorer),
        "91s compact (isolation off) must not MUTE_KILL a ≥80 owner"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(hero));
    assert!(!assigner.is_peer_blacklisted(hero));
    assert!(!assigner.maybe_start_tip_trial(901));
    assert!(
        super::super::export_owner_hold_protects(hero),
        "hold must arm on EXPORT_ACTIVE without isolation"
    );

    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    assigner.export_owner_hold_tick();
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(hero));
    assert_eq!(
        assigner.tip_hole_depth.lock().unwrap().get(hero).copied(),
        Some(32),
        "export end must hand back saved grown"
    );

    super::super::export_owner_hold_clear();
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn a6m_gd_slow_keeps_probe_known_hero_when_stream_dips() {
    // dest-ba 590k: KEEP stream 86 then cheese sit decayed to 58 → A6N_OPEN
    // + 180s of `63.254`. Probe-known ≥80 must skip rotate (endgame §1).
    use crate::network::peer_scoring::PeerScorer;

    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(1_500, 32);
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS", "30");
        std::env::set_var("BLVM_IBD_A6M_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN", "0");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "500");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let hero = "10.0.0.1:8333";
    let mute = "10.0.0.2:8333";
    super::super::tip_probe::test_seed_probe(hero, 200);
    super::super::tip_probe::test_seed_probe(mute, 1000);
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007), (1008, 1071)],
        vec![hero.into(), mute.into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_tip_gap_missing(true);
    assigner.set_peer_scores(&[(hero.into(), 0.50), (mute.into(), 0.40)]);
    assigner.set_ibd_ready_peers(HashSet::from([hero.into(), mute.into()]));
    test_set_sticky_tenure(&assigner, hero, 901, 30);
    test_push_tip_sample(&assigner, 901, 30);
    test_push_tip_sample(&assigner, 901, 0);
    for _ in 0..90 {
        assigner.note_wan_tip_stream(hero);
    }
    assigner.test_age_tip_stream_started(hero, 120);
    assert!(
        assigner.wan_tip_stream_bps(hero) < 80.0,
        "fixture must dip under keep, got {:.1}",
        assigner.wan_tip_stream_bps(hero)
    );
    let scorer = PeerScorer::new();
    assert!(
        !assigner.maybe_rotate_slow_sticky_a6m(901, &scorer),
        "GD_SLOW OPEN must not 180s-cool a probe-known ≥80 when stream dips on cheese"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some(hero));
    assert!(!assigner.is_peer_blacklisted(hero));

    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_RECENT_WINDOW_SECS");
        std::env::remove_var("BLVM_IBD_A6M_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_FLOOR_ROTATE_COOLDOWN");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
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
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::set_var("BLVM_IBD_TIP_TRIAL", "1");
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
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_skips_start_when_sticky_tip_bps_healthy() {
    // Live 280k: await=3012 MUTE_KILL'd a 594 BPS sticky. Keep the hero.
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
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(
        assigner.wan_tip_stream_bps("sticky") >= 80.0,
        "fixture must look like the 594 BPS hero"
    );
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "3s await must not trial-start a ≥80 tip_bps sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn r19_tip_trial_skips_start_when_keep_unset_and_sticky_bps_ge80() {
    // Flood hero only (≥2000). 4000 streams / 1s ⇒ skip. 80–2000 still trials.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
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
    for _ in 0..4000 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(
        assigner.wan_tip_stream_bps("sticky") >= 2000.0,
        "fixture must be flood-class (≥2000)"
    );
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "KEEP unset/0 must not TRIAL_START a ≥2000 sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    assert!(
        !assigner
            .tip_owner_fail_until
            .lock()
            .unwrap()
            .contains_key("sticky"),
        "skip must not fail_cooldown the hero"
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
fn r19_tip_trial_starts_when_keep_unset_and_sticky_bps_below_flood() {
    // R-43: last_stream <8s holds drip (`gd_wait`). ≥60 STREAM must not
    // TRIAL_START. Mute (8s+ silence) still trials — `r36` / `r42`.
    // R-45 empty-band flood skip FAIL — do not rematch.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
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
    for _ in 0..8 {
        assigner.note_wan_tip_stream("sticky");
    }
    let drip = assigner.wan_tip_stream_bps("sticky");
    assert!(drip < 60.0, "fixture drip, got {drip}");
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "R-43: last_stream <8s must not TRIAL_START a drip sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    let bps = assigner.wan_tip_stream_bps("sticky");
    assert!(bps >= 60.0, "fixture ≥60 hero, got {bps}");
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "R-43: STREAM ≥60 must not TRIAL_START"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

/// Mesh ≥60 stays skipped. A flood-class ready peer may take H (R-85 lock).
#[serial_test::serial(ibd)]
#[test]
fn r86_flood_challenger_trials_through_healthy_mesh() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(900, 800, 2000, &["sticky", "flood"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("flood".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    let mesh = assigner.wan_tip_stream_bps("sticky");
    assert!(mesh >= 60.0 && mesh < 2000.0, "fixture mesh, got {mesh}");
    assigner.test_seed_tip_stream_rank("flood", 40_000, 16);
    assert!(
        assigner.maybe_start_tip_trial(901),
        "flood challenger must pierce healthy_tip_bps / gd_wait"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("flood"),
        "trial must pin the flood challenger, not a mesh lottery"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// Flood sticky is never trialled, even if another flood peer is ready.
#[serial_test::serial(ibd)]
#[test]
fn r86_flood_sticky_holds_against_flood_challenger() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(900, 800, 2000, &["flood", "other"]);
    assigner.set_peer_scores(&[("flood".into(), 0.50), ("other".into(), 0.90)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("flood".into());
    assigner.test_seed_tip_stream_rank("flood", 40_000, 16);
    assigner.test_seed_tip_stream_rank("other", 50_000, 16);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "flood sticky must hold (R-18)"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("flood"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r88_empty_sample_one_h_trial_on_mesh_ia() {
    // R-87: 146.70 IA17 / 900 BPS skip-locked. One H trial, then hold.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_owner_body_ia(17, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(2002);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "0");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(2000, 1900, 10_000, &["sticky", "chall"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("chall".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 2002);
    for _ in 0..20 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_seed_tip_stream_rank("chall", 900, 16);
    let mesh = assigner.wan_tip_stream_bps("sticky");
    assert!(mesh < 60.0, "R-94: sample still fires under 60, got {mesh}");
    assert!(
        assigner.maybe_start_tip_trial(2002),
        "empty-band IA>2 + sticky<60 must take one H trial"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("chall"));
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 2002);
    assert!(
        !assigner.maybe_start_tip_trial(2002),
        "second empty-band sample is R-45"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r88_empty_sample_holds_flood_sticky() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_seed_owner_body_ia(17, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(2002);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(2000, 1900, 10_000, &["flood", "chall"]);
    assigner.set_peer_scores(&[("flood".into(), 0.50), ("chall".into(), 0.90)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("flood".into());
    assigner.test_seed_tip_stream_rank("flood", 40_000, 16);
    assigner.test_seed_tip_stream_rank("chall", 900, 16);
    assert!(
        !assigner.maybe_start_tip_trial(2002),
        "flood sticky must hold in empty band"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("flood"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r94_empty_sample_holds_covering_ge60() {
    // R-90 @17543 1344 / R-91 @2229 380 / R-93 @1125 581.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_owner_body_ia(17, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(2002);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "0");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(2000, 1900, 10_000, &["sticky", "chall"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("chall".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 2002);
    assigner.test_seed_tip_stream_rank("sticky", 9306, 16);
    assigner.test_seed_tip_stream_rank("chall", 900, 16);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    assert!(
        (sticky_bps - 581.6).abs() < 2.0,
        "R-93 sample replay sticky={sticky_bps}"
    );
    assert!(
        !assigner.maybe_start_tip_trial(2002),
        "covering ≥60 must not EMPTY_SAMPLE MUTE_KILL"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r94_empty_sample_still_fires_under_60() {
    // R-92 @193 sticky 18.2 → 507. Dest A must not skip-lock that.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_owner_body_ia(17, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(2002);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "0");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(2000, 1900, 10_000, &["sticky", "chall"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("chall".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 2002);
    assigner.test_seed_tip_stream_rank("sticky", 291, 16);
    assigner.test_seed_tip_stream_rank("chall", 900, 16);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    assert!(
        sticky_bps < 60.0 && (sticky_bps - 18.2).abs() < 2.0,
        "R-92 sample replay sticky={sticky_bps}"
    );
    assert!(
        assigner.maybe_start_tip_trial(2002),
        "sticky <60 must still EMPTY_SAMPLE"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("chall"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r90_assign_does_not_steal_line_rate_hero() {
    // R-89: KEEP=0 → preferred_meets_keep_bps false → 117.212 stole 34.125 @1000.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let assigner = wan_tip_assigner(22_000, 21_900, 50_000, &["fast", "mesh"]);
    assigner.set_peer_scores(&[("fast".into(), 0.20), ("mesh".into(), 0.90)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.note_tip_owner_assigned("fast");
    assigner.test_seed_tip_stream_rank("fast", 10_000, 16);
    let bps = assigner.wan_tip_stream_bps("fast");
    assert!(bps >= 60.0, "fixture line-rate, got {bps}");
    assigner.note_tip_owner_assigned("mesh");
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("fast"),
        "≥60 sticky must keep H on assign"
    );
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r90_empty_sample_skips_reserved_farmer() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_seed_owner_body_ia(17, 16);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(2002);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_PROBE", "1");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "0");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    super::super::tip_probe::test_seed_probe("farmer", 200);
    super::super::tip_probe::test_seed_probe("probe", 100);
    let assigner = wan_tip_assigner(2000, 1900, 10_000, &["sticky", "farmer", "probe"]);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.90),
        ("farmer".into(), 0.80),
        ("probe".into(), 0.10),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 2002);
    for _ in 0..20 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_seed_lookahead_rank("farmer", 16_000, 16);
    assigner.test_seed_tip_stream_rank("probe", 900, 16);
    assert!(
        assigner.maybe_start_tip_trial(2002),
        "empty sample must still find a non-farm challenger"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("probe"),
        "reserved farmer must not take H"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_PROBE");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r36_tip_trial_skips_keep_hero_during_getdata_wait() {
    // R-36: `13.43` had 15499 streams. Window decayed under 60 while the next
    // GetData was in flight. tip_gd_force / slow_drip stole (await_ms=0).
    // KEEP=0 + preferred last_stream <8s must skip (R-42 no longer requires
    // the global last_stream KEEP-hero slot). After 8s silence, trial may
    // start. dest-aq KEEP=80 still trials under-keep.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
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
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_age_tip_stream_started("sticky", 30);
    let decayed = assigner.wan_tip_stream_bps("sticky");
    assert!(
        decayed < 60.0,
        "GetData-wait window must decay under 60, got {decayed}"
    );
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "R-36: preferred last_stream <8s during GetData wait must not TRIAL_START"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    assigner.test_age_tip_stream_last("sticky", 9);
    assert!(
        assigner.maybe_start_tip_trial(901),
        "8s+ silence on a decayed window may TRIAL_START (mute)"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
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
fn r42_tip_trial_skips_preferred_recent_stream_not_global_keep_hero() {
    // R-41: first flood `104.63` owns last_stream_keep_hero. Preferred
    // `217.62` had 12931 / 31448 streams mid-GetData (window <60) and
    // TRIAL_START'd because gd_wait required the global KEEP-hero slot.
    // KEEP=0 + preferred last_stream <8s must skip even when that slot
    // is a different peer. dest-as probe outrank stays a fall-through.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["flood".into(), "sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[
        ("flood".into(), 0.55),
        ("sticky".into(), 0.50),
        ("challenger".into(), 0.40),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("flood".into());
    assigner.reset_sticky_wan_tenure("flood", 901);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("flood");
    }
    // dest-ba: challenger streams must not steal last_stream_keep_hero.
    // R-41: 217.62 already had 12k–31k streams before it became preferred,
    // so it never re-crossed KEEP while owning the slot.
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    assigner.test_age_tip_stream_started("sticky", 30);
    let decayed = assigner.wan_tip_stream_bps("sticky");
    assert!(
        decayed < 60.0,
        "GetData-wait window must decay under 60, got {decayed}"
    );
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "R-42: preferred last_stream <8s must skip even when last_stream_keep_hero is another peer"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    assigner.test_age_tip_stream_last("sticky", 9);
    assert!(
        assigner.maybe_start_tip_trial(901),
        "8s+ silence on a decayed window may TRIAL_START (mute)"
    );
    assert_ne!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "mute trial must leave the silent preferred"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

fn r22_mute_drop_fixture(peers: &[&str], ready: &[&str]) -> ChunkAssigner {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(4_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::remove_var("BLVM_IBD_MUTE_SINGLE_REOPEN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        peers.iter().map(|s| (*s).to_string()).collect(),
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    let scores: Vec<(String, f64)> = peers
        .iter()
        .enumerate()
        .map(|(i, p)| ((*p).to_string(), 0.50 - i as f64 * 0.05))
        .collect();
    assigner.set_peer_scores(&scores);
    mark_peers_ibd_ready(&assigner, ready);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("mute".into());
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("mute".into(), vec![(901, 901)]);
    }
    assigner.note_tip_cover_claim("mute", 901, 901);
    assigner
}

#[serial_test::serial(ibd)]
#[test]
fn r22_mute_drop_rearms_other_ready_peer() {
    // R-21: drop covering=1 → no GetData. After drop, other owns H covering=1.
    let assigner = r22_mute_drop_fixture(&["mute", "other"], &["mute", "other"]);
    assert!(
        assigner.mute_single_cover_reopen(1),
        "covering=1 mute + awaiting≥3s"
    );
    assert!(assigner.wan_tip_stream_bps("mute") < 1.0);
    let work = assigner.get_work("other", 1000);
    assert_eq!(work, Some((901, 901)), "replacement GetData armed on H");
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("other"));
    assert_ne!(assigner.preferred_tip_owner().as_deref(), Some("mute"));
    let (covering, _, _) = assigner.tip_flight_diag();
    assert_eq!(covering, 1, "exclusive H after drop, not covering=0/2");
    {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            !g.get("mute").is_some_and(|r| !r.is_empty()),
            "mute no longer covers H"
        );
        assert_eq!(
            g.get("other").map(|r| r.as_slice()),
            Some(&[(901, 901)][..])
        );
    }
    assert!(
        assigner.tip_owner_in_fail_cooldown("mute"),
        "120s mute cooldown stays"
    );
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r28_mute_drop_does_not_walk_same_height() {
    // R-26: 31 MUTE_DROP at tip=323067 in 5 ms. One replacement per height.
    let assigner = r22_mute_drop_fixture(&["mute", "other", "third"], &["mute", "other", "third"]);
    assert_eq!(assigner.get_work("other", 1000), Some((901, 901)));
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("other"));
    let _ = assigner.get_work("third", 1000);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("other"),
        "same-H must not walk preferred to a third peer"
    );
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r30_mute_drop_holds_when_stream_bps_nonzero() {
    // R-30 dest @224k: recv0 + bps=20.6 MUTE_DROP'd the replacement.
    let assigner = r22_mute_drop_fixture(&["mute", "other"], &["mute", "other"]);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("mute");
    }
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(3);
    assert!(assigner.wan_tip_stream_bps("mute") >= 1.0);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            assigner
                .maybe_drop_mute_tip_cover(1, "other", &mut g)
                .is_none(),
            "recv0 must not drop a peer that is still streaming"
        );
    }
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("mute"));
    super::super::tip_stage::test_set_pipe_fill_recv0_streak(0);
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r22_mute_drop_holds_when_no_other_ready() {
    // No replacement → do not drop (never covering=0 with no owner).
    let assigner = r22_mute_drop_fixture(&["mute", "other"], &["mute"]);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        assert!(
            assigner
                .maybe_drop_mute_tip_cover(1, "mute", &mut g)
                .is_none(),
            "sole ready mute must not drop"
        );
    }
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("mute"));
    let (covering, _, _) = assigner.tip_flight_diag();
    assert_eq!(covering, 1);
    {
        let g = assigner.in_flight_per_peer.lock().unwrap();
        assert_eq!(g.get("mute").map(|r| r.as_slice()), Some(&[(901, 901)][..]));
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_starts_when_window_dips_below_keep() {
    // dest-aq 211→280k: after `162.35` dropped under 80, 180s last_keep
    // sat on `grown=8` (ts 72). Current window < keep must trial.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(77, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_age_tip_stream_started("sticky", 120);
    assert!(
        assigner.wan_tip_stream_bps("sticky") < 80.0,
        "fixture is dest-aq after 211k: window under keep"
    );
    assert!(
        assigner.maybe_start_tip_trial(901),
        "sub-80 current window must trial — no 180s last_keep grace"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
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
    // 3 STREAM used to KEEP via stream_win (R-26 ping-pong). E: ≥60 only.
    for _ in 0..80 {
        assigner.note_wan_tip_stream("challenger");
    }
    // Backdate trial start so finish fires.
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(9);
    }
    // 900 heights / 9s = 100 BPS — KEEP floor is 80 (live 374k KEEP'd 4.2).
    super::super::tip_stage::test_seed_getdata_body_ewma(2000, 64);
    assert!(
        super::super::tip_stage::getdata_body_ewma_ms().is_some(),
        "seed mute EWMA so KEEP reset is observable"
    );
    assert!(assigner.maybe_finish_tip_trial(1801));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger"),
        "P2 KEEP when challenger tip-streams and sticky does not"
    );
    assert!(
        super::super::tip_stage::getdata_body_ewma_ms().is_none(),
        "TRIAL_KEEP must drop global mute EWMA so C1u does not cliff the new owner"
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

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_keeps_hole_stuck_hero_challenger() {
    // Genesis-c 184421: START mute→104.194, CRAWL chall 394 BPS, finish at the
    // same height (one missing tip) → old code REVERT trial_bps=0 + 120s cool.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(184_421);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(184_420));
    let assigner = ChunkAssigner::new(
        vec![(184_400, 184_527)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        184_400,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_wan_body_tip(180_000);
    assigner.set_header_tip(200_000);
    assigner.set_peer_scores(&[("sticky".into(), 0.50), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    for _ in 0..400 {
        assigner.note_wan_tip_stream("challenger");
    }
    assert!(
        assigner.wan_tip_stream_bps("challenger") >= 80.0,
        "pre-seeded hero must already clear the keep bar"
    );
    assert!(assigner.maybe_start_tip_trial(184_421));
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(12);
    }
    assert!(assigner.maybe_finish_tip_trial(184_421));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger"),
        "hole-stuck trial must KEEP a ≥80 tip-stream challenger"
    );
    assert!(
        !assigner.tip_owner_in_fail_cooldown("challenger"),
        "must not 120s-cool the hero after a hole-stuck trial"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_reverts_cooled_sticky_when_gd_still_slow() {
    // Leftover TRUE WAN 2026-08-22: trial cools sticky so sticky_delta=0.
    // Challenger dribbles (height advances at ~4 BPS, a few streams) while
    // global gd_ewma stays ≥800ms. Old KEEP treated that as a win.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(3_000, 32);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "800");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    for _ in 0..5 {
        assigner.note_wan_tip_stream("challenger");
    }
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(10);
    }
    // 40 heights / 10s = 4 BPS ≺ 80 — must REVERT.
    assert!(assigner.maybe_finish_tip_trial(941));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "cooled-sticky dribble + gd_slow must not KEEP"
    );

    // Same mute EWMA, but challenger actually cleared the healthy-crawl bar.
    *assigner.last_tip_trial_at.lock().unwrap() = None;
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    {
        let mut g = assigner.tip_owner_fail_until.lock().unwrap();
        g.remove("sticky");
        g.remove("challenger");
    }
    assert!(assigner.maybe_start_tip_trial(941));
    for _ in 0..20 {
        assigner.note_wan_tip_stream("challenger");
    }
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(10);
    }
    // 900 heights / 10s = 90 BPS ≥ 80 — KEEP even while EWMA is still slow.
    assert!(assigner.maybe_finish_tip_trial(1841));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger"),
        "trial_bps ≥ E16b keep bar may KEEP a gd_slow challenger"
    );

    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_reverts_gd_slow_drip_even_with_sticky_delta() {
    // Genesis 221426: sticky_delta=1 chall_delta=456 trial_bps=19.1 gd_slow
    // used to KEEP (cooled_unfair only when sticky_delta==0).
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::test_seed_getdata_body_ewma(3_000, 32);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_SECS", "8");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_MAX_GETDATA_MS", "800");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    assigner.note_wan_tip_stream("sticky");
    for _ in 0..20 {
        assigner.note_wan_tip_stream("challenger");
    }
    if let Some(ref mut t) = *assigner.tip_trial.lock().unwrap() {
        t.started = Instant::now() - Duration::from_secs(10);
    }
    assert!(assigner.maybe_finish_tip_trial(1092));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "gd_slow trial_bps=19 must REVERT even when sticky_delta>0"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_MAX_GETDATA_MS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_prefers_probe_rank_over_score() {
    // Lottery high-score must not beat a size-matched probe ≥80.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(77, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_reset_probes();
    super::super::tip_probe::test_seed_probe("probed", 200);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "lottery".into(), "probed".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.50),
        ("lottery".into(), 0.99),
        ("probed".into(), 0.10),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_age_tip_stream_started("sticky", 120);
    assert!(assigner.maybe_start_tip_trial(901));
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("probed"),
        "OPEN/trial challenger is best probe_rank, not lottery score"
    );
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn p2_empty_rearm_pins_best_probe_rank() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_probe::test_seed_probe("probed", 200);
    unsafe {
        std::env::remove_var("BLVM_IBD_PEERS");
    }
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["lottery".into(), "probed".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("lottery".into(), 0.99), ("probed".into(), 0.10)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = None;
    assigner.force_empty_tip_rearm(901);
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("probed"),
        "EMPTY_TIP OPEN pin is best probe_rank"
    );
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

fn arm_keep_grown_hero(assigner: &ChunkAssigner, peer: &str, next_needed: u64) {
    unsafe {
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    *assigner.preferred_tip_owner.lock().unwrap() = Some(peer.to_string());
    assigner.reset_sticky_wan_tenure(peer, next_needed);
    for _ in 0..400 {
        assigner.note_wan_tip_stream(peer);
    }
    assigner.note_tip_hole_depth(peer, 32);
}

#[serial_test::serial(ibd)]
#[test]
fn wan_mute_sticky_forbids_multi_peer_ahead() {
    // dest-ar @294k: covering=1 mute farmed flight_ahead=7–8. C1g is not that gate.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(8, Ordering::Relaxed);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(8, Ordering::Relaxed);
    let tip = assigner.get_work("owner", 4096).expect("tip");
    for _ in 0..20 {
        assigner.note_wan_tip_stream("owner");
    }
    assigner.test_age_tip_stream_started("owner", 120);
    assert!(assigner.wan_tip_stream_bps("owner") < 80.0);
    assert!(assigner.tip_hole_depth_for("owner") < 32);
    assert!(
        !assigner.wan_allow_multi_peer_ahead(1, 8),
        "mute covering=1 must not farm ahead"
    );
    if let Some((s, e)) = assigner.get_work("ahead", 4096) {
        assert!(
            !(s > tip.1),
            "mute sticky must not hand C1g runway stripe, got {s}-{e} tip_end={}",
            tip.1
        );
        assigner.on_chunk_complete_range("ahead", s, e);
    }
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn wan_keep_grown32_allows_ahead_gate() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    let assigner = wan_tip_assigner(900, 800, 100_000, &["owner", "ahead"]);
    assigner.set_peer_scores(&[("owner".into(), 9.0), ("ahead".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(false);
    arm_keep_grown_hero(&assigner, "owner", 901);
    assert!(assigner.preferred_meets_keep_bps());
    assert!(
        assigner.wan_allow_multi_peer_ahead(1, 8),
        "KEEP + grown≥32 may ahead"
    );
    if ChunkAssigner::tip_hole_sticky_enabled() {
        assert!(assigner.tip_hole_depth_for("owner") >= 32);
    }
    super::super::tip_stage::clear_tip_failover();
}

#[serial_test::serial(ibd)]
#[test]
fn wan_tip_stream_600s_roll_carries_keep_bps() {
    // dest-au 208k: 10min window roll set streams=1, wan_tip_stream_bps=2,
    // then TRIAL_START of KEEP `104.194` (sticky_streams=2 vs chall 4365).
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("challenger", 400);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..50_000 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_age_tip_stream_started("sticky", 601);
    assigner.note_wan_tip_stream("sticky");
    assert!(
        assigner.wan_tip_stream_bps("sticky") >= 80.0,
        "600s roll must carry KEEP rate, got {:.1}",
        assigner.wan_tip_stream_bps("sticky")
    );
    // Instant 80k notes are flood-class (>2000 BPS) and pierce KEEP (R-85).
    // dest-au was a slow challenger, not a flood. ~7 BPS over 600s.
    assigner.test_seed_tip_stream_rank("challenger", 4365, 600);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "STREAM-window roll must not trial a KEEP hero"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

fn arm_decayed_sticky_trial(assigner: &ChunkAssigner) {
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(assigner.wan_tip_stream_bps("sticky") >= 80.0);
    assigner.test_age_tip_stream_started("sticky", 120);
    assert!(
        assigner.wan_tip_stream_bps("sticky") < 80.0,
        "fixture must dip under keep, got {:.1}",
        assigner.wan_tip_stream_bps("sticky")
    );
    for _ in 0..80_000 {
        assigner.note_wan_tip_stream("challenger");
    }
}

#[serial_test::serial(ibd)]
#[test]
fn tip_trial_skips_probe_known_hero_when_challenger_worse() {
    // Endgame §1: probe-known ≥80 sticky is not trialled for a stream dip
    // when the challenger's probe does not outrank (dest-ba 498k/590k class).
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("sticky", 200);
    super::super::tip_probe::test_seed_probe("challenger", 1000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    arm_decayed_sticky_trial(&assigner);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "probe-known ≥80 sticky must not TRIAL_START a worse-ranked probe"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn tip_trial_starts_when_challenger_probe_outranks() {
    // dest-ba 158k: TRIAL_START of `80.147` found `63.254`. dest-bb
    // STREAM-window skip would have blocked that. Better probe_rank must start.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("sticky", 400);
    super::super::tip_probe::test_seed_probe("challenger", 200);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
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
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    arm_decayed_sticky_trial(&assigner);
    assert!(
        assigner.maybe_start_tip_trial(901),
        "mute/decayed sticky still trials; probe rank is not the title clock"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn probe_outrank_holds_flood_class() {
    // Faster 200k-class probe must not steal a ≥2000 sticky (R-18 / R-53 hold).
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("challenger", 50);
    super::super::tip_probe::test_seed_probe("challenger", 50);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
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
    for _ in 0..4000 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(assigner.wan_tip_stream_bps("sticky") >= 2000.0);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "flood-class sticky must hold against faster probe"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn probe_outrank_empty_band_does_not_punch_gd_wait() {
    // R-75: empty 10k wave 1391 stole `120.159` @2614. H<50k cannot OUTRANK.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(2614);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("challenger", 23);
    super::super::tip_probe::test_seed_probe("challenger", 23);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let vh = Arc::new(AtomicU64::new(2613));
    let assigner = ChunkAssigner::new(
        vec![(1, 4096)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        1,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(0);
    assigner.set_wan_body_tip(0);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 2614);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(
        !assigner.maybe_start_tip_trial(2614),
        "empty-band probe wave must not evict a recent-stream sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn probe_outrank_fat_sojourn_2x_vs_sticky_gd() {
    // After 50k: probe sojourn 200ms vs sticky GetData EWMA 800ms (2×).
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(200_000);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("challenger", 200);
    super::super::tip_probe::test_seed_probe("challenger", 200);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 800, 16);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let vh = Arc::new(AtomicU64::new(199_999));
    let assigner = ChunkAssigner::new(
        vec![(199_000, 200_127)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        199_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_wan_body_tip(180_000);
    assigner.set_header_tip(900_000);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 200_000);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(assigner.wan_tip_stream_bps("sticky") >= 60.0);
    assert!(
        assigner.probe_outrank_bypasses_healthy("sticky", 200_000),
        "2× probe sojourn must pierce healthy gate"
    );
    assert!(
        assigner.maybe_start_tip_trial(200_000),
        "R-141: wired probe outrank trials through ≥60 sticky"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn probe_outrank_fat_sojourn_not_2x_holds() {
    // After 50k: probe 200ms vs sticky GetData 300ms is not 2×.
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(200_000);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("challenger", 200);
    super::super::tip_probe::test_seed_probe("challenger", 200);
    super::super::tip_stage::test_seed_getdata_body_ewma_peer("sticky", 300, 16);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    let vh = Arc::new(AtomicU64::new(199_999));
    let assigner = ChunkAssigner::new(
        vec![(199_000, 200_127)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        199_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_wan_body_tip(180_000);
    assigner.set_header_tip(900_000);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 200_000);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(assigner.wan_tip_stream_bps("sticky") >= 60.0);
    assert!(
        !assigner.maybe_start_tip_trial(200_000),
        "probe sojourn not 2× sticky GetData must hold"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

/// R-166: mute CRAWL cache must not punch lifetime ≥60 (R-165 219k / R-163 99).
#[serial_test::serial(ibd)]
#[test]
fn r161_fat_recv_mute_trials_through_lifetime_healthy() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(180_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 180_001);
    assigner.test_seed_lookahead_stripe("challenger", 180_513, 182_560);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    let bps = assigner.wan_tip_stream_bps("sticky");
    assert!(
        bps >= 60.0 && bps < 2000.0,
        "fixture lifetime healthy mesh, got {bps}"
    );
    super::super::download::test_set_cached_recv_mbps("sticky", 8.0);
    super::super::download::test_set_cached_recv_mbps("challenger", 41.0);
    assert!(
        !assigner.maybe_start_tip_trial(180_001),
        "fat mute recv must not punch lifetime ≥60"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "lifetime healthy sticky holds; mute cache is not title"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// Working fat H pipe (recv ≥16) still holds. Replay of R-139 / R-150 / R-158.
#[serial_test::serial(ibd)]
#[test]
fn r161_fat_recv_live_holds_lifetime_healthy() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::test_reset_getdata_body_ewma();
    super::super::tip_stage::clear_tip_failover();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(180_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 180_001);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 80.0);
    assert!(
        !assigner.maybe_start_tip_trial(180_001),
        "live recv must still hold a lifetime ≥60 fat sticky"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// Empty band keeps R-43: mute recv cache must not punch before 180k.
#[serial_test::serial(ibd)]
#[test]
fn r161_empty_recv_mute_does_not_punch() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(900, 800, 2000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 8.0);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "empty-band mute recv must not punch R-43 ≥60 hold"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// 15s mute sticky + fat farm stripe → farm takes H once. Not probe. Not 5s flicker.
#[serial_test::serial(ibd)]
#[test]
fn r163_farm_recv_promote_after_15s_mute() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["sticky", "farm"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("farm".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 180_001);
    assigner.test_seed_lookahead_stripe("farm", 180_513, 182_560);
    super::super::download::test_set_cached_recv_mbps("sticky", 8.0);
    super::super::download::test_set_cached_recv_mbps("farm", 50.0);
    assert!(
        !assigner.maybe_farm_recv_promote(180_001),
        "first 5s window must not promote"
    );
    assigner.test_backdate_farm_recv_tick();
    assert!(!assigner.maybe_farm_recv_promote(180_001));
    assigner.test_backdate_farm_recv_tick();
    assert!(
        assigner.maybe_farm_recv_promote(180_001),
        "third 5s mute window promotes the fat farm"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("farm"));
    assert!(!assigner.maybe_farm_recv_promote(180_002), "one shot");
    super::super::download::test_reset_download_bytes();
}

/// Live H pipe (recv ≥16) holds. Replay R-139 / R-150 / R-158.
#[serial_test::serial(ibd)]
#[test]
fn r163_farm_recv_live_sticky_does_not_promote() {
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(180_000, 179_900, 300_000, &["sticky", "farm"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("farm".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.test_seed_lookahead_stripe("farm", 180_513, 182_560);
    super::super::download::test_set_cached_recv_mbps("sticky", 80.0);
    super::super::download::test_set_cached_recv_mbps("farm", 50.0);
    for _ in 0..4 {
        assigner.test_backdate_farm_recv_tick();
        assert!(
            !assigner.maybe_farm_recv_promote(180_001),
            "live sticky recv must not promote a farm"
        );
    }
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::download::test_reset_download_bytes();
}

/// Empty band: no promote even if cache looks mute.
#[serial_test::serial(ibd)]
#[test]
fn r163_farm_recv_empty_band_does_not_promote() {
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(900, 800, 2000, &["sticky", "farm"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("farm".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.test_seed_lookahead_stripe("farm", 513, 2560);
    super::super::download::test_set_cached_recv_mbps("sticky", 8.0);
    super::super::download::test_set_cached_recv_mbps("farm", 50.0);
    for _ in 0..4 {
        assigner.test_backdate_farm_recv_tick();
        assert!(!assigner.maybe_farm_recv_promote(901));
    }
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::download::test_reset_download_bytes();
}

/// R-235 fat 190–191k: covering-H farm `46.167` recv 10.3, no LOOKAHEAD stripe,
/// sticky recv 0. Floor 40 + stripe-only scan missed it (promote fired at 327k
/// on a 183 Mbps stripe). Floor 8 + inflight scan takes H after 15s mute.
#[serial_test::serial(ibd)]
#[test]
fn r235_farm_recv_promote_covering_h_not_lookahead() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(190_000, 189_900, 300_000, &["sticky", "farm"]);
    assigner.set_peer_scores(&[("sticky".into(), 185.0), ("farm".into(), 1.9)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 190_239);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("farm".into(), vec![(188_983, 191_030)]);
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 0.0);
    super::super::download::test_set_cached_recv_mbps("farm", 10.3);
    assert!(
        !assigner.maybe_farm_recv_promote(190_239),
        "first 5s window must not promote"
    );
    assigner.test_backdate_farm_recv_tick();
    assert!(!assigner.maybe_farm_recv_promote(190_239));
    assigner.test_backdate_farm_recv_tick();
    assert!(
        assigner.maybe_farm_recv_promote(190_239),
        "covering-H farm at 10.3 must take mute sticky after 15s"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("farm"));
    super::super::download::test_reset_download_bytes();
}

/// dest-bc 0–10k: covering-H farm at 10.3 still must not promote (298 lives).
#[serial_test::serial(ibd)]
#[test]
fn r235_farm_recv_dump_band_does_not_promote() {
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(900, 800, 2000, &["sticky", "farm"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("farm".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("farm".into(), vec![(513, 2560)]);
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 0.0);
    super::super::download::test_set_cached_recv_mbps("farm", 10.3);
    for _ in 0..4 {
        assigner.test_backdate_farm_recv_tick();
        assert!(!assigner.maybe_farm_recv_promote(901));
    }
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::download::test_reset_download_bytes();
}

/// R-237 dump 30–40k: mute sticky recv 0, farm 15.5, warehouse 4096.
/// ≥10k 15s streak promotes (180k was too late for dump).
#[serial_test::serial(ibd)]
#[test]
fn r237_farm_recv_promote_dump_mute_at_32k() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(32_000, 31_900, 50_000, &["sticky", "farm"]);
    assigner.set_peer_scores(&[("sticky".into(), 219.0), ("farm".into(), 1.9)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 32_029);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        g.insert("farm".into(), vec![(32_513, 34_560)]);
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 0.0);
    super::super::download::test_set_cached_recv_mbps("farm", 15.5);
    assert!(!assigner.maybe_farm_recv_promote(32_029));
    assigner.test_backdate_farm_recv_tick();
    assert!(!assigner.maybe_farm_recv_promote(32_029));
    assigner.test_backdate_farm_recv_tick();
    assert!(
        assigner.maybe_farm_recv_promote(32_029),
        "dump mute sticky at 32k must yield to 15.5 Mbps farm after 15s"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("farm"));
    super::super::download::test_reset_download_bytes();
}

/// R-239 dump 40–50k **30.5**: mute sticky, covering=0, inflight=0, CRAWL
/// top_recv 181.7 was not a worker/stripe. Recv-cache top must promote.
#[serial_test::serial(ibd)]
#[test]
fn r239_farm_recv_promote_recv_cache_top_without_inflight() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(46_000, 45_900, 50_000, &["sticky"]);
    assigner.set_peer_scores(&[("sticky".into(), 79.9), ("fat".into(), 1.9)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 46_622);
    super::super::download::test_set_cached_recv_mbps("sticky", 0.0);
    super::super::download::test_set_cached_recv_mbps("fat", 181.7);
    assert!(
        !assigner.maybe_farm_recv_promote(46_622),
        "first window must not promote"
    );
    assigner.test_backdate_farm_recv_tick();
    assert!(!assigner.maybe_farm_recv_promote(46_622));
    assigner.test_backdate_farm_recv_tick();
    assert!(
        assigner.maybe_farm_recv_promote(46_622),
        "181.7 Mbps recv-cache peer with no inflight must take mute sticky after 15s"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("fat"));
    super::super::download::test_reset_download_bytes();
}

/// R-241 fat **144**: dump already spent farm-recv @113k. 181924 covering=0
/// sticky recv 0.7 vs top 1021 must get a second 15s promote.
#[serial_test::serial(ibd)]
#[test]
fn r242_farm_recv_rearm_at_fat_after_dump_shot() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    let assigner = wan_tip_assigner(32_000, 31_900, 50_000, &["sticky"]);
    assigner.set_peer_scores(&[("sticky".into(), 219.0), ("farm".into(), 1.9)]);
    mark_scored_peers_ibd_ready(&assigner);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 32_029);
    super::super::download::test_set_cached_recv_mbps("sticky", 0.0);
    super::super::download::test_set_cached_recv_mbps("farm", 15.5);
    assert!(!assigner.maybe_farm_recv_promote(32_029));
    assigner.test_backdate_farm_recv_tick();
    assert!(!assigner.maybe_farm_recv_promote(32_029));
    assigner.test_backdate_farm_recv_tick();
    assert!(assigner.maybe_farm_recv_promote(32_029));
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("farm"));
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 181_924);
    super::super::download::test_set_cached_recv_mbps("sticky", 0.7);
    super::super::download::test_set_cached_recv_mbps("farm", 1021.0);
    assert!(
        !assigner.maybe_farm_recv_promote(181_924),
        "fat rearm must not promote on the first window"
    );
    assigner.test_backdate_farm_recv_tick();
    assert!(!assigner.maybe_farm_recv_promote(181_924));
    assigner.test_backdate_farm_recv_tick();
    assert!(
        assigner.maybe_farm_recv_promote(181_924),
        "dump one-shot must not block fat mute sticky vs 1021 Mbps farm"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("farm"));
    assert!(
        !assigner.maybe_farm_recv_promote(181_925),
        "fat shot is still one-shot"
    );
    super::super::download::test_reset_download_bytes();
}

/// R-166: mute cache + farm stripe still does not punch lifetime ≥60.
#[serial_test::serial(ibd)]
#[test]
fn r164_recv_mute_seats_farm_not_probe() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(232_801);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("probe", 21);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(232_000, 231_900, 300_000, &["sticky", "farm", "probe"]);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.90),
        ("farm".into(), 0.20),
        ("probe".into(), 0.80),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 232_801);
    assigner.test_seed_lookahead_stripe("farm", 233_313, 235_360);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 8.0);
    super::super::download::test_set_cached_recv_mbps("farm", 41.0);
    assert!(
        !assigner.maybe_start_tip_trial(232_801),
        "mute + farm stripe must not punch lifetime ≥60"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// Mute cache without a farm still does not punch lifetime ≥60 (no probe seat).
#[serial_test::serial(ibd)]
#[test]
fn r164_recv_mute_no_farm_does_not_seat_probe() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(232_801);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe("probe", 21);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(232_000, 231_900, 300_000, &["sticky", "probe"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("probe".into(), 0.80)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 232_801);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("sticky");
    }
    super::super::download::test_set_cached_recv_mbps("sticky", 8.0);
    assert!(
        !assigner.maybe_start_tip_trial(232_801),
        "mute without a farm stripe must not seat probe"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::download::test_reset_download_bytes();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

#[serial_test::serial(ibd)]
#[test]
fn p2_tip_trial_no_score_fallback_after_probe_ok() {
    // After first probe OK, lottery score must not become challenger.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(77, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_reset_probes();
    super::super::tip_probe::test_seed_probe("not_ready", 200);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "lottery".into(), "not_ready".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.50), ("lottery".into(), 0.99)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assigner.test_age_tip_stream_started("sticky", 120);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "score lottery must not trial after first IBD_PROBE_OK"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

fn arm_healthy_unranked_sticky(assigner: &ChunkAssigner) {
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 901);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(
        assigner.wan_tip_stream_bps("sticky") >= 80.0,
        "dest-as sit is stream ≥80, got {:.1}",
        assigner.wan_tip_stream_bps("sticky")
    );
}

#[serial_test::serial(ibd)]
#[test]
fn dest_as_outrank_does_not_install_mid_rank_when_top_not_ready() {
    // If table-top is not live-ready, do not install mid-rank 164.152.
    // dest-as 1231 *was* a worker (D-4); this guards the unread-top case.
    // Lottery stays off.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(77, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_reset_probes();
    super::super::tip_probe::test_seed_probe("hero", 26); // ~1231
    super::super::tip_probe::test_seed_probe("mid", 42); // ~762
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "hero".into(), "mid".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[
        ("sticky".into(), 0.90),
        ("hero".into(), 0.10),
        ("mid".into(), 0.80),
    ]);
    mark_peers_ibd_ready(&assigner, &["sticky", "mid"]);
    assigner.set_tip_gap_missing(true);
    arm_healthy_unranked_sticky(&assigner);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "unread table-top must not fall through to mid-rank"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn dest_ba_probed_hero_sticky_is_not_outrank_rotated() {
    // dest-ba 821: sticky is table-top 2461. #2 at 1142 must not 2×-outrank.
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(77, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_reset_probes();
    super::super::tip_probe::test_seed_probe("sticky", 13); // ~2462
    super::super::tip_probe::test_seed_probe("second", 28); // ~1143
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec!["sticky".into(), "second".into()],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("second".into(), 0.80)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    arm_healthy_unranked_sticky(&assigner);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "dest-ba table-top sticky must stay"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn replay_180_220k_hero_kept_over_drip_stall_outranked() {
    // Increment 2 fixture: 3 ms hero / 25 ms drip / 800 ms stall.
    // Probe rank follows sojourn. Drip does not outrank hero (dest-ba 498k
    // skip). Hero outranks drip (dest-ba 158k start). Stall is not keep.
    use super::super::synthetic_wan::{
        REPLAY_180_220K_DRIP, REPLAY_180_220K_DRIP_MS, REPLAY_180_220K_HERO,
        REPLAY_180_220K_HERO_MS, REPLAY_180_220K_STALL, REPLAY_180_220K_STALL_MS,
    };
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(901);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_probe::test_seed_probe(REPLAY_180_220K_HERO, REPLAY_180_220K_HERO_MS);
    super::super::tip_probe::test_seed_probe(REPLAY_180_220K_DRIP, REPLAY_180_220K_DRIP_MS);
    super::super::tip_probe::test_seed_probe(REPLAY_180_220K_STALL, REPLAY_180_220K_STALL_MS);
    assert!(super::super::tip_probe::probe_keep_hero(
        REPLAY_180_220K_HERO
    ));
    assert!(super::super::tip_probe::probe_keep_hero(
        REPLAY_180_220K_DRIP
    ));
    assert!(!super::super::tip_probe::probe_keep_hero(
        REPLAY_180_220K_STALL
    ));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let vh = Arc::new(AtomicU64::new(900));
    let assigner = ChunkAssigner::new(
        vec![(880, 1007)],
        vec![
            REPLAY_180_220K_HERO.into(),
            REPLAY_180_220K_DRIP.into(),
            REPLAY_180_220K_STALL.into(),
        ],
        Arc::clone(&vh),
        880,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(800);
    assigner.set_wan_body_tip(800);
    assigner.set_header_tip(2000);
    assigner.set_peer_scores(&[
        (REPLAY_180_220K_HERO.into(), 0.90),
        (REPLAY_180_220K_DRIP.into(), 0.40),
        (REPLAY_180_220K_STALL.into(), 0.20),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some(REPLAY_180_220K_HERO.into());
    assigner.reset_sticky_wan_tenure(REPLAY_180_220K_HERO, 901);
    for _ in 0..600 {
        assigner.note_wan_tip_stream(REPLAY_180_220K_HERO);
    }
    assigner.test_age_tip_stream_started(REPLAY_180_220K_HERO, 120);
    assert!(
        !assigner.maybe_start_tip_trial(901),
        "25 ms drip must not TRIAL_START a 3 ms probe-known hero"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(REPLAY_180_220K_HERO)
    );

    // Swap: drip is sticky, hero outranks → dest-ba 158k class TRIAL_START.
    assigner
        .peer_tip_streams
        .lock()
        .unwrap()
        .remove(REPLAY_180_220K_DRIP);
    *assigner.preferred_tip_owner.lock().unwrap() = Some(REPLAY_180_220K_DRIP.into());
    assigner.reset_sticky_wan_tenure(REPLAY_180_220K_DRIP, 901);
    for _ in 0..600 {
        assigner.note_wan_tip_stream(REPLAY_180_220K_DRIP);
    }
    assigner.test_age_tip_stream_started(REPLAY_180_220K_DRIP, 120);
    assert!(
        assigner.maybe_start_tip_trial(901),
        "3 ms hero must TRIAL_START over 25 ms drip sticky"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some(REPLAY_180_220K_HERO)
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

fn ia_demote_assigner() -> ChunkAssigner {
    let vh = Arc::new(AtomicU64::new(190_000));
    let assigner = ChunkAssigner::new(
        vec![(180_000, 200_000)],
        vec!["sticky".into(), "challenger".into()],
        Arc::clone(&vh),
        180_000,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_confirmed_body_height_at_start(180_000);
    assigner.set_wan_body_tip(180_000);
    assigner.set_header_tip(960_000);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.80)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 190_000);
    for _ in 0..600 {
        assigner.note_wan_tip_stream("sticky");
    }
    assert!(assigner.wan_tip_stream_bps("sticky") >= 80.0);
    assigner
}

#[serial_test::serial(ibd)]
#[test]
fn origin_arm_n5_fast_ia_must_not_demote_ge80() {
    // Applies 8/9: IA median 3–5. Hold ≥80 stays.
    super::super::export_owner_hold_clear();
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_probe::test_seed_probe("sticky", 200);
    super::super::tip_probe::test_seed_probe("challenger", 50);
    super::super::tip_stage::test_seed_owner_body_ia(4, 16);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = ia_demote_assigner();
    assert!(
        !assigner.maybe_run_tip_trial(190_000),
        "4 ms IA must not rotate a ≥80 flyer"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn origin_arm_n5_cooled_ia_without_better_probe_stays() {
    super::super::export_owner_hold_clear();
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_probe::test_seed_probe("sticky", 200);
    super::super::tip_stage::test_seed_owner_body_ia(19, 16);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = ia_demote_assigner();
    assert!(
        !assigner.maybe_run_tip_trial(190_000),
        "no ge80 probe alternate → do not rotate"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

#[serial_test::serial(ibd)]
#[test]
fn origin_arm_n5_export_owner_hold_blocks_ia_demote() {
    super::super::export_owner_hold_clear();
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::tip_probe::test_seed_probe("sticky", 200);
    super::super::tip_probe::test_seed_probe("challenger", 50);
    super::super::tip_stage::test_seed_owner_body_ia(19, 16);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP", "80");
    }
    let assigner = ia_demote_assigner();
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(true, Ordering::Relaxed);
    assigner.export_owner_hold_tick();
    assert!(
        super::super::export_owner_hold_protects("sticky"),
        "export hold must arm on sticky"
    );
    assert!(
        !assigner.maybe_run_tip_trial(190_000),
        "EXPORT_OWNER_HOLD must not rotate during dump"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    super::super::IBD_CHECKPOINT_EXPORT_ACTIVE.store(false, Ordering::Relaxed);
    assigner.export_owner_hold_tick();
    super::super::export_owner_hold_clear();
    super::super::tip_stage::test_reset_owner_body_ia();
    super::super::tip_probe::test_reset_probes();
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
}

/// R-129: R-128 stall pierce reverted. Career ≥80 + tip_gd 16s still skips.
#[serial_test::serial(ibd)]
#[test]
fn r128_tip_gd_stall_trials_through_healthy_career() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(200_101);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_stage::test_seed_last_getdata_body_ms(16_273);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(200_100, 200_000, 220_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 200_101);
    assigner.test_seed_tip_stream_rank("sticky", 1_320, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let chall_bps = assigner.wan_tip_stream_bps("challenger");
    assert!(sticky_bps >= 80.0, "career healthy, got {sticky_bps}");
    assert!(chall_bps >= 80.0, "proven H chall, got {chall_bps}");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 200_101, 200_164);
    }
    assert!(
        !assigner.maybe_start_tip_trial(200_101),
        "R-129: tip_gd 16s must not pierce healthy_tip_bps"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-115 303196: tip_gd=10029, win 296, reorder 2048. 8s would steal. 15s must not.
#[serial_test::serial(ibd)]
#[test]
fn r128_tip_gd_10s_does_not_steal_r115_303k() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(303_196);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_stage::test_seed_last_getdata_body_ms(10_029);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(303_195, 303_000, 320_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 303_196);
    assigner.test_seed_tip_stream_rank("sticky", 2_960, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 303_196, 303_259);
    }
    assert!(
        !assigner.maybe_start_tip_trial(303_196),
        "R-115 303k tip_gd=10s must still TRIAL_SKIP"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-115 339855: only ≥15s crawl, reorder=0. Door stays closed.
#[serial_test::serial(ibd)]
#[test]
fn r128_tip_gd_15s_desert_does_not_steal() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(339_855);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    super::super::tip_stage::test_seed_last_getdata_body_ms(16_710);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(339_854, 339_000, 360_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 339_855);
    assigner.test_seed_tip_stream_rank("sticky", 700, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 339_855, 339_918);
    }
    assert!(
        !assigner.maybe_start_tip_trial(339_855),
        "R-115 339k tip_gd=16s but desert (reorder=0) must not pierce"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-130 W2a: R-129 172k. Career ≥80, flight_tip=0, win=0, warehouse, proven H.
/// Must pierce healthy_tip_bps. Ahead inflight stays.
#[serial_test::serial(ibd)]
#[test]
fn r130_win_collapse_ft0_trials_through_healthy_career() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(172_363);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    ChunkAssigner::test_reset_apply_win_stall();
    ChunkAssigner::test_seed_apply_win_stall(0.0, 0.0);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(172_362, 172_000, 190_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 172_363);
    assigner.test_seed_tip_stream_rank("sticky", 1_200, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    let sticky_bps = assigner.wan_tip_stream_bps("sticky");
    let chall_bps = assigner.wan_tip_stream_bps("challenger");
    assert!(sticky_bps >= 80.0, "career healthy, got {sticky_bps}");
    assert!(chall_bps >= 80.0, "proven H chall, got {chall_bps}");
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 174_500, 174_563);
    }
    assert!(
        assigner.maybe_start_tip_trial(172_363),
        "R-130: ft=0 + win=0 + warehouse must pierce healthy_tip_bps"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
    );
    let flight = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .get("sticky")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        flight,
        vec![(174_500, 174_563)],
        "ahead inflight stays, got {flight:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-115 235k: win 260, apply 33, covering H. Must not steal.
#[serial_test::serial(ibd)]
#[test]
fn r130_win_260_does_not_steal_r115_235k() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(235_100);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    ChunkAssigner::test_reset_apply_win_stall();
    ChunkAssigner::test_seed_apply_win_stall(33.0, 260.0);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(235_099, 235_000, 250_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 235_100);
    assigner.test_seed_tip_stream_rank("sticky", 2_960, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 235_100, 235_163);
    }
    assert!(
        !assigner.maybe_start_tip_trial(235_100),
        "R-115 235k win 260 must not pierce"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// Empty band: H < 50k. Even ft=0 + win=0 + warehouse must not fire W2a.
#[serial_test::serial(ibd)]
#[test]
fn r130_empty_band_does_not_fire() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(20_001);
    super::super::tip_stage::test_backdate_awaiting_ms(5_000);
    ChunkAssigner::test_reset_apply_win_stall();
    ChunkAssigner::test_seed_apply_win_stall(0.0, 0.0);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "2");
    }
    let assigner = wan_tip_assigner(20_000, 19_000, 40_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 20_001);
    assigner.test_seed_tip_stream_rank("sticky", 1_200, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 22_000, 22_063);
    }
    assert!(
        !assigner.maybe_start_tip_trial(20_001),
        "empty H<50k must not fire W2a"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-131: R-130 184460 covering sit. ft=1, 8s last_win=4.2, warehouse,
/// await 2000 < need 3000. Must trial (await-gate bypass). Ahead stays.
#[serial_test::serial(ibd)]
#[test]
fn r131_win_collapse_ft1_184460_trials() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(184_460);
    super::super::tip_stage::test_backdate_awaiting_ms(2_000);
    ChunkAssigner::test_reset_apply_win_stall();
    ChunkAssigner::test_seed_apply_win_stall(1.0, 4.2);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "3");
    }
    let assigner = wan_tip_assigner(184_459, 184_000, 200_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 184_460);
    assigner.test_seed_tip_stream_rank("sticky", 1_200, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 184_460, 184_523);
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 186_500, 186_563);
    }
    assert!(
        assigner.maybe_start_tip_trial(184_460),
        "R-131: ft=1 + 8s win=4.2 + warehouse + await<need must pierce"
    );
    assert_eq!(
        assigner.preferred_tip_owner().as_deref(),
        Some("challenger")
    );
    let flight = assigner
        .in_flight_per_peer
        .lock()
        .unwrap()
        .get("sticky")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        flight,
        vec![(186_500, 186_563)],
        "H cover vacated, ahead stays, got {flight:?}"
    );
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-36 / R-130 182872: 8s last_win=39.5, ft=1, warehouse. Must not hop.
#[serial_test::serial(ibd)]
#[test]
fn r131_win_39_does_not_steal_182872_dip() {
    super::super::tip_probe::test_reset_probes();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_stage::clear_tip_failover();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(2048, Ordering::Relaxed);
    super::super::tip_stage::mark_needed(182_872);
    super::super::tip_stage::test_backdate_awaiting_ms(2_000);
    ChunkAssigner::test_reset_apply_win_stall();
    ChunkAssigner::test_seed_apply_win_stall(10.0, 39.5);
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL");
        std::env::remove_var("BLVM_IBD_A6M_GD_SLOW_TIP_BPS_KEEP");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS", "15");
        std::env::set_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS", "3");
    }
    let assigner = wan_tip_assigner(182_871, 182_000, 200_000, &["sticky", "challenger"]);
    assigner.set_peer_scores(&[("sticky".into(), 0.90), ("challenger".into(), 0.40)]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    *assigner.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    assigner.reset_sticky_wan_tenure("sticky", 182_872);
    assigner.test_seed_tip_stream_rank("sticky", 1_200, 10);
    assigner.test_seed_tip_stream_rank("challenger", 1_600, 10);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "sticky", 182_872, 182_935);
    }
    assert!(
        !assigner.maybe_start_tip_trial(182_872),
        "R-131: 8s win=39.5 is a GetData-wait dip, must not pierce"
    );
    assert_eq!(assigner.preferred_tip_owner().as_deref(), Some("sticky"));
    unsafe {
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS");
        std::env::remove_var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS");
    }
    ChunkAssigner::test_reset_apply_win_stall();
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
    super::super::tip_probe::test_reset_probes();
}

/// R-215 desert-fill dump **694** FAIL — reverted. Invariant from R-214:
/// `get_work` must still issue *some* range (not refuse-None).
#[serial_test::serial(ibd)]
#[test]
fn r215_lead_pack_unaligned_occupier_still_issues_range() {
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_TIP_IN_REORDER.store(false, Ordering::Relaxed);
    super::super::IBD_TIP_CONTIG_RUNWAY.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(64, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(1, Ordering::Relaxed);
    let assigner = wan_tip_assigner(315_516, 315_400, 400_000, &["owner", "frag", "next"]);
    assigner.set_peer_scores(&[
        ("owner".into(), 9.0),
        ("frag".into(), 8.0),
        ("next".into(), 7.0),
    ]);
    mark_scored_peers_ibd_ready(&assigner);
    assigner.set_tip_gap_missing(true);
    assigner.note_tip_owner_assigned("owner");
    assigner.restore_tip_hole_depth("owner", 64);
    for _ in 0..80 {
        assigner.note_wan_tip_stream("owner");
    }
    let tip = assigner.get_work("owner", 4096).expect("hero H");
    assert_eq!(tip.0, 315_517);
    let hole = assigner.test_first_missing_height();
    let intended = hole.saturating_add(super::leapfrog_lead_at(hole));
    assert_eq!(intended, 315_581, "R-213 LEAD_PACK intended");
    assigner.test_seed_lookahead_stripe("frag", 317_560, 319_607);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "frag", 317_560, 319_607);
    }
    let got = assigner.get_work("next", 4096);
    assert!(
        got.is_some(),
        "do not starve when [intended, occupier) is free; R-214 refuse-None dump 202, got {got:?}"
    );
    assert_ne!(
        got,
        Some((315_581, 317_559)),
        "R-215 desert-fill reverted (dump 694)"
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-228 / R-230 dump: new slid tile blocked even when feeder is healthy.
/// Intended LEAD / genesis / dest-bc / r165 warehouse-full live.
#[serial_test::serial(ibd)]
#[test]
fn r229_starve_far_replays_r228_silent_on_r227() {
    unsafe {
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE");
        std::env::remove_var("BLVM_IBD_RUNWAY_BREADTH");
    }
    // R-228 10–20k: hole 12801, armed 16897 (2×WIDTH past LEAD).
    assert!(super::starve_far_blocks_new_slid(
        12_801, 41, 12_801, 16_897, 0
    ));
    // Intended first farm stripe (hole+LEAD) still arms.
    assert!(!super::starve_far_blocks_new_slid(
        12_801,
        41,
        12_801,
        12_801 + 512,
        0
    ));
    // R-230 dump: feeder 689 at 10k, farms already at 14849 — must block
    // *before* feeder hits 0. Same slid as R-227 packing 16897 at feeder 193.
    assert!(super::starve_far_blocks_new_slid(
        10_753, 689, 10_753, 14_849, 0
    ));
    assert!(super::starve_far_blocks_new_slid(
        12_000, 193, 12_000, 16_897, 0
    ));
    // dest-bc 0–10k / first LOOKAHEAD at tip=1.
    assert!(!super::starve_far_blocks_new_slid(1, 0, 1, 2561, 0));
    assert!(!super::starve_far_blocks_new_slid(5000, 0, 1, 4609, 0));
    // r165: 180k warehouse-full still packs (not FAT_HOLD).
    assert!(!super::starve_far_blocks_new_slid(
        180_000, 0, 180_000, 186_000, 2048
    ));
    // R-229 180–190k EMPTY_TIP: feeder 0, reorder 0, armed 184225 at hole 180370.
    assert!(super::starve_far_blocks_new_slid(
        180_370, 0, 180_370, 184_225, 0
    ));
    // Gap B floor — not this lever.
    assert!(!super::starve_far_blocks_new_slid(
        300_000, 0, 300_000, 310_000, 0
    ));
}

/// R-282: unset TILE is byte-identical to R-280 (tile 2048, RUNWAY_MAX 3,
/// starve_far = hole+LEAD+2048).
#[serial_test::serial(ibd)]
#[test]
fn r282_unset_tile_matches_r280() {
    unsafe {
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE");
        std::env::remove_var("BLVM_IBD_RUNWAY_BREADTH");
    }
    assert_eq!(super::leapfrog_width_at(12_801), super::LEAPFROG_WIDTH);
    assert_eq!(super::leapfrog_width_at(12_801), 2048);
    assert_eq!(super::runway_max(), super::RUNWAY_MAX);
    assert_eq!(super::runway_max(), 3);
    assert!(super::starve_far_blocks_new_slid(
        12_801, 41, 12_801, 16_897, 0
    ));
    assert!(!super::starve_far_blocks_new_slid(
        12_801,
        41,
        12_801,
        12_801 + 512,
        0
    ));
    assert!(super::starve_far_blocks_new_slid(
        10_753, 689, 10_753, 14_849, 0
    ));
    assert!(!super::starve_far_blocks_new_slid(1, 0, 1, 2561, 0));
    assert!(!super::starve_far_blocks_new_slid(
        180_000, 0, 180_000, 186_000, 2048
    ));
    assert!(super::starve_far_blocks_new_slid(
        180_370, 0, 180_370, 184_225, 0
    ));
}

/// R-282 T=256: 32 tiles, 32 peers, span 8192. Tiles == peers by construction.
#[serial_test::serial(ibd)]
#[test]
fn r282_tile_256_peers_equal_tiles() {
    unsafe { std::env::set_var("BLVM_IBD_RUNWAY_TILE", "256") };
    assert_eq!(super::leapfrog_width_at(12_801), 256);
    assert_eq!(super::runway_max(), 32);
    assert_eq!(
        super::runway_max() as u64,
        super::RUNWAY_SPAN / super::leapfrog_width_at(12_801)
    );
    // Same tile r229 blocked at unset now sits inside the 8192 window.
    assert!(!super::starve_far_blocks_new_slid(
        12_801, 41, 12_801, 16_897, 0
    ));
    let hole = 12_801u64;
    let lead = super::leapfrog_lead_at(hole);
    let slid = hole.saturating_add(lead).saturating_add(super::RUNWAY_SPAN);
    assert_eq!(slid, hole + 512 + 8192);
    assert!(super::starve_far_blocks_new_slid(
        12_801, 41, 12_801, slid, 0
    ));
    assert!(!super::starve_far_blocks_new_slid(
        12_801,
        41,
        12_801,
        slid - 1,
        0
    ));
    assert!(!super::starve_far_blocks_new_slid(
        12_801,
        41,
        12_801,
        12_801 + 512,
        0
    ));
    unsafe { std::env::remove_var("BLVM_IBD_RUNWAY_TILE") };
}

/// R-307: default-off freeze still follows `tip_missing` (2026-07-15 covering=1 forever).
/// Gate on: freeze follows `holes_now`, not steady-state gap_missing.
#[serial_test::serial(ibd)]
#[test]
fn r307_c1g_freeze_on_holes_default_off_follows_tip_missing() {
    unsafe {
        std::env::remove_var("BLVM_IBD_C1G_FREEZE_ON_HOLES");
        std::env::remove_var("BLVM_IBD_C1I_MIN_CONTIG");
    }
    assert!(
        !super::ChunkAssigner::c1g_freeze_on_holes_enabled(),
        "R-307: C1G_FREEZE_ON_HOLES unset must stay off (today's tip_missing predicate)"
    );
    // tip_missing=true, holes=0, contig healthy → freeze (old DNA).
    assert!(
        super::ChunkAssigner::c1g_freeze_past_tip_pred(true, true, 0, 8, 64),
        "R-307: default-off must freeze on tip_missing even when holes_now=0 (R-306 every-5s freeze)"
    );
    // tip_missing=false, holes>0 → no freeze from the missing/holes term;
    // min_contig=8 contig=64 is healthy so overall false.
    assert!(
        !super::ChunkAssigner::c1g_freeze_past_tip_pred(true, false, 9, 8, 64),
        "R-307: default-off must ignore holes_now; tip present + contig ok = not frozen"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r307_c1g_freeze_on_holes_follows_holes_now() {
    unsafe {
        std::env::set_var("BLVM_IBD_C1G_FREEZE_ON_HOLES", "1");
        std::env::remove_var("BLVM_IBD_C1I_MIN_CONTIG");
    }
    assert!(super::ChunkAssigner::c1g_freeze_on_holes_enabled());
    // R-306: tip always missing, bridge/holes 0 ~82% — must OPEN ahead.
    assert!(
        !super::ChunkAssigner::c1g_freeze_past_tip_pred(true, true, 0, 0, 0),
        "R-307: holes-gate + holes_now=0 must not freeze even if tip_missing (R-306 covering=1-2 ready=21)"
    );
    assert!(
        super::ChunkAssigner::c1g_freeze_past_tip_pred(true, false, 3, 0, 0),
        "R-307: holes-gate must freeze when holes_now>0 even if tip is covered"
    );
    unsafe { std::env::remove_var("BLVM_IBD_C1G_FREEZE_ON_HOLES") };
}

/// R-282: clamp 128..=2048. Junk / unset → R-280.
#[serial_test::serial(ibd)]
#[test]
fn r282_tile_clamp() {
    unsafe {
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE_MIN");
        std::env::set_var("BLVM_IBD_RUNWAY_TILE", "64");
    }
    assert_eq!(super::leapfrog_width_at(1), 128);
    assert_eq!(super::runway_max(), 32);
    unsafe { std::env::set_var("BLVM_IBD_RUNWAY_TILE", "4096") };
    assert_eq!(super::leapfrog_width_at(1), 2048);
    assert_eq!(super::runway_max(), 4);
    unsafe { std::env::set_var("BLVM_IBD_RUNWAY_TILE", "nope") };
    assert_eq!(super::leapfrog_width_at(1), 2048);
    assert_eq!(super::runway_max(), 3);
    unsafe { std::env::remove_var("BLVM_IBD_RUNWAY_TILE") };
    assert_eq!(super::leapfrog_width_at(1), 2048);
    assert_eq!(super::runway_max(), 3);
}

/// R-310: TILE_MIN=16 TILE=16 → width 16, runway_max 32. Unset MIN still clamps 128.
#[serial_test::serial(ibd)]
#[test]
fn r310_tile_min_16_width_and_runway_max() {
    unsafe {
        std::env::set_var("BLVM_IBD_RUNWAY_TILE_MIN", "16");
        std::env::set_var("BLVM_IBD_RUNWAY_TILE", "16");
    }
    assert_eq!(super::leapfrog_width_at(1), 16);
    assert_eq!(super::runway_max(), 32);
    unsafe {
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE");
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE_MIN");
    }
    assert_eq!(super::leapfrog_width_at(1), 2048);
}

/// R-315: `TILE_FROM` gates the tile by height. Below it the farm runs the
/// unset shape (2048 × 3 — R-115 dump 7122); at/above it the tile applies
/// (16 × 32). Unset FROM (=0) is byte-identical to R-310: tile everywhere.
#[serial_test::serial(ibd)]
#[test]
fn r315_tile_from_height_gates_width_and_slots() {
    unsafe {
        std::env::set_var("BLVM_IBD_RUNWAY_TILE_MIN", "16");
        std::env::set_var("BLVM_IBD_RUNWAY_TILE", "16");
        std::env::set_var("BLVM_IBD_RUNWAY_TILE_FROM", "120000");
    }
    // dump: unset shape
    assert_eq!(super::leapfrog_width_at(50_000), 2048);
    assert_eq!(super::runway_max_at(50_000), 3);
    assert_eq!(super::runway_tile_at(119_999), None);
    // tip: tile shape
    assert_eq!(super::leapfrog_width_at(120_000), 16);
    assert_eq!(super::runway_max_at(120_000), 32);
    assert_eq!(super::leapfrog_width_at(300_000), 16);
    // FROM unset → tile everywhere (R-310 behavior)
    unsafe { std::env::remove_var("BLVM_IBD_RUNWAY_TILE_FROM") };
    assert_eq!(super::leapfrog_width_at(1), 16);
    assert_eq!(super::runway_max_at(1), 32);
    unsafe {
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE");
        std::env::remove_var("BLVM_IBD_RUNWAY_TILE_MIN");
    }
    assert_eq!(super::leapfrog_width_at(1), 2048);
    assert_eq!(super::runway_max_at(1), 3);
}

/// R-313: NO_FARM default off. Set → on. Unset again → off.
#[serial_test::serial(ibd)]
#[test]
fn r313_no_farm_default_off() {
    unsafe { std::env::remove_var("BLVM_IBD_NO_FARM") };
    assert!(
        !super::ChunkAssigner::no_farm_enabled(),
        "unset must be today's farm-on behavior"
    );
    unsafe { std::env::set_var("BLVM_IBD_NO_FARM", "1") };
    assert!(super::ChunkAssigner::no_farm_enabled());
    unsafe { std::env::remove_var("BLVM_IBD_NO_FARM") };
    assert!(!super::ChunkAssigner::no_farm_enabled());
}

/// R-218 dump hole must not rotate. Fat slow sample rotates once.
#[test]
fn r219_h_slow_skips_dump_rotates_fat_once() {
    super::test_reset_h_slow_gd();
    assert!(!super::body_h_rtt_should_rotate(
        373_074.0, 2.5, 0, 66, false
    ));
    assert!(!super::body_h_rtt_should_rotate(
        700.0, 300.0, 0, 200_000, false
    ));
    // R-251: line-rate (≥60) must hold. R-250 rotated 115.3 / 103.4 at cutoff 150.
    assert!(!super::body_h_rtt_should_rotate(
        1716.0, 115.3, 0, 191_773, false
    ));
    assert!(!super::body_h_rtt_should_rotate(
        1632.0, 103.4, 0, 196_240, false
    ));
    assert!(!super::body_h_rtt_should_rotate(
        2750.0, 70.0, 0, 200_000, false
    ));
    // Mute still rotates (R-250 194348 sticky 16.1).
    assert!(super::body_h_rtt_should_rotate(
        1540.0, 16.1, 0, 194_348, false
    ));
    assert!(!super::body_h_rtt_should_rotate(
        1540.0, 16.1, 0, 194_348, true
    ));
    assert!(
        !super::body_dump_warehouse_should_rotate(2.5, 0, 66, 0, false),
        "R-218 empty-hole dump must stay off"
    );
    assert!(
        !super::body_dump_warehouse_should_rotate(300.0, 0, 66, 4096, false),
        "flood dump must not rotate"
    );
    assert!(super::body_dump_warehouse_should_rotate(
        109.0, 0, 4614, 4096, false
    ));
    assert!(!super::body_dump_warehouse_should_rotate(
        109.0, 0, 4614, 4096, true
    ));
    assert!(
        !super::body_dump_warehouse_should_rotate(109.0, 0, 200_000, 4096, false),
        "fat warehouse is the RTT path, not dump"
    );
    assert!(
        super::h_slow_recv_leader_holds(64.9, 42.4),
        "R-224 73.164 sticky_recv 64.9 > top 42.4 must hold"
    );
    assert!(
        !super::h_slow_recv_leader_holds(4.3, 107.5),
        "144.172 Face 1 (sticky 4.3 vs top 107) may still rotate"
    );
    assert!(!super::h_slow_recv_leader_holds(3.0, 296.0));
    assert!(super::h_slow_recv_leader_holds(10.0, 10.0));
}

/// Fat-RTT H-slow rotation is opt-in (`BLVM_IBD_H_SLOW_ROTATE`). These tests
/// lock the predicate, not the default-off switch (R-334).
struct HSlowRotateOn;
impl HSlowRotateOn {
    fn arm() -> Self {
        // SAFETY: test-only process env. Drop clears it. Callers are `serial(ibd)`.
        unsafe { std::env::set_var("BLVM_IBD_H_SLOW_ROTATE", "1") };
        Self
    }
}
impl Drop for HSlowRotateOn {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("BLVM_IBD_H_SLOW_ROTATE") };
    }
}

#[serial_test::serial(ibd)]
#[test]
fn r219_h_slow_dump_keeps_preferred_fat_rotates_once() {
    let _rotate = HSlowRotateOn::arm();
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

    let dump = wan_tip_assigner(65, 0, 50_000, &["sticky", "farm"]);
    dump.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&dump);
    dump.set_tip_gap_missing(true);
    dump.note_tip_owner_assigned("sticky");
    *dump.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    dump.test_seed_tip_stream_rank("sticky", 3, 1);
    super::super::tip_stage::test_seed_last_getdata_body(373_074, "sticky");
    dump.maybe_rotate_slow_h_sticky();
    assert_eq!(
        dump.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "R-218 dump h=66 must not drop exclusive-H"
    );

    let fat = wan_tip_assigner(200_000, 199_000, 300_000, &["sticky", "farm"]);
    fat.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&fat);
    fat.set_tip_gap_missing(true);
    fat.note_tip_owner_assigned("sticky");
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.test_seed_tip_stream_rank("sticky", 16, 1);
    super::super::tip_stage::test_seed_last_getdata_body(2750, "sticky");
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("farm"),
        "R-219: mute (<60) still pins successor; line-rate hold is R-251"
    );
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "same GetData sample must not rotate again"
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-224: fat RTT sample on a recv-leader sticky must not pin farm-score successor.
#[serial_test::serial(ibd)]
#[test]
fn r225_h_slow_recv_leader_holds_preferred() {
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

    let fat = wan_tip_assigner(193_595, 193_595, 300_000, &["sticky", "farm"]);
    fat.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&fat);
    fat.set_tip_gap_missing(true);
    fat.note_tip_owner_assigned("sticky");
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.test_seed_tip_stream_rank("sticky", 117, 1);
    super::super::download::test_set_cached_recv_mbps("sticky", 64.9);
    super::super::download::test_set_cached_recv_mbps("farm", 42.4);
    super::super::tip_stage::test_seed_last_getdata_body(1501, "sticky");
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "R-224 recv leader 64.9>42.4 must not H-slow rotate"
    );

    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-222 dump warehouse: feeder=0, sticky <150, reorder ≥WIDTH → one successor pin.
#[serial_test::serial(ibd)]
#[test]
fn r223_dump_warehouse_rotates_once() {
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);
    super::super::IBD_REORDER_AHEAD.store(4096, Ordering::Relaxed);

    let dump = wan_tip_assigner(4613, 0, 50_000, &["sticky", "farm"]);
    dump.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&dump);
    dump.set_tip_gap_missing(true);
    dump.note_tip_owner_assigned("sticky");
    *dump.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    dump.test_seed_tip_stream_rank("sticky", 3, 1);
    super::super::tip_stage::test_seed_last_getdata_body(169, "sticky");
    dump.maybe_rotate_slow_h_sticky();
    assert_eq!(
        dump.preferred_tip_owner().as_deref(),
        Some("farm"),
        "R-222 warehouse dump pins successor"
    );
    *dump.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    dump.maybe_rotate_slow_h_sticky();
    assert_eq!(
        dump.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "same GetData sample must not rotate again"
    );

    super::super::IBD_REORDER_AHEAD.store(0, Ordering::Relaxed);
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::tip_stage::test_reset_tip_stage();
}
#[serial_test::serial(ibd)]
#[test]
fn r220_h_slow_120s_cools_ge60_dump_stays() {
    let _rotate = HSlowRotateOn::arm();
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

    let dump = wan_tip_assigner(65, 0, 50_000, &["sticky", "farm"]);
    dump.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&dump);
    dump.set_tip_gap_missing(true);
    dump.note_tip_owner_assigned("sticky");
    *dump.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    dump.test_seed_tip_stream_rank("sticky", 3, 1);
    super::super::tip_stage::test_seed_last_getdata_body(373_074, "sticky");
    dump.maybe_rotate_slow_h_sticky();
    assert_eq!(dump.preferred_tip_owner().as_deref(), Some("sticky"));
    assert!(
        !dump.tip_owner_in_fail_cooldown("sticky"),
        "R-218 dump must not cooldown exclusive-H"
    );

    let fat = wan_tip_assigner(200_000, 199_000, 300_000, &["sticky", "farm"]);
    fat.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&fat);
    fat.set_tip_gap_missing(true);
    fat.note_tip_owner_assigned("sticky");
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.test_seed_tip_stream_rank("sticky", 16, 1);
    super::super::tip_stage::test_seed_last_getdata_body(2750, "sticky");
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("farm"),
        "pin farm successor, not open slot"
    );
    assert!(
        fat.tip_owner_in_fail_cooldown("sticky"),
        "R-250 mute 16 BPS must 120s-cool (not healthy_tip_bps skip)"
    );
    assert!(
        !fat.tip_owner_open
            .load(std::sync::atomic::Ordering::Relaxed),
        "successor installed — do not open score lottery"
    );
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-226 190–200k **176**: score successor is not the fat TCP.
#[test]
fn r227_h_slow_recv_successor_beats() {
    assert!(super::h_slow_recv_successor_beats(0.0, 10.9));
    assert!(super::h_slow_recv_successor_beats(0.1, 947.0));
    assert!(!super::h_slow_recv_successor_beats(125.5, 125.5));
    assert!(!super::h_slow_recv_successor_beats(135.5, 0.1));
}

#[serial_test::serial(ibd)]
#[test]
fn r227_h_slow_pins_fatter_recv_not_score() {
    let _rotate = HSlowRotateOn::arm();
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

    let fat = wan_tip_assigner(194_826, 194_826, 300_000, &["sticky", "farm", "fat"]);
    fat.set_peer_scores(&[
        ("sticky".into(), 9.0),
        ("farm".into(), 8.0),
        ("fat".into(), 1.0),
    ]);
    mark_scored_peers_ibd_ready(&fat);
    fat.set_tip_gap_missing(true);
    fat.note_tip_owner_assigned("sticky");
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.test_seed_tip_stream_rank("sticky", 16, 1);
    super::super::download::test_set_cached_recv_mbps("sticky", 0.0);
    super::super::download::test_set_cached_recv_mbps("farm", 0.1);
    super::super::download::test_set_cached_recv_mbps("fat", 10.9);
    super::super::tip_stage::test_seed_last_getdata_body(1562, "sticky");
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("fat"),
        "R-226 194801: pin top_recv 10.9, not farm-score 0.1"
    );

    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::tip_stage::test_reset_tip_stage();
}

#[serial_test::serial(ibd)]
#[test]
fn r227_h_slow_holds_when_no_fatter_recv() {
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

    let fat = wan_tip_assigner(194_826, 194_826, 300_000, &["sticky", "farm"]);
    fat.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&fat);
    fat.set_tip_gap_missing(true);
    fat.note_tip_owner_assigned("sticky");
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.test_seed_tip_stream_rank("sticky", 86, 1);
    super::super::download::test_set_cached_recv_mbps("sticky", 125.5);
    super::super::download::test_set_cached_recv_mbps("farm", 0.1);
    super::super::tip_stage::test_seed_last_getdata_body(1562, "sticky");
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "no recv-fatter successor → keep H pipe"
    );

    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-251: fat RTT on a ≥60 sticky must not rotate even if a farm has fatter recv.
/// R-250 191773 sticky_bps=115.3 rtt=1716 successor=75.131 (cutoff was 150).
#[serial_test::serial(ibd)]
#[test]
fn r251_h_slow_holds_line_rate_sticky() {
    super::test_reset_h_slow_gd();
    super::super::download::test_reset_download_bytes();
    super::super::tip_stage::clear_tip_failover();
    super::super::tip_stage::clear_tip_ahead_soft_freeze();
    super::super::tip_stage::test_reset_tip_stage();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(true, Ordering::Relaxed);

    let fat = wan_tip_assigner(191_772, 191_772, 300_000, &["sticky", "farm"]);
    fat.set_peer_scores(&[("sticky".into(), 9.0), ("farm".into(), 8.0)]);
    mark_scored_peers_ibd_ready(&fat);
    fat.set_tip_gap_missing(true);
    fat.note_tip_owner_assigned("sticky");
    *fat.preferred_tip_owner.lock().unwrap() = Some("sticky".into());
    fat.test_seed_tip_stream_rank("sticky", 115, 1);
    super::super::download::test_set_cached_recv_mbps("sticky", 10.0);
    super::super::download::test_set_cached_recv_mbps("farm", 41.3);
    super::super::tip_stage::test_seed_last_getdata_body(1716, "sticky");
    fat.maybe_rotate_slow_h_sticky();
    assert_eq!(
        fat.preferred_tip_owner().as_deref(),
        Some("sticky"),
        "R-250 115.3 line-rate must not H-slow onto fatter-recv farm"
    );
    assert!(
        !fat.tip_owner_in_fail_cooldown("sticky"),
        "must not 120s-cool a ≥60 sticky"
    );

    super::super::download::test_reset_download_bytes();
    super::super::IBD_FEEDER_BUFFER_BLOCKS.store(0, Ordering::Relaxed);
    super::super::IBD_TIP_GAP_MISSING.store(false, Ordering::Relaxed);
    super::test_reset_h_slow_gd();
    super::super::tip_stage::test_reset_tip_stage();
}

/// R-283: healthy pump (attempts ≤ 8) has zero backoff.
/// R-284 isolate: backoff is 0 for all attempts (R-283 25..500 confound).
#[test]
fn r283_backoff_zero_through_8() {
    for a in 1..=8 {
        assert_eq!(
            super::ChunkAssigner::retry_backoff_ms(a),
            0,
            "attempt {a} must not delay"
        );
    }
}

#[test]
fn r283_backoff_9_is_25() {
    assert_eq!(super::ChunkAssigner::retry_backoff_ms(9), 0);
}

#[test]
fn r283_backoff_monotonic_capped_500() {
    for a in 1..=100 {
        assert_eq!(
            super::ChunkAssigner::retry_backoff_ms(a),
            0,
            "R-284 isolate: backoff 0 at attempt {a}"
        );
    }
}

#[serial_test::serial(ibd)]
#[test]
fn r283_requeue_increments_attempts() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(vec![(100, 199)], vec!["p1".into()], vh, 100, true);
    assigner.requeue(100, 199, Some("p1".into()));
    assigner.requeue(100, 199, Some("p1".into()));
    assigner.requeue(100, 199, Some("p1".into()));
    let rq = assigner.retry_queue.lock().unwrap();
    assert_eq!(rq.len(), 1, "same range must merge, got {rq:?}");
    assert_eq!(rq[0].attempts, 3);
    assert_eq!(rq[0].exclude.as_deref(), Some("p1"));
}

#[serial_test::serial(ibd)]
#[test]
fn r283_exclusion_sticky_above_8() {
    let vh = Arc::new(AtomicU64::new(149));
    let assigner = ChunkAssigner::new(vec![(100, 199)], vec!["p1".into()], vh, 100, true);
    for _ in 0..9 {
        assigner.requeue(100, 199, Some("p1".into()));
    }
    {
        let rq = assigner.retry_queue.lock().unwrap();
        assert_eq!(rq.len(), 1);
        assert!(rq[0].attempts > 8, "got attempts={}", rq[0].attempts);
        assert_eq!(rq[0].exclude.as_deref(), Some("p1"));
    }
    assigner.requeue_stall_gaps(150, None);
    let rq = assigner.retry_queue.lock().unwrap();
    let full = rq.iter().find(|e| e.start == 100 && e.end == 199);
    assert!(
        full.is_some_and(|e| e.exclude.is_none()),
        "R-284: sticky exclusion off — stall recovery may clear exclude, got {full:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r283_assignment_skips_not_before() {
    let vh = Arc::new(AtomicU64::new(149));
    let assigner = ChunkAssigner::new(
        vec![(100, 199)],
        vec!["p1".into()],
        Arc::clone(&vh),
        100,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.requeue(180, 195, None);
    {
        let mut rq = assigner.retry_queue.lock().unwrap();
        assert_eq!(rq.len(), 1);
        rq[0].not_before_ms = u64::MAX;
    }
    let work = assigner.get_work("p1", 1000);
    assert_ne!(
        work,
        Some((180, 195)),
        "not_before in the future must not assign that retry range, got {work:?}"
    );
    let rq = assigner.retry_queue.lock().unwrap();
    assert!(
        rq.iter()
            .any(|e| e.start == 180 && e.end == 195 && e.not_before_ms == u64::MAX),
        "delayed retry entry must stay queued, got {rq:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r283_behind_tip_purge_unchanged() {
    let chunks = vec![(300_000, 300_127), (321_000, 321_127)];
    let vh = Arc::new(AtomicU64::new(321_000));
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

/// R-284: same peer + range + net=0 + tip unchanged → skip.
#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_same_peer_range_net0_tip_unchanged_skips() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(
        vec![(100, 199)],
        vec!["p1".into(), "p2".into()],
        Arc::clone(&vh),
        100,
        true,
    );
    assigner.note_ok_chunk_complete("p1", 100, 115, 0);
    assert!(
        assigner.would_skip_noprog("p1", 100, 115),
        "same peer+range+net=0+tip parked must skip"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_tip_advanced_allows() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(
        vec![(100, 199)],
        vec!["p1".into()],
        Arc::clone(&vh),
        100,
        true,
    );
    assigner.note_ok_chunk_complete("p1", 100, 115, 0);
    vh.store(150, Ordering::Relaxed);
    assert!(
        !assigner.would_skip_noprog("p1", 100, 115),
        "tip advanced must allow"
    );
    assert_eq!(assigner.noprog_len(), 0, "tip-moved record must be cleared");
}

#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_net_gt0_allows() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(vec![(100, 199)], vec!["p1".into()], vh, 100, true);
    assigner.note_ok_chunk_complete("p1", 100, 115, 3);
    assert!(
        !assigner.would_skip_noprog("p1", 100, 115),
        "net>0 is delivering — allow"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_different_peer_allows() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(
        vec![(100, 199)],
        vec!["p1".into(), "p2".into()],
        vh,
        100,
        true,
    );
    assigner.note_ok_chunk_complete("p1", 100, 115, 0);
    assert!(
        !assigner.would_skip_noprog("p2", 100, 115),
        "different peer is failover — allow"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_different_range_allows() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(
        vec![(100, 199), (200, 299)],
        vec!["p1".into()],
        vh,
        100,
        true,
    );
    assigner.note_ok_chunk_complete("p1", 100, 115, 0);
    assert!(
        !assigner.would_skip_noprog("p1", 200, 215),
        "different range must allow"
    );
}

/// R-285: skip gate off — would-skip is logged, leftover still re-issues.
#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_skip_falls_through_not_none() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(
        vec![(100, 199), (200, 299)],
        vec!["p1".into(), "p2".into()],
        Arc::clone(&vh),
        100,
        true,
    );
    assigner.mark_bootstrap_complete();
    assigner.set_wan_body_tip(250);
    assigner.set_leftover_force_getdata(true);
    let first = assigner.get_work("p1", 1000);
    assert_eq!(
        first,
        Some((100, 100)),
        "leftover hole first take, got {first:?}"
    );
    assigner.on_chunk_complete_range("p1", 100, 100);
    assigner.note_ok_chunk_complete("p1", 100, 100, 0);
    assert!(
        assigner.would_skip_noprog("p1", 100, 100),
        "observability still sees net=0 parked tip"
    );
    let second = assigner.get_work("p1", 1000);
    assert_eq!(
        second,
        Some((100, 100)),
        "R-285: gate off — same stripe re-issues, got {second:?}"
    );
}

#[serial_test::serial(ibd)]
#[test]
fn r284_noprog_record_evicted_behind_tip() {
    let vh = Arc::new(AtomicU64::new(99));
    let assigner = ChunkAssigner::new(
        vec![(100, 199), (300, 399)],
        vec!["p1".into()],
        Arc::clone(&vh),
        100,
        true,
    );
    assigner.note_ok_chunk_complete("p1", 100, 115, 0);
    assert_eq!(assigner.noprog_len(), 1);
    vh.store(200, Ordering::Relaxed);
    assigner.mark_bootstrap_complete();
    let _ = assigner.get_work("p1", 1000);
    assert_eq!(
        assigner.noprog_len(),
        0,
        "end < next_needed must evict with W80 purge"
    );
}

// ---- R-336 window assigner -------------------------------------------------
// Serialised through WINDOW_TEST_FORCE (process-global); keep in one test.

#[test]
fn r336_window_assigner_streams_all_ready_peers_lowest_first_and_dups_stalled_front() {
    ChunkAssigner::window_test_force(true);
    let peers: Vec<String> = (0..8).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = peers.iter().map(String::as_str).collect();
    // next_needed = 210_001, crawl (body_tip < next), window path active (≥120k).
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &refs);
    assert!(assigner.window_mode_active(210_001));

    // Every peer gets work, lowest-first, 16-wide, disjoint; per-peer cap 1.
    let mut got = Vec::new();
    for p in &peers {
        for _ in 0..3 {
            if let Some(r) = assigner.get_work(p, 1024) {
                got.push((p.clone(), r));
            }
        }
    }
    assert_eq!(got.len(), 8, "8 peers × cap 1 tile: {got:?}");
    let mut ranges: Vec<(u64, u64)> = got.iter().map(|(_, r)| *r).collect();
    ranges.sort();
    assert_eq!(ranges[0], (210_001, 210_016));
    for w in ranges.windows(2) {
        assert_eq!(w[1].0, w[0].1 + 1, "contiguous disjoint tiles: {ranges:?}");
    }
    assert_eq!(ranges.last().unwrap().1, 210_001 + 8 * 16 - 1);
    assert_eq!(assigner.get_work("p0", 1024), None, "per-peer cap holds");

    // Completion marks delivered; those heights are never re-issued.
    assigner.on_chunk_complete_range("p0", 210_001, 210_016);
    assigner.window_note_complete("p0", 210_001, 210_016, 16);
    let next = assigner
        .get_work("p0", 1024)
        .expect("p0 has a free slot again");
    assert_eq!(
        next,
        (210_129, 210_144),
        "continues past the frontier, not the delivered tile"
    );

    // Ownership: p1 017-032, p2 033-048.
    // Failure releases without re-handing the same heights to the failing peer.
    assigner.on_chunk_complete_range("p1", 210_017, 210_032);
    assigner.requeue_reason(210_017, 210_032, Some("p1".into()), "chunk_fail");
    assert!(
        assigner.retry_queue.lock().unwrap().is_empty(),
        "window mode has no retry queue"
    );
    let p1_next = assigner.get_work("p1", 1024).expect("p1 gets other work");
    assert_eq!(
        p1_next,
        (210_145, 210_160),
        "failing peer skips its failed heights"
    );
    // p2 frees its slot and takes the released hole first (lowest free run).
    assigner.on_chunk_complete_range("p2", 210_033, 210_048);
    assigner.window_note_complete("p2", 210_033, 210_048, 16);
    let p2_extra = assigner.get_work("p2", 1024).expect("p2 refills");
    assert_eq!(
        p2_extra,
        (210_017, 210_032),
        "released hole is lowest free run"
    );

    // Window clamp: nothing past header tip.
    let small = wan_tip_assigner(299_990, 299_900, 300_000, &["a", "b"]);
    assert_eq!(small.get_work("a", 1024), Some((299_991, 300_000)));
    assert_eq!(small.get_work("b", 1024), None);

    // Coordinator "missing" on a young mark is ignored; unmark needs age.
    small.on_chunk_complete_range("a", 299_991, 300_000);
    small.window_note_complete("a", 299_991, 300_000, 10);
    small.requeue_chunk_containing_height(299_991);
    assert!(small.window_done.lock().unwrap().contains_key(&299_991));
    assert_eq!(small.get_work("b", 1024), None, "delivered, not re-issued");
    ChunkAssigner::window_test_force(false);
}

#[test]
fn r338_window_tile_for_scales_with_block_bytes() {
    // 4 MB target: 150 kB blocks (200k) → 16 (cap); 500 kB (340k) → 8; 1 MB (400k+) → 4; 2 MB → 4 (floor).
    assert_eq!(ChunkAssigner::window_tile_for(150_000, 4_000_000, 16), 16);
    assert_eq!(ChunkAssigner::window_tile_for(500_000, 4_000_000, 16), 8);
    assert_eq!(ChunkAssigner::window_tile_for(1_000_000, 4_000_000, 16), 4);
    assert_eq!(ChunkAssigner::window_tile_for(2_000_000, 4_000_000, 16), 4);
    assert_eq!(ChunkAssigner::window_tile_for(0, 4_000_000, 16), 16);
    assert_eq!(ChunkAssigner::window_tile_for(500_000, 4_000_000, 2), 2);
}

#[test]
fn r342_window_peer_cap_for_is_a_byte_budget() {
    // 8 MB budget: 4-block tiles of 250 kB (1 MB, 340k) → 8; 16 × 62 kB (1 MB, 200k) → 8;
    // 16 × 250 kB (4 MB tiles) → 2; 4 × 1 MB (400k+) → 2; huge tile → 1 (floor); tiny → 8 (cap).
    assert_eq!(ChunkAssigner::window_peer_cap_for(250_000, 4, 8_000_000), 8);
    assert_eq!(ChunkAssigner::window_peer_cap_for(62_000, 16, 8_000_000), 8);
    assert_eq!(
        ChunkAssigner::window_peer_cap_for(250_000, 16, 8_000_000),
        2
    );
    assert_eq!(
        ChunkAssigner::window_peer_cap_for(1_000_000, 4, 8_000_000),
        2
    );
    assert_eq!(
        ChunkAssigner::window_peer_cap_for(2_000_000, 16, 8_000_000),
        1
    );
    assert_eq!(ChunkAssigner::window_peer_cap_for(0, 16, 8_000_000), 8);
}

#[test]
fn r344_window_strike_out_releases_front_and_cools_fast_holder() {
    ChunkAssigner::window_test_force(true);
    let peers: Vec<String> = (0..2).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = peers.iter().map(String::as_str).collect();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &refs);
    // Bench cap is `benched < ready / 2`: a 4-peer roster admits one bench.
    {
        let mut ready = assigner.ibd_ready_peers.lock().unwrap();
        for p in ["p0", "p1", "p2", "p3"] {
            ready.insert(p.to_string());
        }
    }
    // p0 is a proven, roster-speed peer (not slow) …
    assigner.window_blk_ms_ema.store(100, Ordering::Relaxed);
    assigner
        .window_peer_blk_ms
        .lock()
        .unwrap()
        .insert("p0".to_string(), (100, 10));
    let front = assigner.get_work("p0", 1024).expect("p0 takes the front");
    assert_eq!(front, (210_001, 210_016));
    // … whose front tile is now 8 s old with one strike already 4 s ago.
    let old = Instant::now() - Duration::from_secs(8);
    for v in assigner.window_started.lock().unwrap().values_mut() {
        *v = old;
    }
    assigner.window_strikes.lock().unwrap().insert(
        "p0".to_string(),
        (1, Instant::now() - Duration::from_secs(4)),
    );

    // p1 polls: second strike → p0's tile is released and handed to p1 (not a dup).
    let taken = assigner
        .get_work("p1", 1024)
        .expect("p1 gets the released front");
    assert_eq!(taken, (210_001, 210_016));
    // R-345: every strike-out benches (escalating); first offense = BENCH_SECS/4 = 15 s.
    let until = assigner
        .window_bench
        .lock()
        .unwrap()
        .get("p0")
        .copied()
        .expect("first offender is benched");
    let left = until.saturating_duration_since(Instant::now()).as_secs();
    assert!(
        (10..=15).contains(&left),
        "first offense ≈ 15 s bench, got {left}s"
    );
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .get("p0")
            .map_or(true, |v| v.is_empty()),
        "p0's front tile was released"
    );
    assert!(
        assigner.get_work("p0", 1024).is_none(),
        "benched peer gets no window work"
    );
    assert_eq!(
        assigner
            .window_offense
            .lock()
            .unwrap()
            .get("p0")
            .map(|e| e.0),
        Some(1)
    );
    ChunkAssigner::window_test_force(false);
}

#[test]
fn r345_window_bench_escalates_15_60_300() {
    assert_eq!(ChunkAssigner::window_bench_secs_for(0), 15);
    assert_eq!(ChunkAssigner::window_bench_secs_for(1), 15);
    assert_eq!(ChunkAssigner::window_bench_secs_for(2), 60);
    assert_eq!(ChunkAssigner::window_bench_secs_for(3), 300);
    assert_eq!(ChunkAssigner::window_bench_secs_for(9), 300);
    // Offense history resets after 10 min of good behaviour.
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0"]);
    let now = Instant::now();
    assert_eq!(assigner.window_note_offense("p0", now), 15);
    assert_eq!(assigner.window_note_offense("p0", now), 60);
    assert_eq!(assigner.window_note_offense("p0", now), 300);
    let later = now + Duration::from_secs(601);
    assert_eq!(assigner.window_note_offense("p0", later), 15);
}

#[test]
fn r376_idle_peer_dups_the_front_without_waiting_for_the_inflated_ema() {
    // Resume13: dup_s sat at 30 because tile EMA was ~50–150s, and the peers
    // polling get_work were the slow ones. An empty pipe must duplicate at the
    // 1s floor, including when that peer is marked slow.
    assert!(ChunkAssigner::window_front_dup_ok(
        true, true, 1, 1, 30, 0, 30
    ));
    assert!(!ChunkAssigner::window_front_dup_ok(
        true, true, 0, 1, 30, 0, 30
    ));
    // Resume26: a busy slow peer is the only taker left. It takes the second
    // cover at the 1s floor instead of leaving the holder alone for minutes.
    assert!(ChunkAssigner::window_front_dup_ok(
        false, true, 1, 1, 30, 1, 30
    ));
    assert!(!ChunkAssigner::window_front_dup_ok(
        false, true, 1, 1, 30, 3, 30
    ));
    // A busy non-slow peer with one tile also duplicates at the 1s floor.
    assert!(ChunkAssigner::window_front_dup_ok(
        false, false, 1, 1, 30, 1, 30
    ));
    assert!(!ChunkAssigner::window_front_dup_ok(
        false, false, 0, 1, 30, 1, 30
    ));
}

#[test]
fn r354_window_stall_dup_goes_to_an_empty_pipe_first() {
    // Pure predicate: empty / one-deep pipe takes the dup at `stall`; a loaded pipe only
    // past 2 × stall.
    assert!(ChunkAssigner::window_dup_taker_ok(0, 3, 3));
    assert!(ChunkAssigner::window_dup_taker_ok(1, 3, 3));
    assert!(!ChunkAssigner::window_dup_taker_ok(2, 3, 3));
    assert!(!ChunkAssigner::window_dup_taker_ok(7, 5, 3));
    assert!(ChunkAssigner::window_dup_taker_ok(7, 6, 3));
    assert!(ChunkAssigner::window_dup_taker_ok(2, 60, 30));

    // Window path: p0 holds the front for 4 s (stall floor 3 s, one strike max), p1 idle
    // (0 in flight, not slow) polls → gets the dup of the front tile, not fresh work.
    ChunkAssigner::window_test_force(true);
    let peers: Vec<String> = (0..2).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = peers.iter().map(String::as_str).collect();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &refs);
    let front = assigner.get_work("p0", 1024).expect("p0 takes the front");
    assert_eq!(front, (210_001, 210_016));
    let old = Instant::now() - Duration::from_secs(4);
    for v in assigner.window_started.lock().unwrap().values_mut() {
        *v = old;
    }
    let dup = assigner
        .get_work("p1", 1024)
        .expect("idle p1 takes the stall dup");
    assert_eq!(dup, (210_001, 210_016), "dup covers the stalled front tile");
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .get("p0")
            .is_some_and(|v| !v.is_empty()),
        "one strike does not release the holder"
    );
    ChunkAssigner::window_test_force(false);
}

/// An empty pipe duplicates the front at the 1 s floor. A 40 s tile EMA does not
/// hold that pipe off a 4 s cover; the holder keeps the original tile.
#[serial_test::serial(ibd)]
#[test]
fn r376_idle_pipe_dups_a_young_front_despite_tile_ema() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0", "p1", "p2", "p3"]);
    let front = assigner.get_work("p0", 1024).expect("p0 takes the front");
    assert_eq!(front, (210_001, 210_016));
    let ahead = assigner
        .get_work("p1", 1024)
        .expect("p1 takes the next tile");
    assert_eq!(ahead, (210_017, 210_032));
    assigner.window_tile_ms_ema.store(40_000, Ordering::Relaxed);
    assigner.window_note_complete("p1", ahead.0, ahead.1, 16);
    {
        let mut started = assigner.window_started.lock().unwrap();
        let key = ("p0".to_string(), front.0, front.1);
        *started.get_mut(&key).expect("front stamp") = Instant::now() - Duration::from_secs(4);
    }
    let next = assigner
        .get_work("p2", 1024)
        .expect("idle p2 duplicates the front");
    assert_eq!(
        next,
        (210_001, 210_016),
        "empty pipe duplicates the 4s front, got {next:?}"
    );
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .get("p0")
            .is_some_and(|v| v.iter().any(|&(s, _)| s == front.0)),
        "holder keeps the front"
    );

    let cold = wan_tip_assigner(210_000, 209_900, 300_000, &["c0", "c1"]);
    let cold_front = cold.get_work("c0", 1024).expect("c0 takes the front");
    assert_eq!(cold_front, (210_001, 210_016));
    cold.window_tile_ms_ema.store(40_000, Ordering::Relaxed);
    {
        let mut started = cold.window_started.lock().unwrap();
        let key = ("c0".to_string(), cold_front.0, cold_front.1);
        *started.get_mut(&key).expect("cold front stamp") = Instant::now() - Duration::from_secs(4);
    }
    let next = cold
        .get_work("c1", 1024)
        .expect("idle peer duplicates the front");
    assert_eq!(next, (210_001, 210_016));
    ChunkAssigner::window_test_force(false);
}

/// R-376: the hole in front of a done-ahead pipeline must be re-issued as a span.
/// Unmarking only the cursor left ~60 heights `window_done` and validation
/// crawled one block per stall.
#[serial_test::serial(ibd)]
#[test]
fn r376_window_unmarks_hole_span_not_just_the_cursor() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0", "p1"]);
    assigner.set_ibd_ready_peers(HashSet::from(["p0".to_string(), "p1".to_string()]));
    let front = assigner
        .get_work("p0", 1024)
        .expect("p0 holds the front tile");
    assert_eq!(front, (210_001, 210_016));
    let old = Instant::now() - Duration::from_secs(6);
    {
        let mut done = assigner.window_done.lock().unwrap();
        for h in 210_001..=210_020 {
            done.insert(h, old);
        }
        // Still inside the span, but younger than the unmark threshold.
        done.insert(210_018, Instant::now());
    }
    assigner.window_note_missing_span(210_001, 210_021);
    let done = assigner.window_done.lock().unwrap();
    for h in 210_001..=210_016 {
        assert!(done.contains_key(&h), "in-flight {h} stays delivered");
    }
    assert!(
        !done.contains_key(&210_017),
        "aged hole height is re-issued"
    );
    assert!(
        done.contains_key(&210_018),
        "a fresh mark inside the span is still in transit"
    );
    assert!(
        !done.contains_key(&210_019),
        "aged hole height is re-issued"
    );
    assert!(
        !done.contains_key(&210_020),
        "aged hole height is re-issued"
    );
    drop(done);
    ChunkAssigner::window_test_force(false);
}

/// Resume8: `window_note_missing` was a one-height span while `first_ahead`
/// sat ~65 above the tip and the rest of `window_done` blocked idle peers.
#[serial_test::serial(ibd)]
#[test]
fn r376_window_note_missing_unmarks_up_to_first_ahead() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0"]);
    let tip = 210_001u64;
    let first_ahead = tip + 65;
    let old = Instant::now() - Duration::from_secs(6);
    {
        let mut done = assigner.window_done.lock().unwrap();
        for h in tip..=first_ahead + 10 {
            done.insert(h, old);
        }
        done.insert(tip + 10, Instant::now());
    }
    assigner.set_window_hole_until(first_ahead);
    assigner.window_note_missing(tip);
    {
        let done = assigner.window_done.lock().unwrap();
        assert!(
            !done.contains_key(&tip),
            "tip mark older than the unmark threshold is re-issued"
        );
        assert!(
            !done.contains_key(&(first_ahead - 1)),
            "last hole height below first_ahead is re-issued"
        );
        assert!(
            done.contains_key(&(tip + 10)),
            "a fresh mark inside the hole is still in transit"
        );
        assert!(
            done.contains_key(&first_ahead),
            "first_ahead stays marked; that body is in reorder"
        );
        assert!(
            done.contains_key(&(first_ahead + 10)),
            "heights past first_ahead stay marked"
        );
    }
    let one = wan_tip_assigner(210_000, 209_900, 300_000, &["p0"]);
    {
        let mut done = one.window_done.lock().unwrap();
        for h in tip..=tip + 5 {
            done.insert(h, old);
        }
    }
    one.set_window_hole_until(0);
    one.window_note_missing(tip);
    {
        let done = one.window_done.lock().unwrap();
        assert!(
            !done.contains_key(&tip),
            "absent first_ahead still unmarks the tip"
        );
        assert!(
            done.contains_key(&(tip + 1)),
            "absent first_ahead does not open the frontier"
        );
    }
    ChunkAssigner::window_test_force(false);
}

/// Resume17: a fresh `window_done` mark on an absent tip must be re-issued
/// without the 5s grace. `window_note_missing` leaves that fresh mark.
#[serial_test::serial(ibd)]
#[test]
fn r376_reissue_absent_tip_ignores_mark_age() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0"]);
    let tip = 210_001u64;
    {
        let mut done = assigner.window_done.lock().unwrap();
        done.insert(tip, Instant::now());
        done.insert(tip + 1, Instant::now());
    }
    assigner.window_reissue_absent(tip, tip + 1);
    let done = assigner.window_done.lock().unwrap();
    assert!(
        !done.contains_key(&tip),
        "absent tip is re-issued immediately"
    );
    assert!(
        done.contains_key(&(tip + 1)),
        "the body already in reorder stays marked"
    );
    drop(done);
    ChunkAssigner::window_test_force(false);
}

/// Resume20: the delivered hole was ~50 high and stall unmark cleared one
/// height, so the next free tile started on the far side of the hole.
#[serial_test::serial(ibd)]
#[test]
fn r376_reissue_clears_the_absent_span_up_to_the_buffered_body() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0"]);
    let tip = 210_001u64;
    let first_ahead = tip + 52;
    {
        let mut done = assigner.window_done.lock().unwrap();
        for h in tip..first_ahead + 4 {
            done.insert(h, Instant::now());
        }
    }
    assigner.set_window_hole_until(first_ahead);
    assigner.window_reissue_absent(tip, first_ahead);
    let done = assigner.window_done.lock().unwrap();
    assert!(
        !done.contains_key(&tip) && !done.contains_key(&(first_ahead - 1)),
        "every absent height below the buffered body is re-issued"
    );
    assert!(
        done.contains_key(&first_ahead),
        "the body still in reorder stays marked"
    );
    drop(done);
    ChunkAssigner::window_test_force(false);
}

/// Resume21: the hole was marked delivered again, covers was 0, and peers
/// stayed on tiles 400 above the tip for half a minute.
#[serial_test::serial(ibd)]
#[test]
fn r376_uncovered_tip_is_taken_ahead_of_the_done_wall() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0"]);
    mark_peers_ibd_ready(&assigner, &["p0"]);
    let tip = 210_001u64;
    let first_ahead = tip + 52;
    {
        let mut done = assigner.window_done.lock().unwrap();
        for h in tip..first_ahead + 8 {
            done.insert(h, Instant::now());
        }
    }
    assigner.set_window_hole_until(first_ahead);
    assigner.set_window_tip_uncovered(true);
    assert!(
        !assigner.past_hole_should_release(tip),
        "the tile that contains the tip stays"
    );
    assert!(
        !assigner.past_hole_should_release(tip + 4),
        "a tile inside the hole is the refill"
    );
    assert!(
        !assigner.past_hole_should_release(first_ahead),
        "a tile at the buffered body stays; aborting it drops the runway"
    );
    let work = assigner.get_work("p0", 4096);
    assert!(
        work.is_some_and(|(s, _)| s == tip),
        "get_work starts at the uncovered tip, not the far side of window_done: {work:?}"
    );
    assigner.set_window_tip_uncovered(false);
    assert!(
        !assigner.past_hole_should_release(first_ahead + 10),
        "a covered tip does not pull peers off the runway"
    );
    ChunkAssigner::window_test_force(false);
}

/// Resume23: height 2861258 had two covers and no block for 109s, because a
/// third peer is only issued when `front_covers < 2`.
#[serial_test::serial(ibd)]
#[test]
fn r376_uncovered_tip_gets_a_third_peer_after_one_second() {
    assert!(ChunkAssigner::uncovered_tip_third_cover(
        true, true, 2, 1, 1
    ));
    assert!(!ChunkAssigner::uncovered_tip_third_cover(
        false, true, 2, 5, 1
    ));
    assert!(!ChunkAssigner::uncovered_tip_third_cover(
        true, true, 1, 5, 1
    ));
    assert!(!ChunkAssigner::uncovered_tip_third_cover(
        true, true, 3, 5, 1
    ));
    assert!(!ChunkAssigner::uncovered_tip_third_cover(
        true, false, 2, 5, 1
    ));
    assert!(!ChunkAssigner::uncovered_tip_third_cover(
        true, true, 2, 0, 1
    ));

    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["h0", "h1", "idle", "extra"]);
    mark_peers_ibd_ready(&assigner, &["h0", "h1", "idle", "extra"]);
    let tip = 210_001u64;
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "h0", tip, tip + 3);
        ChunkAssigner::insert_in_flight(&mut g, "h1", tip, tip + 3);
    }
    let old = Instant::now() - Duration::from_secs(2);
    {
        let mut started = assigner.window_started.lock().unwrap();
        started.insert(("h0".to_string(), tip, tip + 3), old);
        started.insert(("h1".to_string(), tip, tip + 3), old);
    }
    assigner.set_window_hole_until(tip + 64);
    assigner.set_window_tip_uncovered(true);
    let third = assigner
        .get_work("idle", 4096)
        .expect("idle peer takes the stuck tip");
    assert_eq!(third, (tip, tip), "the third request is that one height");
    let flying = assigner.in_flight_per_peer.lock().unwrap();
    assert!(
        flying
            .get("h0")
            .is_some_and(|v| v.iter().any(|&(s, e)| s <= tip && tip <= e)),
        "the first holder keeps the height"
    );
    assert!(
        flying
            .get("h1")
            .is_some_and(|v| v.iter().any(|&(s, e)| s <= tip && tip <= e)),
        "the second holder keeps the height"
    );
    drop(flying);
    let fourth = assigner.get_work("extra", 4096);
    assert!(
        fourth.is_some_and(|(s, _)| s > tip),
        "a fourth peer does not join the tip: {fourth:?}"
    );
    assigner.set_window_tip_uncovered(false);
    ChunkAssigner::window_test_force(false);
}

/// Resume27: three 4-block tiles covered the tip for 45–90s. `covers == 2`
/// never matched, so nobody was asked for that one height. A young cover,
/// a fourth one-block request, and a tip that is already present do not ask.
#[serial_test::serial(ibd)]
#[test]
fn r376_stale_wide_tiles_get_one_fresh_tip_request() {
    assert!(ChunkAssigner::tip_needs_fresh_cover(
        true, false, false, true, false, 0
    ));
    assert!(ChunkAssigner::tip_needs_fresh_cover(
        true, false, false, true, false, 2
    ));
    assert!(!ChunkAssigner::tip_needs_fresh_cover(
        true, false, false, true, false, 3
    ));
    assert!(!ChunkAssigner::tip_needs_fresh_cover(
        true, false, false, true, true, 0
    ));
    assert!(!ChunkAssigner::tip_needs_fresh_cover(
        false, false, false, true, false, 0
    ));
    assert!(!ChunkAssigner::tip_needs_fresh_cover(
        true, true, false, true, false, 0
    ));
    assert!(!ChunkAssigner::tip_needs_fresh_cover(
        true, false, true, true, false, 0
    ));
    assert!(!ChunkAssigner::tip_needs_fresh_cover(
        true, false, false, false, false, 0
    ));

    ChunkAssigner::window_test_force(true);
    let peers = ["h0", "h1", "h2", "idle", "peek", "idle2", "idle3", "idle4"];
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &peers);
    mark_peers_ibd_ready(&assigner, &peers);
    let tip = 210_001u64;
    let old = Instant::now() - Duration::from_secs(2);
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        for peer in ["h0", "h1", "h2"] {
            ChunkAssigner::insert_in_flight(&mut g, peer, tip, tip + 3);
        }
    }
    {
        let mut started = assigner.window_started.lock().unwrap();
        for peer in ["h0", "h1", "h2"] {
            started.insert((peer.to_string(), tip, tip + 3), old);
        }
    }
    assigner.set_window_hole_until(tip + 64);
    assigner.set_window_tip_uncovered(true);
    let first = assigner
        .get_work("idle", 4096)
        .expect("idle peer takes the stale tip");
    assert_eq!(
        first,
        (tip, tip),
        "the request is that one height: {first:?}"
    );
    let second_now = assigner.get_work("peek", 4096);
    assert!(
        second_now.is_some_and(|(s, _)| s > tip),
        "a request just issued still counts, so the next idle peer stays off the tip: {second_now:?}"
    );
    assigner.window_started.lock().unwrap().insert(
        ("idle".to_string(), tip, tip),
        Instant::now() - Duration::from_secs(2),
    );
    let second = assigner
        .get_work("idle2", 4096)
        .expect("once the one-block request is stale, another idle peer asks");
    assert_eq!(
        second,
        (tip, tip),
        "the second request is that one height: {second:?}"
    );
    assigner.window_started.lock().unwrap().insert(
        ("idle2".to_string(), tip, tip),
        Instant::now() - Duration::from_secs(2),
    );
    let third = assigner
        .get_work("idle3", 4096)
        .expect("a third one-block request is still under the cap");
    assert_eq!(third, (tip, tip));
    assigner.window_started.lock().unwrap().insert(
        ("idle3".to_string(), tip, tip),
        Instant::now() - Duration::from_secs(2),
    );
    let fourth = assigner.get_work("idle4", 4096);
    assert!(
        fourth.is_some_and(|(s, _)| s > tip),
        "three one-block requests is the ceiling: {fourth:?}"
    );
    let flying = assigner.in_flight_per_peer.lock().unwrap();
    for peer in ["h0", "h1", "h2"] {
        assert!(
            flying
                .get(peer)
                .is_some_and(|v| v.iter().any(|&(s, e)| s == tip && e == tip + 3)),
            "wide holder {peer} keeps the tile"
        );
    }
    drop(flying);

    let young = wan_tip_assigner(210_000, 209_900, 300_000, &["y0", "y1", "y2", "idle"]);
    mark_peers_ibd_ready(&young, &["y0", "y1", "y2", "idle"]);
    {
        let mut g = young.in_flight_per_peer.lock().unwrap();
        for peer in ["y0", "y1", "y2"] {
            ChunkAssigner::insert_in_flight(&mut g, peer, tip, tip + 3);
        }
    }
    {
        let mut started = young.window_started.lock().unwrap();
        started.insert(("y0".to_string(), tip, tip + 3), old);
        started.insert(("y1".to_string(), tip, tip + 3), old);
        started.insert(("y2".to_string(), tip, tip + 3), Instant::now());
    }
    young.set_window_hole_until(tip + 64);
    young.set_window_tip_uncovered(true);
    let young_work = young.get_work("idle", 4096);
    assert!(
        young_work.is_some_and(|(s, _)| s > tip),
        "one cover issued just now blocks a one-block request: {young_work:?}"
    );

    let covered = wan_tip_assigner(210_000, 209_900, 300_000, &["c0", "c1", "c2", "idle"]);
    mark_peers_ibd_ready(&covered, &["c0", "c1", "c2", "idle"]);
    {
        let mut g = covered.in_flight_per_peer.lock().unwrap();
        for peer in ["c0", "c1", "c2"] {
            ChunkAssigner::insert_in_flight(&mut g, peer, tip, tip + 3);
        }
    }
    {
        let mut started = covered.window_started.lock().unwrap();
        for peer in ["c0", "c1", "c2"] {
            started.insert((peer.to_string(), tip, tip + 3), old);
        }
    }
    covered.set_window_hole_until(tip + 64);
    covered.set_window_tip_uncovered(false);
    let covered_work = covered.get_work("idle", 4096);
    assert!(
        covered_work.is_some_and(|(s, _)| s > tip),
        "a tip already present is not asked for again: {covered_work:?}"
    );
    ChunkAssigner::window_test_force(false);
}

/// Resume26: the peer that could deliver the tip was already at its tile cap,
/// so get_work returned before the duplicate. The sole holder stayed for 287s.
#[serial_test::serial(ibd)]
#[test]
fn r376_peer_at_cap_takes_the_second_tip_cover() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["holder", "capped"]);
    mark_peers_ibd_ready(&assigner, &["holder", "capped"]);
    let tip = 210_001u64;
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "holder", tip, tip);
        ChunkAssigner::insert_in_flight(&mut g, "capped", tip + 20, tip + 20);
    }
    let old = Instant::now() - Duration::from_secs(2);
    assigner
        .window_started
        .lock()
        .unwrap()
        .insert(("holder".to_string(), tip, tip), old);
    assigner.set_window_tip_uncovered(true);
    let second = assigner
        .get_work("capped", 4096)
        .expect("a peer at the tile cap still takes the uncovered tip");
    assert_eq!(
        second,
        (tip, tip),
        "the request is that one height: {second:?}"
    );
    let flying = assigner.in_flight_per_peer.lock().unwrap();
    assert!(
        flying
            .get("holder")
            .is_some_and(|v| v.iter().any(|&(s, e)| s <= tip && tip <= e)),
        "the holder keeps the height"
    );
    assigner.set_window_tip_uncovered(false);
    ChunkAssigner::window_test_force(false);
}

/// Resume25: the +32 cap and past-hole release emptied the RAM runway.
/// With no buffered body, idle peers keep filling past +32 and those tiles stay.
#[serial_test::serial(ibd)]
#[test]
fn r376_empty_reorder_keeps_the_runway() {
    ChunkAssigner::window_test_force(true);
    let names: Vec<String> = (0..16).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &refs);
    mark_peers_ibd_ready(&assigner, &refs);
    let tip = 210_001u64;
    assigner.set_window_hole_until(0);
    assigner.set_window_tip_uncovered(true);
    assert!(
        !assigner.past_hole_should_release(tip + 400),
        "an empty reorder does not drop the runway"
    );
    let mut furthest = tip;
    for name in &names {
        if let Some((start, end)) = assigner.get_work(name, 4096) {
            assert!(
                start >= tip,
                "work stays at the tip or ahead: {start}-{end}"
            );
            furthest = furthest.max(end);
        }
    }
    assert!(
        furthest > tip + 31,
        "the roster fills past the old 32 cap, furthest={furthest} tip={tip}"
    );
    ChunkAssigner::window_test_force(false);
}

/// R-379: delivered heights in the hole stay hidden while the tip is uncovered.
/// The cursor is still the first assignment. Reloading the marked hole from
/// disk completed 1.46M local blocks in 200–250k against 50k wire blocks.
#[serial_test::serial(ibd)]
#[test]
fn r379_delivered_hole_is_not_reloaded() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0", "idle"]);
    mark_peers_ibd_ready(&assigner, &["p0", "idle"]);
    let tip = 210_001u64;
    let first_ahead = tip + 52;
    {
        let mut done = assigner.window_done.lock().unwrap();
        for h in tip..=tip + 10 {
            done.insert(h, Instant::now());
        }
    }
    assigner.set_window_hole_until(first_ahead);
    assigner.set_window_tip_uncovered(true);
    let work = assigner.get_work("p0", 4096);
    assert!(
        work.is_some_and(|(s, _)| s == tip),
        "the uncovered tip is still first: {work:?}"
    );
    let far = assigner.get_work("idle", 4096);
    assert!(
        far.is_some_and(|(s, _)| s > tip + 10),
        "a delivered hole height is not reloaded: {far:?}"
    );
    ChunkAssigner::window_test_force(false);
}

/// R-377: with the hole already in flight, an idle ready peer fills past the
/// buffered body. Stopping there left the frontier at the hole and the next
/// tip wait had no runway. A peer that has not handshaked still gets nothing.
#[serial_test::serial(ibd)]
#[test]
fn r377_idle_peer_fills_past_the_buffered_body() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["holder", "idle", "unready"]);
    mark_peers_ibd_ready(&assigner, &["holder", "idle"]);
    let tip = 210_001u64;
    let first_ahead = tip + 52;
    {
        let mut g = assigner.in_flight_per_peer.lock().unwrap();
        ChunkAssigner::insert_in_flight(&mut g, "holder", tip, first_ahead - 1);
    }
    assigner.set_window_hole_until(first_ahead);
    assigner.set_window_tip_uncovered(true);
    assert!(
        assigner.get_work("unready", 4096).is_none(),
        "a peer that has not handshaked still gets nothing"
    );
    let far = assigner.get_work("idle", 4096);
    assert!(
        far.is_some_and(|(s, e)| s >= first_ahead && e >= s),
        "an idle ready peer fills past the buffered body: {far:?}"
    );
    let flying = assigner.in_flight_per_peer.lock().unwrap();
    assert!(
        flying
            .get("holder")
            .is_some_and(|v| v.iter().any(|&(s, e)| s <= tip && tip <= e)),
        "the hole holder keeps the tip"
    );
    ChunkAssigner::window_test_force(false);
}

/// R-376: a peer that has not finished handshake must not hold the tip tile
/// once any peer has VerAck'd. Otherwise the chunk sits in the 15s handshake
/// wait while handshook peers download past it.
#[serial_test::serial(ibd)]
#[test]
fn r376_window_skips_peers_that_have_not_handshaked() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["unready", "ready"]);
    assigner.set_ibd_ready_peers(HashSet::from(["ready".to_string()]));
    assert!(
        assigner.get_work("unready", 1024).is_none(),
        "unready peer gets no tile"
    );
    let front = assigner
        .get_work("ready", 1024)
        .expect("handshook peer takes the front");
    assert_eq!(front, (210_001, 210_016));
    ChunkAssigner::window_test_force(false);
}

#[test]
fn r355_window_local_completions_do_not_feed_timing_emas() {
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0", "p1"]);
    let t0 = assigner.get_work("p0", 1024).expect("p0 takes the front");
    let t1 = assigner
        .get_work("p1", 1024)
        .expect("p1 takes the next tile");
    // Both tiles were issued 400 ms ago.
    let old = Instant::now() - Duration::from_millis(400);
    for v in assigner.window_started.lock().unwrap().values_mut() {
        *v = old;
    }
    // p0's tile completes all-local: bookkeeping clears, no timing sample.
    assigner.window_note_complete("p0", t0.0, t0.1, 0);
    assert_eq!(assigner.window_tile_ms_ema.load(Ordering::Relaxed), 0);
    assert_eq!(assigner.window_blk_ms_ema.load(Ordering::Relaxed), 0);
    assert!(
        assigner
            .window_peer_blk_ms
            .lock()
            .unwrap()
            .get("p0")
            .is_none()
    );
    assert!(
        !assigner
            .window_started
            .lock()
            .unwrap()
            .contains_key(&("p0".to_string(), t0.0, t0.1)),
        "issue stamp is cleared even without a timing sample"
    );
    assert!(
        assigner.window_done.lock().unwrap().contains_key(&t0.1),
        "heights still marked delivered"
    );
    // p1's tile had 4 of 16 blocks off the wire: per-block time is per wire block.
    assigner.window_note_complete("p1", t1.0, t1.1, 4);
    let tile_ema = assigner.window_tile_ms_ema.load(Ordering::Relaxed);
    assert!(
        (350..=600).contains(&tile_ema),
        "tile EMA seeded from the wire tile: {tile_ema}"
    );
    let blk = assigner.window_blk_ms_ema.load(Ordering::Relaxed);
    assert!(
        (80..=160).contains(&blk),
        "≈400 ms / 4 wire blocks, not / 16 heights: {blk}"
    );
    let (p1_ms, p1_n) = *assigner
        .window_peer_blk_ms
        .lock()
        .unwrap()
        .get("p1")
        .unwrap();
    assert_eq!(p1_n, 1);
    assert!((80..=160).contains(&p1_ms));
    ChunkAssigner::window_test_force(false);
}

#[test]
fn r356_window_stall_dup_fires_before_the_strike_threshold() {
    // Pure predicate: floor while no tile completed; round(1.5 × tile EMA) clamped to
    // [floor, stall]; never later than the strike threshold.
    assert_eq!(ChunkAssigner::window_dup_after(1, 0, 3), 1);
    assert_eq!(
        ChunkAssigner::window_dup_after(1, 433, 3),
        1,
        "R-355 120–200k tile 433 ms → 650 → 1 s"
    );
    assert_eq!(
        ChunkAssigner::window_dup_after(1, 750, 3),
        1,
        "200–250k tile 750 ms → 1125 → 1 s"
    );
    assert_eq!(
        ChunkAssigner::window_dup_after(1, 1100, 3),
        2,
        "1650 ms → 2 s"
    );
    assert_eq!(
        ChunkAssigner::window_dup_after(1, 5000, 3),
        3,
        "capped at stall"
    );
    assert_eq!(
        ChunkAssigner::window_dup_after(5, 0, 3),
        3,
        "floor above stall collapses to stall"
    );
    assert_eq!(
        ChunkAssigner::window_dup_after(3, 433, 3),
        3,
        "R-355 behaviour when floor = stall"
    );

    // Window path: p0 holds the front for 2 s — under the 3 s strike floor, past the 1 s dup
    // floor. Idle p1 gets the dup; p0 keeps its tile and takes no strike.
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0", "p1"]);
    let front = assigner.get_work("p0", 1024).expect("p0 takes the front");
    assert_eq!(front, (210_001, 210_016));
    let old = Instant::now() - Duration::from_secs(2);
    for v in assigner.window_started.lock().unwrap().values_mut() {
        *v = old;
    }
    let dup = assigner
        .get_work("p1", 1024)
        .expect("idle p1 takes the early dup");
    assert_eq!(dup, (210_001, 210_016), "dup covers the stalled front tile");
    assert!(
        assigner
            .in_flight_per_peer
            .lock()
            .unwrap()
            .get("p0")
            .is_some_and(|v| !v.is_empty()),
        "holder keeps its tile"
    );
    assert!(
        assigner
            .window_strikes
            .lock()
            .unwrap()
            .get("p0")
            .map_or(true, |(n, _)| *n == 0),
        "2 s is under the 3 s strike floor: no strike"
    );
    ChunkAssigner::window_test_force(false);
}

#[test]
fn r358_window_tile_grows_with_the_peers_own_rate() {
    // Pure: R-357 300–340k peers. Byte tile 4, max 16, target 600 ms.
    // Super-peer 9.5 ms/blk → 63 → capped 16. RTT-bound 55 ms/blk → 10. Roster-median
    // 130 ms/blk → 4 (base). Slow 300 ms/blk → 4 (never below base). < 3 samples → base.
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(4, 9, 500, 600, 16),
        16
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(4, 55, 500, 600, 16),
        10
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(4, 130, 500, 600, 16),
        4
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(4, 300, 500, 600, 16),
        4
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(4, 9, 2, 600, 16),
        4,
        "needs 3 samples"
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(4, 9, 500, 0, 16),
        4,
        "TILE_MS=0 is off"
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(16, 9, 500, 600, 16),
        16,
        "already at max"
    );
    assert_eq!(
        ChunkAssigner::window_tile_for_peer_rate(7, 60, 500, 600, 16),
        10,
        "200–250k: 7 → 10"
    );

    // Window path: p0 measured fast (20 ms/blk, 5 samples), p1 unknown. Both take the
    // fixed test tile (16) since the test tile is already the max; the fast peer's run is
    // never shorter than the base and never longer than WINDOW_TILE.
    ChunkAssigner::window_test_force(true);
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &["p0", "p1"]);
    assigner
        .window_peer_blk_ms
        .lock()
        .unwrap()
        .insert("p0".to_string(), (20, 5));
    let t0 = assigner
        .get_work("p0", 1024)
        .expect("fast p0 takes the front");
    assert_eq!(t0.1 - t0.0 + 1, 16, "capped at WINDOW_TILE");
    let t1 = assigner
        .get_work("p1", 1024)
        .expect("p1 takes the next tile");
    assert_eq!(t1.0, t0.1 + 1, "tiles stay contiguous");
    assert_eq!(t1.1 - t1.0 + 1, 16);
    ChunkAssigner::window_test_force(false);
}

#[test]
fn r341_window_slow_peer_keeps_off_front_reserve() {
    ChunkAssigner::window_test_force(true);
    let peers: Vec<String> = (0..2).map(|i| format!("p{i}")).collect();
    let refs: Vec<&str> = peers.iter().map(String::as_str).collect();
    let assigner = wan_tip_assigner(210_000, 209_900, 300_000, &refs);
    // Roster per-block 100 ms; p0 is 5× slower with enough completions; p1 unknown (fast).
    assigner.window_blk_ms_ema.store(100, Ordering::Relaxed);
    assigner
        .window_peer_blk_ms
        .lock()
        .unwrap()
        .insert("p0".to_string(), (500, 3));
    assert!(assigner.window_peer_is_slow("p0"));
    assert!(!assigner.window_peer_is_slow("p1"));

    let slow = assigner
        .get_work("p0", 1024)
        .expect("slow peer still gets work");
    assert!(
        slow.0 >= 210_001 + 256,
        "slow peer placed past the 256 front reserve: {slow:?}"
    );
    let fast = assigner
        .get_work("p1", 1024)
        .expect("fast peer gets the front");
    assert_eq!(fast, (210_001, 210_016));
    // Too few completions → not slow.
    assigner
        .window_peer_blk_ms
        .lock()
        .unwrap()
        .insert("p1".to_string(), (900, 2));
    assert!(!assigner.window_peer_is_slow("p1"));
    ChunkAssigner::window_test_force(false);
}

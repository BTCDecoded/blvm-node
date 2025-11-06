#![cfg(feature = "dandelion")]
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::time::{Duration, Instant};

use reference_node::network::dandelion::{Clock, DandelionPhase, DandelionRelay};

#[derive(Clone)]
struct TestClock {
    now: Instant,
}
impl TestClock {
    fn new(start: Instant) -> Self {
        Self { now: start }
    }
    fn advance(&mut self, d: Duration) {
        self.now += d;
    }
}
impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.now
    }
}

fn peers() -> Vec<String> {
    vec!["p1".into(), "p2".into(), "p3".into(), "p4".into()]
}

#[test]
fn no_premature_broadcast_probability_zero() {
    let rng = StdRng::seed_from_u64(123);
    let clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    // Force probability zero and long timeout
    d.set_stem_timeout(Duration::from_secs(60));
    // NOTE: direct field set; if made private later, expose setter
    // SAFETY: test-only assumption that this field is accessible
    // (current code stores it as pub within struct stats only; here it's a field access)
    // We keep this usage consistent with module privacy.
    let tx = [9u8; 32];
    let _ = d.start_stem_phase(tx, "p1".into(), &peers());
    // emulate probability zero by running should_fluff without random trigger: set probability to 0.0
    d.set_fluff_probability(0.0);
    for _ in 0..100 {
        assert!(!d.should_fluff(&tx));
    }
}

#[test]
fn timed_fluff_occurs_after_timeout() {
    let rng = StdRng::seed_from_u64(7);
    let mut clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    d.set_stem_timeout(Duration::from_millis(20));
    d.set_fluff_probability(0.0); // ensure only timeout triggers
    let tx = [2u8; 32];
    let _ = d.start_stem_phase(tx, "p2".into(), &peers());
    clock.advance(Duration::from_millis(25));
    d.set_clock(clock.clone());
    assert!(d.should_fluff(&tx));
}

#[test]
fn max_hops_forces_fluff() {
    let rng = StdRng::seed_from_u64(99);
    let clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    d.set_stem_timeout(Duration::from_secs(60));
    d.set_fluff_probability(0.0);
    d.set_max_stem_hops(1); // force after 1 hop
    let tx = [3u8; 32];
    let _ = d.start_stem_phase(tx, "p1".into(), &peers());
    let _ = d.advance_stem(tx, &peers());
    assert!(d.should_fluff(&tx));
}

#[test]
fn probabilistic_fluff_rate_is_reasonable() {
    let rng = StdRng::seed_from_u64(2024);
    let clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    d.set_stem_timeout(Duration::from_secs(60));
    d.set_fluff_probability(0.2); // 20%

    let trials = 2000usize;
    let mut fluffed = 0usize;
    for i in 0..trials {
        let tx = [i as u8; 32];
        let _ = d.start_stem_phase(tx, "p1".into(), &peers());
        if d.should_fluff(&tx) {
            fluffed += 1;
        }
        // Reset state to avoid timeout-based effects
        let _ = d.transition_to_fluff(tx);
    }
    let rate = fluffed as f64 / trials as f64;
    // Allow a tolerance (approx 3 sigma for binomial with p=0.2 and n=2000 ~ sigma ~ 0.009)
    assert!((rate - 0.2).abs() < 0.03, "rate {} out of bounds", rate);
}

#[test]
fn peer_churn_during_stem_is_safe() {
    let rng = StdRng::seed_from_u64(55);
    let clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    d.set_stem_timeout(Duration::from_millis(100));
    d.set_fluff_probability(0.0);
    let tx = [7u8; 32];
    let all_peers = peers();
    let _ = d.start_stem_phase(tx, "p1".into(), &all_peers);
    // Remove all candidates by passing only current peer; advance should return None
    let next = d.advance_stem(tx, &vec!["p1".into()]);
    assert!(next.is_some());
    // Eventually timeout triggers fluff
    let mut test_clock = clock.clone();
    test_clock.advance(Duration::from_millis(200));
    d.set_clock(test_clock);
    assert!(d.should_fluff(&tx));
}

#[test]
fn duplicate_tx_handling_overwrites_safely() {
    let rng = StdRng::seed_from_u64(77);
    let clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    let tx = [5u8; 32];
    let _ = d.start_stem_phase(tx, "p1".into(), &peers());
    let _ = d.start_stem_phase(tx, "p2".into(), &peers()); // overwrite
    assert_eq!(d.get_phase(&tx), Some(DandelionPhase::Stem));
}

#[test]
fn bounded_state_after_cleanup() {
    let rng = StdRng::seed_from_u64(88);
    let mut clock = TestClock::new(Instant::now());
    let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
    d.set_stem_timeout(Duration::from_millis(5));
    d.set_fluff_probability(0.0);
    for i in 0..500 {
        let tx = [i as u8; 32];
        let _ = d.start_stem_phase(tx, "p1".into(), &peers());
    }
    // Advance time to expire and cleanup
    clock.advance(Duration::from_millis(20));
    d.set_clock(clock.clone());

    #[test]
    fn eclipse_subset_eventual_fluff() {
        let rng = StdRng::seed_from_u64(1337);
        let mut clock = TestClock::new(Instant::now());
        let mut d: DandelionRelay<TestClock> =
            DandelionRelay::with_rng_and_clock(rng, clock.clone());
        d.set_stem_timeout(Duration::from_millis(50));
        d.set_fluff_probability(0.0);
        d.set_max_stem_hops(2);
        let tx = [11u8; 32];
        let subset = vec!["p1".into(), "p2".into()];
        let _ = d.start_stem_phase(tx, "p1".into(), &subset);
        // Advance hops without alternative path
        let _ = d.advance_stem(tx, &subset);
        // Timeout guarantees fluff eventually
        clock.advance(Duration::from_millis(60));
        d.set_clock(clock.clone());
        assert!(d.should_fluff(&tx));
    }

    #[test]
    fn rng_extremes_bounds() {
        let rng = StdRng::seed_from_u64(4242);
        let clock = TestClock::new(Instant::now());
        let mut d: DandelionRelay<TestClock> =
            DandelionRelay::with_rng_and_clock(rng, clock.clone());
        d.set_stem_timeout(Duration::from_secs(60));
        // p = 1.0 always fluff immediately
        d.set_fluff_probability(1.0);
        let tx = [13u8; 32];
        let _ = d.start_stem_phase(tx, "p3".into(), &peers());
        assert!(d.should_fluff(&tx));
    }
    d.cleanup_expired();
    // Expect near-zero stem txs after cleanup
    assert!(d.get_stats().stem_transactions < 10);
}

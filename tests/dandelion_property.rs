#![cfg(feature = "dandelion")]
use std::time::{Duration, Instant};
use rand::rngs::StdRng;
use rand::SeedableRng;
use proptest::prelude::*;

use reference_node::network::dandelion::{DandelionRelay, Clock};

#[derive(Clone)]
struct TestClock { now: Instant }
impl TestClock { fn new(start: Instant) -> Self { Self { now: start } } fn advance(&mut self, d: Duration) { self.now += d; } }
impl Clock for TestClock { fn now(&self) -> Instant { self.now } }

proptest! {
    #[test]
    fn no_premature_broadcast_when_p_zero_and_not_timed_out(num_trials in 10usize..200usize) {
        let rng = StdRng::seed_from_u64(999);
        let clock = TestClock::new(Instant::now());
        let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
        d.set_fluff_probability(0.0);
        d.set_stem_timeout(Duration::from_secs(5));
        let peers = vec!["a".into(), "b".into(), "c".into()];
        for i in 0..num_trials {
            let tx = [i as u8; 32];
            let _ = d.start_stem_phase(tx, "a".into(), &peers);
            prop_assert!(!d.should_fluff(&tx));
        }
    }

    #[test]
    fn timeout_eventually_fluffs_within_bound(timeout_ms in 5u64..50u64) {
        let rng = StdRng::seed_from_u64(1001);
        let mut clock = TestClock::new(Instant::now());
        let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
        d.set_fluff_probability(0.0);
        d.set_stem_timeout(Duration::from_millis(timeout_ms));
        let tx = [42u8; 32];
        let _ = d.start_stem_phase(tx, "a".into(), &vec!["a".into(), "b".into()]);
        clock.advance(Duration::from_millis(timeout_ms + 1));
        d.set_clock(clock.clone());
        prop_assert!(d.should_fluff(&tx));
    }
}

proptest! {
    #[test]
    fn hop_bound_respected(max_hops in 0u8..5u8) {
        let rng = StdRng::seed_from_u64(2025);
        let clock = TestClock::new(Instant::now());
        let mut d: DandelionRelay<TestClock> = DandelionRelay::with_rng_and_clock(rng, clock.clone());
        d.set_fluff_probability(0.0);
        d.set_stem_timeout(Duration::from_secs(10));
        d.set_max_stem_hops(max_hops);
        let tx = [77u8; 32];
        let peers = vec!["a".into(), "b".into(), "c".into()];
        let _ = d.start_stem_phase(tx, "a".into(), &peers);
        for _ in 0..max_hops {
            let _ = d.advance_stem(tx, &peers);
        }
        // After max_hops advances, should_fluff() must be true
        prop_assert!(d.should_fluff(&tx));
    }
}



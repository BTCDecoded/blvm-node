use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::time::{Duration, Instant};

use reference_node::network::dandelion::{Clock, DandelionRelay};

#[derive(Clone)]
struct BenchClock {
    now: Instant,
}
impl BenchClock {
    fn new() -> Self {
        Self {
            now: Instant::now(),
        }
    }
    fn advance(&mut self, d: Duration) {
        self.now += d;
    }
}
impl Clock for BenchClock {
    fn now(&self) -> Instant {
        self.now
    }
}

fn bench_dandelion(c: &mut Criterion) {
    let mut group = c.benchmark_group("dandelion");

    group.bench_function("start_and_advance_10k", |b| {
        b.iter_batched(
            || {
                let rng = StdRng::seed_from_u64(1234);
                let clock = BenchClock::new();
                let mut d: DandelionRelay<BenchClock> =
                    DandelionRelay::with_rng_and_clock(rng, clock);
                d.set_fluff_probability(0.0);
                d.set_stem_timeout(Duration::from_secs(60));
                let peers = vec!["p1".into(), "p2".into(), "p3".into(), "p4".into()];
                (d, peers)
            },
            |(mut d, peers)| {
                for i in 0..10_000u32 {
                    let mut tx = [0u8; 32];
                    tx[0..4].copy_from_slice(&i.to_le_bytes());
                    let _ = d.start_stem_phase(tx, "p1".into(), &peers);
                    let _ = d.advance_stem(tx, &peers);
                    black_box(&d);
                }
            },
            BatchSize::LargeInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_dandelion);
criterion_main!(benches);

use super::*;
use crate::module::module_name_from_id;
use crate::module::timers::clock::{ChainEvent, FakeWallClock, WallClock};
use crate::module::timers::types::*;
use rand::Rng;

fn test_service(config: TimerConfig) -> (TimerService, mpsc::UnboundedReceiver<PendingFire>) {
    let (transport, rx) = ChannelTimerTransport::pair();
    let service = TimerService::spawn_with(config, HashMap::new(), transport);
    (service, rx)
}

fn small_config() -> TimerConfig {
    TimerConfig {
        max_timers_per_module: 8,
        max_timers_total: 32,
        max_durable_per_module: 2,
        max_consecutive_failures: 3,
        min_period_ms: 1,
        ..TimerConfig::default()
    }
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

async fn next_fire(rx: &mut mpsc::UnboundedReceiver<PendingFire>) -> PendingFire {
    for _ in 0..40 {
        if let Ok(fire) = rx.try_recv() {
            return fire;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for a timer fire");
}

async fn wait_fire(rx: &mut mpsc::UnboundedReceiver<PendingFire>, step: Duration) -> PendingFire {
    for _ in 0..10_000 {
        if let Ok(fire) = rx.try_recv() {
            return fire;
        }
        tokio::time::advance(step).await;
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for a timer fire");
}

fn hex_id(name: &str) -> String {
    format!("{name}_{}", "ab".repeat(16))
}

#[tokio::test(start_paused = true)]
async fn fixed_rate_does_not_drift() {
    let mut config = small_config();
    config.rng_seed = Some(9);
    config.max_fires_per_sec_per_module = 100_000;
    let (service, mut rx) = test_service(config);
    service.attach_module("fees", "fees").await;
    let mut pick = StdRng::seed_from_u64(9);
    let period_ms = [20u64, 30, 40, 50][pick.gen_range(0..4usize)];
    let period = Duration::from_millis(period_ms);
    let jitter_ms = period_ms / 5;
    let mut spec = monotonic_interval(period, Pacing::FixedRate);
    spec.randomness = Randomness::Jitter {
        max: Duration::from_millis(jitter_ms),
        shape: JitterShape::Symmetric,
    };
    if let Repeat::Interval {
        ref mut max_fires, ..
    } = spec.repeat
    {
        *max_fires = Some(10_000);
    }
    let created = service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let anchor = created.created_unix_ms;
    let step = Duration::from_millis((jitter_ms / 2).max(1));
    let mut late = 0u64;
    for k in 0..10_000u64 {
        let pending = wait_fire(&mut rx, step).await;
        let ideal = anchor + k * period_ms;
        let planned = pending.fire.planned_unix_ms;
        assert!(
            planned.abs_diff(ideal) <= jitter_ms,
            "fire {k} planned {planned} ideal {ideal} jitter {jitter_ms}"
        );
        assert_eq!(pending.fire.seq, k + 1, "fire {k}");
        if pending.fire.fired_unix_ms > planned {
            late += 1;
        }
        pending.reply.send(Ok(TimerAck::Continue)).unwrap();
        settle().await;
    }
    assert!(late > 0, "late dispatch never happened");
    settle().await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn fixed_delay_waits_for_acknowledgement() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    let period = Duration::from_millis(50);
    let spec = monotonic_interval(period, Pacing::FixedDelay);
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let mut previous_ack = None;
    for _ in 0..5 {
        let pending = next_fire(&mut rx).await;
        if let Some(ack_at) = previous_ack {
            assert!(pending.fire.scheduled_unix_ms >= ack_at + period.as_millis() as u64);
        }
        let ack_at = pending.fire.fired_unix_ms;
        pending.reply.send(Ok(TimerAck::Continue)).unwrap();
        settle().await;
        previous_ack = Some(ack_at);
        tokio::time::advance(period).await;
    }
}

#[tokio::test(start_paused = true)]
async fn max_fires_and_until_stop_dispatch() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    let mut spec = monotonic_interval(Duration::from_millis(10), Pacing::FixedRate);
    if let Repeat::Interval {
        ref mut max_fires, ..
    } = spec.repeat
    {
        *max_fires = Some(3);
    }
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let mut last = false;
    for i in 0..3 {
        let pending = next_fire(&mut rx).await;
        last = pending.fire.is_last;
        assert_eq!(last, i == 2);
        pending.reply.send(Ok(TimerAck::Continue)).unwrap();
        settle().await;
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    assert!(last);
    settle().await;
    assert!(rx.try_recv().is_err());

    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
    if let Repeat::Interval { ref mut until, .. } = spec.repeat {
        *until = Some(End::AfterDuration(Duration::from_millis(250)));
    }
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let mut seen = 0;
    for _ in 0..4 {
        tokio::task::yield_now().await;
        if let Ok(pending) = rx.try_recv() {
            seen += 1;
            pending.reply.send(Ok(TimerAck::Continue)).unwrap();
            settle().await;
        }
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
    }
    assert!(seen <= 3);
    assert!(seen >= 1);
}

#[tokio::test(start_paused = true)]
async fn ownership_hides_foreign_timers() {
    let (service, _rx) = test_service(small_config());
    let spec = monotonic_interval(Duration::from_secs(10), Pacing::FixedRate);
    let mut spec = spec;
    spec.key = Some("poll".into());
    let info = service
        .create("alpha", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let missing = service
        .get("beta", TimerTarget::Id(TimerId([9; 16])))
        .await
        .unwrap_err();
    for target in [
        TimerTarget::Id(info.timer_id),
        TimerTarget::Key("poll".into()),
    ] {
        assert_eq!(
            service.get("beta", target.clone()).await.unwrap_err(),
            missing
        );
        assert_eq!(
            service.cancel("beta", target.clone()).await.unwrap_err(),
            missing
        );
        assert_eq!(
            service.pause("beta", target.clone()).await.unwrap_err(),
            missing
        );
        assert_eq!(service.trigger("beta", target).await.unwrap_err(), missing);
    }
    assert!(service.list("beta").await.unwrap().is_empty());
    assert_eq!(service.list("alpha").await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn quota_is_per_module() {
    let config = TimerConfig {
        max_timers_per_module: 2,
        max_timers_total: 10,
        ..small_config()
    };
    let (service, _rx) = test_service(config);
    let spec = monotonic_interval(Duration::from_secs(5), Pacing::FixedRate);
    service
        .create("alpha", spec.clone(), CreateMode::CreateOnly)
        .await
        .unwrap();
    service
        .create("alpha", spec.clone(), CreateMode::CreateOnly)
        .await
        .unwrap();
    let err = service
        .create("alpha", spec.clone(), CreateMode::CreateOnly)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        TimerError::QuotaExceeded("max_timers_per_module".into())
    );
    service
        .create("beta", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn ensure_is_idempotent_and_create_only_conflicts() {
    let (service, _rx) = test_service(small_config());
    let mut spec = monotonic_interval(Duration::from_secs(5), Pacing::FixedRate);
    spec.key = Some("poll".into());
    let first = service
        .create("alpha", spec.clone(), CreateMode::Ensure)
        .await
        .unwrap();
    let again = service
        .create("alpha", spec.clone(), CreateMode::Ensure)
        .await
        .unwrap();
    assert_eq!(first.timer_id, again.timer_id);
    let err = service
        .create("alpha", spec.clone(), CreateMode::CreateOnly)
        .await
        .unwrap_err();
    assert_eq!(err, TimerError::KeyExists);
    let mut changed = spec.clone();
    changed.payload = b"next".to_vec();
    let replaced = service
        .create("alpha", changed, CreateMode::Ensure)
        .await
        .unwrap();
    assert_ne!(replaced.timer_id, first.timer_id);
    assert_eq!(
        service
            .get("alpha", TimerTarget::Id(first.timer_id))
            .await
            .unwrap_err(),
        TimerError::NotFound
    );
}

#[tokio::test(start_paused = true)]
async fn heap_stays_bounded_across_a_million_cancels() {
    let (service, _rx) = test_service(TimerConfig::default());
    let mut spec = monotonic_interval(Duration::from_secs(30), Pacing::FixedRate);
    spec.first = First::Delay(Duration::from_secs(3600));
    for _ in 0..1_000 {
        let info = service
            .create("alpha", spec.clone(), CreateMode::CreateOnly)
            .await
            .unwrap();
        service
            .cancel("alpha", TimerTarget::Id(info.timer_id))
            .await
            .unwrap();
    }
    let info = service
        .create("alpha", spec.clone(), CreateMode::CreateOnly)
        .await
        .unwrap();
    service
        .pause("alpha", TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    assert_eq!(service.heap_len().await, 0);
    service
        .resume("alpha", TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    service
        .cancel("alpha", TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    assert!(service.heap_len().await < 64);

    for _ in 0..20_000 {
        let info = service
            .create("alpha", spec.clone(), CreateMode::CreateOnly)
            .await
            .unwrap();
        service
            .cancel("alpha", TimerTarget::Id(info.timer_id))
            .await
            .unwrap();
    }
    assert!(service.heap_len().await < 64);
}

#[tokio::test(start_paused = true)]
async fn detach_cancels_ephemeral_and_keeps_durable() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees-1").await;
    settle().await;
    let ephemeral = monotonic_interval(Duration::from_secs(10), Pacing::FixedRate);
    let mut durable = ephemeral.clone();
    durable.key = Some("compact".into());
    durable.persist = Persist::Durable { audit: false };
    durable.missed = Missed::FireOnce;
    if let Repeat::Interval { ref mut period, .. } = durable.repeat {
        *period = Duration::from_secs(2);
    }
    let gone = service
        .create("fees-1", ephemeral, CreateMode::CreateOnly)
        .await
        .unwrap();
    let kept = service
        .create("fees-1", durable, CreateMode::CreateOnly)
        .await
        .unwrap();
    // The first fires are in flight. Detach must still drop the ephemeral timer.
    while let Ok(pending) = rx.try_recv() {
        let _ = pending.reply.send(Ok(TimerAck::Continue));
    }
    service.detach_module("fees", None).await;
    settle().await;
    assert_eq!(
        service
            .get("fees", TimerTarget::Id(gone.timer_id))
            .await
            .unwrap_err(),
        TimerError::NotFound
    );
    let info = service
        .get("fees", TimerTarget::Id(kept.timer_id))
        .await
        .unwrap();
    assert_eq!(info.state, TimerState::Detached);
    service.attach_module("fees", "fees-2").await;
    settle().await;
    let info = service
        .get("fees-2", TimerTarget::Id(kept.timer_id))
        .await
        .unwrap();
    assert_eq!(info.state, TimerState::Active);
}

#[tokio::test(start_paused = true)]
async fn stale_detach_does_not_cancel_the_new_connection() {
    let (service, _rx) = test_service(small_config());
    let old = hex_id("my_module");
    let new = hex_id("my_module").replace('a', "c");
    assert_eq!(module_name_from_id(&old), "my_module");
    service.attach_module("my_module", &old).await;
    settle().await;
    let spec = monotonic_interval(Duration::from_secs(10), Pacing::FixedRate);
    let info = service
        .create(&old, spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    service.attach_module("my_module", &new).await;
    service.detach_module("my_module", Some(&old)).await;
    settle().await;
    service
        .get(&new, TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    service.detach_module("my_module", Some(&new)).await;
    settle().await;
    assert_eq!(
        service
            .get(&new, TimerTarget::Id(info.timer_id))
            .await
            .unwrap_err(),
        TimerError::NotFound
    );
}

#[tokio::test(start_paused = true)]
async fn stop_ends_the_timer_and_a_silent_module_trips_the_breaker() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    service.attach_module("other", "other").await;
    settle().await;
    let mut spec = monotonic_interval(Duration::from_millis(20), Pacing::FixedRate);
    if let Repeat::Interval {
        ref mut max_fires, ..
    } = spec.repeat
    {
        *max_fires = Some(5);
    }
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    for i in 0..5 {
        let pending = next_fire(&mut rx).await;
        let ack = if i == 4 {
            TimerAck::Stop
        } else {
            TimerAck::Continue
        };
        pending.reply.send(Ok(ack)).unwrap();
        settle().await;
        tokio::time::advance(Duration::from_millis(20)).await;
    }
    settle().await;
    assert!(service.list("fees").await.unwrap().is_empty());

    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedDelay);
    spec.delivery.ack_timeout = Duration::from_secs(1);
    spec.delivery.max_consecutive_failures = 3;
    let silent = service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let mut other = monotonic_interval(Duration::from_secs(3600), Pacing::FixedRate);
    other.first = First::Delay(Duration::from_secs(3600));
    let other_info = service
        .create("other", other, CreateMode::CreateOnly)
        .await
        .unwrap();
    for i in 0..3 {
        let pending = next_fire(&mut rx).await;
        assert_eq!(pending.module_id, "fees");
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        if i + 1 < 3 {
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
        }
    }
    settle().await;
    let info = service
        .get("fees", TimerTarget::Id(silent.timer_id))
        .await
        .unwrap();
    assert_eq!(info.state, TimerState::PausedByBreaker);
    assert_eq!(info.in_flight, 0);
    let other_state = service
        .get("other", TimerTarget::Id(other_info.timer_id))
        .await
        .unwrap();
    assert_eq!(other_state.state, TimerState::Active);
    assert_eq!(other_state.fires_failed, 0);
}

#[test]
fn module_name_keeps_underscores() {
    let id = hex_id("my_module");
    assert_eq!(module_name_from_id(&id), "my_module");
    assert_eq!(module_name_from_id("my_module"), "my_module");
    assert_eq!(module_name_from_id(&hex_id("my")), "my");
}

#[test]
fn jitter_never_reorders_a_fixed_rate_grid() {
    let mut rng = StdRng::seed_from_u64(7);
    for _ in 0..200 {
        let period_ms = 20 + rng.gen_range(0..5_000);
        let max = period_ms / 2;
        if max <= 1 {
            continue;
        }
        let max = max - 1;
        let mut previous = i64::MIN;
        let mut ideal = 0i64;
        for _k in 0..64 {
            let unit: f64 = rng.r#gen();
            let jitter = ((unit * 2.0 - 1.0) * max as f64) as i64;
            let planned = ideal + jitter;
            assert!(planned > previous, "period {period_ms} jitter {jitter}");
            previous = planned;
            ideal += period_ms;
        }
    }
}

#[test]
fn exponential_gaps_stay_inside_the_bounds() {
    let mean = Duration::from_secs(1);
    let floor = Duration::from_millis(1);
    let cap = Duration::from_secs(60);
    let mut rng = StdRng::seed_from_u64(11);
    let mut total = 0.0f64;
    for _ in 0..100_000 {
        let unit: f64 = rng.r#gen::<f64>().clamp(f64::MIN_POSITIVE, 1.0);
        let gap = exponential_gap(unit, mean, floor, cap);
        assert!(gap >= floor && gap <= cap);
        total += gap.as_secs_f64();
    }
    let average = total / 100_000.0;
    assert!(
        (average - mean.as_secs_f64()).abs() / mean.as_secs_f64() < 0.02,
        "{average}"
    );
}

#[tokio::test(start_paused = true)]
async fn missed_policies_follow_the_suspend_matrix() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    settle().await;
    let period = Duration::from_millis(100);
    let cases = [
        (Missed::Skip, 0u32, 1usize, 0u32),
        (Missed::FireOnce, 0, 1, 0),
        (Missed::CatchUp { max: 2 }, 0, 1, 0),
        (Missed::Skip, 1, 0, 0),
        (Missed::FireOnce, 1, 1, 1),
        (Missed::CatchUp { max: 2 }, 2, 2, 1),
        (Missed::Skip, 100, 0, 0),
        (Missed::FireOnce, 100, 1, 100),
        (Missed::CatchUp { max: 2 }, 100, 2, 99),
    ];
    for (policy, extra_periods, fires, missed_on_last) in cases {
        let mut spec = monotonic_interval(period, Pacing::FixedRate);
        spec.first = First::Delay(period);
        spec.missed = policy;
        spec.overlap = Overlap::Allow { max_concurrent: 8 };
        spec.key = Some(format!("m{extra_periods}"));
        service
            .create("fees", spec, CreateMode::Replace)
            .await
            .unwrap();
        service
            .pause("fees", TimerTarget::Key(format!("m{extra_periods}")))
            .await
            .unwrap();
        tokio::time::advance(period * (1 + extra_periods)).await;
        settle().await;
        service
            .resume("fees", TimerTarget::Key(format!("m{extra_periods}")))
            .await
            .unwrap();
        settle().await;
        let mut got = Vec::new();
        while let Ok(pending) = rx.try_recv() {
            got.push(pending.fire.missed);
            let _ = pending.reply.send(Ok(TimerAck::Stop));
        }
        assert_eq!(got.len(), fires, "{policy:?} extra {extra_periods} {got:?}");
        if fires > 0 {
            assert_eq!(*got.last().unwrap(), missed_on_last, "{policy:?} {got:?}");
        }
        settle().await;
    }
}

#[tokio::test(start_paused = true)]
async fn overlap_skip_drops_a_second_fire_until_the_first_is_acked() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    settle().await;
    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
    spec.overlap = Overlap::Skip;
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let first = next_fire(&mut rx).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    assert!(rx.try_recv().is_err());
    first.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    let second = next_fire(&mut rx).await;
    assert!(
        second.fire.skipped_overlap >= 1,
        "{}",
        second.fire.skipped_overlap
    );
    let _ = second.reply.send(Ok(TimerAck::Stop));
}

#[tokio::test(start_paused = true)]
async fn overlap_queue_coalesce_and_allow() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    settle().await;

    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
    spec.overlap = Overlap::Queue { max: 1 };
    spec.key = Some("queue".into());
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let first = next_fire(&mut rx).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    assert!(rx.try_recv().is_err());
    first.reply.send(Ok(TimerAck::Continue)).unwrap();
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    let queued = next_fire(&mut rx).await;
    assert!(
        queued.fire.skipped_overlap >= 1,
        "{}",
        queued.fire.skipped_overlap
    );
    queued.reply.send(Ok(TimerAck::Stop)).unwrap();
    settle().await;

    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
    spec.overlap = Overlap::Coalesce;
    spec.key = Some("coalesce".into());
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let first = next_fire(&mut rx).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    assert!(rx.try_recv().is_err());
    first.reply.send(Ok(TimerAck::Continue)).unwrap();
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    let coalesced = next_fire(&mut rx).await;
    assert!(
        coalesced.fire.coalesced >= 1,
        "{}",
        coalesced.fire.coalesced
    );
    coalesced.reply.send(Ok(TimerAck::Stop)).unwrap();
    settle().await;

    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
    spec.overlap = Overlap::Allow { max_concurrent: 2 };
    spec.key = Some("allow".into());
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let first = next_fire(&mut rx).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    let second = next_fire(&mut rx).await;
    tokio::time::advance(Duration::from_millis(100)).await;
    settle().await;
    assert!(rx.try_recv().is_err());
    first.reply.send(Ok(TimerAck::Continue)).unwrap();
    second.reply.send(Ok(TimerAck::Stop)).unwrap();
    settle().await;
}

/// The older of two open fires has to free its slot. The newer acknowledgement
/// is the control that the following period is actually due.
#[tokio::test(start_paused = true)]
async fn allow_overlap_frees_a_slot_when_either_open_fire_acks() {
    async fn fires_again(ack_older: bool) -> (u64, u64, bool) {
        let mut config = small_config();
        config.max_in_flight_per_module = 64;
        config.max_fires_per_sec_per_module = 10_000;
        let (service, mut rx) = test_service(config);
        service.attach_module("fees", "fees").await;
        settle().await;
        let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
        spec.overlap = Overlap::Allow { max_concurrent: 2 };
        spec.key = Some(format!("allow-{ack_older}"));
        service
            .create("fees", spec, CreateMode::CreateOnly)
            .await
            .unwrap();
        let first = next_fire(&mut rx).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        let second = next_fire(&mut rx).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        assert!(rx.try_recv().is_err());
        let older = first.fire.seq;
        let newer = second.fire.seq;
        if ack_older {
            first.reply.send(Ok(TimerAck::Continue)).unwrap();
        } else {
            second.reply.send(Ok(TimerAck::Continue)).unwrap();
        }
        let mut arrived = false;
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
            if rx.try_recv().is_ok() {
                arrived = true;
                break;
            }
        }
        (older, newer, arrived)
    }

    let older_ack = fires_again(true).await;
    let newer_ack = fires_again(false).await;
    assert!(
        older_ack.0 < older_ack.1 && newer_ack.0 < newer_ack.1,
        "sequences older={older_ack:?} newer={newer_ack:?}"
    );
    assert!(older_ack.2, "older ack did not free a slot: {older_ack:?}");
    assert!(newer_ack.2, "newer ack did not free a slot: {newer_ack:?}");
}

#[tokio::test(start_paused = true)]
async fn fixed_delay_and_exponential_ignore_missed_policy() {
    let mut config = small_config();
    config.rng_seed = Some(3);
    config.max_fires_per_sec_per_module = 10_000;
    config.max_in_flight_per_module = 64;
    let (service, mut rx) = test_service(config);
    service.attach_module("fees", "fees").await;
    settle().await;

    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedDelay);
    spec.missed = Missed::CatchUp { max: 10 };
    spec.key = Some("delay".into());
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    let first = next_fire(&mut rx).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert!(rx.try_recv().is_err());
    assert_eq!(first.fire.missed, 0);
    first.reply.send(Ok(TimerAck::Stop)).unwrap();
    settle().await;

    let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
    spec.missed = Missed::Skip;
    spec.overlap = Overlap::Allow { max_concurrent: 16 };
    spec.randomness = Randomness::Exponential {
        mean: Duration::from_millis(100),
        floor: Duration::from_millis(10),
        cap: Duration::from_secs(1),
    };
    spec.key = Some("poisson".into());
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    let mut fires = Vec::new();
    while let Ok(pending) = rx.try_recv() {
        fires.push(pending.fire.missed);
        let _ = pending.reply.send(Ok(TimerAck::Stop));
    }
    assert!(fires.len() > 1, "fires {}", fires.len());
    assert!(fires.iter().all(|missed| *missed == 0), "{fires:?}");
}

#[tokio::test(start_paused = true)]
async fn retry_then_breaker_then_resume() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;
    settle().await;
    let mut spec = monotonic_interval(Duration::from_secs(30), Pacing::FixedRate);
    spec.first = First::Delay(Duration::from_millis(10));
    spec.delivery.max_consecutive_failures = 2;
    spec.delivery.on_failure = OnFailure::Retry {
        max: 1,
        backoff: Backoff {
            initial: Duration::from_millis(10),
            factor_milli: 1000,
            max: Duration::from_secs(1),
            jitter: false,
        },
    };
    let info = service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(10)).await;
    let first = next_fire(&mut rx).await;
    assert_eq!(first.fire.attempt, 1);
    first
        .reply
        .send(Err(TransportError::Failed("no".into())))
        .unwrap();
    settle().await;
    tokio::time::advance(Duration::from_millis(10)).await;
    let retry = next_fire(&mut rx).await;
    assert!(retry.fire.attempt >= 2, "{}", retry.fire.attempt);
    retry
        .reply
        .send(Err(TransportError::Failed("still".into())))
        .unwrap();
    settle().await;
    let paused = service
        .get("fees", TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    assert_eq!(paused.state, TimerState::PausedByBreaker);
    service
        .resume("fees", TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    settle().await;
    let again = service
        .get("fees", TimerTarget::Id(info.timer_id))
        .await
        .unwrap();
    assert_eq!(again.state, TimerState::Active);
    assert!(
        service
            .prometheus()
            .contains("blvm_timer_breaker_trips_total")
    );
    assert!(service.prometheus().contains("blvm_timer_lateness_ms"));
    assert!(service.prometheus().contains("blvm_timer_fires_total"));
}

#[tokio::test(start_paused = true)]
async fn a_full_bucket_counts_skipped_rate() {
    let mut config = small_config();
    config.max_fires_per_sec_per_module = 1;
    let (service, mut rx) = test_service(config);
    service.attach_module("fees", "fees").await;
    settle().await;
    for _ in 0..2 {
        let mut spec = monotonic_interval(Duration::from_secs(60), Pacing::FixedRate);
        spec.repeat = Repeat::Once;
        service
            .create("fees", spec, CreateMode::CreateOnly)
            .await
            .unwrap();
    }
    let _first = next_fire(&mut rx).await;
    settle().await;
    assert!(rx.try_recv().is_err());
    assert!(service.stats()["skipped_rate"].as_u64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn two_modules_keep_p99_lateness_under_25ms() {
    let mut config = small_config();
    config.max_timers_per_module = 1000;
    config.max_timers_total = 2000;
    config.max_fires_per_sec_per_module = 10_000;
    let (service, mut rx) = test_service(config);
    service.attach_module("a", "a").await;
    service.attach_module("b", "b").await;
    for module in ["a", "b"] {
        for i in 0..1000 {
            let mut spec = monotonic_interval(Duration::from_secs(60), Pacing::FixedRate);
            spec.repeat = Repeat::Once;
            spec.first = First::Delay(Duration::from_millis(300 + (i % 50) * 10));
            service
                .create(module, spec, CreateMode::CreateOnly)
                .await
                .unwrap();
        }
    }
    let mut late = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while late.len() < 2000 && tokio::time::Instant::now() < deadline {
        match rx.try_recv() {
            Ok(pending) => {
                late.push(pending.fire.lateness_ms);
                let _ = pending.reply.send(Ok(TimerAck::Continue));
            }
            Err(_) => tokio::task::yield_now().await,
        }
    }
    assert_eq!(late.len(), 2000, "fires {}", late.len());
    late.sort_unstable();
    let p99 = late[late.len() * 99 / 100];
    assert!(p99 < 25, "p99 lateness {p99} ms");
}

fn test_service_clock(
    config: TimerConfig,
    wall: Arc<dyn WallClock>,
) -> (TimerService, mpsc::UnboundedReceiver<PendingFire>) {
    let (transport, rx) = ChannelTimerTransport::pair();
    let service = TimerService::spawn_clocked(config, HashMap::new(), transport, wall, None, None);
    (service, rx)
}

fn chain_once(first: First, reorg: ReorgPolicy) -> TimerSpec {
    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.clock = Clock::Chain;
    spec.first = first;
    spec.repeat = Repeat::Once;
    spec.reorg = reorg;
    spec
}

async fn quiet(rx: &mut mpsc::UnboundedReceiver<PendingFire>) {
    settle().await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn wall_step_forward_applies_missed_and_backward_does_not_double_fire() {
    let start = 1_700_000_000_000u64;
    let clock = FakeWallClock::new(start);
    let (service, mut rx) =
        test_service_clock(small_config(), Arc::clone(&clock) as Arc<dyn WallClock>);
    service.attach_module("fees", "fees").await;
    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.clock = Clock::Wall;
    spec.first = First::Delay(Duration::from_secs(1));
    spec.missed = Missed::FireOnce;
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    clock.set(start + 1_000 + 3_600_000);
    service.note_wall().await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.clock_step_ms, 3_601_000);
    assert_eq!(pending.fire.missed, 3600);
    assert_eq!(pending.fire.clock, Clock::Wall);
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;
    assert!(rx.try_recv().is_err());
    clock.set(start);
    service.note_wall().await;
    quiet(&mut rx).await;
}

#[tokio::test(start_paused = true)]
async fn chain_height_mtp_depth_blocks_bulk_and_reorg() {
    let (service, mut rx) = test_service(small_config());
    service.attach_module("fees", "fees").await;

    service
        .create(
            "fees",
            chain_once(
                First::AtHeight {
                    height: 5,
                    depth: 0,
                },
                ReorgPolicy::Ignore,
            ),
            CreateMode::CreateOnly,
        )
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 4,
            hash: [4; 32],
            mtp: 4,
        })
        .await;
    quiet(&mut rx).await;
    service
        .chain_event(ChainEvent::Connected {
            height: 5,
            hash: [5; 32],
            mtp: 5,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_height, Some(5));
    assert_eq!(pending.fire.tip_hash, Some([5; 32]));
    assert_eq!(pending.fire.missed, 0);
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;

    service
        .create(
            "fees",
            chain_once(
                First::AtHeight {
                    height: 10,
                    depth: 6,
                },
                ReorgPolicy::Ignore,
            ),
            CreateMode::CreateOnly,
        )
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 15,
            hash: [15; 32],
            mtp: 15,
        })
        .await;
    quiet(&mut rx).await;
    service
        .chain_event(ChainEvent::Connected {
            height: 16,
            hash: [16; 32],
            mtp: 16,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_height, Some(16));
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;

    service
        .chain_event(ChainEvent::Connected {
            height: 20,
            hash: [20; 32],
            mtp: 20,
        })
        .await;
    service
        .create(
            "fees",
            chain_once(
                First::AfterBlocks {
                    blocks: 6,
                    depth: 0,
                },
                ReorgPolicy::Ignore,
            ),
            CreateMode::CreateOnly,
        )
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 25,
            hash: [25; 32],
            mtp: 25,
        })
        .await;
    quiet(&mut rx).await;
    service
        .chain_event(ChainEvent::Connected {
            height: 26,
            hash: [26; 32],
            mtp: 26,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_height, Some(26));
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;

    service
        .create(
            "fees",
            chain_once(First::AtMtp(5_000), ReorgPolicy::Ignore),
            CreateMode::CreateOnly,
        )
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 30,
            hash: [30; 32],
            mtp: 4_999,
        })
        .await;
    quiet(&mut rx).await;
    service
        .chain_event(ChainEvent::Connected {
            height: 31,
            hash: [31; 32],
            mtp: 5_000,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_mtp, Some(5_000));
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;

    let mut every = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    every.clock = Clock::Chain;
    every.first = First::Now;
    every.repeat = Repeat::Blocks {
        every: 2,
        max_fires: None,
        until: None,
    };
    let every_info = service
        .create("fees", every, CreateMode::CreateOnly)
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 32,
            hash: [32; 32],
            mtp: 32,
        })
        .await;
    quiet(&mut rx).await;
    service
        .chain_event(ChainEvent::Connected {
            height: 33,
            hash: [33; 32],
            mtp: 33,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_height, Some(33));
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;
    service
        .cancel("fees", TimerTarget::Id(every_info.timer_id))
        .await
        .unwrap();

    let mut bulk = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    bulk.clock = Clock::Chain;
    bulk.first = First::Now;
    bulk.repeat = Repeat::Blocks {
        every: 1,
        max_fires: None,
        until: None,
    };
    bulk.overlap = Overlap::Skip;
    bulk.missed = Missed::FireOnce;
    service
        .create("fees", bulk, CreateMode::CreateOnly)
        .await
        .unwrap();
    for height in 40..80 {
        service
            .chain_event(ChainEvent::Connected {
                height,
                hash: [9; 32],
                mtp: height,
            })
            .await;
    }
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_height, Some(40));
    settle().await;
    assert!(
        rx.try_recv().is_err(),
        "bulk connect queued more than one fire"
    );

    service
        .create(
            "fees",
            chain_once(
                First::AtHeight {
                    height: 90,
                    depth: 0,
                },
                ReorgPolicy::Ignore,
            ),
            CreateMode::CreateOnly,
        )
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 90,
            hash: [1; 32],
            mtp: 90,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;
    service
        .chain_event(ChainEvent::Disconnected {
            height: 90,
            hash: [1; 32],
        })
        .await;
    service
        .chain_event(ChainEvent::Connected {
            height: 90,
            hash: [2; 32],
            mtp: 90,
        })
        .await;
    quiet(&mut rx).await;

    service
        .create(
            "fees",
            chain_once(
                First::AtHeight {
                    height: 91,
                    depth: 0,
                },
                ReorgPolicy::Refire,
            ),
            CreateMode::CreateOnly,
        )
        .await
        .unwrap();
    service
        .chain_event(ChainEvent::Connected {
            height: 91,
            hash: [3; 32],
            mtp: 91,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_hash, Some([3; 32]));
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;
    service
        .chain_event(ChainEvent::Disconnected {
            height: 91,
            hash: [3; 32],
        })
        .await;
    service
        .chain_event(ChainEvent::Connected {
            height: 91,
            hash: [4; 32],
            mtp: 91,
        })
        .await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.tip_hash, Some([4; 32]));
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
}

struct MemTree {
    map: Arc<std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>>,
}

impl MemTree {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            map: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        })
    }
}

struct MemBatch {
    map: Arc<std::sync::Mutex<std::collections::BTreeMap<Vec<u8>, Vec<u8>>>>,
    ops: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl crate::storage::database::BatchWriter for MemBatch {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.ops.push((key.to_vec(), Some(value.to_vec())));
    }

    fn delete(&mut self, key: &[u8]) {
        self.ops.push((key.to_vec(), None));
    }

    fn commit(self: Box<Self>) -> anyhow::Result<()> {
        let mut map = self.map.lock().unwrap_or_else(|err| err.into_inner());
        for (key, value) in self.ops {
            match value {
                Some(value) => {
                    map.insert(key, value);
                }
                None => {
                    map.remove(&key);
                }
            }
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.ops.len()
    }
}

impl Tree for MemTree {
    fn insert(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        self.map
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn get(&self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self
            .map
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(key)
            .cloned())
    }

    fn remove(&self, key: &[u8]) -> anyhow::Result<()> {
        self.map
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(key);
        Ok(())
    }

    fn contains_key(&self, key: &[u8]) -> anyhow::Result<bool> {
        Ok(self
            .map
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .contains_key(key))
    }

    fn clear(&self) -> anyhow::Result<()> {
        self.map
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
        Ok(())
    }

    fn len(&self) -> anyhow::Result<usize> {
        Ok(self.map.lock().unwrap_or_else(|err| err.into_inner()).len())
    }

    fn iter(&self) -> Box<dyn Iterator<Item = anyhow::Result<(Vec<u8>, Vec<u8>)>> + '_> {
        let pairs: Vec<_> = self
            .map
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .map(|(key, value)| Ok((key.clone(), value.clone())))
            .collect();
        Box::new(pairs.into_iter())
    }

    fn batch(&self) -> anyhow::Result<Box<dyn crate::storage::database::BatchWriter + '_>> {
        Ok(Box::new(MemBatch {
            map: Arc::clone(&self.map),
            ops: Vec::new(),
        }))
    }
}

fn stored_service(
    config: TimerConfig,
    tree: Arc<MemTree>,
) -> (TimerService, mpsc::UnboundedReceiver<PendingFire>) {
    let (transport, rx) = ChannelTimerTransport::pair();
    let service = TimerService::spawn_clocked(
        config,
        HashMap::new(),
        transport,
        Arc::new(super::clock::SystemWallClock),
        Some(tree),
        None,
    );
    (service, rx)
}

#[tokio::test(start_paused = true)]
async fn durable_record_survives_restart_with_seq_and_next_due() {
    let tree = MemTree::new();
    let (service, mut rx) = stored_service(small_config(), Arc::clone(&tree));
    service.attach_module("fees", "fees").await;
    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.first = First::Delay(Duration::from_secs(1));
    spec.key = Some("compact".into());
    spec.persist = Persist::Durable { audit: false };
    spec.missed = Missed::FireOnce;
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.seq, 1);
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();
    settle().await;
    let before = service
        .get("fees", TimerTarget::Key("compact".into()))
        .await
        .unwrap();
    assert_eq!(before.seq, 1);
    let next_due = before.next_due_unix_ms.expect("next due");
    drop(service);
    settle().await;
    tree.insert(b"fees/corrupt", b"not a timer record").unwrap();

    let (service, _rx) = stored_service(small_config(), Arc::clone(&tree));
    let restored = service.list_all(Some("fees")).await;
    assert_eq!(restored.len(), 1, "corrupt record must be skipped");
    assert_eq!(restored[0].seq, before.seq);
    assert_eq!(restored[0].state, TimerState::Detached);
    let restored_due = restored[0].next_due_unix_ms.expect("restored due");
    assert!(
        restored_due.abs_diff(next_due) < 2_000,
        "next due {restored_due} vs {next_due}"
    );
    service.attach_module("fees", "fees").await;
    settle().await;
    let attached = service
        .get("fees", TimerTarget::Key("compact".into()))
        .await
        .unwrap();
    assert_eq!(attached.state, TimerState::Active);
    assert_eq!(attached.seq, 1);

    service.delete_module_timers("fees").await;
    assert!(service.list_all(Some("fees")).await.is_empty());
    assert!(tree.get(b"fees/compact").unwrap().is_none());
    drop(service);
    settle().await;
    let (service, _rx) = stored_service(small_config(), Arc::clone(&tree));
    assert!(service.list_all(None).await.is_empty());
    assert!(tree.get(b"fees/corrupt").unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn detached_durable_timer_expires_after_the_limit() {
    let clock = FakeWallClock::new(1_700_000_000_000);
    let mut config = small_config();
    config.durable_max_detached_secs = 1;
    let (transport, _rx) = ChannelTimerTransport::pair();
    let service = TimerService::spawn_clocked(
        config,
        HashMap::new(),
        transport,
        Arc::clone(&clock) as Arc<dyn WallClock>,
        Some(MemTree::new()),
        None,
    );
    service.attach_module("fees", "fees").await;
    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.key = Some("compact".into());
    spec.persist = Persist::Durable { audit: false };
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    service.detach_module("fees", None).await;
    settle().await;
    clock.set(1_700_000_000_000 + 2_000);
    service.note_wall().await;
    let info = service.list_all(Some("fees")).await;
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].state, TimerState::Expired);
}

#[tokio::test(start_paused = true)]
async fn verify_journal_reports_a_modified_record() {
    let journal = MemTree::new();
    let (transport, _rx) = ChannelTimerTransport::pair();
    let service = TimerService::spawn_clocked(
        small_config(),
        HashMap::new(),
        transport,
        Arc::new(super::clock::SystemWallClock),
        None,
        Some(journal.clone()),
    );
    service.attach_module("fees", "fees").await;
    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.key = Some("audit".into());
    spec.persist = Persist::Durable { audit: true };
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    settle().await;
    assert!(service.verify_journal().is_ok());
    let records = service.journal_entries(0, u64::MAX, Some("fees")).unwrap();
    assert!(records.len() >= 1);
    assert!(matches!(
        records[0].event,
        crate::module::timers::journal::JournalEvent::Created
    ));
    let before = records.len();
    let mut quiet = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    quiet.key = Some("quiet".into());
    quiet.persist = Persist::Durable { audit: false };
    service
        .create("fees", quiet, CreateMode::CreateOnly)
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        service.journal_entries(0, u64::MAX, None).unwrap().len(),
        before
    );
    let key = 0u64.to_be_bytes();
    let raw = journal.get(&key).unwrap().unwrap();
    let mut record: crate::module::timers::journal::JournalRecord =
        bincode::deserialize(&raw).unwrap();
    record.hash[0] ^= 0xff;
    journal
        .insert(&key, &bincode::serialize(&record).unwrap())
        .unwrap();
    let broken = service.verify_journal().unwrap_err();
    assert_eq!(broken.index, 0);
    assert!(broken.reason.contains("hash"), "{}", broken.reason);
}

#[cfg(feature = "timer-cron")]
#[tokio::test(start_paused = true)]
async fn cron_fires_at_the_next_utc_slot() {
    let start = 1_704_067_200_000u64;
    let clock = FakeWallClock::new(start);
    let (transport, mut rx) = ChannelTimerTransport::pair();
    let service = TimerService::spawn_clocked(
        small_config(),
        HashMap::new(),
        transport,
        Arc::clone(&clock) as Arc<dyn WallClock>,
        None,
        None,
    );
    service.attach_module("fees", "fees").await;
    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.clock = Clock::Wall;
    spec.repeat = Repeat::Cron {
        expr: "0 0 3 * * * *".into(),
        max_fires: None,
        until: None,
    };
    spec.key = Some("daily".into());
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();
    settle().await;
    assert!(rx.try_recv().is_err());
    clock.set(start + 3 * 3_600_000);
    service.note_wall().await;
    let pending = next_fire(&mut rx).await;
    assert_eq!(pending.fire.scheduled_unix_ms, start + 3 * 3_600_000);
    assert_eq!(pending.fire.seq, 1);
}

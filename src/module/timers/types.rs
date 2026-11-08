//! Wire types for the module timer service.
//!
//! Durations are [`std::time::Duration`] in the API and `u64` milliseconds on the wire.
//! `std::time::Duration` has no serde implementation, and bincode encodes through serde.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;

pub mod dur_ms {
    use super::*;

    pub fn serialize<S: Serializer>(d: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        u64::try_from(d.as_millis())
            .unwrap_or(u64::MAX)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

pub mod opt_dur_ms {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(|d| d.as_millis() as u64).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_millis))
    }
}

/// Legacy `RegisterTimer`, `CancelTimer`, and `ScheduleTask` answer with this string.
pub const LEGACY_TIMER_UNSUPPORTED: &str = "timer_unsupported:use TimerCreate";

/// IPC layout this node speaks on `HandshakeV2`.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// 16 random bytes. Displayed as 32 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimerId(pub [u8; 16]);

impl std::fmt::Debug for TimerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TimerId({self})")
    }
}

impl std::fmt::Display for TimerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl TimerId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerSpec {
    /// Stable name inside the owning module. Required for Durable. Enables idempotent create.
    pub key: Option<String>,
    pub clock: Clock,
    pub first: First,
    pub repeat: Repeat,
    pub randomness: Randomness,
    pub missed: Missed,
    pub overlap: Overlap,
    pub delivery: Delivery,
    pub persist: Persist,
    /// Treat any fire later than this as dropped. None means never drop for lateness.
    #[serde(with = "opt_dur_ms")]
    pub max_lateness: Option<Duration>,
    /// Chain clock only. What to do when the block that triggered a fire is reorged out.
    pub reorg: ReorgPolicy,
    /// Opaque, echoed in every fire.
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Clock {
    Monotonic,
    Wall,
    Chain,
}

/// Ignore (default): a fire already sent for height h is never repeated for h.
/// Refire: a disconnect below the last fired height resets it, so the replacement chain fires again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReorgPolicy {
    Ignore,
    Refire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum First {
    /// Due immediately.
    Now,
    /// Monotonic or Wall: after this delay on that clock.
    Delay(#[serde(with = "dur_ms")] Duration),
    /// Wall only: absolute UTC unix milliseconds.
    AtUnixMs(u64),
    /// Chain only: when tip height >= height + depth.
    AtHeight { height: u64, depth: u32 },
    /// Chain only: when tip height >= current tip + blocks + depth.
    AfterBlocks { blocks: u32, depth: u32 },
    /// Chain only: when the tip's median time past >= this (BIP 113 seconds).
    AtMtp(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    Once,
    /// Monotonic or Wall.
    Interval {
        #[serde(with = "dur_ms")]
        period: Duration,
        pacing: Pacing,
        max_fires: Option<u32>,
        until: Option<End>,
    },
    /// Chain only: every `every` connected blocks, starting at the first due height.
    Blocks {
        every: u32,
        max_fires: Option<u32>,
        until: Option<End>,
    },
    /// Wall only. Phase T5, feature `timer-cron`. UTC only.
    Cron {
        expr: String,
        max_fires: Option<u32>,
        until: Option<End>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pacing {
    FixedRate,
    FixedDelay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum End {
    AtUnixMs(u64),
    AtHeight(u64),
    AfterDuration(#[serde(with = "dur_ms")] Duration),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Randomness {
    None,
    /// Added to each fire time. The grid itself does not move.
    Jitter {
        #[serde(with = "dur_ms")]
        max: Duration,
        shape: JitterShape,
    },
    /// One random offset in [0, max) added to the first fire only.
    Spread {
        #[serde(with = "dur_ms")]
        max: Duration,
    },
    /// Gaps between fires are exponentially distributed. Replaces the fixed period.
    Exponential {
        #[serde(with = "dur_ms")]
        mean: Duration,
        #[serde(with = "dur_ms")]
        floor: Duration,
        #[serde(with = "dur_ms")]
        cap: Duration,
    },
    /// Jitter on every fire plus Spread on the first.
    JitterAndSpread {
        #[serde(with = "dur_ms")]
        jitter_max: Duration,
        shape: JitterShape,
        #[serde(with = "dur_ms")]
        spread_max: Duration,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JitterShape {
    Positive,
    Symmetric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Missed {
    /// Drop missed occurrences. Resume on the next grid slot.
    Skip,
    /// Fire once now, report the count in `missed`.
    FireOnce,
    /// Fire up to `max` of the missed occurrences back to back, then resume.
    CatchUp { max: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Overlap {
    /// Drop a fire while a previous one is unacknowledged.
    Skip,
    /// Hold up to `max` fires, deliver as acknowledgements arrive.
    Queue { max: u32 },
    /// Hold at most one pending fire.
    Coalesce,
    /// Allow this many unacknowledged fires at once.
    Allow { max_concurrent: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub ack: AckMode,
    #[serde(with = "dur_ms")]
    pub ack_timeout: Duration,
    pub on_failure: OnFailure,
    /// Consecutive failures or timeouts before the timer is paused. Default 5. 0 disables.
    pub max_consecutive_failures: u32,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            ack: AckMode::Completion,
            ack_timeout: Duration::from_secs(30),
            on_failure: OnFailure::Ignore,
            max_consecutive_failures: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckMode {
    Completion,
    Receipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnFailure {
    Ignore,
    Retry { max: u32, backoff: Backoff },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backoff {
    #[serde(with = "dur_ms")]
    pub initial: Duration,
    pub factor_milli: u32,
    #[serde(with = "dur_ms")]
    pub max: Duration,
    pub jitter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Persist {
    Never,
    /// Stored node side by (module name, key). Requires `key`.
    Durable {
        audit: bool,
    },
}

/// What a module returns for one fire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerAck {
    Continue,
    /// Cancel this timer. Counts as a normal completion.
    Stop,
    /// Delay the next fire to now + this duration. The grid resumes from that point.
    Snooze(#[serde(with = "dur_ms")] Duration),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerFire {
    pub timer_id: TimerId,
    pub key: Option<String>,
    pub payload: Vec<u8>,
    pub seq: u64,
    pub attempt: u32,
    pub clock: Clock,
    pub scheduled_unix_ms: u64,
    pub planned_unix_ms: u64,
    pub fired_unix_ms: u64,
    pub lateness_ms: u32,
    pub missed: u32,
    pub coalesced: u32,
    pub skipped_overlap: u32,
    pub clock_step_ms: i64,
    pub tip_height: Option<u64>,
    pub tip_hash: Option<[u8; 32]>,
    pub tip_mtp: Option<u64>,
    pub is_last: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerInfo {
    pub timer_id: TimerId,
    pub key: Option<String>,
    pub spec: TimerSpec,
    pub state: TimerState,
    pub seq: u64,
    pub in_flight: u32,
    pub queued: u32,
    pub next_due_unix_ms: Option<u64>,
    pub created_unix_ms: u64,
    pub fires_ok: u64,
    pub fires_failed: u64,
    pub fires_timed_out: u64,
    pub skipped_overlap_total: u64,
    pub missed_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerState {
    Active,
    Paused,
    PausedByBreaker,
    Detached,
    Done,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateMode {
    /// Fail with `KeyExists` if the key exists.
    CreateOnly,
    /// If the key exists with an equal spec, return it unchanged. If the spec differs, replace it.
    Ensure,
    /// Replace any existing timer with this key.
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimerTarget {
    Id(TimerId),
    Key(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerError {
    Invalid(String),
    KeyExists,
    QuotaExceeded(String),
    Disabled,
    NotFound,
    Unsupported(String),
}

impl std::fmt::Display for TimerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimerError::Invalid(reason) => write!(f, "timer_invalid:{reason}"),
            TimerError::KeyExists => write!(f, "timer_key_exists"),
            TimerError::QuotaExceeded(which) => write!(f, "timer_quota_exceeded:{which}"),
            TimerError::Disabled => write!(f, "timer_disabled"),
            TimerError::NotFound => write!(f, "timer_not_found"),
            TimerError::Unsupported(reason) => write!(f, "timer_unsupported:{reason}"),
        }
    }
}

impl std::error::Error for TimerError {}

/// Node `[timers]` table. Integer fields match the config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimerConfig {
    pub enabled: bool,
    pub max_timers_per_module: usize,
    pub max_durable_per_module: usize,
    pub max_timers_total: usize,
    pub min_period_ms: u64,
    pub min_wall_period_secs: u64,
    pub max_payload_bytes: usize,
    pub max_in_flight_per_module: u32,
    pub max_queued_per_module: u32,
    pub max_fires_per_sec_per_module: u32,
    pub default_ack_timeout_secs: u64,
    pub max_ack_timeout_secs: u64,
    pub max_consecutive_failures: u32,
    pub clock_step_threshold_ms: u64,
    pub durable_max_detached_secs: u64,
    pub count_manual: bool,
    /// Honored only when the library is built for tests. A production build ignores it.
    #[serde(default)]
    pub rng_seed: Option<u64>,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_timers_per_module: 256,
            max_durable_per_module: 16,
            max_timers_total: 16_384,
            min_period_ms: 100,
            min_wall_period_secs: 1,
            max_payload_bytes: 256,
            max_in_flight_per_module: 64,
            max_queued_per_module: 256,
            max_fires_per_sec_per_module: 200,
            default_ack_timeout_secs: 30,
            max_ack_timeout_secs: 600,
            max_consecutive_failures: 5,
            clock_step_threshold_ms: 500,
            durable_max_detached_secs: 2_592_000,
            count_manual: false,
            rng_seed: None,
        }
    }
}

/// Per-module cap overlay from `[module_timers.<name>]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModuleTimerLimits {
    pub max_timers: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ValidateCtx {
    pub now_unix_ms: u64,
    pub tip_height: Option<u64>,
}

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Reject a spec that breaks a rule in the timer design. Never clamps.
pub fn validate_spec(
    spec: &TimerSpec,
    limits: &TimerConfig,
    ctx: &ValidateCtx,
) -> Result<(), TimerError> {
    if !clock_allows_first(spec.clock, &spec.first) {
        return Err(TimerError::Invalid("clock mismatch: first".into()));
    }
    if !clock_allows_repeat(spec.clock, &spec.repeat) {
        return Err(TimerError::Invalid("clock mismatch: repeat".into()));
    }
    if let Some(end) = repeat_end(&spec.repeat) {
        if !clock_allows_end(spec.clock, end) {
            return Err(TimerError::Invalid("clock mismatch: until".into()));
        }
        if !end_in_future(end, ctx) {
            return Err(TimerError::Invalid("until is not in the future".into()));
        }
    }
    if spec.reorg == ReorgPolicy::Refire && spec.clock != Clock::Chain {
        return Err(TimerError::Invalid(
            "reorg refire requires the chain clock".into(),
        ));
    }

    match &spec.repeat {
        Repeat::Interval {
            period,
            pacing,
            max_fires,
            ..
        } => {
            check_max_fires(*max_fires)?;
            let min = match spec.clock {
                Clock::Monotonic => Duration::from_millis(limits.min_period_ms),
                Clock::Wall => Duration::from_secs(limits.min_wall_period_secs),
                Clock::Chain => Duration::from_millis(limits.min_period_ms),
            };
            if *period < min {
                return Err(TimerError::Invalid("period too small".into()));
            }
            check_randomness(&spec.randomness, *period, *pacing, false)?;
            if matches!(
                spec.overlap,
                Overlap::Queue { .. } | Overlap::Coalesce | Overlap::Allow { .. }
            ) && *pacing == Pacing::FixedDelay
            {
                return Err(TimerError::Invalid(
                    "fixed delay cannot queue, coalesce, or overlap".into(),
                ));
            }
        }
        Repeat::Once => {
            check_randomness_once(&spec.randomness)?;
        }
        Repeat::Blocks {
            every, max_fires, ..
        } => {
            if *every < 1 {
                return Err(TimerError::Invalid("every must be at least 1".into()));
            }
            check_max_fires(*max_fires)?;
            check_randomness_once(&spec.randomness)?;
        }
        Repeat::Cron {
            expr, max_fires, ..
        } => {
            if expr.is_empty() {
                return Err(TimerError::Invalid("cron expression is empty".into()));
            }
            if let Err(reason) = crate::module::timers::cron::parse_expr(expr) {
                return Err(TimerError::Invalid(reason));
            }
            check_max_fires(*max_fires)?;
            check_randomness_once(&spec.randomness)?;
        }
    }

    if let Some(key) = &spec.key {
        check_key(key)?;
    }
    if spec.payload.len() > limits.max_payload_bytes {
        return Err(TimerError::Invalid("payload too large".into()));
    }
    if spec.delivery.ack_timeout < Duration::from_secs(1)
        || spec.delivery.ack_timeout > Duration::from_secs(limits.max_ack_timeout_secs)
    {
        return Err(TimerError::Invalid("ack_timeout out of range".into()));
    }
    if let OnFailure::Retry { max, .. } = &spec.delivery.on_failure {
        if *max > 10 {
            return Err(TimerError::Invalid("retry max exceeds 10".into()));
        }
    }
    match spec.overlap {
        Overlap::Queue { max } if max > 64 => {
            return Err(TimerError::Invalid("queue max exceeds 64".into()));
        }
        Overlap::Allow { max_concurrent } if max_concurrent == 0 || max_concurrent > 16 => {
            return Err(TimerError::Invalid(
                "allow max_concurrent must be 1..=16".into(),
            ));
        }
        _ => {}
    }
    if matches!(spec.persist, Persist::Durable { .. }) {
        if spec.key.is_none() {
            return Err(TimerError::Invalid("durable requires a key".into()));
        }
        let period_ok = match &spec.repeat {
            Repeat::Interval { period, .. } => *period >= Duration::from_secs(1),
            Repeat::Blocks { .. } | Repeat::Cron { .. } => {
                spec.clock == Clock::Chain || spec.clock == Clock::Wall
            }
            Repeat::Once => true,
        };
        let chain = spec.clock == Clock::Chain;
        if !chain && !period_ok {
            return Err(TimerError::Invalid(
                "durable period must be at least 1s".into(),
            ));
        }
    }
    Ok(())
}

fn check_max_fires(max_fires: Option<u32>) -> Result<(), TimerError> {
    if matches!(max_fires, Some(0)) {
        return Err(TimerError::Invalid("max_fires must be at least 1".into()));
    }
    Ok(())
}

fn check_key(key: &str) -> Result<(), TimerError> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return Err(TimerError::Invalid("key length".into()));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(TimerError::Invalid("key characters".into()));
    }
    Ok(())
}

fn check_randomness(
    randomness: &Randomness,
    period: Duration,
    pacing: Pacing,
    once_or_blocks: bool,
) -> Result<(), TimerError> {
    if once_or_blocks {
        return check_randomness_once(randomness);
    }
    match randomness {
        Randomness::None => Ok(()),
        Randomness::Jitter { max, .. }
        | Randomness::JitterAndSpread {
            jitter_max: max, ..
        } => {
            if pacing == Pacing::FixedRate && *max >= period / 2 {
                return Err(TimerError::Invalid(
                    "jitter must be less than half the period".into(),
                ));
            }
            if let Randomness::JitterAndSpread { spread_max, .. } = randomness {
                check_spread(*spread_max)?;
            }
            Ok(())
        }
        Randomness::Spread { max } => check_spread(*max),
        Randomness::Exponential { mean, floor, cap } => {
            if once_or_blocks || pacing != Pacing::FixedRate {
                return Err(TimerError::Invalid(
                    "exponential requires fixed rate".into(),
                ));
            }
            if *floor == Duration::ZERO || *floor > *mean || *mean > *cap || *cap > DAY {
                return Err(TimerError::Invalid("exponential bounds".into()));
            }
            if *mean != period {
                return Err(TimerError::Invalid(
                    "exponential mean must equal the period".into(),
                ));
            }
            Ok(())
        }
    }
}

fn check_randomness_once(randomness: &Randomness) -> Result<(), TimerError> {
    match randomness {
        Randomness::Exponential { .. } => Err(TimerError::Invalid(
            "exponential cannot combine with once or blocks".into(),
        )),
        Randomness::Spread { max } => check_spread(*max),
        Randomness::JitterAndSpread { spread_max, .. } => check_spread(*spread_max),
        Randomness::None | Randomness::Jitter { .. } => Ok(()),
    }
}

fn check_spread(max: Duration) -> Result<(), TimerError> {
    if max > DAY {
        return Err(TimerError::Invalid("spread exceeds one day".into()));
    }
    Ok(())
}

fn clock_allows_first(clock: Clock, first: &First) -> bool {
    match first {
        First::Now => true,
        First::Delay(_) => matches!(clock, Clock::Monotonic | Clock::Wall),
        First::AtUnixMs(_) => clock == Clock::Wall,
        First::AtHeight { .. } | First::AfterBlocks { .. } | First::AtMtp(_) => {
            clock == Clock::Chain
        }
    }
}

fn clock_allows_repeat(clock: Clock, repeat: &Repeat) -> bool {
    match repeat {
        Repeat::Once => true,
        Repeat::Interval { .. } => matches!(clock, Clock::Monotonic | Clock::Wall),
        Repeat::Blocks { .. } => clock == Clock::Chain,
        Repeat::Cron { .. } => clock == Clock::Wall,
    }
}

fn clock_allows_end(clock: Clock, end: &End) -> bool {
    match end {
        End::AtUnixMs(_) => clock == Clock::Wall,
        End::AtHeight(_) => clock == Clock::Chain,
        End::AfterDuration(_) => clock == Clock::Monotonic,
    }
}

fn repeat_end(repeat: &Repeat) -> Option<&End> {
    match repeat {
        Repeat::Once => None,
        Repeat::Interval { until, .. }
        | Repeat::Blocks { until, .. }
        | Repeat::Cron { until, .. } => until.as_ref(),
    }
}

fn end_in_future(end: &End, ctx: &ValidateCtx) -> bool {
    match end {
        End::AtUnixMs(ms) => *ms > ctx.now_unix_ms,
        End::AtHeight(height) => ctx.tip_height.map(|tip| *height > tip).unwrap_or(true),
        End::AfterDuration(d) => *d > Duration::ZERO,
    }
}

/// Monotonic interval used by the engine tests and by callers that do not need a builder yet.
pub fn monotonic_interval(period: Duration, pacing: Pacing) -> TimerSpec {
    TimerSpec {
        key: None,
        clock: Clock::Monotonic,
        first: First::Now,
        repeat: Repeat::Interval {
            period,
            pacing,
            max_fires: None,
            until: None,
        },
        randomness: Randomness::None,
        missed: Missed::Skip,
        overlap: Overlap::Skip,
        delivery: Delivery::default(),
        persist: Persist::Never,
        max_lateness: None,
        reorg: ReorgPolicy::Ignore,
        payload: Vec::new(),
    }
}

pub fn interval_period(spec: &TimerSpec) -> Option<Duration> {
    match spec.repeat {
        Repeat::Interval { period, .. } => Some(period),
        _ => None,
    }
}

pub fn interval_pacing(spec: &TimerSpec) -> Option<Pacing> {
    match spec.repeat {
        Repeat::Interval { pacing, .. } => Some(pacing),
        _ => None,
    }
}

pub fn interval_max_fires(spec: &TimerSpec) -> Option<u32> {
    match &spec.repeat {
        Repeat::Interval { max_fires, .. }
        | Repeat::Blocks { max_fires, .. }
        | Repeat::Cron { max_fires, .. } => *max_fires,
        Repeat::Once => Some(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> TimerConfig {
        TimerConfig::default()
    }

    fn ctx() -> ValidateCtx {
        ValidateCtx {
            now_unix_ms: 1_700_000_000_000,
            tip_height: Some(100),
        }
    }

    fn ok(spec: &TimerSpec) {
        validate_spec(spec, &limits(), &ctx()).unwrap();
    }

    fn bad(spec: &TimerSpec) -> String {
        validate_spec(spec, &limits(), &ctx())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn validation_table_covers_section_4() {
        let mut spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
        ok(&spec);

        spec.first = First::AtUnixMs(1);
        assert!(bad(&spec).contains("clock mismatch"));

        spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
        spec.clock = Clock::Chain;
        spec.first = First::AtHeight {
            height: 10,
            depth: 0,
        };
        spec.repeat = Repeat::Blocks {
            every: 1,
            max_fires: None,
            until: None,
        };
        ok(&spec);

        spec.repeat = Repeat::Blocks {
            every: 0,
            max_fires: None,
            until: None,
        };
        assert!(bad(&spec).contains("every"));

        spec = monotonic_interval(Duration::from_millis(50), Pacing::FixedRate);
        assert!(bad(&spec).contains("period too small"));

        spec = monotonic_interval(Duration::from_millis(100), Pacing::FixedRate);
        spec.clock = Clock::Wall;
        spec.repeat = Repeat::Interval {
            period: Duration::from_millis(500),
            pacing: Pacing::FixedRate,
            max_fires: None,
            until: None,
        };
        assert!(bad(&spec).contains("period too small"));

        spec = monotonic_interval(Duration::from_secs(2), Pacing::FixedRate);
        if let Repeat::Interval { max_fires, .. } = &mut spec.repeat {
            *max_fires = Some(0);
        }
        assert!(bad(&spec).contains("max_fires"));

        spec = monotonic_interval(Duration::from_secs(2), Pacing::FixedRate);
        if let Repeat::Interval { until, .. } = &mut spec.repeat {
            *until = Some(End::AfterDuration(Duration::ZERO));
        }
        assert!(bad(&spec).contains("until"));

        spec = monotonic_interval(Duration::from_secs(2), Pacing::FixedRate);
        spec.randomness = Randomness::Jitter {
            max: Duration::from_secs(2),
            shape: JitterShape::Positive,
        };
        assert!(bad(&spec).contains("jitter"));

        spec.randomness = Randomness::Jitter {
            max: Duration::from_millis(100),
            shape: JitterShape::Symmetric,
        };
        ok(&spec);

        spec.randomness = Randomness::Spread {
            max: Duration::from_secs(24 * 60 * 60 + 1),
        };
        assert!(bad(&spec).contains("spread"));

        spec.randomness = Randomness::Exponential {
            mean: Duration::from_secs(2),
            floor: Duration::from_millis(1),
            cap: Duration::from_secs(3),
        };
        ok(&spec);

        spec.randomness = Randomness::Exponential {
            mean: Duration::from_secs(3),
            floor: Duration::from_millis(1),
            cap: Duration::from_secs(4),
        };
        assert!(bad(&spec).contains("mean"));

        spec.repeat = Repeat::Once;
        spec.randomness = Randomness::Exponential {
            mean: Duration::from_secs(2),
            floor: Duration::from_millis(1),
            cap: Duration::from_secs(3),
        };
        assert!(bad(&spec).contains("exponential"));

        spec = monotonic_interval(Duration::from_secs(2), Pacing::FixedRate);
        spec.persist = Persist::Durable { audit: false };
        assert!(bad(&spec).contains("durable"));
        spec.key = Some("housekeeping.compact".into());
        ok(&spec);

        spec.delivery.ack_timeout = Duration::from_millis(500);
        assert!(bad(&spec).contains("ack_timeout"));
        spec.delivery.ack_timeout = Duration::from_secs(30);
        spec.delivery.on_failure = OnFailure::Retry {
            max: 11,
            backoff: Backoff {
                initial: Duration::from_secs(1),
                factor_milli: 2000,
                max: Duration::from_secs(30),
                jitter: false,
            },
        };
        assert!(bad(&spec).contains("retry"));

        spec.delivery.on_failure = OnFailure::Ignore;
        spec.overlap = Overlap::Queue { max: 65 };
        assert!(bad(&spec).contains("queue"));
        spec.overlap = Overlap::Allow { max_concurrent: 17 };
        assert!(bad(&spec).contains("allow"));

        spec.overlap = Overlap::Skip;
        spec.key = Some("bad key".into());
        assert!(bad(&spec).contains("key"));
        spec.key = Some("ok.key".into());
        spec.payload = vec![0; 257];
        assert!(bad(&spec).contains("payload"));

        spec.payload.clear();
        spec.repeat = Repeat::Interval {
            period: Duration::from_secs(2),
            pacing: Pacing::FixedDelay,
            max_fires: None,
            until: None,
        };
        spec.overlap = Overlap::Queue { max: 2 };
        assert!(bad(&spec).contains("fixed delay"));
    }

    #[test]
    fn timer_id_displays_lowercase_hex() {
        let id = TimerId([0x0a, 0x0b, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff]);
        let text = id.to_string();
        assert_eq!(text.len(), 32);
        assert!(text.starts_with("0a0b"));
        assert!(text.ends_with("ff"));
    }

    #[test]
    fn duration_round_trips_as_milliseconds() {
        let spec = monotonic_interval(Duration::from_millis(1500), Pacing::FixedRate);
        let bytes = bincode::serialize(&spec).unwrap();
        let back: TimerSpec = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, spec);
    }
}

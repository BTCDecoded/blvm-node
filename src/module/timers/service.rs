//! One actor, one heap of deadlines.
//!
//! The actor never awaits IPC. A dispatcher task sends the fire and reports
//! the acknowledgement back on a channel. Randomness, missed and overlap
//! policy, retry, the per-module rate limit, the wall clock, and chain triggers
//! live here. Cron and the audit journal are later phases.
//! Durable timers are written to the `module_timers` tree and reloaded on start.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, error, info, trace, warn};

use super::clock::{self, ChainEvent, WallClock};
use super::journal::JournalEvent;
use super::persist::{self, PersistedTimer};
use super::types::{
    Clock, CreateMode, End, First, JitterShape, Missed, ModuleTimerLimits, OnFailure, Overlap,
    Pacing, Persist, Randomness, ReorgPolicy, Repeat, TimerAck, TimerConfig, TimerError, TimerFire,
    TimerId, TimerInfo, TimerSpec, TimerState, TimerTarget, ValidateCtx, interval_pacing,
    interval_period, validate_spec,
};
use crate::module::module_name_from_id;
use crate::storage::database::Tree;

#[derive(Debug)]
pub enum TransportError {
    Timeout,
    Closed,
    Failed(String),
}

#[async_trait::async_trait]
pub trait TimerTransport: Send + Sync {
    async fn invoke(
        &self,
        module_id: &str,
        fire: TimerFire,
        ack_timeout: Duration,
    ) -> Result<TimerAck, TransportError>;
}

pub struct PendingFire {
    pub module_id: String,
    pub fire: TimerFire,
    pub reply: oneshot::Sender<Result<TimerAck, TransportError>>,
}

pub struct ChannelTimerTransport {
    tx: mpsc::UnboundedSender<PendingFire>,
}

impl ChannelTimerTransport {
    pub fn pair() -> (Arc<Self>, mpsc::UnboundedReceiver<PendingFire>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(Self { tx }), rx)
    }
}

#[async_trait::async_trait]
impl TimerTransport for ChannelTimerTransport {
    async fn invoke(
        &self,
        module_id: &str,
        fire: TimerFire,
        ack_timeout: Duration,
    ) -> Result<TimerAck, TransportError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(PendingFire {
                module_id: module_id.to_string(),
                fire,
                reply,
            })
            .map_err(|_| TransportError::Closed)?;
        match tokio::time::timeout(ack_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TransportError::Closed),
            Err(_) => Err(TransportError::Timeout),
        }
    }
}

/// Counters the actor updates and the metrics endpoint renders.
#[derive(Default)]
pub struct TimerMetrics {
    inner: std::sync::Mutex<MetricsInner>,
}

#[derive(Default)]
struct MetricsInner {
    fires: HashMap<(String, &'static str), u64>,
    active: HashMap<String, u64>,
    breaker: HashMap<String, u64>,
    lateness: Hist,
    ack: Hist,
    skipped_rate: u64,
}

#[derive(Default, Clone)]
struct Hist {
    buckets: [u64; 8],
    sum: u64,
    count: u64,
}

const LATENCY_BOUNDS_MS: [u64; 8] = [1, 5, 10, 50, 100, 500, 1000, 5000];

impl Hist {
    fn observe(&mut self, ms: u64) {
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(ms);
        for (index, bound) in LATENCY_BOUNDS_MS.iter().enumerate() {
            if ms <= *bound {
                self.buckets[index] = self.buckets[index].saturating_add(1);
            }
        }
    }
}

impl TimerMetrics {
    pub fn record_fire(&self, module: &str, outcome: &'static str) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        *inner
            .fires
            .entry((module.to_string(), outcome))
            .or_insert(0) += 1;
        if outcome == "skipped_rate" {
            inner.skipped_rate = inner.skipped_rate.saturating_add(1);
        }
    }

    pub fn record_lateness(&self, ms: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .lateness
            .observe(ms);
    }

    pub fn record_ack(&self, ms: u64) {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .ack
            .observe(ms);
    }

    pub fn add_active(&self, module: &str, delta: i64) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let slot = inner.active.entry(module.to_string()).or_insert(0);
        if delta >= 0 {
            *slot = slot.saturating_add(delta as u64);
        } else {
            *slot = slot.saturating_sub(delta.unsigned_abs());
        }
    }

    pub fn record_breaker(&self, module: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        *inner.breaker.entry(module.to_string()).or_insert(0) += 1;
    }

    pub fn prometheus(&self) -> String {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let mut out = String::new();
        out.push_str("# HELP blvm_timer_active Resident timers\n# TYPE blvm_timer_active gauge\n");
        for (module, count) in &inner.active {
            out.push_str(&format!(
                "blvm_timer_active{{module=\"{module}\"}} {count}\n"
            ));
        }
        out.push_str("# HELP blvm_timer_fires_total Timer fire outcomes\n# TYPE blvm_timer_fires_total counter\n");
        for ((module, outcome), count) in &inner.fires {
            out.push_str(&format!(
                "blvm_timer_fires_total{{module=\"{module}\",outcome=\"{outcome}\"}} {count}\n"
            ));
        }
        render_hist(
            &mut out,
            "blvm_timer_lateness_ms",
            "Fire lateness",
            &inner.lateness,
        );
        render_hist(
            &mut out,
            "blvm_timer_ack_ms",
            "Acknowledgement latency",
            &inner.ack,
        );
        out.push_str("# HELP blvm_timer_breaker_trips_total Breaker pauses\n# TYPE blvm_timer_breaker_trips_total counter\n");
        for (module, count) in &inner.breaker {
            out.push_str(&format!(
                "blvm_timer_breaker_trips_total{{module=\"{module}\"}} {count}\n"
            ));
        }
        out
    }

    pub fn stats(&self) -> serde_json::Value {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let mut fires = serde_json::Map::new();
        for ((module, outcome), count) in &inner.fires {
            fires.insert(format!("{module}:{outcome}"), serde_json::json!(count));
        }
        serde_json::json!({
            "fires": fires,
            "lateness_count": inner.lateness.count,
            "ack_count": inner.ack.count,
            "skipped_rate": inner.skipped_rate,
        })
    }
}

fn render_hist(out: &mut String, name: &str, help: &str, hist: &Hist) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} histogram\n"));
    for (index, bound) in LATENCY_BOUNDS_MS.iter().enumerate() {
        out.push_str(&format!(
            "{name}_bucket{{le=\"{bound}\"}} {}\n",
            hist.buckets[index]
        ));
    }
    out.push_str(&format!("{name}_bucket{{le=\"+Inf\"}} {}\n", hist.count));
    out.push_str(&format!(
        "{name}_sum {}\n{name}_count {}\n",
        hist.sum, hist.count
    ));
}

struct ClosedTransport;

#[async_trait::async_trait]
impl TimerTransport for ClosedTransport {
    async fn invoke(
        &self,
        _module_id: &str,
        _fire: TimerFire,
        _ack_timeout: Duration,
    ) -> Result<TimerAck, TransportError> {
        Err(TransportError::Closed)
    }
}

#[derive(Clone)]
pub struct TimerService {
    cmd: mpsc::Sender<Cmd>,
    chain_tx: mpsc::UnboundedSender<ChainEvent>,
    metrics: Arc<TimerMetrics>,
    journal: Option<Arc<dyn Tree>>,
}

enum Cmd {
    Create {
        module_id: String,
        spec: TimerSpec,
        mode: CreateMode,
        reply: oneshot::Sender<Result<TimerInfo, TimerError>>,
    },
    Cancel {
        module_id: String,
        target: TimerTarget,
        reply: oneshot::Sender<Result<(), TimerError>>,
    },
    Pause {
        module_id: String,
        target: TimerTarget,
        reply: oneshot::Sender<Result<(), TimerError>>,
    },
    Resume {
        module_id: String,
        target: TimerTarget,
        reply: oneshot::Sender<Result<(), TimerError>>,
    },
    Trigger {
        module_id: String,
        target: TimerTarget,
        reply: oneshot::Sender<Result<(), TimerError>>,
    },
    Get {
        module_id: String,
        target: TimerTarget,
        reply: oneshot::Sender<Result<TimerInfo, TimerError>>,
    },
    List {
        module_id: String,
        reply: oneshot::Sender<Result<Vec<TimerInfo>, TimerError>>,
    },
    Attach {
        name: String,
        module_id: String,
    },
    Detach {
        name: String,
        module_id: Option<String>,
    },
    SetTransport {
        transport: Arc<dyn TimerTransport>,
    },
    HeapLen {
        reply: oneshot::Sender<usize>,
    },
    ListAll {
        module: Option<String>,
        reply: oneshot::Sender<Vec<TimerInfo>>,
    },
    NoteWall {
        reply: oneshot::Sender<()>,
    },
    Chain {
        event: ChainEvent,
        reply: oneshot::Sender<()>,
    },
    DeleteModule {
        name: String,
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DueKind {
    Grid,
    Retry,
    Persist,
}

struct QueuedFire {
    planned: Instant,
    missed: u32,
    is_last: bool,
}

struct Live {
    id: TimerId,
    owner: String,
    spec: TimerSpec,
    state: TimerState,
    generation: u64,
    creation_seq: u64,
    anchor: Instant,
    period: Duration,
    pacing: Pacing,
    next_k: u64,
    deadline: Instant,
    dispatched: u32,
    next_seq: u64,
    in_flight: u32,
    /// Sequences whose acknowledgement is still outstanding.
    outstanding: HashSet<u64>,
    /// Retry deadlines, oldest first. Each entry is also in `outstanding`.
    retry_seqs: VecDeque<u64>,
    consecutive_failures: u32,
    fires_ok: u64,
    fires_failed: u64,
    fires_timed_out: u64,
    skipped_overlap: u64,
    missed_total: u64,
    missed_pending: u32,
    skipped_overlap_pending: u32,
    created_unix_ms: u64,
    until: Option<Instant>,
    queue: VecDeque<QueuedFire>,
    coalesce_pending: u32,
    planned_at: HashMap<u64, Instant>,
    spread: Duration,
    anchor_unix_ms: u64,
    wall_period_ms: u64,
    chain: Option<ChainSched>,
    clock_step_ms: i64,
    unix_scheduled: Option<u64>,
    unix_planned: Option<u64>,
    unix_fired: Option<u64>,
    last_persist: Option<Instant>,
    persist_scheduled: bool,
    detached_unix_ms: u64,
}

#[derive(Clone)]
struct ChainSched {
    height_target: Option<u64>,
    mtp_target: Option<u64>,
    every: Option<u32>,
    next_height: u64,
    last_height: Option<u64>,
    once: bool,
}

struct AckEvent {
    id: TimerId,
    seq: u64,
    attempt: u32,
    outcome: AckOutcome,
}

enum AckOutcome {
    Ok(TimerAck),
    Failed(String),
    TimedOut,
}

struct Actor {
    config: TimerConfig,
    overrides: HashMap<String, ModuleTimerLimits>,
    timers: HashMap<TimerId, Live>,
    keys: HashMap<(String, String), TimerId>,
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(Instant, u64, TimerId, u64, DueKind)>>,
    stale: usize,
    counts: HashMap<String, usize>,
    durable_counts: HashMap<String, usize>,
    inflight: HashMap<String, u32>,
    attached: HashMap<String, String>,
    by_module_id: HashMap<String, String>,
    next_creation: u64,
    epoch: Instant,
    epoch_unix_ms: u64,
    transport: Arc<RwLock<Arc<dyn TimerTransport>>>,
    ack_tx: mpsc::UnboundedSender<AckEvent>,
    inflight_cap_hit: u64,
    metrics: Arc<TimerMetrics>,
    rng: StdRng,
    rng_draws: u64,
    rng_fixed: bool,
    rng_reseed: Instant,
    buckets: HashMap<String, TokenBucket>,
    sent_at: HashMap<(TimerId, u64), Instant>,
    wall: Arc<dyn WallClock>,
    wall_base_ms: u64,
    mono_base: Instant,
    tip_height: Option<u64>,
    tip_hash: Option<[u8; 32]>,
    tip_mtp: Option<u64>,
    store: Option<Arc<dyn Tree>>,
    durable_dirty: Vec<TimerId>,
    journal: Option<Arc<dyn Tree>>,
    journal_prev: [u8; 32],
    journal_next: u64,
    late_warn: HashMap<TimerId, Instant>,
}

struct TokenBucket {
    tokens: f64,
    updated: Instant,
}

impl TimerService {
    pub fn spawn(config: TimerConfig, overrides: HashMap<String, ModuleTimerLimits>) -> Self {
        Self::spawn_stored(config, overrides, None, None)
    }

    pub fn spawn_stored(
        config: TimerConfig,
        overrides: HashMap<String, ModuleTimerLimits>,
        store: Option<Arc<dyn Tree>>,
        journal: Option<Arc<dyn Tree>>,
    ) -> Self {
        Self::spawn_with_store(config, overrides, Arc::new(ClosedTransport), store, journal)
    }

    pub fn spawn_with(
        config: TimerConfig,
        overrides: HashMap<String, ModuleTimerLimits>,
        transport: Arc<dyn TimerTransport>,
    ) -> Self {
        Self::spawn_with_store(config, overrides, transport, None, None)
    }

    fn spawn_with_store(
        config: TimerConfig,
        overrides: HashMap<String, ModuleTimerLimits>,
        transport: Arc<dyn TimerTransport>,
        store: Option<Arc<dyn Tree>>,
        journal: Option<Arc<dyn Tree>>,
    ) -> Self {
        Self::spawn_clocked(
            config,
            overrides,
            transport,
            Arc::new(super::clock::SystemWallClock),
            store,
            journal,
        )
    }

    fn spawn_clocked(
        config: TimerConfig,
        overrides: HashMap<String, ModuleTimerLimits>,
        transport: Arc<dyn TimerTransport>,
        wall: Arc<dyn WallClock>,
        store: Option<Arc<dyn Tree>>,
        journal: Option<Arc<dyn Tree>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let (ack_tx, ack_rx) = mpsc::unbounded_channel();
        let (chain_tx, chain_rx) = mpsc::unbounded_channel();
        let transport = Arc::new(RwLock::new(transport));
        let metrics = Arc::new(TimerMetrics::default());
        let actor = Actor::new(
            config,
            overrides,
            Arc::clone(&transport),
            ack_tx,
            Arc::clone(&metrics),
            wall,
            store,
            journal.clone(),
        );
        tokio::spawn(actor.run(cmd_rx, ack_rx, chain_rx));
        Self {
            cmd: cmd_tx,
            chain_tx,
            metrics,
            journal,
        }
    }

    pub fn journal_entries(
        &self,
        from: u64,
        to: u64,
        module: Option<&str>,
    ) -> Result<Vec<super::journal::JournalRecord>, String> {
        let Some(journal) = &self.journal else {
            return Ok(Vec::new());
        };
        super::journal::read(journal.as_ref(), from, to, module)
    }

    pub fn verify_journal(&self) -> Result<(), super::journal::JournalBreak> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        super::journal::verify(journal.as_ref())
    }

    pub fn chain_sender(&self) -> mpsc::UnboundedSender<ChainEvent> {
        self.chain_tx.clone()
    }

    /// Wait until the actor has applied one chain event.
    pub async fn chain_event(&self, event: ChainEvent) {
        let (reply, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Chain { event, reply }).await.is_err() {
            return;
        }
        let _ = rx.await;
    }

    /// Re-read the wall clock and apply a step before the caller continues.
    pub async fn note_wall(&self) {
        let (reply, rx) = oneshot::channel();
        if self.cmd.send(Cmd::NoteWall { reply }).await.is_err() {
            return;
        }
        let _ = rx.await;
    }

    pub fn prometheus(&self) -> String {
        self.metrics.prometheus()
    }

    pub fn stats(&self) -> serde_json::Value {
        self.metrics.stats()
    }

    pub async fn list_all(&self, module: Option<&str>) -> Vec<TimerInfo> {
        let (reply, rx) = oneshot::channel();
        if self
            .cmd
            .send(Cmd::ListAll {
                module: module.map(str::to_string),
                reply,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    pub async fn set_transport(&self, transport: Arc<dyn TimerTransport>) {
        let _ = self.cmd.send(Cmd::SetTransport { transport }).await;
    }

    pub async fn create(
        &self,
        module_id: &str,
        spec: TimerSpec,
        mode: CreateMode,
    ) -> Result<TimerInfo, TimerError> {
        self.roundtrip(|reply| Cmd::Create {
            module_id: module_id.to_string(),
            spec,
            mode,
            reply,
        })
        .await
    }

    pub async fn cancel(&self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        self.roundtrip(|reply| Cmd::Cancel {
            module_id: module_id.to_string(),
            target,
            reply,
        })
        .await
    }

    pub async fn pause(&self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        self.roundtrip(|reply| Cmd::Pause {
            module_id: module_id.to_string(),
            target,
            reply,
        })
        .await
    }

    pub async fn resume(&self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        self.roundtrip(|reply| Cmd::Resume {
            module_id: module_id.to_string(),
            target,
            reply,
        })
        .await
    }

    pub async fn trigger(&self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        self.roundtrip(|reply| Cmd::Trigger {
            module_id: module_id.to_string(),
            target,
            reply,
        })
        .await
    }

    pub async fn get(&self, module_id: &str, target: TimerTarget) -> Result<TimerInfo, TimerError> {
        self.roundtrip(|reply| Cmd::Get {
            module_id: module_id.to_string(),
            target,
            reply,
        })
        .await
    }

    pub async fn list(&self, module_id: &str) -> Result<Vec<TimerInfo>, TimerError> {
        self.roundtrip(|reply| Cmd::List {
            module_id: module_id.to_string(),
            reply,
        })
        .await
    }

    pub async fn attach_module(&self, name: &str, module_id: &str) {
        let _ = self
            .cmd
            .send(Cmd::Attach {
                name: name.to_string(),
                module_id: module_id.to_string(),
            })
            .await;
    }

    pub async fn delete_module_timers(&self, name: &str) {
        let (reply, rx) = oneshot::channel();
        if self
            .cmd
            .send(Cmd::DeleteModule {
                name: name.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    pub async fn detach_module(&self, name: &str, module_id: Option<&str>) {
        let _ = self
            .cmd
            .send(Cmd::Detach {
                name: name.to_string(),
                module_id: module_id.map(str::to_string),
            })
            .await;
    }

    pub async fn heap_len(&self) -> usize {
        let (reply, rx) = oneshot::channel();
        if self.cmd.send(Cmd::HeapLen { reply }).await.is_err() {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    async fn roundtrip<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, TimerError>>) -> Cmd,
    ) -> Result<T, TimerError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(build(reply))
            .await
            .map_err(|_| TimerError::Unsupported("timer actor stopped".into()))?;
        rx.await
            .map_err(|_| TimerError::Unsupported("timer actor stopped".into()))?
    }
}

impl Actor {
    fn new(
        config: TimerConfig,
        overrides: HashMap<String, ModuleTimerLimits>,
        transport: Arc<RwLock<Arc<dyn TimerTransport>>>,
        ack_tx: mpsc::UnboundedSender<AckEvent>,
        metrics: Arc<TimerMetrics>,
        wall: Arc<dyn WallClock>,
        store: Option<Arc<dyn Tree>>,
        journal: Option<Arc<dyn Tree>>,
    ) -> Self {
        let epoch = Instant::now();
        let wall_base_ms = wall.unix_ms();
        let epoch_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let rng_fixed = cfg!(test) && config.rng_seed.is_some();
        if !cfg!(test) && config.rng_seed.is_some() {
            warn!("[timers] rng_seed is ignored outside test builds");
        }
        let rng = match config.rng_seed {
            Some(seed) if cfg!(test) => StdRng::seed_from_u64(seed),
            _ => StdRng::from_entropy(),
        };
        Self {
            config,
            overrides,
            timers: HashMap::new(),
            keys: HashMap::new(),
            heap: std::collections::BinaryHeap::new(),
            stale: 0,
            counts: HashMap::new(),
            durable_counts: HashMap::new(),
            inflight: HashMap::new(),
            attached: HashMap::new(),
            by_module_id: HashMap::new(),
            next_creation: 1,
            epoch,
            epoch_unix_ms,
            transport,
            ack_tx,
            inflight_cap_hit: 0,
            metrics,
            rng,
            rng_draws: 0,
            rng_fixed,
            rng_reseed: epoch,
            buckets: HashMap::new(),
            sent_at: HashMap::new(),
            wall,
            wall_base_ms,
            mono_base: epoch,
            tip_height: None,
            tip_hash: None,
            tip_mtp: None,
            store,
            durable_dirty: Vec::new(),
            journal,
            journal_prev: [0u8; 32],
            journal_next: 0,
            late_warn: HashMap::new(),
        }
    }

    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<Cmd>,
        mut ack_rx: mpsc::UnboundedReceiver<AckEvent>,
        mut chain_rx: mpsc::UnboundedReceiver<ChainEvent>,
    ) {
        self.load_durable();
        self.load_journal();
        let mut step_at = Instant::now() + Duration::from_secs(1);
        loop {
            let next = self.next_deadline();
            let sleep = async {
                match next {
                    Some(when) => tokio::time::sleep_until(when).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = sleep => self.on_due(),
                _ = tokio::time::sleep_until(step_at) => {
                    step_at = Instant::now() + Duration::from_secs(1);
                    if self.config.enabled {
                        self.observe_wall();
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.on_cmd(cmd),
                        None => return,
                    }
                }
                ack = ack_rx.recv() => {
                    if let Some(ack) = ack {
                        self.on_ack(ack);
                    }
                }
                event = chain_rx.recv() => {
                    if let Some(event) = event {
                        if self.config.enabled {
                            self.on_chain(event);
                        }
                    }
                }
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.heap
            .peek()
            .map(|std::cmp::Reverse((when, _, _, _, _))| *when)
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        if !self.config.enabled {
            match cmd {
                Cmd::Create { reply, .. } => {
                    let _ = reply.send(Err(TimerError::Disabled));
                }
                Cmd::Cancel { reply, .. }
                | Cmd::Pause { reply, .. }
                | Cmd::Resume { reply, .. }
                | Cmd::Trigger { reply, .. } => {
                    let _ = reply.send(Err(TimerError::Disabled));
                }
                Cmd::Get { reply, .. } => {
                    let _ = reply.send(Err(TimerError::Disabled));
                }
                Cmd::List { reply, .. } => {
                    let _ = reply.send(Err(TimerError::Disabled));
                }
                Cmd::Attach { .. } | Cmd::Detach { .. } | Cmd::SetTransport { .. } => {}
                Cmd::HeapLen { reply } => {
                    let _ = reply.send(0);
                }
                Cmd::ListAll { reply, .. } => {
                    let _ = reply.send(Vec::new());
                }
                Cmd::NoteWall { reply }
                | Cmd::Chain { reply, .. }
                | Cmd::DeleteModule { reply, .. } => {
                    let _ = reply.send(());
                }
            }
            return;
        }
        match cmd {
            Cmd::Create {
                module_id,
                spec,
                mode,
                reply,
            } => {
                let _ = reply.send(self.create(&module_id, spec, mode));
            }
            Cmd::Cancel {
                module_id,
                target,
                reply,
            } => {
                let _ = reply.send(self.cancel_cmd(&module_id, target));
            }
            Cmd::Pause {
                module_id,
                target,
                reply,
            } => {
                let _ = reply.send(self.pause_cmd(&module_id, target));
            }
            Cmd::Resume {
                module_id,
                target,
                reply,
            } => {
                let _ = reply.send(self.resume_cmd(&module_id, target));
            }
            Cmd::Trigger {
                module_id,
                target,
                reply,
            } => {
                let _ = reply.send(self.trigger_cmd(&module_id, target));
            }
            Cmd::Get {
                module_id,
                target,
                reply,
            } => {
                let _ = reply.send(self.get_cmd(&module_id, target));
            }
            Cmd::List { module_id, reply } => {
                let _ = reply.send(self.list_cmd(&module_id));
            }
            Cmd::Attach { name, module_id } => self.attach(&name, &module_id),
            Cmd::Detach { name, module_id } => self.detach(&name, module_id.as_deref()),
            Cmd::SetTransport { transport } => {
                if let Ok(mut slot) = self.transport.write() {
                    *slot = transport;
                }
            }
            Cmd::HeapLen { reply } => {
                self.compact();
                let _ = reply.send(self.heap.len());
            }
            Cmd::ListAll { module, reply } => {
                let ids: Vec<TimerId> = self.timers.keys().copied().collect();
                let infos = ids
                    .into_iter()
                    .filter(|id| match &module {
                        None => true,
                        Some(name) => self
                            .timers
                            .get(id)
                            .is_some_and(|timer| timer.owner == *name),
                    })
                    .map(|id| self.info(id))
                    .collect();
                let _ = reply.send(infos);
            }
            Cmd::NoteWall { reply } => {
                self.on_due();
                let _ = reply.send(());
                return;
            }
            Cmd::Chain { event, reply } => {
                self.on_chain(event);
                let _ = reply.send(());
                return;
            }
            Cmd::DeleteModule { name, reply } => {
                self.delete_module(&name);
                let _ = reply.send(());
                return;
            }
        }
        self.on_due();
        self.flush_durable();
    }

    fn owner_of(&self, module_id: &str) -> String {
        self.by_module_id
            .get(module_id)
            .cloned()
            .unwrap_or_else(|| module_name_from_id(module_id).to_string())
    }

    fn create(
        &mut self,
        module_id: &str,
        spec: TimerSpec,
        mode: CreateMode,
    ) -> Result<TimerInfo, TimerError> {
        let owner = self.owner_of(module_id);
        let ctx = ValidateCtx {
            now_unix_ms: self.wall.unix_ms(),
            tip_height: self.tip_height,
        };
        validate_spec(&spec, &self.config, &ctx)?;
        if matches!(mode, CreateMode::Ensure | CreateMode::Replace) && spec.key.is_none() {
            return Err(TimerError::Invalid(
                "key required for this create mode".into(),
            ));
        }
        if let Some(key) = spec.key.clone() {
            if let Some(existing) = self.keys.get(&(owner.clone(), key)).copied() {
                return self.create_existing(existing, spec, mode);
            }
        }
        self.enforce_quota(&owner, &spec)?;
        let info = self.insert_new(owner, spec, false);
        Ok(info)
    }

    fn create_existing(
        &mut self,
        existing: TimerId,
        spec: TimerSpec,
        mode: CreateMode,
    ) -> Result<TimerInfo, TimerError> {
        match mode {
            CreateMode::CreateOnly => Err(TimerError::KeyExists),
            CreateMode::Ensure
                if self
                    .timers
                    .get(&existing)
                    .map(|t| t.spec == spec)
                    .unwrap_or(false) =>
            {
                Ok(self.info(existing))
            }
            CreateMode::Ensure | CreateMode::Replace => {
                let owner = self
                    .timers
                    .get(&existing)
                    .map(|t| t.owner.clone())
                    .ok_or(TimerError::NotFound)?;
                self.drop_timer(existing);
                Ok(self.insert_new(owner, spec, true))
            }
        }
    }

    fn enforce_quota(&self, owner: &str, spec: &TimerSpec) -> Result<(), TimerError> {
        let per = self
            .overrides
            .get(owner)
            .and_then(|o| o.max_timers)
            .unwrap_or(self.config.max_timers_per_module);
        if self.counts.get(owner).copied().unwrap_or(0) >= per {
            return Err(TimerError::QuotaExceeded("max_timers_per_module".into()));
        }
        if self.timers.len() >= self.config.max_timers_total {
            return Err(TimerError::QuotaExceeded("max_timers_total".into()));
        }
        if matches!(spec.persist, Persist::Durable { .. })
            && self.durable_counts.get(owner).copied().unwrap_or(0)
                >= self.config.max_durable_per_module
        {
            return Err(TimerError::QuotaExceeded("max_durable_per_module".into()));
        }
        Ok(())
    }

    fn insert_new(&mut self, owner: String, spec: TimerSpec, update: bool) -> TimerInfo {
        let id = new_timer_id();
        let now = Instant::now();
        let period = interval_period(&spec).unwrap_or(Duration::ZERO);
        let pacing = interval_pacing(&spec).unwrap_or(Pacing::FixedRate);
        let until = until_instant(&spec, now);
        let deadline = first_deadline(&spec, now);
        let creation_seq = self.next_creation;
        self.next_creation = self.next_creation.wrapping_add(1);
        let durable = matches!(spec.persist, Persist::Durable { .. });
        if let Some(key) = spec.key.clone() {
            self.keys.insert((owner.clone(), key), id);
        }
        *self.counts.entry(owner.clone()).or_insert(0) += 1;
        if durable {
            *self.durable_counts.entry(owner.clone()).or_insert(0) += 1;
        }
        let created_unix_ms = self.to_unix(now);
        let clock = spec.clock;
        let spec_for_plan = spec.clone();
        let spread = self.draw_spread(&spec);
        info!(module = %owner, timer = %id, "timer created");
        self.metrics.add_active(&owner, 1);
        let live = Live {
            id,
            owner,
            spec,
            state: TimerState::Active,
            generation: 1,
            creation_seq,
            anchor: match deadline {
                d if d == now => now,
                other => other,
            },
            period,
            pacing,
            next_k: 0,
            deadline,
            dispatched: 0,
            next_seq: 1,
            in_flight: 0,
            outstanding: HashSet::new(),
            retry_seqs: VecDeque::new(),
            consecutive_failures: 0,
            fires_ok: 0,
            fires_failed: 0,
            fires_timed_out: 0,
            skipped_overlap: 0,
            missed_total: 0,
            missed_pending: 0,
            skipped_overlap_pending: 0,
            created_unix_ms,
            until,
            queue: VecDeque::new(),
            coalesce_pending: 0,
            planned_at: HashMap::new(),
            spread,
            anchor_unix_ms: 0,
            wall_period_ms: period.as_millis() as u64,
            chain: None,
            clock_step_ms: 0,
            unix_scheduled: None,
            unix_planned: None,
            unix_fired: None,
            last_persist: None,
            persist_scheduled: false,
            detached_unix_ms: 0,
        };
        if clock == Clock::Chain {
            let tip = self.tip_height.unwrap_or(0);
            let chain = chain_sched(&spec_for_plan, tip);
            let mut live = live;
            live.chain = Some(chain);
            self.timers.insert(id, live);
            self.eval_chain_timer(id, self.tip_height, self.tip_mtp);
            self.touch_durable(id);
            self.audit(
                id,
                if update {
                    JournalEvent::Updated
                } else {
                    JournalEvent::Created
                },
            );
            return self.info(id);
        }
        if clock == Clock::Wall {
            let wall_now = self.wall.unix_ms();
            let (mut anchor_unix, mut wall_deadline) = wall_first(&spec_for_plan, wall_now, now);
            #[cfg(feature = "timer-cron")]
            if let Repeat::Cron { expr, .. } = &spec_for_plan.repeat {
                if let Some(next) = super::cron::next_after(expr, wall_now) {
                    anchor_unix = wall_now;
                    wall_deadline = now + Duration::from_millis(next.saturating_sub(wall_now));
                }
            }
            let mut live = live;
            live.anchor_unix_ms = anchor_unix;
            live.anchor = wall_deadline;
            live.deadline = wall_deadline;
            let generation = live.generation;
            let creation_seq = live.creation_seq;
            self.timers.insert(id, live);
            self.push(wall_deadline, creation_seq, id, generation, DueKind::Grid);
            self.touch_durable(id);
            self.audit(
                id,
                if update {
                    JournalEvent::Updated
                } else {
                    JournalEvent::Created
                },
            );
            return self.info(id);
        }
        // First::Delay anchors the grid at the first deadline, not at now.
        let anchor = match &live.spec.first {
            First::Delay(_) => deadline,
            _ => now,
        };
        let mut live = live;
        live.anchor = anchor;
        let generation = live.generation;
        self.timers.insert(id, live);
        let planned = self.ensure_planned(id, 0).unwrap_or(deadline);
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.deadline = planned;
        }
        self.push(planned, creation_seq, id, generation, DueKind::Grid);
        self.touch_durable(id);
        self.audit(
            id,
            if update {
                JournalEvent::Updated
            } else {
                JournalEvent::Created
            },
        );
        self.info(id)
    }

    fn lookup(&self, owner: &str, target: &TimerTarget) -> Result<TimerId, TimerError> {
        let id = match target {
            TimerTarget::Id(id) => *id,
            TimerTarget::Key(key) => self
                .keys
                .get(&(owner.to_string(), key.clone()))
                .copied()
                .ok_or(TimerError::NotFound)?,
        };
        match self.timers.get(&id) {
            Some(timer) if timer.owner == owner => Ok(id),
            _ => Err(TimerError::NotFound),
        }
    }

    fn cancel_cmd(&mut self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        let owner = self.owner_of(module_id);
        let id = self.lookup(&owner, &target)?;
        info!(module = %owner, timer = %id, "timer cancelled");
        self.remove_timer(id);
        Ok(())
    }

    fn pause_cmd(&mut self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        let owner = self.owner_of(module_id);
        let id = self.lookup(&owner, &target)?;
        let timer = self.timers.get_mut(&id).ok_or(TimerError::NotFound)?;
        timer.state = TimerState::Paused;
        timer.generation = timer.generation.wrapping_add(1);
        self.stale += 1;
        self.maybe_compact();
        self.touch_durable(id);
        self.audit(id, JournalEvent::Paused);
        info!(module = %owner, timer = %id, "timer paused");
        Ok(())
    }

    fn resume_cmd(&mut self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        let owner = self.owner_of(module_id);
        let id = self.lookup(&owner, &target)?;
        let now = Instant::now();
        {
            let timer = self.timers.get_mut(&id).ok_or(TimerError::NotFound)?;
            timer.state = TimerState::Active;
            timer.generation = timer.generation.wrapping_add(1);
            timer.consecutive_failures = 0;
        }
        self.stale += 1;
        self.catch_up_or_schedule(id, now);
        self.touch_durable(id);
        self.audit(id, JournalEvent::Resumed);
        info!(module = %owner, timer = %id, "timer resumed");
        Ok(())
    }

    fn trigger_cmd(&mut self, module_id: &str, target: TimerTarget) -> Result<(), TimerError> {
        let owner = self.owner_of(module_id);
        let id = self.lookup(&owner, &target)?;
        let now = Instant::now();
        let count = self.config.count_manual;
        self.emit(id, now, now, 0, 0, false, 1, count);
        Ok(())
    }

    fn get_cmd(&self, module_id: &str, target: TimerTarget) -> Result<TimerInfo, TimerError> {
        let owner = self.owner_of(module_id);
        let id = self.lookup(&owner, &target)?;
        Ok(self.info(id))
    }

    fn list_cmd(&self, module_id: &str) -> Result<Vec<TimerInfo>, TimerError> {
        let owner = self.owner_of(module_id);
        let mut out: Vec<TimerInfo> = self
            .timers
            .values()
            .filter(|t| t.owner == owner)
            .map(|t| self.info(t.id))
            .collect();
        out.truncate(self.config.max_timers_per_module);
        Ok(out)
    }

    fn attach(&mut self, name: &str, module_id: &str) {
        if let Some(previous) = self
            .attached
            .insert(name.to_string(), module_id.to_string())
        {
            self.by_module_id.remove(&previous);
        }
        self.by_module_id
            .insert(module_id.to_string(), name.to_string());
        let now = Instant::now();
        let ids: Vec<TimerId> = self
            .timers
            .values()
            .filter(|t| t.owner == name && t.state == TimerState::Detached)
            .map(|t| t.id)
            .collect();
        for id in ids {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Active;
            }
            let pending = self.timers.get(&id).map(|t| t.missed_pending).unwrap_or(0);
            let missed_policy = self
                .timers
                .get(&id)
                .map(|t| t.spec.missed)
                .unwrap_or(Missed::Skip);
            if pending > 0 {
                match missed_policy {
                    Missed::FireOnce => {
                        if let Some(timer) = self.timers.get_mut(&id) {
                            timer.missed_pending = 0;
                        }
                        self.emit(id, now, now, pending.saturating_sub(1), 0, false, 1, true);
                    }
                    Missed::CatchUp { max } => {
                        let n = pending.min(max).max(1);
                        if let Some(timer) = self.timers.get_mut(&id) {
                            timer.missed_pending = pending.saturating_sub(n);
                        }
                        for i in 0..n {
                            let missed = if i + 1 == n {
                                pending.saturating_sub(n)
                            } else {
                                0
                            };
                            self.emit(
                                id,
                                now,
                                now,
                                missed,
                                0,
                                i + 1 == n && pending <= max,
                                1,
                                true,
                            );
                        }
                    }
                    Missed::Skip => {}
                }
            }
        }
    }

    fn detach(&mut self, name: &str, module_id: Option<&str>) {
        if let Some(id) = module_id {
            if self.attached.get(name).map(String::as_str) != Some(id) {
                return;
            }
        }
        if let Some(current) = self.attached.remove(name) {
            self.by_module_id.remove(&current);
        }
        let ids: Vec<(TimerId, bool)> = self
            .timers
            .values()
            .filter(|t| t.owner == name)
            .map(|t| (t.id, matches!(t.spec.persist, Persist::Durable { .. })))
            .collect();
        for (id, durable) in ids {
            if durable {
                let entry = if let Some(timer) = self.timers.get_mut(&id) {
                    timer.state = TimerState::Detached;
                    timer.generation = timer.generation.wrapping_add(1);
                    Some((timer.deadline, timer.creation_seq, timer.generation))
                } else {
                    None
                };
                if let Some((when, creation, generation)) = entry {
                    self.push(when, creation, id, generation, DueKind::Grid);
                }
                if let Some(timer) = self.timers.get_mut(&id) {
                    timer.detached_unix_ms = self.wall.unix_ms();
                }
                self.touch_durable(id);
                self.stale += 1;
            } else {
                self.remove_timer(id);
            }
        }
        self.maybe_compact();
    }

    fn on_due(&mut self) {
        self.observe_wall();
        let now = Instant::now();
        loop {
            let Some(std::cmp::Reverse((when, _, id, generation, kind))) =
                self.heap.peek().copied()
            else {
                break;
            };
            if when > now {
                break;
            }
            self.heap.pop();
            if kind == DueKind::Persist {
                if let Some(timer) = self.timers.get_mut(&id) {
                    timer.persist_scheduled = false;
                }
                self.write_record(id);
                continue;
            }
            if !self.is_live(id, generation) {
                continue;
            }
            match kind {
                DueKind::Retry => self.fire_retry(id, now),
                DueKind::Grid => self.fire_grid(id, now),
                DueKind::Persist => {}
            }
        }
        self.flush_durable();
    }

    fn is_live(&self, id: TimerId, generation: u64) -> bool {
        self.timers
            .get(&id)
            .is_some_and(|t| t.generation == generation && t.state != TimerState::Cancelled)
    }

    fn fire_retry(&mut self, id: TimerId, now: Instant) {
        let (seq, attempt) = {
            let Some(timer) = self.timers.get_mut(&id) else {
                return;
            };
            let Some(seq) = timer.retry_seqs.pop_front() else {
                return;
            };
            let attempt = timer.consecutive_failures.saturating_add(1).max(2);
            (seq, attempt)
        };
        self.spawn_fire(id, self.fire_from(id, now, now, 0, 0, false, seq, attempt));
    }

    fn fire_grid(&mut self, id: TimerId, now: Instant) {
        let clock = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            if timer.state == TimerState::Detached {
                Clock::Monotonic
            } else if timer.state != TimerState::Active {
                return;
            } else {
                timer.spec.clock
            }
        };
        if self
            .timers
            .get(&id)
            .is_some_and(|timer| timer.state == TimerState::Detached)
        {
            self.accumulate_detached(id, now);
            return;
        }
        if clock == Clock::Wall {
            #[cfg(feature = "timer-cron")]
            if self
                .timers
                .get(&id)
                .is_some_and(|timer| matches!(timer.spec.repeat, Repeat::Cron { .. }))
            {
                self.fire_cron(id, now);
                return;
            }
            self.fire_wall(id, now);
            return;
        }
        if clock == Clock::Chain {
            return;
        }
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        let pacing = timer.pacing;
        let once = matches!(timer.spec.repeat, Repeat::Once);
        let planned = timer.deadline;
        let max_lateness = timer.spec.max_lateness;
        let dispatched = timer.dispatched;
        let hits_max = timer
            .spec
            .repeat
            .max_fires_hit(dispatched.saturating_add(1));
        let _ = timer;
        if pacing == Pacing::FixedDelay || once {
            let late = now.saturating_duration_since(planned);
            if max_lateness.is_some_and(|max| late > max) {
                self.drop_late(id);
                return;
            }
            let is_last = once || hits_max;
            self.emit(id, planned, planned, 0, 0, is_last, 1, true);
            if is_last {
                if let Some(timer) = self.timers.get_mut(&id) {
                    if timer.in_flight == 0 {
                        timer.state = TimerState::Done;
                    }
                }
            }
            return;
        }
        self.fire_fixed_rate(id, now);
    }

    fn fire_fixed_rate(&mut self, id: TimerId, now: Instant) {
        let start_k = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            timer.next_k
        };
        // Exponential gaps are a process, not a grid. Missed does not apply:
        // fire the current slot and schedule the next one, even if it is already due.
        if self
            .timers
            .get(&id)
            .is_some_and(|timer| matches!(timer.spec.randomness, Randomness::Exponential { .. }))
        {
            let planned = self.ensure_planned(id, start_k).unwrap_or(now);
            let is_last = self.would_be_last(id, 1);
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.next_k = start_k.saturating_add(1);
            }
            self.emit(id, planned, planned, 0, 0, is_last, 1, true);
            if !is_last {
                self.reschedule_after(id, start_k.saturating_add(1));
            } else if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Done;
            }
            return;
        }
        let mut k = start_k;
        let mut m = 0u64;
        while self.ensure_planned(id, k).is_some_and(|t| t <= now) {
            m += 1;
            k += 1;
            if m > 1_000_000 {
                break;
            }
        }
        if m == 0 {
            if let Some(when) = self.ensure_planned(id, start_k) {
                self.reschedule(id, when, start_k);
            }
            return;
        }
        let policy = self
            .timers
            .get(&id)
            .map(|t| t.spec.missed)
            .unwrap_or(Missed::Skip);
        if m == 1 {
            let ideal = self.ideal(id, start_k).unwrap_or(now);
            let raw = self.ensure_planned(id, start_k).unwrap_or(now);
            let planned = self.clamp_planned(raw, ideal, now);
            if self.too_late(id, now, planned) {
                self.drop_late(id);
                self.reschedule_after(id, k);
                return;
            }
            let is_last = self.would_be_last(id, 1);
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.next_k = k;
            }
            self.emit(id, ideal, planned, 0, 0, is_last, 1, true);
            if !is_last {
                self.reschedule_after(id, k);
            } else if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Done;
            }
            return;
        }
        match policy {
            Missed::Skip => {
                if let Some(timer) = self.timers.get_mut(&id) {
                    timer.missed_pending = timer.missed_pending.saturating_add(m as u32);
                    timer.missed_total = timer.missed_total.saturating_add(m);
                    timer.next_k = k;
                }
                self.reschedule_after(id, k);
            }
            Missed::FireOnce => {
                let ideal = self.ideal(id, start_k).unwrap_or(now);
                let raw = self.ensure_planned(id, start_k).unwrap_or(now);
                let planned = self.clamp_planned(raw, ideal, now);
                if let Some(timer) = self.timers.get_mut(&id) {
                    timer.next_k = k;
                    timer.missed_total = timer.missed_total.saturating_add(m - 1);
                }
                let is_last = self.would_be_last(id, 1);
                self.emit(id, ideal, planned, (m - 1) as u32, 0, is_last, 1, true);
                if !is_last {
                    self.reschedule_after(id, k);
                }
            }
            Missed::CatchUp { max } => {
                let n = m.min(u64::from(max.max(1)));
                let leftover = (m - n) as u32;
                for i in 0..n {
                    let slot = start_k + i;
                    let ideal = self.ideal(id, slot).unwrap_or(now);
                    let raw = self.ensure_planned(id, slot).unwrap_or(now);
                    let planned = self.clamp_planned(raw, ideal, now);
                    let missed = if i + 1 == n { leftover } else { 0 };
                    let is_last = self.would_be_last(id, 1) && i + 1 == n;
                    self.emit(id, ideal, planned, missed, 0, is_last, 1, true);
                }
                if let Some(timer) = self.timers.get_mut(&id) {
                    timer.next_k = k;
                    timer.missed_total = timer.missed_total.saturating_add(u64::from(leftover));
                }
                self.reschedule_after(id, k);
            }
        }
    }

    fn accumulate_detached(&mut self, id: TimerId, now: Instant) {
        let (pacing, period) = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            (timer.pacing, timer.period)
        };
        if pacing != Pacing::FixedRate || period.is_zero() {
            let entry = if let Some(timer) = self.timers.get_mut(&id) {
                timer.missed_pending = timer.missed_pending.saturating_add(1);
                timer.missed_total = timer.missed_total.saturating_add(1);
                timer.deadline = now + period.max(Duration::from_millis(1));
                Some((timer.deadline, timer.creation_seq, timer.generation))
            } else {
                None
            };
            if let Some((when, creation, generation)) = entry {
                self.push(when, creation, id, generation, DueKind::Grid);
            }
            return;
        }
        let mut k = self.timers.get(&id).map(|t| t.next_k).unwrap_or(0);
        let mut m = 0u64;
        while self.ensure_planned(id, k).is_some_and(|t| t <= now) {
            m += 1;
            k += 1;
            if m > 1_000_000 {
                break;
            }
        }
        if m == 0 {
            m = 1;
            k += 1;
        }
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.missed_pending = timer.missed_pending.saturating_add(m as u32);
            timer.missed_total = timer.missed_total.saturating_add(m);
            timer.next_k = k;
        }
        self.reschedule_after(id, k);
    }

    fn catch_up_or_schedule(&mut self, id: TimerId, now: Instant) {
        let fixed_rate = self
            .timers
            .get(&id)
            .is_some_and(|timer| timer.pacing == Pacing::FixedRate);
        let exponential = self
            .timers
            .get(&id)
            .is_some_and(|timer| matches!(timer.spec.randomness, Randomness::Exponential { .. }));
        if !fixed_rate || exponential {
            let deadline = self
                .timers
                .get(&id)
                .map(|timer| timer.deadline)
                .unwrap_or(now);
            if deadline <= now {
                self.emit(id, deadline, now, 0, 0, false, 1, true);
            } else {
                let k = self.timers.get(&id).map(|timer| timer.next_k).unwrap_or(0);
                self.reschedule(id, deadline, k);
            }
            return;
        }
        self.fire_fixed_rate(id, now);
    }

    fn ideal(&self, id: TimerId, k: u64) -> Option<Instant> {
        let timer = self.timers.get(&id)?;
        if matches!(timer.spec.randomness, Randomness::Exponential { .. }) {
            return timer.planned_at.get(&k).copied().or(Some(timer.anchor));
        }
        if timer.period.is_zero() {
            return Some(timer.anchor);
        }
        timer
            .anchor
            .checked_add(timer.period.saturating_mul(k as u32))
    }

    fn planned(&self, id: TimerId, k: u64) -> Option<Instant> {
        if let Some(at) = self.timers.get(&id)?.planned_at.get(&k) {
            return Some(*at);
        }
        self.ideal(id, k)
    }

    fn ensure_planned(&mut self, id: TimerId, k: u64) -> Option<Instant> {
        if self.timers.get(&id)?.planned_at.contains_key(&k) {
            return self.timers.get(&id)?.planned_at.get(&k).copied();
        }
        let mut n = 0u64;
        while n <= k {
            if self.timers.get(&id)?.planned_at.contains_key(&n) {
                n += 1;
                continue;
            }
            let planned = self.compute_planned(id, n)?;
            self.timers.get_mut(&id)?.planned_at.insert(n, planned);
            n += 1;
        }
        self.timers.get(&id)?.planned_at.get(&k).copied()
    }

    fn compute_planned(&mut self, id: TimerId, k: u64) -> Option<Instant> {
        let randomness = self.timers.get(&id)?.spec.randomness.clone();
        let anchor = self.timers.get(&id)?.anchor;
        let period = self.timers.get(&id)?.period;
        let spread = self.timers.get(&id)?.spread;
        let base = if let Randomness::Exponential { mean, floor, cap } = randomness {
            if k == 0 {
                anchor
            } else {
                let prev = self.timers.get(&id)?.planned_at.get(&(k - 1)).copied()?;
                let gap = exponential_gap(self.next_unit(), mean, floor, cap);
                prev + gap
            }
        } else if period.is_zero() {
            anchor
        } else {
            anchor.checked_add(period.saturating_mul(k as u32))?
        };
        let jitter = jitter_ms(&randomness, self.next_unit());
        let spread_ms = if k == 0 { spread.as_millis() as i64 } else { 0 };
        Some(shift_ms(base, jitter.saturating_add(spread_ms)))
    }

    fn clamp_planned(&self, planned: Instant, ideal: Instant, now: Instant) -> Instant {
        if planned < now && ideal > now {
            now
        } else {
            planned
        }
    }

    fn next_unit(&mut self) -> f64 {
        self.rng_draws = self.rng_draws.saturating_add(1);
        if !self.rng_fixed
            && (self.rng_draws % (1 << 20) == 0
                || self.rng_reseed.elapsed() >= Duration::from_secs(3600))
        {
            self.rng = StdRng::from_entropy();
            self.rng_draws = 1;
            self.rng_reseed = Instant::now();
        }
        let unit: f64 = self.rng.r#gen();
        unit.clamp(f64::MIN_POSITIVE, 1.0)
    }

    fn draw_spread(&mut self, spec: &TimerSpec) -> Duration {
        let max = match spec.randomness {
            Randomness::Spread { max }
            | Randomness::JitterAndSpread {
                spread_max: max, ..
            } => max,
            _ => return Duration::ZERO,
        };
        let unit = self.next_unit();
        Duration::from_secs_f64(max.as_secs_f64() * unit)
    }

    fn take_token(&mut self, owner: &str, now: Instant) -> bool {
        let rate = f64::from(self.config.max_fires_per_sec_per_module);
        if rate <= 0.0 {
            return false;
        }
        let bucket = self
            .buckets
            .entry(owner.to_string())
            .or_insert(TokenBucket {
                tokens: rate,
                updated: now,
            });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(rate);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn would_be_last(&self, id: TimerId, additional: u32) -> bool {
        let Some(timer) = self.timers.get(&id) else {
            return true;
        };
        let hits = timer
            .spec
            .repeat
            .max_fires_hit(timer.dispatched.saturating_add(additional));
        let until = timer.until;
        let next_k = timer.next_k;
        let _ = timer;
        if hits {
            return true;
        }
        if let (Some(until), Some(next)) = (
            until,
            self.planned(id, next_k.saturating_add(u64::from(additional))),
        ) {
            if next > until {
                return true;
            }
        }
        false
    }

    fn too_late(&self, id: TimerId, now: Instant, planned: Instant) -> bool {
        let Some(max) = self.timers.get(&id).and_then(|t| t.spec.max_lateness) else {
            return false;
        };
        now.saturating_duration_since(planned) > max
    }

    fn drop_late(&mut self, id: TimerId) {
        if let Some(owner) = self.timers.get(&id).map(|timer| timer.owner.clone()) {
            self.metrics.record_fire(&owner, "dropped_late");
        }
        let once = self
            .timers
            .get(&id)
            .is_some_and(|t| matches!(t.spec.repeat, Repeat::Once));
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.missed_total = timer.missed_total.saturating_add(1);
            if once {
                timer.state = TimerState::Expired;
            }
        }
    }

    fn reschedule_after(&mut self, id: TimerId, k: u64) {
        let Some(when) = self.ensure_planned(id, k) else {
            return;
        };
        if self
            .timers
            .get(&id)
            .and_then(|t| t.until)
            .is_some_and(|until| when > until)
        {
            if let Some(timer) = self.timers.get_mut(&id) {
                if timer.state == TimerState::Active {
                    timer.state = TimerState::Expired;
                }
            }
            return;
        }
        if self.timers.get(&id).is_some_and(|t| {
            t.spec.repeat.max_fires_hit(t.dispatched) && t.state != TimerState::Detached
        }) {
            return;
        }
        self.reschedule(id, when, k);
    }

    fn reschedule(&mut self, id: TimerId, when: Instant, k: u64) {
        let pushed = {
            let Some(timer) = self.timers.get_mut(&id) else {
                return;
            };
            if !matches!(timer.state, TimerState::Active | TimerState::Detached) {
                return;
            }
            timer.deadline = when;
            timer.next_k = k;
            timer.generation = timer.generation.wrapping_add(1);
            (timer.creation_seq, timer.generation)
        };
        self.stale += 1;
        self.push(when, pushed.0, id, pushed.1, DueKind::Grid);
        self.maybe_compact();
    }

    fn emit(
        &mut self,
        id: TimerId,
        scheduled: Instant,
        planned: Instant,
        missed: u32,
        coalesced: u32,
        is_last: bool,
        attempt: u32,
        count_fire: bool,
    ) -> bool {
        let Some(timer) = self.timers.get(&id) else {
            return false;
        };
        if timer.state != TimerState::Active {
            return false;
        }
        let owner = timer.owner.clone();
        let module_id = match self.attached.get(&owner) {
            Some(id) => id.clone(),
            None => return false,
        };
        let overlap = timer.spec.overlap;
        let in_flight = timer.in_flight;
        let module_in_flight = self.inflight.get(&owner).copied().unwrap_or(0);
        if module_in_flight >= self.config.max_in_flight_per_module {
            self.inflight_cap_hit = self.inflight_cap_hit.saturating_add(1);
            self.metrics.record_fire(&owner, "inflight_cap");
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.skipped_overlap = timer.skipped_overlap.saturating_add(1);
            }
            return false;
        }
        if attempt == 1 {
            match overlap {
                Overlap::Skip if in_flight >= 1 => {
                    self.note_skip(id);
                    return false;
                }
                Overlap::Allow { max_concurrent } if in_flight >= max_concurrent => {
                    self.note_skip(id);
                    return false;
                }
                Overlap::Queue { max } if in_flight >= 1 => {
                    let queued = self.timers.get(&id).map(|t| t.queue.len()).unwrap_or(0);
                    let module_queued: usize = self
                        .timers
                        .values()
                        .filter(|t| t.owner == owner)
                        .map(|t| t.queue.len())
                        .sum();
                    if queued < max as usize
                        && module_queued < self.config.max_queued_per_module as usize
                    {
                        if let Some(timer) = self.timers.get_mut(&id) {
                            timer.queue.push_back(QueuedFire {
                                planned,
                                missed,
                                is_last,
                            });
                        }
                    } else {
                        self.note_skip(id);
                    }
                    return false;
                }
                Overlap::Coalesce if in_flight >= 1 => {
                    if let Some(timer) = self.timers.get_mut(&id) {
                        timer.coalesce_pending = timer.coalesce_pending.saturating_add(1);
                    }
                    return false;
                }
                _ => {}
            }
        }
        if attempt == 1 && !self.take_token(&owner, Instant::now()) {
            self.metrics.record_fire(&owner, "skipped_rate");
            return false;
        }
        let (seq, skipped) = {
            let Some(timer) = self.timers.get_mut(&id) else {
                return false;
            };
            let seq = if attempt == 1 {
                let seq = timer.next_seq;
                timer.next_seq = timer.next_seq.saturating_add(1);
                if count_fire {
                    timer.dispatched = timer.dispatched.saturating_add(1);
                }
                seq
            } else {
                timer
                    .retry_seqs
                    .back()
                    .copied()
                    .unwrap_or(timer.next_seq.saturating_sub(1).max(1))
            };
            let skipped = std::mem::take(&mut timer.skipped_overlap_pending);
            let missed = missed.saturating_add(std::mem::take(&mut timer.missed_pending));
            timer.in_flight = timer.in_flight.saturating_add(1);
            timer.outstanding.insert(seq);
            (seq, (skipped, missed))
        };
        *self.inflight.entry(owner).or_insert(0) += 1;
        let fire = self.fire_from(
            id, scheduled, planned, skipped.1, coalesced, is_last, seq, attempt,
        );
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.clock_step_ms = 0;
            timer.unix_scheduled = None;
            timer.unix_planned = None;
            timer.unix_fired = None;
        }
        let mut fire = fire;
        fire.skipped_overlap = skipped.0;
        self.metrics.record_lateness(u64::from(fire.lateness_ms));
        if fire.lateness_ms >= 1000 {
            let now = Instant::now();
            let due = match self.late_warn.get(&id) {
                Some(at) => now.saturating_duration_since(*at) >= Duration::from_secs(60),
                None => true,
            };
            if due {
                warn!(
                    timer = %id,
                    lateness_ms = fire.lateness_ms,
                    "timer fire is more than one second late"
                );
                self.late_warn.insert(id, now);
            }
        }
        self.sent_at.insert((id, seq), Instant::now());
        trace!(timer = %id, seq, "timer fire");
        self.touch_durable(id);
        if attempt == 1 {
            self.audit(id, JournalEvent::Fired(seq));
        }
        self.spawn_fire(id, fire);
        true
    }

    fn note_skip(&mut self, id: TimerId) {
        let owner = {
            let Some(timer) = self.timers.get_mut(&id) else {
                return;
            };
            timer.skipped_overlap = timer.skipped_overlap.saturating_add(1);
            timer.skipped_overlap_pending = timer.skipped_overlap_pending.saturating_add(1);
            timer.owner.clone()
        };
        self.metrics.record_fire(&owner, "skipped_overlap");
    }

    fn fire_from(
        &self,
        id: TimerId,
        scheduled: Instant,
        planned: Instant,
        missed: u32,
        coalesced: u32,
        is_last: bool,
        seq: u64,
        attempt: u32,
    ) -> TimerFire {
        let now = Instant::now();
        let timer = &self.timers[&id];
        let scheduled_unix_ms = timer
            .unix_scheduled
            .unwrap_or_else(|| self.to_unix(scheduled));
        let planned_unix_ms = timer.unix_planned.unwrap_or_else(|| self.to_unix(planned));
        let fired_unix_ms = timer.unix_fired.unwrap_or_else(|| self.to_unix(now));
        TimerFire {
            timer_id: id,
            key: timer.spec.key.clone(),
            payload: timer.spec.payload.clone(),
            seq,
            attempt,
            clock: timer.spec.clock,
            scheduled_unix_ms,
            planned_unix_ms,
            fired_unix_ms,
            lateness_ms: fired_unix_ms
                .saturating_sub(planned_unix_ms)
                .min(u64::from(u32::MAX)) as u32,
            missed,
            coalesced,
            skipped_overlap: 0,
            clock_step_ms: timer.clock_step_ms,
            tip_height: self.tip_height,
            tip_hash: self.tip_hash,
            tip_mtp: self.tip_mtp,
            is_last,
        }
    }

    fn spawn_fire(&self, id: TimerId, fire: TimerFire) {
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        let module_id = match self.attached.get(&timer.owner) {
            Some(module_id) => module_id.clone(),
            None => return,
        };
        let timeout = timer.spec.delivery.ack_timeout;
        let seq = fire.seq;
        let attempt = fire.attempt;
        let transport = Arc::clone(&self.transport);
        let ack_tx = self.ack_tx.clone();
        tokio::spawn(async move {
            let transport = transport
                .read()
                .map(|guard| Arc::clone(&*guard))
                .unwrap_or_else(|_| Arc::new(ClosedTransport));
            let outcome = match transport.invoke(&module_id, fire, timeout).await {
                Ok(ack) => AckOutcome::Ok(ack),
                Err(TransportError::Timeout) => AckOutcome::TimedOut,
                Err(TransportError::Closed) => {
                    AckOutcome::Failed("module connection closed".into())
                }
                Err(TransportError::Failed(message)) => AckOutcome::Failed(message),
            };
            let _ = ack_tx.send(AckEvent {
                id,
                seq,
                attempt,
                outcome,
            });
        });
    }

    fn on_ack(&mut self, event: AckEvent) {
        let Some(timer) = self.timers.get(&event.id) else {
            return;
        };
        if !timer.outstanding.contains(&event.seq) {
            debug!(timer = %event.id, seq = event.seq, "late timer acknowledgement ignored");
            return;
        }
        let owner = timer.owner.clone();
        if let Some(timer) = self.timers.get_mut(&event.id) {
            timer.outstanding.remove(&event.seq);
            timer.in_flight = timer.in_flight.saturating_sub(1);
        }
        if let Some(count) = self.inflight.get_mut(&owner) {
            *count = count.saturating_sub(1);
        }
        if let Some(sent) = self.sent_at.remove(&(event.id, event.seq)) {
            self.metrics.record_ack(sent.elapsed().as_millis() as u64);
        }
        match event.outcome {
            AckOutcome::Ok(TimerAck::Continue) => {
                if let Some(timer) = self.timers.get_mut(&event.id) {
                    timer.consecutive_failures = 0;
                    timer.fires_ok = timer.fires_ok.saturating_add(1);
                }
                self.metrics.record_fire(&owner, "ok");
                self.audit(event.id, JournalEvent::Acked(event.seq));
                self.after_completion(event.id);
            }
            AckOutcome::Ok(TimerAck::Stop) => {
                if let Some(timer) = self.timers.get_mut(&event.id) {
                    timer.fires_ok = timer.fires_ok.saturating_add(1);
                }
                self.metrics.record_fire(&owner, "ok");
                self.audit(event.id, JournalEvent::Acked(event.seq));
                info!(timer = %event.id, "timer stopped by handler");
                self.remove_timer(event.id);
            }
            AckOutcome::Ok(TimerAck::Snooze(delay)) => {
                if let Some(timer) = self.timers.get_mut(&event.id) {
                    timer.consecutive_failures = 0;
                    timer.fires_ok = timer.fires_ok.saturating_add(1);
                }
                self.metrics.record_fire(&owner, "ok");
                self.audit(event.id, JournalEvent::Acked(event.seq));
                self.snooze(event.id, delay);
            }
            AckOutcome::Failed(message) => {
                debug!(timer = %event.id, %message, "timer fire failed");
                self.metrics.record_fire(&owner, "failed");
                self.audit(event.id, JournalEvent::Failed(event.seq));
                self.on_failure(event.id, event.seq, event.attempt, false);
            }
            AckOutcome::TimedOut => {
                self.metrics.record_fire(&owner, "timed_out");
                self.audit(event.id, JournalEvent::Failed(event.seq));
                self.on_failure(event.id, event.seq, event.attempt, true);
            }
        }
        self.flush_durable();
    }

    fn after_completion(&mut self, id: TimerId) {
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        if let Some(queued) = self.timers.get_mut(&id).and_then(|t| t.queue.pop_front()) {
            self.emit(
                id,
                queued.planned,
                queued.planned,
                queued.missed,
                0,
                queued.is_last,
                1,
                true,
            );
            return;
        }
        let coalesced = self
            .timers
            .get(&id)
            .map(|t| t.coalesce_pending)
            .unwrap_or(0);
        if coalesced > 0 {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.coalesce_pending = 0;
            }
            let now = Instant::now();
            self.emit(id, now, now, 0, coalesced, false, 1, true);
            return;
        }
        let pacing = self
            .timers
            .get(&id)
            .map(|t| t.pacing)
            .unwrap_or(Pacing::FixedRate);
        let done = self.timers.get(&id).is_some_and(|t| {
            t.state == TimerState::Done || t.spec.repeat.max_fires_hit(t.dispatched)
        });
        if done {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Done;
            }
            return;
        }
        if pacing == Pacing::FixedDelay {
            let now = Instant::now();
            let period = self
                .timers
                .get(&id)
                .map(|t| t.period)
                .unwrap_or(Duration::ZERO);
            let when = now + period;
            if self
                .timers
                .get(&id)
                .and_then(|t| t.until)
                .is_some_and(|until| when > until)
            {
                if let Some(timer) = self.timers.get_mut(&id) {
                    timer.state = TimerState::Expired;
                }
                return;
            }
            let k = self
                .timers
                .get(&id)
                .map(|t| t.next_k.saturating_add(1))
                .unwrap_or(1);
            self.reschedule(id, when, k);
        }
    }

    fn on_failure(&mut self, id: TimerId, seq: u64, attempt: u32, timed_out: bool) {
        enum Step {
            Break {
                failures: u32,
            },
            Retry {
                when: Instant,
                owner: String,
                creation: u64,
                generation: u64,
            },
            Continue,
        }
        let step = {
            let Some(timer) = self.timers.get_mut(&id) else {
                return;
            };
            if timed_out {
                timer.fires_timed_out = timer.fires_timed_out.saturating_add(1);
            } else {
                timer.fires_failed = timer.fires_failed.saturating_add(1);
            }
            timer.consecutive_failures = timer.consecutive_failures.saturating_add(1);
            let failures = timer.consecutive_failures;
            let limit = timer.spec.delivery.max_consecutive_failures;
            let on_failure = timer.spec.delivery.on_failure.clone();
            if limit > 0 && failures >= limit {
                timer.state = TimerState::PausedByBreaker;
                timer.generation = timer.generation.wrapping_add(1);
                Step::Break { failures }
            } else if let OnFailure::Retry { max, backoff } = on_failure {
                if attempt <= max {
                    let when = Instant::now() + backoff_delay(&backoff, attempt);
                    timer.in_flight = timer.in_flight.saturating_add(1);
                    timer.outstanding.insert(seq);
                    timer.retry_seqs.push_back(seq);
                    Step::Retry {
                        when,
                        owner: timer.owner.clone(),
                        creation: timer.creation_seq,
                        generation: timer.generation,
                    }
                } else {
                    Step::Continue
                }
            } else {
                Step::Continue
            }
        };
        match step {
            Step::Break { failures } => {
                self.stale += 1;
                if let Some(owner) = self.timers.get(&id).map(|timer| timer.owner.clone()) {
                    self.metrics.record_breaker(&owner);
                }
                warn!(timer = %id, failures, "timer paused by breaker");
            }
            Step::Retry {
                when,
                owner,
                creation,
                generation,
            } => {
                *self.inflight.entry(owner).or_insert(0) += 1;
                self.push(when, creation, id, generation, DueKind::Retry);
            }
            Step::Continue => self.after_completion(id),
        }
    }

    fn snooze(&mut self, id: TimerId, delay: Duration) {
        let when = Instant::now() + delay;
        let pushed = {
            let Some(timer) = self.timers.get_mut(&id) else {
                return;
            };
            timer.anchor = when;
            timer.next_k = 0;
            timer.state = TimerState::Active;
            timer.generation = timer.generation.wrapping_add(1);
            timer.deadline = when;
            (timer.creation_seq, timer.generation)
        };
        self.stale += 1;
        self.push(when, pushed.0, id, pushed.1, DueKind::Grid);
        self.maybe_compact();
    }

    fn remove_timer(&mut self, id: TimerId) {
        self.audit(id, JournalEvent::Cancelled);
        self.drop_timer(id);
    }

    fn drop_timer(&mut self, id: TimerId) {
        let Some(timer) = self.timers.remove(&id) else {
            return;
        };
        if matches!(timer.spec.persist, Persist::Durable { .. }) {
            self.delete_record(&timer.owner, timer.spec.key.as_deref());
        }
        self.metrics.add_active(&timer.owner, -1);
        if let Some(key) = &timer.spec.key {
            self.keys.remove(&(timer.owner.clone(), key.clone()));
        }
        if let Some(count) = self.counts.get_mut(&timer.owner) {
            *count = count.saturating_sub(1);
        }
        if matches!(timer.spec.persist, Persist::Durable { .. }) {
            if let Some(count) = self.durable_counts.get_mut(&timer.owner) {
                *count = count.saturating_sub(1);
            }
        }
        if let Some(in_flight) = self.inflight.get_mut(&timer.owner) {
            *in_flight = in_flight.saturating_sub(timer.in_flight);
        }
        self.stale += 1;
        self.maybe_compact();
    }

    fn push(&mut self, when: Instant, creation: u64, id: TimerId, generation: u64, kind: DueKind) {
        self.heap
            .push(std::cmp::Reverse((when, creation, id, generation, kind)));
    }

    fn maybe_compact(&mut self) {
        if self.stale > self.timers.len().saturating_mul(2).saturating_add(32) {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut live = Vec::new();
        while let Some(std::cmp::Reverse((when, creation, id, generation, kind))) = self.heap.pop()
        {
            if self.is_live(id, generation) {
                live.push(std::cmp::Reverse((when, creation, id, generation, kind)));
            }
        }
        self.heap = std::collections::BinaryHeap::from(live);
        self.stale = 0;
    }

    fn info(&self, id: TimerId) -> TimerInfo {
        let timer = &self.timers[&id];
        let next_due_unix_ms = match timer.state {
            TimerState::Active | TimerState::Detached => Some(self.to_unix(timer.deadline)),
            _ => None,
        };
        TimerInfo {
            timer_id: id,
            key: timer.spec.key.clone(),
            spec: timer.spec.clone(),
            state: timer.state,
            seq: timer.next_seq.saturating_sub(1),
            in_flight: timer.in_flight,
            queued: timer.queue.len() as u32 + timer.coalesce_pending,
            next_due_unix_ms,
            created_unix_ms: timer.created_unix_ms,
            fires_ok: timer.fires_ok,
            fires_failed: timer.fires_failed,
            fires_timed_out: timer.fires_timed_out,
            skipped_overlap_total: timer.skipped_overlap,
            missed_total: timer.missed_total,
        }
    }

    fn to_unix(&self, when: Instant) -> u64 {
        let delta = when.saturating_duration_since(self.epoch);
        self.epoch_unix_ms.saturating_add(delta.as_millis() as u64)
    }

    fn observe_wall(&mut self) {
        let wall = self.wall.unix_ms();
        let elapsed = self.mono_base.elapsed().as_millis() as u64;
        let expected = self.wall_base_ms.saturating_add(elapsed);
        let step = wall as i64 - expected as i64;
        self.expire_detached();
        if step.unsigned_abs() < self.config.clock_step_threshold_ms {
            return;
        }
        self.wall_base_ms = wall;
        self.mono_base = Instant::now();
        self.apply_wall_step(step);
    }

    fn apply_wall_step(&mut self, step_ms: i64) {
        let ids: Vec<TimerId> = self
            .timers
            .values()
            .filter(|timer| timer.spec.clock == Clock::Wall && timer.state == TimerState::Active)
            .map(|timer| timer.id)
            .collect();
        let now = Instant::now();
        for id in ids {
            let Some(timer) = self.timers.get_mut(&id) else {
                continue;
            };
            timer.clock_step_ms = step_ms;
            if step_ms > 0 {
                timer.generation = timer.generation.wrapping_add(1);
                timer.deadline = now;
                let seq = timer.creation_seq;
                let generation = timer.generation;
                self.stale += 1;
                self.push(now, seq, id, generation, DueKind::Grid);
            } else {
                let delta = Duration::from_millis(step_ms.unsigned_abs());
                timer.anchor += delta;
                timer.deadline += delta;
                timer.anchor_unix_ms = timer.anchor_unix_ms.saturating_add(step_ms.unsigned_abs());
                for planned in timer.planned_at.values_mut() {
                    *planned += delta;
                }
                timer.generation = timer.generation.wrapping_add(1);
                let when = timer.deadline;
                let seq = timer.creation_seq;
                let generation = timer.generation;
                self.stale += 1;
                self.push(when, seq, id, generation, DueKind::Grid);
            }
        }
    }

    fn fire_wall(&mut self, id: TimerId, now: Instant) {
        let wall_now = self.wall.unix_ms();
        let (start_k, period_ms, anchor_ms, once) = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            (
                timer.next_k,
                timer.wall_period_ms,
                timer.anchor_unix_ms,
                matches!(timer.spec.repeat, Repeat::Once),
            )
        };
        let target = anchor_ms.saturating_add(start_k.saturating_mul(period_ms));
        if wall_now.saturating_add(5) < target {
            self.reschedule_wall(id, start_k, wall_now);
            return;
        }
        if once || period_ms == 0 {
            if wall_now < anchor_ms {
                return;
            }
            self.stamp_unix(id, anchor_ms, wall_now);
            let is_last = true;
            self.emit(id, now, now, 0, 0, is_last, 1, true);
            if let Some(timer) = self.timers.get_mut(&id) {
                if timer.in_flight == 0 {
                    timer.state = TimerState::Done;
                }
            }
            return;
        }
        let mut k = start_k;
        let mut m = 0u64;
        while anchor_ms.saturating_add(k.saturating_mul(period_ms)) <= wall_now {
            m += 1;
            k += 1;
            if m > 1_000_000 {
                break;
            }
        }
        if m == 0 {
            return;
        }
        let policy = self
            .timers
            .get(&id)
            .map(|timer| timer.spec.missed)
            .unwrap_or(Missed::Skip);
        let first_unix = anchor_ms.saturating_add(start_k.saturating_mul(period_ms));
        if m == 1 || matches!(policy, Missed::FireOnce) {
            let missed = if m == 1 { 0 } else { (m - 1) as u32 };
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.next_k = k;
                if m > 1 {
                    timer.missed_total = timer.missed_total.saturating_add(u64::from(missed));
                }
            }
            self.stamp_unix(id, first_unix, wall_now);
            let is_last = self.would_be_last(id, 1);
            self.emit(id, now, now, missed, 0, is_last, 1, true);
            if !is_last {
                self.reschedule_wall(id, k, wall_now);
            } else if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Done;
            }
            return;
        }
        if let Missed::Skip = policy {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.missed_pending = timer.missed_pending.saturating_add(m as u32);
                timer.missed_total = timer.missed_total.saturating_add(m);
                timer.next_k = k;
            }
            self.reschedule_wall(id, k, wall_now);
            return;
        }
        let max = match policy {
            Missed::CatchUp { max } => u64::from(max.max(1)),
            _ => 1,
        };
        let fires = m.min(max);
        for index in 0..fires {
            let slot = start_k + index;
            let slot_unix = anchor_ms.saturating_add(slot.saturating_mul(period_ms));
            let missed = if index + 1 == fires {
                (m - fires) as u32
            } else {
                0
            };
            self.stamp_unix(id, slot_unix, wall_now);
            let is_last = self.would_be_last(id, 1);
            if !self.emit(id, now, now, missed, 0, is_last, 1, true) {
                break;
            }
        }
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.next_k = k;
            timer.missed_total = timer.missed_total.saturating_add(m.saturating_sub(fires));
        }
        self.reschedule_wall(id, k, wall_now);
    }

    fn stamp_unix(&mut self, id: TimerId, scheduled: u64, fired: u64) {
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.unix_scheduled = Some(scheduled);
            timer.unix_planned = Some(scheduled);
            timer.unix_fired = Some(fired);
        }
    }

    fn reschedule_wall(&mut self, id: TimerId, k: u64, wall_now: u64) {
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        let next_unix = timer
            .anchor_unix_ms
            .saturating_add(k.saturating_mul(timer.wall_period_ms));
        let wait = next_unix.saturating_sub(wall_now);
        self.reschedule(id, Instant::now() + Duration::from_millis(wait), k);
    }

    fn on_chain(&mut self, event: ChainEvent) {
        match event {
            ChainEvent::Connected { height, hash, mtp } => {
                self.tip_height = Some(height);
                self.tip_hash = Some(hash);
                self.tip_mtp = Some(mtp);
                let ids = self.chain_ids();
                for id in ids {
                    self.eval_chain_timer(id, Some(height), Some(mtp));
                }
            }
            ChainEvent::Disconnected { height, .. } => {
                if self.tip_height == Some(height) {
                    self.tip_height = Some(height.saturating_sub(1));
                    self.tip_hash = None;
                }
                let ids = self.chain_ids();
                for id in ids {
                    self.refire_if_disconnected(id, height);
                }
            }
        }
        self.flush_durable();
    }

    fn chain_ids(&self) -> Vec<TimerId> {
        self.timers
            .values()
            .filter(|timer| timer.spec.clock == Clock::Chain)
            .map(|timer| timer.id)
            .collect()
    }

    fn eval_chain_timer(&mut self, id: TimerId, height: Option<u64>, mtp: Option<u64>) {
        let Some(height) = height else {
            return;
        };
        let (sched, reorg, state) = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            if timer.spec.clock != Clock::Chain {
                return;
            }
            let Some(sched) = timer.chain.clone() else {
                return;
            };
            (sched, timer.spec.reorg, timer.state)
        };
        if !matches!(state, TimerState::Active | TimerState::Detached) {
            return;
        }
        if let Some(target) = sched.mtp_target {
            let Some(mtp) = mtp else {
                return;
            };
            if mtp < target || sched.last_height.is_some() {
                return;
            }
            self.fire_chain(id, height, 0, sched.once);
            return;
        }
        if let Some(every) = sched.every {
            if height < sched.next_height {
                return;
            }
            let mut cursor = sched.next_height;
            let mut crossings = 0u64;
            let step = u64::from(every.max(1));
            while cursor <= height && crossings < 100_000 {
                crossings += 1;
                cursor = cursor.saturating_add(step);
            }
            self.finish_chain_crossings(id, height, crossings, cursor, sched.once);
            return;
        }
        let Some(target) = sched.height_target else {
            return;
        };
        if height < target || sched.last_height.is_some() {
            return;
        }
        let missed = height.saturating_sub(target).min(u64::from(u32::MAX)) as u32;
        let _ = reorg;
        self.fire_chain(id, height, missed, true);
    }

    fn finish_chain_crossings(
        &mut self,
        id: TimerId,
        height: u64,
        crossings: u64,
        next_height: u64,
        once: bool,
    ) {
        if crossings == 0 {
            return;
        }
        let policy = self
            .timers
            .get(&id)
            .map(|timer| timer.spec.missed)
            .unwrap_or(Missed::Skip);
        if let Some(timer) = self.timers.get_mut(&id) {
            if let Some(sched) = timer.chain.as_mut() {
                sched.next_height = next_height;
            }
        }
        if crossings > 1 && matches!(policy, Missed::Skip) {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.missed_pending = timer.missed_pending.saturating_add(crossings as u32);
                timer.missed_total = timer.missed_total.saturating_add(crossings);
            }
            return;
        }
        let missed = if crossings == 1 {
            0
        } else {
            (crossings - 1) as u32
        };
        self.fire_chain(id, height, missed, once);
    }

    fn fire_chain(&mut self, id: TimerId, height: u64, missed: u32, is_last: bool) {
        if self
            .timers
            .get(&id)
            .is_some_and(|timer| timer.state == TimerState::Detached)
        {
            if let Some(timer) = self.timers.get_mut(&id) {
                if let Some(sched) = timer.chain.as_mut() {
                    sched.last_height = Some(height);
                }
                timer.missed_pending = timer.missed_pending.saturating_add(missed.max(1));
                timer.missed_total = timer.missed_total.saturating_add(u64::from(missed.max(1)));
            }
            self.touch_durable(id);
            return;
        }
        let mtp_ms = self.tip_mtp.unwrap_or(0).saturating_mul(1000);
        self.stamp_unix(id, mtp_ms, mtp_ms);
        if let Some(timer) = self.timers.get_mut(&id) {
            if let Some(sched) = timer.chain.as_mut() {
                sched.last_height = Some(height);
            }
            if missed > 0 {
                timer.missed_total = timer.missed_total.saturating_add(u64::from(missed));
            }
        }
        let now = Instant::now();
        self.emit(id, now, now, missed, 0, is_last, 1, true);
        if is_last {
            if let Some(timer) = self.timers.get_mut(&id) {
                if timer.in_flight == 0 {
                    timer.state = TimerState::Done;
                }
            }
        }
    }

    fn refire_if_disconnected(&mut self, id: TimerId, height: u64) {
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        if timer.spec.reorg != ReorgPolicy::Refire {
            return;
        }
        let last = timer.chain.as_ref().and_then(|sched| sched.last_height);
        if last.is_none_or(|fired| height > fired) {
            return;
        }
        let Some(timer) = self.timers.get_mut(&id) else {
            return;
        };
        if let Some(sched) = timer.chain.as_mut() {
            let fired = sched.last_height;
            sched.last_height = None;
            if let Some(every) = sched.every {
                sched.next_height = height;
                let _ = every;
            } else if let Some(fired) = fired {
                sched.height_target = Some(fired);
            }
        }
        if timer.state == TimerState::Done {
            timer.state = TimerState::Active;
        }
    }

    fn touch_durable(&mut self, id: TimerId) {
        let durable = self
            .timers
            .get(&id)
            .is_some_and(|timer| matches!(timer.spec.persist, Persist::Durable { .. }));
        if !durable || self.store.is_none() || self.durable_dirty.contains(&id) {
            return;
        }
        self.durable_dirty.push(id);
    }

    fn flush_durable(&mut self) {
        let ids = std::mem::take(&mut self.durable_dirty);
        for id in ids {
            self.persist_one(id);
        }
    }

    fn persist_one(&mut self, id: TimerId) {
        let last = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            if !matches!(timer.spec.persist, Persist::Durable { .. }) {
                return;
            }
            timer.last_persist
        };
        let now = Instant::now();
        if let Some(last) = last {
            if now.saturating_duration_since(last) < Duration::from_secs(1) {
                self.schedule_persist(id, last + Duration::from_secs(1));
                return;
            }
        }
        self.write_record(id);
    }

    fn schedule_persist(&mut self, id: TimerId, when: Instant) {
        let Some(timer) = self.timers.get_mut(&id) else {
            return;
        };
        if timer.persist_scheduled {
            return;
        }
        timer.persist_scheduled = true;
        let creation = timer.creation_seq;
        let generation = timer.generation;
        self.push(when, creation, id, generation, DueKind::Persist);
    }

    fn write_record(&mut self, id: TimerId) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        let Some(key) = timer.spec.key.clone() else {
            return;
        };
        let deadline = timer.deadline;
        let saved_id = timer.id;
        let spec = timer.spec.clone();
        let seq = timer.next_seq.saturating_sub(1);
        let state = timer.state;
        let next_due_height = timer.chain.as_ref().map(|sched| sched.next_height);
        let fires_remaining = interval_max_fires_of(&timer.spec.repeat)
            .map(|max| max.saturating_sub(timer.dispatched));
        let created_unix_ms = timer.created_unix_ms;
        let owner = timer.owner.clone();
        let saved = PersistedTimer {
            id: saved_id,
            spec,
            seq,
            state,
            next_due_unix_ms: Some(self.to_unix(deadline)),
            next_due_height,
            fires_remaining,
            created_unix_ms,
        };
        let bytes = match persist::encode(&saved) {
            Ok(bytes) => bytes,
            Err(err) => {
                error!(%err, "module_timers encode failed");
                return;
            }
        };
        let record_key = persist::record_key(&owner, &key);
        if let Err(err) = store.insert(record_key.as_bytes(), &bytes) {
            error!(%err, "module_timers insert failed");
            return;
        }
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.last_persist = Some(Instant::now());
        }
    }

    fn audit(&mut self, id: TimerId, event: JournalEvent) {
        let Some(journal) = self.journal.clone() else {
            return;
        };
        let Some(timer) = self.timers.get(&id) else {
            return;
        };
        if !matches!(timer.spec.persist, Persist::Durable { audit: true }) {
            return;
        }
        let module = timer.owner.clone();
        let timer_id = timer.id;
        let key = timer.spec.key.clone();
        let unix_ms = self.wall.unix_ms();
        let prev = self.journal_prev;
        let index = self.journal_next;
        match super::journal::append(
            journal.as_ref(),
            prev,
            index,
            unix_ms,
            &module,
            timer_id,
            key,
            event,
            String::new(),
        ) {
            Ok(record) => {
                self.journal_prev = record.hash;
                self.journal_next = index.saturating_add(1);
            }
            Err(err) => error!(%err, "module_timer_journal append failed"),
        }
    }

    fn load_journal(&mut self) {
        let Some(journal) = self.journal.clone() else {
            return;
        };
        let (prev, next) = super::journal::tip(journal.as_ref());
        self.journal_prev = prev;
        self.journal_next = next;
    }

    #[cfg(feature = "timer-cron")]
    fn fire_cron(&mut self, id: TimerId, now: Instant) {
        let wall_now = self.wall.unix_ms();
        let (expr, cursor) = {
            let Some(timer) = self.timers.get(&id) else {
                return;
            };
            let Repeat::Cron { expr, .. } = &timer.spec.repeat else {
                return;
            };
            (expr.clone(), timer.anchor_unix_ms)
        };
        let slots = super::cron::slots_through(&expr, cursor, wall_now);
        if slots.is_empty() {
            if let Some(next) = super::cron::next_after(&expr, wall_now) {
                self.schedule_cron(id, next, wall_now);
            }
            return;
        }
        let policy = self
            .timers
            .get(&id)
            .map(|timer| timer.spec.missed)
            .unwrap_or(Missed::Skip);
        let m = slots.len();
        let last = *slots.last().unwrap_or(&wall_now);
        if m == 1 || matches!(policy, Missed::FireOnce) {
            let missed = if m == 1 { 0 } else { (m - 1) as u32 };
            self.stamp_unix(id, slots[0], wall_now);
            let is_last = self.would_be_last(id, 1);
            self.emit(id, now, now, missed, 0, is_last, 1, true);
            if !is_last {
                if let Some(next) = super::cron::next_after(&expr, last) {
                    self.schedule_cron(id, next, last);
                }
            } else if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Done;
            }
            return;
        }
        if let Missed::Skip = policy {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.missed_pending = timer.missed_pending.saturating_add(m as u32);
                timer.missed_total = timer.missed_total.saturating_add(m as u64);
            }
            if let Some(next) = super::cron::next_after(&expr, last) {
                self.schedule_cron(id, next, last);
            }
            return;
        }
        let max = match policy {
            Missed::CatchUp { max } => usize::try_from(max.max(1)).unwrap_or(1),
            _ => 1,
        };
        let fires = m.min(max);
        for (index, slot) in slots.iter().take(fires).enumerate() {
            let missed = if index + 1 == fires {
                (m - fires) as u32
            } else {
                0
            };
            self.stamp_unix(id, *slot, wall_now);
            let is_last = self.would_be_last(id, 1);
            if !self.emit(id, now, now, missed, 0, is_last, 1, true) {
                break;
            }
        }
        if let Some(next) = super::cron::next_after(&expr, last) {
            self.schedule_cron(id, next, last);
        }
    }

    #[cfg(feature = "timer-cron")]
    fn schedule_cron(&mut self, id: TimerId, next_unix: u64, cursor: u64) {
        let wall_now = self.wall.unix_ms();
        let when = Instant::now() + Duration::from_millis(next_unix.saturating_sub(wall_now));
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.anchor_unix_ms = cursor;
        }
        let k = self.timers.get(&id).map(|timer| timer.next_k).unwrap_or(0);
        self.reschedule(id, when, k);
    }

    fn delete_record(&self, owner: &str, key: Option<&str>) {
        let (Some(store), Some(key)) = (self.store.clone(), key) else {
            return;
        };
        let record_key = persist::record_key(owner, key);
        if let Err(err) = store.remove(record_key.as_bytes()) {
            error!(%err, "module_timers remove failed");
        }
    }

    fn delete_module(&mut self, name: &str) {
        let ids: Vec<TimerId> = self
            .timers
            .values()
            .filter(|timer| timer.owner == name)
            .map(|timer| timer.id)
            .collect();
        for id in ids {
            self.remove_timer(id);
        }
    }

    fn expire_detached(&mut self) {
        let limit_ms = self.config.durable_max_detached_secs.saturating_mul(1000);
        let now_ms = self.wall.unix_ms();
        let ids: Vec<TimerId> = self
            .timers
            .values()
            .filter(|timer| {
                timer.state == TimerState::Detached
                    && matches!(timer.spec.persist, Persist::Durable { .. })
                    && timer.detached_unix_ms > 0
                    && now_ms.saturating_sub(timer.detached_unix_ms) >= limit_ms
            })
            .map(|timer| timer.id)
            .collect();
        for id in ids {
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.state = TimerState::Expired;
                timer.generation = timer.generation.wrapping_add(1);
            }
            self.touch_durable(id);
            self.audit(id, JournalEvent::Expired);
            info!(timer = %id, "durable timer expired while detached");
        }
    }

    fn load_durable(&mut self) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let rows: Vec<Result<(Vec<u8>, Vec<u8>), anyhow::Error>> = store.iter().collect();
        for row in rows {
            let (key, value) = match row {
                Ok(pair) => pair,
                Err(err) => {
                    error!(%err, "module_timers iter failed");
                    continue;
                }
            };
            let saved = match persist::decode(&value) {
                Ok(saved) => saved,
                Err(err) => {
                    error!(%err, "module_timers skipped a corrupt record");
                    continue;
                }
            };
            let Some((module, timer_key)) = std::str::from_utf8(&key)
                .ok()
                .and_then(|text| text.split_once('/'))
            else {
                error!("module_timers skipped a record with no module key");
                continue;
            };
            if saved.spec.key.as_deref() != Some(timer_key) {
                error!("module_timers skipped a record whose key does not match");
                continue;
            }
            if matches!(saved.state, TimerState::Cancelled) {
                continue;
            }
            self.restore_durable(module.to_string(), saved);
        }
    }

    fn restore_durable(&mut self, owner: String, saved: PersistedTimer) {
        let Some(key) = saved.spec.key.clone() else {
            return;
        };
        if self.keys.contains_key(&(owner.clone(), key.clone())) {
            return;
        }
        let id = saved.id;
        let now = Instant::now();
        let wall_now = self.wall.unix_ms();
        let due = saved.next_due_unix_ms.unwrap_or(wall_now);
        let wait = due.saturating_sub(wall_now);
        let deadline = now + Duration::from_millis(wait);
        let period = interval_period(&saved.spec).unwrap_or(Duration::ZERO);
        let pacing = interval_pacing(&saved.spec).unwrap_or(Pacing::FixedRate);
        let dispatched = match (
            interval_max_fires_of(&saved.spec.repeat),
            saved.fires_remaining,
        ) {
            (Some(max), Some(remaining)) => max.saturating_sub(remaining),
            _ => u32::try_from(saved.seq).unwrap_or(u32::MAX),
        };
        let terminal = matches!(saved.state, TimerState::Done | TimerState::Expired);
        let state = if terminal {
            saved.state
        } else {
            TimerState::Detached
        };
        let creation_seq = self.next_creation;
        self.next_creation = self.next_creation.wrapping_add(1);
        let mut chain = if saved.spec.clock == Clock::Chain {
            Some(chain_sched(&saved.spec, self.tip_height.unwrap_or(0)))
        } else {
            None
        };
        if let Some(height) = saved.next_due_height {
            if let Some(sched) = chain.as_mut() {
                sched.next_height = height;
            }
        }
        self.keys.insert((owner.clone(), key), id);
        *self.counts.entry(owner.clone()).or_insert(0) += 1;
        *self.durable_counts.entry(owner.clone()).or_insert(0) += 1;
        self.metrics.add_active(&owner, 1);
        self.timers.insert(
            id,
            Live {
                id,
                owner,
                spec: saved.spec,
                state,
                generation: 1,
                creation_seq,
                anchor: deadline,
                period,
                pacing,
                next_k: 0,
                deadline,
                dispatched,
                next_seq: saved.seq.saturating_add(1),
                in_flight: 0,
                outstanding: HashSet::new(),
                retry_seqs: VecDeque::new(),
                consecutive_failures: 0,
                fires_ok: saved.seq,
                fires_failed: 0,
                fires_timed_out: 0,
                skipped_overlap: 0,
                missed_total: 0,
                missed_pending: 0,
                skipped_overlap_pending: 0,
                created_unix_ms: saved.created_unix_ms,
                until: None,
                queue: VecDeque::new(),
                coalesce_pending: 0,
                planned_at: HashMap::new(),
                spread: Duration::ZERO,
                anchor_unix_ms: due,
                wall_period_ms: period.as_millis() as u64,
                chain,
                clock_step_ms: 0,
                unix_scheduled: None,
                unix_planned: None,
                unix_fired: None,
                last_persist: None,
                persist_scheduled: false,
                detached_unix_ms: wall_now,
            },
        );
        if !terminal {
            self.push(deadline, creation_seq, id, 1, DueKind::Grid);
        }
    }
}

trait RepeatFires {
    fn max_fires_hit(&self, dispatched: u32) -> bool;
}

impl RepeatFires for Repeat {
    fn max_fires_hit(&self, dispatched: u32) -> bool {
        match interval_max_fires_of(self) {
            Some(max) => dispatched >= max,
            None => false,
        }
    }
}

fn interval_max_fires_of(repeat: &Repeat) -> Option<u32> {
    match repeat {
        Repeat::Once => Some(1),
        Repeat::Interval { max_fires, .. }
        | Repeat::Blocks { max_fires, .. }
        | Repeat::Cron { max_fires, .. } => *max_fires,
    }
}

fn wall_first(spec: &TimerSpec, wall_now: u64, now: Instant) -> (u64, Instant) {
    match spec.first {
        First::Delay(delay) => {
            let ms = delay.as_millis() as u64;
            (wall_now.saturating_add(ms), now + delay)
        }
        First::AtUnixMs(ms) => {
            let wait = ms.saturating_sub(wall_now);
            (ms, now + Duration::from_millis(wait))
        }
        _ => (wall_now, now),
    }
}

fn chain_sched(spec: &TimerSpec, tip: u64) -> ChainSched {
    let once = matches!(spec.repeat, Repeat::Once);
    let (height_target, mtp_target) = match spec.first {
        First::AtHeight { height, depth } => (Some(height.saturating_add(u64::from(depth))), None),
        First::AfterBlocks { blocks, depth } => (
            Some(
                tip.saturating_add(u64::from(blocks))
                    .saturating_add(u64::from(depth)),
            ),
            None,
        ),
        First::AtMtp(mtp) => (None, Some(mtp)),
        _ => (None, None),
    };
    let every = match spec.repeat {
        Repeat::Blocks { every, .. } => Some(every),
        _ => None,
    };
    let next_height = if let Some(every) = every {
        height_target
            .unwrap_or(tip)
            .saturating_add(u64::from(every.max(1)))
            - if height_target.is_some() {
                u64::from(every.max(1))
            } else {
                0
            }
    } else {
        height_target.unwrap_or(u64::MAX)
    };
    let next_height = if every.is_some() && height_target.is_none() {
        tip.saturating_add(u64::from(every.unwrap_or(1).max(1)))
    } else if let Some(target) = height_target {
        target
    } else {
        next_height
    };
    ChainSched {
        height_target,
        mtp_target,
        every,
        next_height,
        last_height: None,
        once,
    }
}

fn first_deadline(spec: &TimerSpec, now: Instant) -> Instant {
    match spec.first {
        First::Now => now,
        First::Delay(delay) => now + delay,
        First::AtUnixMs(_)
        | First::AtHeight { .. }
        | First::AfterBlocks { .. }
        | First::AtMtp(_) => now,
    }
}

fn until_instant(spec: &TimerSpec, now: Instant) -> Option<Instant> {
    let end = match &spec.repeat {
        Repeat::Interval { until, .. } => until.as_ref(),
        _ => None,
    };
    match end {
        Some(End::AfterDuration(d)) => Some(now + *d),
        _ => None,
    }
}

fn jitter_ms(randomness: &Randomness, unit: f64) -> i64 {
    let (max, shape) = match randomness {
        Randomness::Jitter { max, shape }
        | Randomness::JitterAndSpread {
            jitter_max: max,
            shape,
            ..
        } => (*max, *shape),
        _ => return 0,
    };
    let span = max.as_millis() as i64;
    if span <= 0 {
        return 0;
    }
    match shape {
        JitterShape::Positive => (unit * span as f64) as i64,
        JitterShape::Symmetric => ((unit * 2.0 - 1.0) * span as f64) as i64,
    }
}

fn exponential_gap(unit: f64, mean: Duration, floor: Duration, cap: Duration) -> Duration {
    let raw = -mean.as_secs_f64() * unit.ln();
    Duration::from_secs_f64(raw).clamp(floor, cap)
}

fn shift_ms(base: Instant, delta_ms: i64) -> Instant {
    if delta_ms >= 0 {
        base.checked_add(Duration::from_millis(delta_ms as u64))
            .unwrap_or(base)
    } else {
        base.checked_sub(Duration::from_millis(delta_ms.unsigned_abs()))
            .unwrap_or(base)
    }
}

fn backoff_delay(backoff: &super::types::Backoff, attempt: u32) -> Duration {
    let mut delay = backoff.initial;
    for _ in 1..attempt {
        let millis = delay
            .as_millis()
            .saturating_mul(u128::from(backoff.factor_milli))
            / 1000;
        delay = Duration::from_millis(millis.min(u128::from(u64::MAX)) as u64);
        if delay > backoff.max {
            return backoff.max;
        }
    }
    delay.min(backoff.max)
}

fn new_timer_id() -> TimerId {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    TimerId(bytes)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

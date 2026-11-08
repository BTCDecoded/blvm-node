//! Built-in fee estimator.
//!
//! Hooks count events and keep the transactions the projection needs. A quote
//! reads a cached table. Building that table happens off the hook path, on a
//! cloned snapshot. The estimator never calls back into the mempool while
//! holding its own lock.

mod combine;
mod eval_log;
mod external;
mod health;
mod history;
#[cfg(test)]
mod integration;
mod persist;
mod projection;
mod types;

pub use types::{
    BlockObservation, EstimatorSelection, ExternalFeeTable, ExternalMode, FeeEstimatorRegistration,
    FeeEstimatorRow, FeeEstimatorSpec, FeeMode, FeeNameError, FeeQuote, FeeSource, FeeTable,
    FeeUnavailable, HistoryResult, MAX_EXTERNAL_QUOTES, NativeFeeRead, SourceView,
    SubmitFeeTableError, TxAdmission,
};

use crate::config::FeeEstimatorConfig;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, Hash};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Past this age a changed mempool waits for a new table instead of being served stale.
const TABLE_HARD_STALE: Duration = Duration::from_secs(60);

/// What a hook recorded, in call order. Off unless a test asks for the trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HookEvent {
    Admitted {
        txid: Hash,
        from_reload: bool,
        from_package: bool,
    },
    Removed {
        txid: Hash,
    },
    Cleared,
    Block {
        height: u64,
    },
}

/// Totals the hooks have accepted. An ignored block is not a `blocks` increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HookCounts {
    pub admitted: u64,
    pub removed: u64,
    pub cleared: u64,
    pub blocks: u64,
    pub ignored_blocks: u64,
    pub gap_clears: u64,
    /// History-eligible transactions the block observation confirmed.
    pub confirmed: u64,
    /// History-eligible removals that waited at least `CBlockPolicyEstimator::SHORT_SCALE` blocks.
    pub failed: u64,
    /// Admissions flagged `from_reload`.
    pub reloaded: u64,
}

struct TrackedTx {
    fee_sats: u64,
    vsize: u64,
    admit_height: u64,
    parents: Vec<Hash>,
    /// Kept for the history tracker. Projection uses every tracked transaction.
    history_eligible: bool,
}

struct Inner {
    counts: HookCounts,
    best_seen_height: Option<u64>,
    trace: bool,
    events: Vec<HookEvent>,
    tracked: HashMap<Hash, TrackedTx>,
    history: history::HistoryTracker,
    health: health::HealthRing,
    tip_hash: Hash,
    tip_height: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableAction {
    Serve,
    Background,
    Wait,
}

/// Facade the mempool calls after it has released its own locks.
pub struct FeeEstimator {
    cfg: RwLock<FeeEstimatorConfig>,
    relay_floor_sat_per_kvb: AtomicU64,
    network_is_regtest: AtomicBool,
    inner: Mutex<Inner>,
    table: Arc<RwLock<Arc<FeeTable>>>,
    generation: AtomicU64,
    block_seq: AtomicU64,
    recompute_gate: Arc<tokio::sync::Mutex<()>>,
    eval_log_gate: Arc<Mutex<()>>,
    external: external::ExternalRegistry,
    event_publisher: Mutex<Option<Arc<crate::node::event_publisher::EventPublisher>>>,
    last_ready_tip: Arc<Mutex<Option<Hash>>>,
}

impl FeeEstimator {
    pub fn new(cfg: FeeEstimatorConfig, network_is_regtest: bool) -> Self {
        let health = health::HealthRing::new(cfg.health_window_blocks);
        let health_ok = health::mempool_layer_trusted(
            &health,
            cfg.health_min_ratio,
            health::health_required(cfg.health_required, network_is_regtest),
        );
        let table = combine::build_fee_table(combine::MempoolSnapshot {
            txs: Vec::new(),
            health_ok,
            health_window: cfg.health_window_blocks as u64,
            health_ratio: health.aggregate_ratio(),
            health_required_ratio: cfg.health_min_ratio,
            window_full: false,
            tip_hash: [0u8; 32],
            tip_height: 0,
            block_seq: 0,
            generation: 0,
            generated_at: Instant::now(),
            cfg: cfg.clone(),
            relay_floor_sat_per_kvb: 100,
            history: combine::empty_history(),
            requested_estimator: cfg.estimator.clone(),
            active_estimator: "native".to_string(),
            degraded: cfg.estimator != "auto" && cfg.estimator != "native",
            external: None,
        });
        Self {
            cfg: RwLock::new(cfg),
            relay_floor_sat_per_kvb: AtomicU64::new(100),
            network_is_regtest: AtomicBool::new(network_is_regtest),
            inner: Mutex::new(Inner {
                counts: HookCounts {
                    admitted: 0,
                    removed: 0,
                    cleared: 0,
                    blocks: 0,
                    ignored_blocks: 0,
                    gap_clears: 0,
                    confirmed: 0,
                    failed: 0,
                    reloaded: 0,
                },
                best_seen_height: None,
                trace: false,
                events: Vec::new(),
                tracked: HashMap::new(),
                history: history::HistoryTracker::new(),
                health,
                tip_hash: [0u8; 32],
                tip_height: 0,
            }),
            table: Arc::new(RwLock::new(Arc::new(table))),
            generation: AtomicU64::new(0),
            block_seq: AtomicU64::new(0),
            recompute_gate: Arc::new(tokio::sync::Mutex::new(())),
            eval_log_gate: Arc::new(Mutex::new(())),
            external: external::ExternalRegistry::new(),
            event_publisher: Mutex::new(None),
            last_ready_tip: Arc::new(Mutex::new(None)),
        }
    }

    /// The mempool's publisher. `FeeTableReady` is sent from a recompute, not from block connect.
    pub fn set_event_publisher(
        &self,
        publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    ) {
        *self
            .event_publisher
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = publisher;
    }

    /// Replace the operator config. Fails when a knob is outside its range.
    pub fn configure(
        &self,
        cfg: FeeEstimatorConfig,
        network_is_regtest: bool,
    ) -> anyhow::Result<()> {
        cfg.validate()?;
        let window = cfg.health_window_blocks;
        *self.cfg.write().unwrap_or_else(|err| err.into_inner()) = cfg;
        self.network_is_regtest
            .store(network_is_regtest, Ordering::Release);
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .health = health::HealthRing::new(window);
        Ok(())
    }

    pub fn set_relay_floor(&self, sat_per_kvb: u64) {
        self.relay_floor_sat_per_kvb
            .store(sat_per_kvb, Ordering::Release);
    }

    pub fn relay_floor_sat_per_kvb(&self) -> u64 {
        self.relay_floor_sat_per_kvb.load(Ordering::Acquire)
    }

    /// Targets published on the lnd `feeurl` route.
    pub fn lnd_targets(&self) -> Vec<u32> {
        self.cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .lnd_targets
            .clone()
    }

    pub fn network_is_regtest(&self) -> bool {
        self.network_is_regtest.load(Ordering::Acquire)
    }

    /// Keep the next admissions, removals, and blocks in order.
    pub(crate) fn enable_hook_trace(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .trace = true;
    }

    #[cfg(test)]
    pub(crate) fn history_short_tx_count(&self) -> f64 {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .history
            .short_tx_count()
    }

    pub(crate) fn hook_counts(&self) -> HookCounts {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .counts
    }

    pub(crate) fn hook_events(&self) -> Vec<HookEvent> {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .events
            .clone()
    }

    pub fn on_tx_admitted(&self, admission: TxAdmission) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.counts.admitted = inner.counts.admitted.saturating_add(1);
        if admission.from_reload {
            inner.counts.reloaded = inner.counts.reloaded.saturating_add(1);
        }
        let history_eligible =
            !admission.from_reload && !admission.from_package && admission.parents.is_empty();
        if inner.trace {
            inner.events.push(HookEvent::Admitted {
                txid: admission.txid,
                from_reload: admission.from_reload,
                from_package: admission.from_package,
            });
        }
        if history_eligible {
            if let Some(rate) = history::fee_sat_per_kvb(admission.fee_sats, admission.vsize) {
                let best_seen = inner.best_seen_height;
                inner
                    .history
                    .admit(admission.txid, admission.admit_height, rate, best_seen);
            }
        }
        inner.tracked.insert(
            admission.txid,
            TrackedTx {
                fee_sats: admission.fee_sats,
                vsize: admission.vsize,
                admit_height: admission.admit_height,
                parents: admission.parents,
                history_eligible,
            },
        );
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn on_tx_removed(&self, txid: &Hash) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.counts.removed = inner.counts.removed.saturating_add(1);
        if let Some(tx) = inner.tracked.remove(txid) {
            if tx.history_eligible {
                let best_seen = inner.best_seen_height;
                inner.history.remove(txid, best_seen);
                if let Some(blocks_ago) = blocks_since_admission(best_seen, tx.admit_height) {
                    if blocks_ago >= history::SHORT_SCALE {
                        inner.counts.failed = inner.counts.failed.saturating_add(1);
                    }
                }
            }
        }
        if inner.trace {
            inner.events.push(HookEvent::Removed { txid: *txid });
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn on_pool_cleared(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        inner.counts.cleared = inner.counts.cleared.saturating_add(1);
        let best_seen = inner.best_seen_height;
        inner.history.discard_without_failures(best_seen);
        inner.tracked.clear();
        if inner.trace {
            inner.events.push(HookEvent::Cleared);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Record one connected block.
    ///
    /// A height at or below the last accepted height is ignored. A gap above
    /// that height clears the health ring. Transactions named in the
    /// observation are dropped from tracking first, so the mempool's following
    /// removal is not a failure. The call does not build a table.
    pub fn on_block_connected(&self, obs: &BlockObservation) {
        let eval_path = self.eval_log_path();
        let pre_block = eval_path.as_ref().map(|_| self.cached_table());
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(seen) = inner.best_seen_height {
            if obs.height <= seen {
                inner.counts.ignored_blocks = inner.counts.ignored_blocks.saturating_add(1);
                return;
            }
            if obs.height > seen.saturating_add(1) {
                inner.counts.gap_clears = inner.counts.gap_clears.saturating_add(1);
            }
        }
        let eval_line = pre_block
            .as_ref()
            .map(|table| eval_log::record_line(obs, &inner.tracked, table));
        let found = obs.tx_weights.iter().fold(0u64, |sum, (txid, weight)| {
            if inner.tracked.contains_key(txid) {
                sum.saturating_add(*weight)
            } else {
                sum
            }
        });
        inner.best_seen_height = Some(obs.height);
        let confirmed: Vec<(Hash, bool, u64, u64, u64)> = obs
            .tx_weights
            .iter()
            .filter_map(|(txid, _)| {
                inner.tracked.get(txid).map(|tx| {
                    (
                        *txid,
                        tx.history_eligible,
                        tx.admit_height,
                        tx.fee_sats,
                        tx.vsize,
                    )
                })
            })
            .collect();
        inner.history.begin_block(obs.height);
        for (txid, eligible, admit_height, _fee_sats, _vsize) in confirmed {
            inner.tracked.remove(&txid);
            let blocks_to_confirm = obs.height.saturating_sub(admit_height);
            if eligible && inner.history.confirm(txid, blocks_to_confirm, obs.height) {
                inner.counts.confirmed = inner.counts.confirmed.saturating_add(1);
            }
        }
        inner.health.observe(obs.height, obs.weight, found);
        inner.tip_hash = obs.hash;
        inner.tip_height = obs.height;
        inner.counts.blocks = inner.counts.blocks.saturating_add(1);
        if inner.trace {
            inner.events.push(HookEvent::Block { height: obs.height });
        }
        let accepted_blocks = inner.counts.blocks;
        drop(inner);
        self.block_seq.fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let (Some(path), Some(line)) = (eval_path, eval_line) {
            self.schedule_eval_append(path, line);
        }
        self.persist_after_block(accepted_blocks);
        self.external.note_tip(obs.hash);
    }

    /// Drop every external table. Registrations stay. Called from `apply_reorg`.
    pub fn on_reorg(&self) {
        self.external.clear_tables();
    }

    pub fn register_estimators(
        &self,
        module_id: &str,
        specs: Vec<FeeEstimatorSpec>,
    ) -> FeeEstimatorRegistration {
        self.external.register(module_id, specs)
    }

    /// Accept a table, bump `generation`, and start one recompute.
    pub fn submit_fee_table(
        &self,
        module_id: &str,
        table: ExternalFeeTable,
        now: Instant,
    ) -> Result<u64, SubmitFeeTableError> {
        let (tip_hash, tip_height, max_lag, max_rate) = {
            let cfg = self.cfg.read().unwrap_or_else(|err| err.into_inner());
            let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
            (
                inner.tip_hash,
                inner.tip_height,
                cfg.external.max_lag_blocks,
                cfg.external.max_sat_per_kvb,
            )
        };
        self.external.submit(
            module_id, table, tip_hash, tip_height, max_lag, max_rate, now,
        )?;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.wake_recompute();
        Ok(generation)
    }

    pub fn select_estimator(&self, now: Instant) -> Result<EstimatorSelection, FeeUnavailable> {
        let (requested, fallback_native, allow, ttl) = {
            let cfg = self.cfg.read().unwrap_or_else(|err| err.into_inner());
            (
                cfg.estimator.clone(),
                cfg.fallback == "native",
                cfg.allow_auto_override.clone(),
                Duration::from_secs(cfg.external.ttl_secs),
            )
        };
        let tip_hash = self
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .tip_hash;
        self.external
            .select(&requested, fallback_native, &allow, tip_hash, ttl, now)
    }

    pub fn drop_module_estimates(&self, module_id: &str) {
        self.external.drop_module(module_id);
    }

    pub fn drop_module_estimates_named(&self, module_name: &str) {
        self.external.drop_module_name(module_name);
    }

    /// Native columns only. An external table is not readable here.
    pub async fn native_fee_table(
        &self,
        view: SourceView,
        mode: FeeMode,
        targets: &[u32],
    ) -> Result<(Hash, u64, u64, Vec<(u32, Option<FeeQuote>)>), NativeFeeRead> {
        if targets.len() > types::MAX_EXTERNAL_QUOTES {
            return Err(NativeFeeRead::TooManyTargets);
        }
        let table = self.table().await.map_err(NativeFeeRead::Unavailable)?;
        let quotes = targets
            .iter()
            .map(|target| (*target, table.quote(*target, mode, view).ok()))
            .collect();
        Ok((table.tip_hash, table.tip_height, table.generation, quotes))
    }

    fn eval_log_path(&self) -> Option<String> {
        let cfg = self.cfg.read().unwrap_or_else(|err| err.into_inner());
        let path = cfg.eval_log_path.as_ref()?.trim();
        if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        }
    }

    /// Append one evaluation line off this thread. A missing runtime skips it.
    fn schedule_eval_append(&self, path: String, line: String) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let gate = Arc::clone(&self.eval_log_gate);
        handle.spawn_blocking(move || {
            let _guard = gate.lock().unwrap_or_else(|err| err.into_inner());
            if let Err(err) = eval_log::append_line(std::path::Path::new(&path), &line) {
                tracing::warn!(%err, %path, "fee estimator evaluation log write failed");
            }
        });
    }

    /// Every `persist_interval_blocks` accepted blocks, write the state file off
    /// this thread. A missing runtime skips the write; shutdown still saves.
    fn persist_after_block(&self, accepted_blocks: u64) {
        let cfg = self
            .cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let interval = u64::from(cfg.persist_interval_blocks);
        if interval == 0 || accepted_blocks % interval != 0 {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = self.capture_state();
        let path = std::path::PathBuf::from(cfg.state_path);
        handle.spawn_blocking(move || {
            if let Err(err) = persist::write_atomic(&path, &state) {
                tracing::warn!(
                    "Failed to save fee estimator state to {}: {err}",
                    path.display()
                );
            }
        });
    }

    fn capture_state(&self) -> persist::PersistedState {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let history = inner.history.export();
        persist::PersistedState {
            magic: persist::MAGIC,
            version: persist::VERSION,
            bucket_params_hash: history.bucket_params_hash,
            best_seen_height: inner.best_seen_height.unwrap_or(0),
            tip_hash: inner.tip_hash,
            history,
            health: inner.health.entries(),
        }
    }

    fn apply_state(
        &self,
        state: &persist::PersistedState,
        current_height: u64,
        max_state_age_blocks: u64,
    ) -> std::result::Result<(), &'static str> {
        if persist::age_exceeded(current_height, state.best_seen_height, max_state_age_blocks) {
            return Err("stale height");
        }
        let mut inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if state.bucket_params_hash != inner.history.bucket_params_hash() {
            return Err("bucket parameter mismatch");
        }
        if !inner.history.import(&state.history) {
            return Err("bucket parameter mismatch");
        }
        let never_connected = state.best_seen_height == 0
            && state.history.first_recorded_height == 0
            && state.health.is_empty();
        inner.best_seen_height = if never_connected {
            None
        } else {
            Some(state.best_seen_height)
        };
        inner.tip_hash = state.tip_hash;
        inner.tip_height = state.best_seen_height;
        inner.health.restore(&state.health);
        Ok(())
    }

    /// Combined, history, or mempool quote for one target. Rebuilds when the
    /// cached table is from before the latest block.
    pub async fn quote(
        &self,
        target: u32,
        mode: FeeMode,
        view: SourceView,
    ) -> Result<FeeQuote, FeeUnavailable> {
        if !self.is_enabled() {
            return Err(FeeUnavailable::Disabled);
        }
        if view == SourceView::Combined {
            self.select_estimator(Instant::now())?;
        }
        let table = self.table().await?;
        table.quote(target, mode, view)
    }

    /// One merged quote for a registered name. A missing or stale table is `ExternalStale`.
    pub async fn quote_named(
        &self,
        name: &str,
        target: u32,
        mode: FeeMode,
    ) -> Result<FeeQuote, FeeUnavailable> {
        if !self.is_enabled() {
            return Err(FeeUnavailable::Disabled);
        }
        let now = Instant::now();
        let (tip_hash, ttl, merge_mode) = {
            let cfg = self.cfg.read().unwrap_or_else(|err| err.into_inner());
            let tip_hash = self
                .inner
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .tip_hash;
            let override_mode = cfg
                .external
                .by_name
                .get(name)
                .and_then(|over| over.mode.as_deref())
                .and_then(parse_merge_override);
            (
                tip_hash,
                Duration::from_secs(cfg.external.ttl_secs),
                override_mode,
            )
        };
        let Some(table) = self.external.fresh_table(name, tip_hash, ttl, now) else {
            return Err(FeeUnavailable::ExternalStale);
        };
        let Some(default_merge) = self.external.default_merge(name) else {
            return Err(FeeUnavailable::ExternalStale);
        };
        let mut snap = self.snapshot_base();
        snap.requested_estimator = name.to_string();
        snap.active_estimator = name.to_string();
        snap.degraded = false;
        snap.external = Some(combine::ExternalMerge {
            name: name.to_string(),
            mode: merge_mode.unwrap_or(default_merge),
            fee_mode: table.mode,
            quotes: table.quotes,
        });
        let built = tokio::task::spawn_blocking(move || combine::build_fee_table(snap))
            .await
            .expect("fee table build");
        built.quote(target, mode, SourceView::Combined)
    }

    pub fn has_estimator(&self, name: &str) -> bool {
        self.external.contains(name)
    }

    /// `native`, every registered name, and a configured name that is not registered yet.
    pub fn fee_estimator_rows(&self) -> Vec<FeeEstimatorRow> {
        let now = Instant::now();
        let (requested, ttl) = {
            let cfg = self.cfg.read().unwrap_or_else(|err| err.into_inner());
            (
                cfg.estimator.clone(),
                Duration::from_secs(cfg.external.ttl_secs),
            )
        };
        let tip_hash = self
            .inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .tip_hash;
        let (active, requested_label) = match self.select_estimator(now) {
            Ok(selection) => (selection.active, selection.requested),
            Err(_) => ("native".to_string(), requested.clone()),
        };
        let mut rows = vec![FeeEstimatorRow {
            name: "native".to_string(),
            module: None,
            description: "built-in".to_string(),
            resolved: true,
            fresh: true,
            last_submit_age_secs: None,
            active: active == "native",
        }];
        let mut saw_requested = requested == "auto" || requested == "native";
        for entry in self.external.registered_status(tip_hash, ttl, now) {
            if entry.name == requested {
                saw_requested = true;
            }
            rows.push(FeeEstimatorRow {
                active: entry.name == active,
                name: entry.name,
                module: Some(entry.module_id),
                description: entry.description,
                resolved: true,
                fresh: entry.fresh,
                last_submit_age_secs: entry.age_secs,
            });
        }
        if !saw_requested {
            rows.push(FeeEstimatorRow {
                name: requested_label,
                module: None,
                description: String::new(),
                resolved: false,
                fresh: false,
                last_submit_age_secs: None,
                active: false,
            });
        }
        rows
    }

    /// Names for verbosity 1. A failed selection is reported as degraded to `native`.
    pub fn estimator_labels(&self) -> (String, String, bool) {
        match self.select_estimator(Instant::now()) {
            Ok(selection) => (selection.requested, selection.active, selection.degraded),
            Err(_) => {
                let requested = self
                    .cfg
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .estimator
                    .clone();
                (requested, "native".to_string(), true)
            }
        }
    }

    /// The published table. A new block is never answered with the previous one.
    pub async fn table(&self) -> Result<Arc<FeeTable>, FeeUnavailable> {
        if !self.is_enabled() {
            return Err(FeeUnavailable::Disabled);
        }
        loop {
            let current = self.cached_table();
            match self.action_for(&current, Instant::now()) {
                TableAction::Serve => {
                    let again = self.cached_table();
                    if self.action_for(&again, Instant::now()) == TableAction::Serve {
                        return Ok(again);
                    }
                }
                TableAction::Background => {
                    self.start_background_recompute();
                    let again = self.cached_table();
                    if self.action_for(&again, Instant::now()) != TableAction::Wait {
                        return Ok(again);
                    }
                }
                TableAction::Wait => self.recompute_and_publish().await,
            }
        }
    }

    fn is_enabled(&self) -> bool {
        self.cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .enabled
    }

    fn cached_table(&self) -> Arc<FeeTable> {
        Arc::clone(&self.table.read().unwrap_or_else(|err| err.into_inner()))
    }

    fn action_for(&self, table: &FeeTable, now: Instant) -> TableAction {
        let active = self
            .select_estimator(now)
            .map(|selection| selection.active)
            .unwrap_or_else(|_| "native".to_string());
        if table.active_estimator != active {
            return TableAction::Wait;
        }
        let interval = Duration::from_secs(
            self.cfg
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .recompute_interval_secs,
        );
        table_action(
            table.block_seq,
            self.block_seq.load(Ordering::Acquire),
            table.generation,
            self.generation.load(Ordering::Acquire),
            table.generated_at,
            now,
            interval,
        )
    }

    /// Admission records the projection reads. Test-only, so the differential
    /// check uses the same fees and sizes the table build would clone.
    #[cfg(test)]
    pub(crate) fn tracked_for_projection(&self) -> Vec<projection::ProjTx> {
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let mut txs: Vec<projection::ProjTx> = inner
            .tracked
            .iter()
            .map(|(id, tx)| projection::ProjTx {
                id: *id,
                fee: tx.fee_sats,
                vsize: tx.vsize,
                parents: tx.parents.clone(),
            })
            .collect();
        txs.sort_by_key(|tx| tx.id);
        txs
    }

    fn snapshot(&self) -> combine::MempoolSnapshot {
        let mut snap = self.snapshot_base();
        let (selection, external) = self.selection_and_merge(snap.tip_hash);
        snap.requested_estimator = selection.requested;
        snap.active_estimator = selection.active;
        snap.degraded = selection.degraded;
        snap.external = external;
        snap
    }

    fn snapshot_base(&self) -> combine::MempoolSnapshot {
        let cfg = self
            .cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let inner = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        let mut txs: Vec<projection::ProjTx> = inner
            .tracked
            .iter()
            .map(|(id, tx)| projection::ProjTx {
                id: *id,
                fee: tx.fee_sats,
                vsize: tx.vsize,
                parents: tx.parents.clone(),
            })
            .collect();
        txs.sort_by_key(|tx| tx.id);
        let required = health::health_required(cfg.health_required, self.network_is_regtest());
        let health_ok =
            health::mempool_layer_trusted(&inner.health, cfg.health_min_ratio, required);
        let health_window = cfg.health_window_blocks as u64;
        let health_ratio = inner.health.aggregate_ratio();
        let health_required_ratio = cfg.health_min_ratio;
        let window_full =
            cfg.health_window_blocks > 0 && inner.health.len() == cfg.health_window_blocks;
        let best_seen = inner.best_seen_height.unwrap_or(0);
        let mut history = combine::empty_history();
        for (index, mode) in [FeeMode::Conservative, FeeMode::Economical]
            .into_iter()
            .enumerate()
        {
            for target in 1..=types::MAX_FEE_TARGET {
                history[index][(target - 1) as usize] =
                    inner.history.estimate(target, mode, best_seen);
            }
        }
        let requested = cfg.estimator.clone();
        combine::MempoolSnapshot {
            txs,
            health_ok,
            health_window,
            health_ratio,
            health_required_ratio,
            window_full,
            tip_hash: inner.tip_hash,
            tip_height: inner.tip_height,
            block_seq: self.block_seq.load(Ordering::Acquire),
            generation: self.generation.load(Ordering::Acquire),
            generated_at: Instant::now(),
            cfg,
            relay_floor_sat_per_kvb: self.relay_floor_sat_per_kvb(),
            history,
            requested_estimator: requested.clone(),
            active_estimator: requested,
            degraded: false,
            external: None,
        }
    }

    /// Drop `inner` before this. The external store is not taken while that lock is held.
    fn selection_and_merge(
        &self,
        tip_hash: Hash,
    ) -> (EstimatorSelection, Option<combine::ExternalMerge>) {
        let now = Instant::now();
        let requested = self
            .cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .estimator
            .clone();
        let selection = match self.select_estimator(now) {
            Ok(selection) => selection,
            Err(_) => {
                return (
                    EstimatorSelection {
                        requested,
                        active: "native".to_string(),
                        degraded: true,
                        unresolved: true,
                    },
                    None,
                );
            }
        };
        if selection.active == "native" {
            return (selection, None);
        }
        let (ttl, override_mode) = {
            let cfg = self.cfg.read().unwrap_or_else(|err| err.into_inner());
            let override_mode = cfg
                .external
                .by_name
                .get(&selection.active)
                .and_then(|over| over.mode.as_deref())
                .and_then(parse_merge_override);
            (Duration::from_secs(cfg.external.ttl_secs), override_mode)
        };
        let Some(table) = self
            .external
            .fresh_table(&selection.active, tip_hash, ttl, now)
        else {
            return (
                EstimatorSelection {
                    requested: selection.requested,
                    active: "native".to_string(),
                    degraded: true,
                    unresolved: selection.unresolved,
                },
                None,
            );
        };
        let Some(default_merge) = self.external.default_merge(&selection.active) else {
            return (
                EstimatorSelection {
                    requested: selection.requested,
                    active: "native".to_string(),
                    degraded: true,
                    unresolved: false,
                },
                None,
            );
        };
        let name = selection.active.clone();
        (
            selection,
            Some(combine::ExternalMerge {
                name,
                mode: override_mode.unwrap_or(default_merge),
                fee_mode: table.mode,
                quotes: table.quotes,
            }),
        )
    }

    fn publish(&self, table: FeeTable) {
        let publisher = self
            .event_publisher
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        install_table(&self.table, &self.last_ready_tip, publisher, table);
    }

    /// Age the cached table past the hard stale bound so the next quote rebuilds
    /// after a hook. Production calls keep the normal interval.
    #[cfg(test)]
    pub(crate) fn mark_table_stale_for_test(&self) {
        let mut table = (*self.cached_table()).clone();
        if let Some(generated_at) = Instant::now().checked_sub(TABLE_HARD_STALE) {
            table.generated_at = generated_at;
        }
        self.publish(table);
    }

    async fn recompute_and_publish(&self) {
        let _guard = self.recompute_gate.lock().await;
        let current = self.cached_table();
        if self.action_for(&current, Instant::now()) != TableAction::Wait {
            return;
        }
        let snap = self.snapshot();
        let built = tokio::task::spawn_blocking(move || combine::build_fee_table(snap))
            .await
            .expect("fee table build");
        self.publish(built);
    }

    fn start_background_recompute(&self) {
        let gate = Arc::clone(&self.recompute_gate);
        let Ok(guard) = gate.try_lock_owned() else {
            return;
        };
        let current = self.cached_table();
        if self.action_for(&current, Instant::now()) == TableAction::Serve {
            return;
        }
        let snap = self.snapshot();
        let slot = Arc::clone(&self.table);
        let last_ready = Arc::clone(&self.last_ready_tip);
        let publisher = self
            .event_publisher
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        tokio::spawn(async move {
            let _guard = guard;
            if let Ok(table) =
                tokio::task::spawn_blocking(move || combine::build_fee_table(snap)).await
            {
                install_table(&slot, &last_ready, publisher, table);
            }
        });
    }

    /// One rebuild after an accepted external submit. The interval rule is unchanged.
    fn wake_recompute(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let gate = Arc::clone(&self.recompute_gate);
        let Ok(guard) = gate.try_lock_owned() else {
            return;
        };
        let snap = self.snapshot();
        let slot = Arc::clone(&self.table);
        let last_ready = Arc::clone(&self.last_ready_tip);
        let publisher = self
            .event_publisher
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        handle.spawn(async move {
            let _guard = guard;
            if let Ok(table) =
                tokio::task::spawn_blocking(move || combine::build_fee_table(snap)).await
            {
                install_table(&slot, &last_ready, publisher, table);
            }
        });
    }

    /// Load `path`. A missing, corrupt, mismatched, or stale file leaves the
    /// estimator empty and returns `Ok`.
    pub fn load_state(&self, path: &std::path::Path, current_height: u64) -> anyhow::Result<()> {
        let max_state_age_blocks = self
            .cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .max_state_age_blocks;
        match persist::read_state(path)? {
            None => Ok(()),
            Some(Err(reason)) => {
                persist::warn_discard(path, reason);
                Ok(())
            }
            Some(Ok(state)) => {
                if let Err(reason) = self.apply_state(&state, current_height, max_state_age_blocks)
                {
                    persist::warn_discard(path, reason);
                }
                Ok(())
            }
        }
    }

    /// Atomically replace `path` with the current history averages and health ring.
    pub fn save_state(&self, path: &std::path::Path) -> anyhow::Result<()> {
        persist::write_atomic(path, &self.capture_state())
    }
}

fn table_action(
    table_block_seq: u64,
    live_block_seq: u64,
    table_generation: u64,
    live_generation: u64,
    generated_at: Instant,
    now: Instant,
    recompute_interval: Duration,
) -> TableAction {
    if table_block_seq != live_block_seq {
        return TableAction::Wait;
    }
    let age = now.saturating_duration_since(generated_at);
    if age >= recompute_interval && table_generation != live_generation {
        if age >= TABLE_HARD_STALE {
            TableAction::Wait
        } else {
            TableAction::Background
        }
    } else {
        TableAction::Serve
    }
}

/// `TxConfirmStats::removeTx` treats a best height of zero as no blocks seen.
/// A negative age is ignored, which is Core's early return.
fn blocks_since_admission(best_seen: Option<u64>, admit_height: u64) -> Option<u64> {
    let Some(best) = best_seen else {
        return Some(0);
    };
    if best == 0 {
        return Some(0);
    }
    best.checked_sub(admit_height)
}

/// Observation for a connect site. A weight error still lets the caller remove
/// the block's transactions, with no observation.
pub(crate) fn block_observation(
    block: &Block,
    witnesses: &[Vec<Witness>],
    height: u64,
    hash: Hash,
) -> Option<BlockObservation> {
    match BlockObservation::from_block(block, witnesses, height, hash) {
        Ok(obs) => Some(obs),
        Err(err) => {
            tracing::warn!("fee estimator skipped block observation at height {height}: {err}");
            None
        }
    }
}

fn parse_merge_override(mode: &str) -> Option<ExternalMode> {
    match mode {
        "lower_only" => Some(ExternalMode::LowerOnly),
        "replace" => Some(ExternalMode::Replace),
        _ => None,
    }
}

fn install_table(
    slot: &RwLock<Arc<FeeTable>>,
    last_ready: &Mutex<Option<Hash>>,
    publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    table: FeeTable,
) {
    let announce = publisher.and_then(|publisher| {
        if table.tip_hash == [0u8; 32] {
            return None;
        }
        let mut last = last_ready.lock().unwrap_or_else(|err| err.into_inner());
        if *last == Some(table.tip_hash) {
            return None;
        }
        *last = Some(table.tip_hash);
        Some((
            publisher,
            table.tip_hash,
            table.tip_height,
            table.generation,
        ))
    });
    *slot.write().unwrap_or_else(|err| err.into_inner()) = Arc::new(table);
    if let Some((publisher, tip_hash, tip_height, generation)) = announce {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            publisher
                .publish_fee_table_ready(tip_hash, tip_height, generation)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{BlockHeader, OutPoint, Transaction, TransactionInput, TransactionOutput};

    fn observation(height: u64) -> BlockObservation {
        BlockObservation {
            height,
            hash: [height as u8; 32],
            weight: 4_000,
            tx_weights: Vec::new(),
        }
    }

    #[test]
    fn duplicate_height_is_ignored_and_a_gap_is_counted() {
        let est = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        est.on_block_connected(&observation(10));
        est.on_block_connected(&observation(10));
        est.on_block_connected(&observation(9));
        est.on_block_connected(&observation(12));
        let counts = est.hook_counts();
        assert_eq!(counts.blocks, 2);
        assert_eq!(counts.ignored_blocks, 2);
        assert_eq!(counts.gap_clears, 1);
    }

    #[test]
    fn trace_keeps_hook_order() {
        let est = FeeEstimator::new(FeeEstimatorConfig::default(), false);
        est.enable_hook_trace();
        let txid = [7u8; 32];
        est.on_tx_admitted(TxAdmission {
            txid,
            fee_sats: 100,
            vsize: 100,
            admit_height: 1,
            parents: Vec::new(),
            from_reload: false,
            from_package: false,
        });
        est.on_block_connected(&observation(2));
        est.on_tx_removed(&txid);
        est.on_pool_cleared();
        assert_eq!(
            est.hook_events(),
            vec![
                HookEvent::Admitted {
                    txid,
                    from_reload: false,
                    from_package: false,
                },
                HookEvent::Block { height: 2 },
                HookEvent::Removed { txid },
                HookEvent::Cleared,
            ]
        );
    }

    fn coinbase() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffffffff,
                },
                script_sig: vec![1],
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 50_0000_0000,
                script_pubkey: vec![0x51].into(),
            }]
            .into(),
            lock_time: 0,
        }
    }

    fn spend() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [1u8; 32],
                    index: 0,
                },
                script_sig: Vec::new(),
                sequence: 0xffffffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 1000,
                script_pubkey: vec![0x51].into(),
            }]
            .into(),
            lock_time: 0,
        }
    }

    #[test]
    fn observation_skips_the_coinbase_and_keeps_block_weight() {
        let coinbase = coinbase();
        let spend = spend();
        let spend_id = blvm_protocol::block::calculate_tx_id(&spend);
        let block = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1_700_000_000,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![coinbase, spend].into_boxed_slice(),
        };
        let witnesses = vec![vec![vec![vec![0u8; 32]]], vec![Vec::new()]];
        let obs = BlockObservation::from_block(&block, &witnesses, 1, [9u8; 32]).unwrap();
        assert_eq!(obs.tx_weights.len(), 1);
        assert_eq!(obs.tx_weights[0].0, spend_id);
        assert!(obs.tx_weights[0].1 > 0);
        assert!(obs.weight > obs.tx_weights[0].1);
    }

    fn admission(fee_sats: u64, vsize: u64) -> TxAdmission {
        TxAdmission {
            txid: [4u8; 32],
            fee_sats,
            vsize,
            admit_height: 1,
            parents: Vec::new(),
            from_reload: false,
            from_package: false,
        }
    }

    fn open_cfg() -> FeeEstimatorConfig {
        let mut cfg = FeeEstimatorConfig::default();
        cfg.health_required = Some(false);
        cfg.projection_full_threshold = 0.0;
        cfg.allow_mempool_only = true;
        cfg.multiplier = 1.0;
        cfg
    }

    #[test]
    fn staleness_waits_for_a_new_block_and_serves_a_young_table() {
        let now = Instant::now();
        let fresh = now.checked_sub(Duration::from_secs(1)).unwrap();
        let interval = Duration::from_secs(7);
        assert_eq!(
            table_action(1, 2, 1, 1, fresh, now, interval),
            TableAction::Wait
        );
        assert_eq!(
            table_action(1, 1, 1, 4, fresh, now, interval),
            TableAction::Serve
        );
        let aging = now.checked_sub(Duration::from_secs(10)).unwrap();
        assert_eq!(
            table_action(1, 1, 1, 4, aging, now, interval),
            TableAction::Background
        );
        let stale = now.checked_sub(Duration::from_secs(60)).unwrap();
        assert_eq!(
            table_action(1, 1, 1, 4, stale, now, interval),
            TableAction::Wait
        );
        assert_eq!(
            table_action(1, 1, 4, 4, stale, now, interval),
            TableAction::Serve
        );
    }

    #[tokio::test]
    async fn a_new_block_is_never_served_from_the_previous_table() {
        let est = FeeEstimator::new(open_cfg(), true);
        let first = est.table().await.unwrap();
        assert_eq!(first.block_seq, 0);
        est.on_tx_admitted(admission(5_000, 100));
        let during_interval = est.table().await.unwrap();
        assert!(Arc::ptr_eq(&first, &during_interval));
        est.on_block_connected(&observation(1));
        let second = est.table().await.unwrap();
        assert_eq!(second.block_seq, 1);
        assert_eq!(second.tip_height, 1);
        assert!(!Arc::ptr_eq(&first, &second));
        let quote = est
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await
            .unwrap();
        assert_eq!(quote.sat_per_kvb, 50_000);
        assert_eq!(quote.source, FeeSource::Mempool);
        assert_eq!(quote.blocks, 1);
    }

    #[tokio::test]
    async fn quotes_follow_the_health_gate_and_an_empty_or_disabled_estimator() {
        let regtest = FeeEstimator::new(FeeEstimatorConfig::default(), true);
        assert_eq!(
            regtest
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .await,
            Err(FeeUnavailable::EmptyMempool)
        );

        let mainnet = FeeEstimator::new(FeeEstimatorConfig::default(), false);
        assert_eq!(
            mainnet
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .await,
            Err(FeeUnavailable::WarmingUp)
        );

        let mut disabled = open_cfg();
        disabled.enabled = false;
        let disabled = FeeEstimator::new(disabled, true);
        assert_eq!(
            disabled.table().await.unwrap_err(),
            FeeUnavailable::Disabled
        );

        let mut strict = open_cfg();
        strict.health_required = Some(true);
        strict.health_window_blocks = 1;
        let strict = FeeEstimator::new(strict, false);
        strict.on_tx_admitted(admission(5_000, 100));
        strict.on_block_connected(&observation(1));
        assert_eq!(
            strict
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .await,
            Err(FeeUnavailable::MempoolUnhealthy)
        );

        let mut no_only = open_cfg();
        no_only.allow_mempool_only = false;
        let no_only = FeeEstimator::new(no_only, true);
        no_only.on_tx_admitted(admission(5_000, 100));
        no_only.on_block_connected(&observation(1));
        assert_eq!(
            no_only
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .await,
            Err(FeeUnavailable::WarmingUp)
        );
        assert_eq!(
            no_only
                .quote(1, FeeMode::Conservative, SourceView::Mempool)
                .await
                .unwrap()
                .sat_per_kvb,
            50_000
        );
    }

    #[tokio::test]
    async fn external_store_keeps_native_quotes_and_announces_one_table_per_tip() {
        let est = FeeEstimator::new(open_cfg(), true);
        let first = observation(1);
        est.on_tx_admitted(admission(5_000, 100));
        est.on_block_connected(&first);
        let events = Arc::new(crate::module::api::events::EventManager::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        events
            .subscribe_module(
                "listener".into(),
                vec![crate::module::traits::EventType::FeeTableReady],
                tx,
            )
            .await
            .unwrap();
        est.set_event_publisher(Some(Arc::new(
            crate::node::event_publisher::EventPublisher::new(Arc::clone(&events)),
        )));
        let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let registered = est.register_estimators(
            id,
            vec![FeeEstimatorSpec {
                name: "blend".to_string(),
                description: "blend".to_string(),
                modes: vec![FeeMode::Conservative],
                default_merge: ExternalMode::LowerOnly,
                auto_priority: Some(3),
            }],
        );
        assert_eq!(registered.accepted, vec!["blend".to_string()]);
        let generation = est
            .submit_fee_table(
                id,
                ExternalFeeTable {
                    estimator: "blend".to_string(),
                    tip_hash: first.hash,
                    upstream_height: 1,
                    mode: None,
                    quotes: vec![(1, 1)],
                },
                Instant::now(),
            )
            .unwrap();
        assert!(generation >= 1);

        let (_tip, _height, _generation, quotes) = est
            .native_fee_table(SourceView::Combined, FeeMode::Conservative, &[1])
            .await
            .unwrap();
        let quote = quotes[0].1.as_ref().expect("native quote");
        assert_eq!(quote.sat_per_kvb, 50_000);
        assert_eq!(quote.estimator, "native");

        let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("FeeTableReady")
            .expect("channel");
        let crate::module::ipc::protocol::ModuleMessage::Event(event) = message else {
            panic!("expected an event");
        };
        assert_eq!(
            event.event_type,
            crate::module::traits::EventType::FeeTableReady
        );
        est.mark_table_stale_for_test();
        let _ = est
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err()
        );

        est.on_block_connected(&observation(2));
        let mut named = open_cfg();
        named.estimator = "blend".to_string();
        est.configure(named, true).unwrap();
        let selected = est.select_estimator(Instant::now()).unwrap();
        assert!(selected.degraded);
        assert_eq!(selected.active, "native");
        est.on_reorg();
        assert!(est.select_estimator(Instant::now()).unwrap().degraded);
        est.drop_module_estimates(id);
        let gone = est.select_estimator(Instant::now()).unwrap();
        assert!(gone.unresolved);
    }

    fn blend_spec(mode: ExternalMode) -> FeeEstimatorSpec {
        FeeEstimatorSpec {
            name: "blend".to_string(),
            description: "blend".to_string(),
            modes: vec![FeeMode::Conservative],
            default_merge: mode,
            auto_priority: Some(2),
        }
    }

    #[tokio::test]
    async fn auto_without_consent_stays_native_and_a_merge_cannot_beat_the_floor() {
        let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let denied = FeeEstimator::new(open_cfg(), true);
        let tip = observation(1);
        denied.on_tx_admitted(admission(5_000, 100));
        denied.on_block_connected(&tip);
        denied.register_estimators(id, vec![blend_spec(ExternalMode::LowerOnly)]);
        denied
            .submit_fee_table(
                id,
                ExternalFeeTable {
                    estimator: "blend".to_string(),
                    tip_hash: tip.hash,
                    upstream_height: 1,
                    mode: None,
                    quotes: vec![(1, 1)],
                },
                Instant::now(),
            )
            .unwrap();
        let native = denied
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await
            .unwrap();
        assert_eq!(native.sat_per_kvb, 50_000);
        assert_eq!(native.estimator, "native");
        assert!(
            denied
                .fee_estimator_rows()
                .iter()
                .any(|row| row.name == "blend" && !row.active && row.fresh)
        );

        let mut allowed_cfg = open_cfg();
        allowed_cfg.allow_auto_override = vec![id.to_string()];
        let allowed = FeeEstimator::new(allowed_cfg, true);
        allowed.on_tx_admitted(admission(5_000, 100));
        allowed.on_block_connected(&tip);
        allowed.register_estimators(id, vec![blend_spec(ExternalMode::LowerOnly)]);
        allowed
            .submit_fee_table(
                id,
                ExternalFeeTable {
                    estimator: "blend".to_string(),
                    tip_hash: tip.hash,
                    upstream_height: 1,
                    mode: None,
                    quotes: vec![(1, 1)],
                },
                Instant::now(),
            )
            .unwrap();
        let lowered = allowed
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await
            .unwrap();
        assert_eq!(lowered.sat_per_kvb, 100);
        assert_eq!(lowered.source, FeeSource::ExternalMin);
        assert_eq!(lowered.estimator, "blend");
        assert!(
            allowed
                .fee_estimator_rows()
                .iter()
                .any(|row| row.name == "native" && !row.active)
        );

        let mut replace_cfg = open_cfg();
        replace_cfg.estimator = "blend".to_string();
        replace_cfg.external.by_name.insert(
            "blend".to_string(),
            crate::config::ExternalEstimatorOverride {
                mode: Some("replace".to_string()),
            },
        );
        let replace = FeeEstimator::new(replace_cfg, true);
        replace.on_tx_admitted(admission(5_000, 100));
        replace.on_block_connected(&tip);
        replace.register_estimators(id, vec![blend_spec(ExternalMode::LowerOnly)]);
        replace
            .submit_fee_table(
                id,
                ExternalFeeTable {
                    estimator: "blend".to_string(),
                    tip_hash: tip.hash,
                    upstream_height: 1,
                    mode: None,
                    quotes: vec![(1, 80_000)],
                },
                Instant::now(),
            )
            .unwrap();
        let replaced = replace
            .quote(1, FeeMode::Conservative, SourceView::Combined)
            .await
            .unwrap();
        assert_eq!(replaced.sat_per_kvb, 80_000);
        assert_eq!(replaced.source, FeeSource::External);
        assert_eq!(replaced.estimator, "blend");

        let mut strict = open_cfg();
        strict.estimator = "blend".to_string();
        strict.fallback = "none".to_string();
        let strict = FeeEstimator::new(strict, true);
        assert_eq!(
            strict
                .quote(1, FeeMode::Conservative, SourceView::Combined)
                .await,
            Err(FeeUnavailable::ExternalStale)
        );
    }
}

//! Named estimators modules push. Validation and storage live here.
//! Combining a table with the native quote does not.

use super::types::{
    EstimatorSelection, ExternalFeeTable, ExternalMode, FeeEstimatorRegistration, FeeEstimatorSpec,
    FeeMode, FeeNameError, MAX_ESTIMATOR_DESCRIPTION_BYTES, MAX_EXTERNAL_QUOTES, MAX_FEE_TARGET,
    SubmitFeeTableError,
};
use crate::config::fees::estimator_name_ok;
use blvm_protocol::Hash;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

const RESERVED: &[&str] = &["auto", "native", "none", "history", "mempool", "external"];

struct SpecRecord {
    module_id: String,
    spec: FeeEstimatorSpec,
}

struct StoredTable {
    module_id: String,
    table: ExternalFeeTable,
    at: Instant,
}

struct State {
    specs: HashMap<String, SpecRecord>,
    tables: HashMap<String, StoredTable>,
    last_accept: HashMap<String, Instant>,
    unresolved_logged_at: Option<Instant>,
    fallback_logged: bool,
}

/// `HashMap<name, (module, table, instant)>` plus the registration that owns each name.
pub struct ExternalRegistry {
    inner: RwLock<State>,
}

impl ExternalRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(State {
                specs: HashMap::new(),
                tables: HashMap::new(),
                last_accept: HashMap::new(),
                unresolved_logged_at: None,
                fallback_logged: false,
            }),
        }
    }

    /// Replace this module's names with the specs that pass. A rejected name is reported and dropped.
    pub fn register(
        &self,
        module_id: &str,
        specs: Vec<FeeEstimatorSpec>,
    ) -> FeeEstimatorRegistration {
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        let previous: Vec<String> = state
            .specs
            .iter()
            .filter(|(_, record)| record.module_id == module_id)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &previous {
            state.specs.remove(name);
            state.tables.remove(name);
        }
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        for spec in specs {
            match check_spec(&spec, module_id, &state.specs) {
                Ok(()) => {
                    let name = spec.name.clone();
                    state.specs.insert(
                        name.clone(),
                        SpecRecord {
                            module_id: module_id.to_string(),
                            spec,
                        },
                    );
                    accepted.push(name);
                }
                Err(err) => {
                    tracing::debug!(
                        module_id,
                        name = %spec.name,
                        ?err,
                        "fee estimator registration rejected"
                    );
                    rejected.push((spec.name, err));
                }
            }
        }
        FeeEstimatorRegistration { accepted, rejected }
    }

    pub fn submit(
        &self,
        module_id: &str,
        table: ExternalFeeTable,
        node_tip: Hash,
        node_height: u64,
        max_lag_blocks: u64,
        max_sat_per_kvb: u64,
        now: Instant,
    ) -> Result<(), SubmitFeeTableError> {
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        let Some(record) = state.specs.get(&table.estimator) else {
            tracing::debug!(estimator = %table.estimator, "fee table rejected: unknown estimator");
            return Err(SubmitFeeTableError::UnknownEstimator);
        };
        if record.module_id != module_id {
            tracing::debug!(
                module_id,
                estimator = %table.estimator,
                "fee table rejected: estimator owned by another module"
            );
            return Err(SubmitFeeTableError::NotOwner);
        }
        if let Some(err) = check_quotes(&table.quotes, max_sat_per_kvb) {
            tracing::debug!(module_id, ?err, "fee table rejected");
            return Err(err);
        }
        if table.tip_hash != node_tip {
            tracing::debug!(module_id, "fee table rejected: stale tip");
            return Err(SubmitFeeTableError::StaleTip);
        }
        if node_height.abs_diff(table.upstream_height) > max_lag_blocks {
            tracing::debug!(module_id, "fee table rejected: height lag");
            return Err(SubmitFeeTableError::HeightLag);
        }
        if let Some(last) = state.last_accept.get(module_id)
            && now.saturating_duration_since(*last) < Duration::from_secs(1)
        {
            tracing::debug!(module_id, "fee table rejected: rate limited");
            return Err(SubmitFeeTableError::RateLimited);
        }
        let name = table.estimator.clone();
        state.tables.insert(
            name,
            StoredTable {
                module_id: module_id.to_string(),
                table,
                at: now,
            },
        );
        state.last_accept.insert(module_id.to_string(), now);
        Ok(())
    }

    /// Drop tables whose tip is no longer current. Registrations stay.
    pub fn note_tip(&self, tip_hash: Hash) {
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        state
            .tables
            .retain(|_, stored| stored.table.tip_hash == tip_hash);
    }

    /// A reorg drops every stored table. Registrations stay.
    pub fn clear_tables(&self) {
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        state.tables.clear();
    }

    /// Unload or crash. Names and tables owned by this session go away.
    pub fn drop_module(&self, module_id: &str) {
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        state
            .specs
            .retain(|_, record| record.module_id != module_id);
        state
            .tables
            .retain(|_, stored| stored.module_id != module_id);
        state.last_accept.remove(module_id);
    }

    pub fn drop_module_name(&self, module_name: &str) {
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        let gone = |id: &str| crate::module::module_name_from_id(id) == module_name;
        state.specs.retain(|_, record| !gone(&record.module_id));
        state.tables.retain(|_, stored| !gone(&stored.module_id));
        state.last_accept.retain(|id, _| !gone(id));
    }

    pub fn fresh_table(
        &self,
        name: &str,
        tip_hash: Hash,
        ttl: Duration,
        now: Instant,
    ) -> Option<ExternalFeeTable> {
        let state = self.inner.read().unwrap_or_else(|err| err.into_inner());
        let stored = state.tables.get(name)?;
        if stored.table.tip_hash != tip_hash {
            return None;
        }
        if now.saturating_duration_since(stored.at) >= ttl {
            return None;
        }
        Some(stored.table.clone())
    }

    pub fn owns(&self, module_id: &str, name: &str) -> bool {
        let state = self.inner.read().unwrap_or_else(|err| err.into_inner());
        state
            .specs
            .get(name)
            .is_some_and(|record| record.module_id == module_id)
    }

    pub fn contains(&self, name: &str) -> bool {
        let state = self.inner.read().unwrap_or_else(|err| err.into_inner());
        state.specs.contains_key(name)
    }

    pub fn registered_status(
        &self,
        tip_hash: Hash,
        ttl: Duration,
        now: Instant,
    ) -> Vec<RegisteredEstimator> {
        let state = self.inner.read().unwrap_or_else(|err| err.into_inner());
        let mut rows: Vec<RegisteredEstimator> = state
            .specs
            .iter()
            .map(|(name, record)| {
                let stored = state.tables.get(name);
                let fresh = stored.is_some_and(|stored| {
                    stored.table.tip_hash == tip_hash
                        && now.saturating_duration_since(stored.at) < ttl
                });
                let age_secs =
                    stored.map(|stored| now.saturating_duration_since(stored.at).as_secs());
                RegisteredEstimator {
                    name: name.clone(),
                    module_id: record.module_id.clone(),
                    description: record.spec.description.clone(),
                    fresh,
                    age_secs,
                }
            })
            .collect();
        rows.sort_by(|left, right| left.name.cmp(&right.name));
        rows
    }

    pub fn default_merge(&self, name: &str) -> Option<ExternalMode> {
        let state = self.inner.read().unwrap_or_else(|err| err.into_inner());
        state
            .specs
            .get(name)
            .map(|record| record.spec.default_merge)
    }

    /// Design 20.2. Does not read or merge quotes into the native table.
    pub fn select(
        &self,
        requested: &str,
        fallback_native: bool,
        allow: &[String],
        tip_hash: Hash,
        ttl: Duration,
        now: Instant,
    ) -> Result<EstimatorSelection, super::types::FeeUnavailable> {
        if requested == "native" {
            return Ok(native_selection(requested, false, false));
        }
        let mut state = self.inner.write().unwrap_or_else(|err| err.into_inner());
        if requested == "auto" {
            if let Some(name) = best_auto(&state, allow, tip_hash, ttl, now) {
                state.fallback_logged = false;
                return Ok(EstimatorSelection {
                    requested: requested.to_string(),
                    active: name,
                    degraded: false,
                    unresolved: false,
                });
            }
            let allowed_claim = state.specs.values().any(|record| {
                record.spec.auto_priority.is_some()
                    && allow.iter().any(|id| id == &record.module_id)
            });
            if !allowed_claim {
                return Ok(native_selection(requested, false, false));
            }
            return finish_fallback(&mut state, requested, false, fallback_native, now);
        }
        if !state.specs.contains_key(requested) {
            return finish_fallback(&mut state, requested, true, fallback_native, now);
        }
        let fresh = state.tables.get(requested).is_some_and(|stored| {
            stored.table.tip_hash == tip_hash && now.saturating_duration_since(stored.at) < ttl
        });
        if fresh {
            state.fallback_logged = false;
            return Ok(EstimatorSelection {
                requested: requested.to_string(),
                active: requested.to_string(),
                degraded: false,
                unresolved: false,
            });
        }
        finish_fallback(&mut state, requested, false, fallback_native, now)
    }
}

fn native_selection(requested: &str, degraded: bool, unresolved: bool) -> EstimatorSelection {
    EstimatorSelection {
        requested: requested.to_string(),
        active: "native".to_string(),
        degraded,
        unresolved,
    }
}

fn finish_fallback(
    state: &mut State,
    requested: &str,
    unresolved: bool,
    fallback_native: bool,
    now: Instant,
) -> Result<EstimatorSelection, super::types::FeeUnavailable> {
    if unresolved {
        let due = state
            .unresolved_logged_at
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(60));
        if due {
            tracing::error!(
                estimator = requested,
                "fee estimator name is not registered; falling back"
            );
            state.unresolved_logged_at = Some(now);
        }
    }
    if !state.fallback_logged {
        tracing::warn!(estimator = requested, "fee estimator falling back");
        state.fallback_logged = true;
    }
    if fallback_native {
        Ok(native_selection(requested, true, unresolved))
    } else {
        Err(super::types::FeeUnavailable::ExternalStale)
    }
}

pub struct RegisteredEstimator {
    pub name: String,
    pub module_id: String,
    pub description: String,
    pub fresh: bool,
    pub age_secs: Option<u64>,
}

impl Default for ExternalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn check_spec(
    spec: &FeeEstimatorSpec,
    module_id: &str,
    specs: &HashMap<String, SpecRecord>,
) -> Result<(), FeeNameError> {
    if RESERVED.contains(&spec.name.as_str()) {
        return Err(FeeNameError::Reserved);
    }
    if !estimator_name_ok(&spec.name) {
        return Err(FeeNameError::BadFormat);
    }
    if spec.description.len() > MAX_ESTIMATOR_DESCRIPTION_BYTES {
        return Err(FeeNameError::DescriptionTooLong);
    }
    if spec.modes.is_empty() {
        return Err(FeeNameError::EmptyModes);
    }
    if let Some(existing) = specs.get(&spec.name)
        && existing.module_id != module_id
    {
        return Err(FeeNameError::Taken);
    }
    Ok(())
}

fn check_quotes(quotes: &[(u32, u64)], max_sat_per_kvb: u64) -> Option<SubmitFeeTableError> {
    if quotes.is_empty() {
        return Some(SubmitFeeTableError::EmptyQuotes);
    }
    if quotes.len() > MAX_EXTERNAL_QUOTES {
        return Some(SubmitFeeTableError::TooManyQuotes);
    }
    let mut previous: Option<u32> = None;
    for (target, rate) in quotes {
        if !(1..=MAX_FEE_TARGET).contains(target) {
            return Some(SubmitFeeTableError::BadTarget);
        }
        if previous.is_some_and(|prev| *target <= prev) {
            return Some(SubmitFeeTableError::TargetsNotIncreasing);
        }
        if *rate < 1 {
            return Some(SubmitFeeTableError::RateTooLow);
        }
        if *rate > max_sat_per_kvb {
            return Some(SubmitFeeTableError::RateTooHigh);
        }
        previous = Some(*target);
    }
    None
}

fn best_auto(
    state: &State,
    allow: &[String],
    tip_hash: Hash,
    ttl: Duration,
    now: Instant,
) -> Option<String> {
    let mut best: Option<(&str, u32)> = None;
    for (name, record) in &state.specs {
        let Some(priority) = record.spec.auto_priority else {
            continue;
        };
        if !allow.iter().any(|id| id == &record.module_id) {
            continue;
        }
        let fresh = state.tables.get(name).is_some_and(|stored| {
            stored.table.tip_hash == tip_hash && now.saturating_duration_since(stored.at) < ttl
        });
        if !fresh {
            continue;
        }
        let replace = match best {
            None => true,
            Some((prev_name, prev_priority)) => {
                priority > prev_priority || (priority == prev_priority && name.as_str() > prev_name)
            }
        };
        if replace {
            best = Some((name.as_str(), priority));
        }
    }
    best.map(|(name, _)| name.to_string())
}

pub fn parse_mode_name(text: &str) -> Result<FeeMode, SubmitFeeTableError> {
    match text {
        "conservative" => Ok(FeeMode::Conservative),
        "economical" => Ok(FeeMode::Economical),
        _ => Err(SubmitFeeTableError::BadMode),
    }
}

pub fn parse_merge_name(text: &str) -> Result<ExternalMode, FeeNameError> {
    match text {
        "lower_only" => Ok(ExternalMode::LowerOnly),
        "replace" => Ok(ExternalMode::Replace),
        _ => Err(FeeNameError::BadMerge),
    }
}

pub fn parse_modes(modes: &[String]) -> Result<Vec<FeeMode>, SubmitFeeTableError> {
    modes.iter().map(|mode| parse_mode_name(mode)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, priority: Option<u32>) -> FeeEstimatorSpec {
        FeeEstimatorSpec {
            name: name.to_string(),
            description: name.to_string(),
            modes: vec![FeeMode::Conservative],
            default_merge: ExternalMode::LowerOnly,
            auto_priority: priority,
        }
    }

    fn table(name: &str, tip: Hash, height: u64) -> ExternalFeeTable {
        ExternalFeeTable {
            estimator: name.to_string(),
            tip_hash: tip,
            upstream_height: height,
            mode: None,
            quotes: vec![(1, 2_000), (6, 1_000)],
        }
    }

    #[test]
    fn registration_rejects_reserved_taken_and_bad_names() {
        let reg = ExternalRegistry::new();
        let first = reg.register(
            "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            vec![
                spec("blend", Some(1)),
                spec("native", None),
                spec("Bad", None),
            ],
        );
        assert_eq!(first.accepted, vec!["blend".to_string()]);
        assert!(
            first
                .rejected
                .iter()
                .any(|(name, err)| name == "native" && *err == FeeNameError::Reserved)
        );
        assert!(
            first
                .rejected
                .iter()
                .any(|(name, err)| name == "Bad" && *err == FeeNameError::BadFormat)
        );

        let second = reg.register(
            "other_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            vec![spec("blend", None)],
        );
        assert_eq!(second.accepted, Vec::<String>::new());
        assert_eq!(second.rejected[0].1, FeeNameError::Taken);
        assert!(reg.owns("fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "blend"));
    }

    #[test]
    fn register_replaces_the_modules_previous_names() {
        let reg = ExternalRegistry::new();
        let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        reg.register(id, vec![spec("blend", None), spec("other", None)]);
        let next = reg.register(id, vec![spec("blend", None)]);
        assert_eq!(next.accepted, vec!["blend".to_string()]);
        assert!(!reg.owns(id, "other"));
        assert!(reg.owns(id, "blend"));
    }

    #[test]
    fn submit_checks_owner_tip_lag_rate_and_quotes() {
        let reg = ExternalRegistry::new();
        let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        reg.register(id, vec![spec("blend", Some(2))]);
        let tip = [9u8; 32];
        let now = Instant::now();
        let err = reg
            .submit("nope", table("blend", tip, 10), tip, 10, 1, 10_000_000, now)
            .unwrap_err();
        assert_eq!(err, SubmitFeeTableError::NotOwner);

        let mut bad = table("blend", tip, 10);
        bad.quotes = vec![(6, 1), (1, 1)];
        assert_eq!(
            reg.submit(id, bad, tip, 10, 1, 10_000_000, now)
                .unwrap_err(),
            SubmitFeeTableError::TargetsNotIncreasing
        );

        assert_eq!(
            reg.submit(
                id,
                table("blend", [1u8; 32], 10),
                tip,
                10,
                1,
                10_000_000,
                now
            )
            .unwrap_err(),
            SubmitFeeTableError::StaleTip
        );
        assert_eq!(
            reg.submit(id, table("blend", tip, 20), tip, 10, 1, 10_000_000, now)
                .unwrap_err(),
            SubmitFeeTableError::HeightLag
        );
        reg.submit(id, table("blend", tip, 10), tip, 10, 1, 10_000_000, now)
            .unwrap();
        assert_eq!(
            reg.submit(
                id,
                table("blend", tip, 10),
                tip,
                10,
                1,
                10_000_000,
                now + Duration::from_millis(500)
            )
            .unwrap_err(),
            SubmitFeeTableError::RateLimited
        );
        reg.submit(
            id,
            table("blend", tip, 10),
            tip,
            10,
            1,
            10_000_000,
            now + Duration::from_secs(1),
        )
        .unwrap();
    }

    #[test]
    fn ttl_tip_change_reorg_and_unload_drop_tables() {
        let reg = ExternalRegistry::new();
        let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        reg.register(id, vec![spec("blend", Some(1))]);
        let tip = [4u8; 32];
        let now = Instant::now();
        reg.submit(id, table("blend", tip, 3), tip, 3, 1, 10_000_000, now)
            .unwrap();
        assert!(
            reg.fresh_table("blend", tip, Duration::from_secs(120), now)
                .is_some()
        );
        assert!(
            reg.fresh_table(
                "blend",
                tip,
                Duration::from_secs(120),
                now + Duration::from_secs(120)
            )
            .is_none()
        );
        let fallen = reg
            .select(
                "blend",
                true,
                &[],
                tip,
                Duration::from_secs(120),
                now + Duration::from_secs(120),
            )
            .unwrap();
        assert_eq!(fallen.active, "native");
        assert!(fallen.degraded);

        let next = [5u8; 32];
        reg.note_tip(next);
        assert!(
            reg.fresh_table("blend", tip, Duration::from_secs(120), now)
                .is_none()
        );
        reg.submit(
            id,
            table("blend", next, 4),
            next,
            4,
            1,
            10_000_000,
            now + Duration::from_secs(1),
        )
        .unwrap();
        reg.clear_tables();
        assert!(
            reg.fresh_table("blend", next, Duration::from_secs(120), now)
                .is_none()
        );
        assert!(reg.owns(id, "blend"));
        reg.drop_module(id);
        assert!(!reg.owns(id, "blend"));
    }

    #[test]
    fn auto_uses_the_highest_fresh_allowed_claim() {
        let reg = ExternalRegistry::new();
        let low = "low_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let high = "high_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        reg.register(low, vec![spec("low-est", Some(1))]);
        reg.register(high, vec![spec("high-est", Some(5))]);
        let tip = [7u8; 32];
        let now = Instant::now();
        reg.submit(low, table("low-est", tip, 1), tip, 1, 1, 10_000_000, now)
            .unwrap();
        reg.submit(high, table("high-est", tip, 1), tip, 1, 1, 10_000_000, now)
            .unwrap();
        let ttl = Duration::from_secs(120);
        let denied = reg.select("auto", true, &[], tip, ttl, now).unwrap();
        assert_eq!(denied.active, "native");
        assert!(!denied.degraded);

        let picked = reg
            .select(
                "auto",
                true,
                &[high.to_string(), low.to_string()],
                tip,
                ttl,
                now,
            )
            .unwrap();
        assert_eq!(picked.active, "high-est");
        assert!(!picked.degraded);

        let missing = reg.select("blend", true, &[], tip, ttl, now).unwrap();
        assert!(missing.degraded);
        assert!(missing.unresolved);
        assert_eq!(missing.active, "native");

        let stale = reg.select("blend", false, &[], tip, ttl, now).unwrap_err();
        assert_eq!(stale, super::super::types::FeeUnavailable::ExternalStale);
    }

    #[test]
    fn description_and_quote_bounds_are_typed() {
        let reg = ExternalRegistry::new();
        let id = "fees_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut long = spec("blend", None);
        long.description = "x".repeat(MAX_ESTIMATOR_DESCRIPTION_BYTES + 1);
        let rejected = reg.register(id, vec![long]);
        assert_eq!(rejected.rejected[0].1, FeeNameError::DescriptionTooLong);
        let mut ok = spec("blend", None);
        ok.description = "x".repeat(MAX_ESTIMATOR_DESCRIPTION_BYTES);
        assert_eq!(
            reg.register(id, vec![ok]).accepted,
            vec!["blend".to_string()]
        );

        let tip = [2u8; 32];
        let now = Instant::now();
        let mut empty = table("blend", tip, 1);
        empty.quotes.clear();
        assert_eq!(
            reg.submit(id, empty, tip, 1, 1, 10_000, now).unwrap_err(),
            SubmitFeeTableError::EmptyQuotes
        );
        let mut low = table("blend", tip, 1);
        low.quotes = vec![(1, 0)];
        assert_eq!(
            reg.submit(id, low, tip, 1, 1, 10_000, now).unwrap_err(),
            SubmitFeeTableError::RateTooLow
        );
        let mut high = table("blend", tip, 1);
        high.quotes = vec![(1, 10_001)];
        assert_eq!(
            reg.submit(id, high, tip, 1, 1, 10_000, now).unwrap_err(),
            SubmitFeeTableError::RateTooHigh
        );
        let mut zero = table("blend", tip, 1);
        zero.quotes = vec![(0, 1)];
        assert_eq!(
            reg.submit(id, zero, tip, 1, 1, 10_000, now).unwrap_err(),
            SubmitFeeTableError::BadTarget
        );
        let mut past = table("blend", tip, 1);
        past.quotes = vec![(MAX_FEE_TARGET + 1, 1)];
        assert_eq!(
            reg.submit(id, past, tip, 1, 1, 10_000, now).unwrap_err(),
            SubmitFeeTableError::BadTarget
        );
        let mut many = table("blend", tip, 1);
        many.quotes = (1..=MAX_EXTERNAL_QUOTES as u32 + 1)
            .map(|target| (target, 1))
            .collect();
        assert_eq!(
            reg.submit(id, many, tip, 1, 1, 10_000, now).unwrap_err(),
            SubmitFeeTableError::TooManyQuotes
        );
    }
}

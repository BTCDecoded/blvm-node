//! Fee estimator configuration.
//!
//! The history constants (bucket spacing, decays, success thresholds) are fixed
//! in the estimator. This struct exposes only the operator knobs.

use serde::{Deserialize, Serialize};

/// Operator settings for the built-in fee estimator.
///
/// Absent `[fees]` uses [`FeeEstimatorConfig::default`], which leaves the estimator on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeeEstimatorConfig {
    pub enabled: bool,
    pub history_enabled: bool,
    pub mempool_enabled: bool,
    pub allow_mempool_only: bool,

    /// How many projected blocks may lower a history estimate. 1..=12.
    pub mempool_horizon_blocks: u32,
    /// Weight percentile for conservative quotes. A lower percentile is a higher rate. 1..=99.
    pub mempool_percentile_conservative: u8,
    /// Weight percentile for economical quotes. Must be >= the conservative percentile. 1..=99.
    pub mempool_percentile_economical: u8,
    /// A projected block lighter than this fraction of max weight quotes the floor.
    /// `0.0` disables that rule.
    pub projection_full_threshold: f64,

    /// Recent blocks that must be present before the mempool layer is trusted. 1..=144.
    pub health_window_blocks: usize,
    /// Aggregate share of block weight that must have been in the local pool. 0.0..=1.0.
    pub health_min_ratio: f64,
    /// `None` means required on every network except regtest.
    #[serde(default)]
    pub health_required: Option<bool>,

    /// Seconds between table rebuilds while the tip is unchanged. 1..=300.
    pub recompute_interval_secs: u64,
    /// Scales the estimate. Does not scale the floor. 0.5..=10.0.
    pub multiplier: f64,
    /// Extra floor in sat/kvB, applied together with the relay floor.
    #[serde(default)]
    pub min_fee_sat_per_kvb: Option<u64>,

    pub state_path: String,
    /// Connected blocks between state writes. 1..=1008.
    pub persist_interval_blocks: u32,
    pub max_state_age_blocks: u64,

    /// JSONL path. Absent or blank writes nothing. Each accepted block appends
    /// the quotes from just before that block and the block's fee-rate band.
    #[serde(default)]
    pub eval_log_path: Option<String>,

    /// Confirmation targets the lnd `feeurl` route reports.
    ///
    /// lnd `WebAPIEstimator.getCachedFee` (`lnwallet/chainfee/estimator.go`)
    /// uses an exact map hit, then walks down to `minBlockTarget` (1), then
    /// the cheapest cached target. `MaxBlockTarget` is 1008. The default list
    /// starts at 1 so a target lnd does not find stays on a higher fee.
    pub lnd_targets: Vec<u32>,

    /// `auto`, `native`, or a name a module registers. An unknown name is kept.
    pub estimator: String,
    /// `native` serves the built-in quotes when the selection is unusable.
    /// `none` fails the quote instead.
    pub fallback: String,
    /// Session module ids allowed to claim `auto`.
    pub allow_auto_override: Vec<String>,
    pub external: ExternalFeeLimits,
}

/// `[fees.external]`. Per-name tables such as `[fees.external.blend]` set `mode`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalFeeLimits {
    /// 10..=3600. A stored table older than this is stale.
    pub ttl_secs: u64,
    /// 0..=6. `|node_height - upstream_height|` may not exceed this.
    pub max_lag_blocks: u64,
    /// Highest accepted external rate, in sat/kvB.
    pub max_sat_per_kvb: u64,
    #[serde(flatten)]
    pub by_name: std::collections::HashMap<String, ExternalEstimatorOverride>,
}

/// Optional `[fees.external.<name>]`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExternalEstimatorOverride {
    /// `lower_only` or `replace`. Absent keeps the module's default.
    #[serde(default)]
    pub mode: Option<String>,
}

impl Default for ExternalFeeLimits {
    fn default() -> Self {
        Self {
            ttl_secs: 120,
            max_lag_blocks: 1,
            max_sat_per_kvb: 10_000_000,
            by_name: std::collections::HashMap::new(),
        }
    }
}

impl Default for FeeEstimatorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            history_enabled: true,
            mempool_enabled: true,
            allow_mempool_only: true,
            mempool_horizon_blocks: 1,
            mempool_percentile_conservative: 50,
            mempool_percentile_economical: 75,
            projection_full_threshold: 0.95,
            health_window_blocks: 6,
            health_min_ratio: 0.75,
            health_required: None,
            recompute_interval_secs: 7,
            multiplier: 1.0,
            min_fee_sat_per_kvb: None,
            state_path: "data/fee_estimator.dat".to_string(),
            persist_interval_blocks: 6,
            max_state_age_blocks: 6048,
            eval_log_path: None,
            lnd_targets: vec![1, 2, 3, 6, 12, 24, 48, 144, 504, 1008],
            estimator: "auto".to_string(),
            fallback: "native".to_string(),
            allow_auto_override: Vec::new(),
            external: ExternalFeeLimits::default(),
        }
    }
}

impl FeeEstimatorConfig {
    /// Reject a value outside the range named in the design. The error names the key.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.history_enabled && !self.mempool_enabled {
            anyhow::bail!("history_enabled and mempool_enabled cannot both be false");
        }
        if !(1..=12).contains(&self.mempool_horizon_blocks) {
            anyhow::bail!(
                "mempool_horizon_blocks must be from 1 to 12, got {}",
                self.mempool_horizon_blocks
            );
        }
        if !(1..=99).contains(&self.mempool_percentile_conservative) {
            anyhow::bail!(
                "mempool_percentile_conservative must be from 1 to 99, got {}",
                self.mempool_percentile_conservative
            );
        }
        if !(1..=99).contains(&self.mempool_percentile_economical) {
            anyhow::bail!(
                "mempool_percentile_economical must be from 1 to 99, got {}",
                self.mempool_percentile_economical
            );
        }
        if self.mempool_percentile_economical < self.mempool_percentile_conservative {
            anyhow::bail!(
                "mempool_percentile_economical must be >= mempool_percentile_conservative"
            );
        }
        if !self.projection_full_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.projection_full_threshold)
        {
            anyhow::bail!(
                "projection_full_threshold must be from 0.0 to 1.0, got {}",
                self.projection_full_threshold
            );
        }
        if !(1..=144).contains(&self.health_window_blocks) {
            anyhow::bail!(
                "health_window_blocks must be from 1 to 144, got {}",
                self.health_window_blocks
            );
        }
        if !self.health_min_ratio.is_finite() || !(0.0..=1.0).contains(&self.health_min_ratio) {
            anyhow::bail!(
                "health_min_ratio must be from 0.0 to 1.0, got {}",
                self.health_min_ratio
            );
        }
        if !(1..=300).contains(&self.recompute_interval_secs) {
            anyhow::bail!(
                "recompute_interval_secs must be from 1 to 300, got {}",
                self.recompute_interval_secs
            );
        }
        if !self.multiplier.is_finite() || !(0.5..=10.0).contains(&self.multiplier) {
            anyhow::bail!(
                "multiplier must be from 0.5 to 10.0, got {}",
                self.multiplier
            );
        }
        if self.state_path.trim().is_empty() {
            anyhow::bail!("state_path must not be empty");
        }
        if !(1..=1008).contains(&self.persist_interval_blocks) {
            anyhow::bail!(
                "persist_interval_blocks must be from 1 to 1008, got {}",
                self.persist_interval_blocks
            );
        }
        for target in &self.lnd_targets {
            if !(1..=1008).contains(target) {
                anyhow::bail!("lnd_targets values must be from 1 to 1008, got {target}");
            }
        }
        if !estimator_name_ok(&self.estimator) {
            anyhow::bail!(
                "estimator must match [a-z0-9][a-z0-9-]{{0,31}}, got {}",
                self.estimator
            );
        }
        if self.fallback != "native" && self.fallback != "none" {
            anyhow::bail!("fallback must be native or none, got {}", self.fallback);
        }
        if !(10..=3600).contains(&self.external.ttl_secs) {
            anyhow::bail!(
                "ttl_secs must be from 10 to 3600, got {}",
                self.external.ttl_secs
            );
        }
        if self.external.max_lag_blocks > 6 {
            anyhow::bail!(
                "max_lag_blocks must be from 0 to 6, got {}",
                self.external.max_lag_blocks
            );
        }
        if self.external.max_sat_per_kvb < 1 {
            anyhow::bail!(
                "max_sat_per_kvb must be at least 1, got {}",
                self.external.max_sat_per_kvb
            );
        }
        for (name, over) in &self.external.by_name {
            if let Some(mode) = over.mode.as_deref() {
                if mode != "lower_only" && mode != "replace" {
                    anyhow::bail!(
                        "fees.external.{name}.mode must be lower_only or replace, got {mode}"
                    );
                }
            }
        }
        Ok(())
    }
}

/// Design 20.2. `auto` and `native` match. Reserved names are rejected at registration, not here.
pub(crate) fn estimator_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() && !first.is_ascii_lowercase() {
        return false;
    }
    for (extra, ch) in chars.enumerate() {
        if extra == 31 || !(ch.is_ascii_digit() || ch.is_ascii_lowercase() || ch == '-') {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        assert!(FeeEstimatorConfig::default().validate().is_ok());
    }

    #[test]
    fn both_layers_off_names_the_keys() {
        let mut cfg = FeeEstimatorConfig::default();
        cfg.history_enabled = false;
        cfg.mempool_enabled = false;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("history_enabled"), "{err}");
        assert!(err.contains("mempool_enabled"), "{err}");
    }

    #[test]
    fn horizon_and_percentile_ranges_name_the_key() {
        let mut cfg = FeeEstimatorConfig::default();
        cfg.mempool_horizon_blocks = 0;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mempool_horizon_blocks"), "{err}");

        let mut cfg = FeeEstimatorConfig::default();
        cfg.mempool_percentile_economical = 10;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mempool_percentile_economical"), "{err}");
    }

    #[test]
    fn partial_table_fills_the_defaults() {
        let cfg: FeeEstimatorConfig = toml::from_str("mempool_horizon_blocks = 3\n").unwrap();
        assert_eq!(cfg.mempool_horizon_blocks, 3);
        assert!(cfg.history_enabled);
        assert_eq!(cfg.state_path, "data/fee_estimator.dat");
        assert!(cfg.health_required.is_none());
        assert_eq!(cfg.estimator, "auto");
        assert_eq!(cfg.fallback, "native");
        assert_eq!(cfg.external.ttl_secs, 120);
    }

    #[test]
    fn external_ranges_name_the_key() {
        let mut cfg = FeeEstimatorConfig::default();
        cfg.external.ttl_secs = 9;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("ttl_secs"), "{err}");

        let mut cfg = FeeEstimatorConfig::default();
        cfg.external.max_lag_blocks = 7;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("max_lag_blocks"), "{err}");

        let mut cfg = FeeEstimatorConfig::default();
        cfg.fallback = "blend".to_string();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("fallback"), "{err}");

        let mut cfg = FeeEstimatorConfig::default();
        cfg.estimator = "Blend".to_string();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("estimator"), "{err}");
    }

    #[test]
    fn external_table_parses_a_per_name_mode() {
        let cfg: FeeEstimatorConfig = toml::from_str(
            r#"
estimator = "blend"
fallback = "native"

[external]
ttl_secs = 30
max_lag_blocks = 2
max_sat_per_kvb = 5000

[external.blend]
mode = "replace"
"#,
        )
        .unwrap();
        assert_eq!(cfg.estimator, "blend");
        assert_eq!(cfg.external.ttl_secs, 30);
        assert_eq!(cfg.external.max_lag_blocks, 2);
        assert_eq!(cfg.external.max_sat_per_kvb, 5000);
        assert_eq!(
            cfg.external.by_name["blend"].mode.as_deref(),
            Some("replace")
        );
        assert!(cfg.validate().is_ok());
    }
}

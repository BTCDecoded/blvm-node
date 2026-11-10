//! Mempool policy and RBF (Replace-By-Fee) configuration.

use blvm_protocol::spam_filter::SpamFilterConfigSerializable;
use serde::{Deserialize, Serialize};

/// RBF (Replace-By-Fee) mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RbfMode {
    /// RBF disabled - no replacements allowed
    Disabled,

    /// Conservative RBF - strict BIP125 rules with additional safety checks
    Conservative,

    /// Standard RBF - Bitcoin Core's default. A fee-paying replacement is admitted
    /// whether or not the original signals BIP125.
    Standard,

    /// Aggressive RBF - relaxed rules for miners
    Aggressive,
}

/// RBF (Replace-By-Fee) configuration
#[derive(Debug, Clone, Serialize)]
pub struct RbfConfig {
    #[serde(default = "default_rbf_mode")]
    pub mode: RbfMode,

    #[serde(default = "default_rbf_fee_rate_multiplier")]
    pub min_fee_rate_multiplier: f64,

    #[serde(default = "default_rbf_min_fee_bump")]
    pub min_fee_bump_satoshis: u64,

    #[serde(default = "default_rbf_min_confirmations")]
    pub min_confirmations: u32,

    #[serde(default = "default_rbf_allow_packages")]
    pub allow_package_replacements: bool,

    #[serde(default = "default_rbf_max_replacements")]
    pub max_replacements_per_tx: u32,

    #[serde(default = "default_rbf_cooldown_seconds")]
    pub cooldown_seconds: u64,
}

fn default_rbf_mode() -> RbfMode {
    RbfMode::Standard
}

fn default_rbf_fee_rate_multiplier() -> f64 {
    1.0
}

fn default_rbf_min_fee_bump() -> u64 {
    1000
}

fn default_rbf_min_confirmations() -> u32 {
    0
}

fn default_rbf_allow_packages() -> bool {
    true
}

fn default_rbf_max_replacements() -> u32 {
    10
}

fn default_rbf_cooldown_seconds() -> u64 {
    0
}

impl Default for RbfConfig {
    fn default() -> Self {
        Self {
            mode: RbfMode::Standard,
            min_fee_rate_multiplier: 1.0,
            min_fee_bump_satoshis: 1000,
            min_confirmations: 0,
            allow_package_replacements: true,
            max_replacements_per_tx: 10,
            cooldown_seconds: 0,
        }
    }
}

impl RbfConfig {
    /// Get mode-specific defaults
    pub fn with_mode(mode: RbfMode) -> Self {
        match mode {
            RbfMode::Disabled => Self {
                mode: RbfMode::Disabled,
                min_fee_rate_multiplier: f64::INFINITY,
                min_fee_bump_satoshis: u64::MAX,
                min_confirmations: u32::MAX,
                allow_package_replacements: false,
                max_replacements_per_tx: 0,
                cooldown_seconds: u64::MAX,
            },
            RbfMode::Conservative => Self {
                mode: RbfMode::Conservative,
                min_fee_rate_multiplier: 2.0,
                min_fee_bump_satoshis: 5000,
                min_confirmations: 1,
                allow_package_replacements: false,
                max_replacements_per_tx: 3,
                cooldown_seconds: 300,
            },
            RbfMode::Standard => Self::default(),
            RbfMode::Aggressive => Self {
                mode: RbfMode::Aggressive,
                min_fee_rate_multiplier: 1.05,
                min_fee_bump_satoshis: 500,
                min_confirmations: 0,
                allow_package_replacements: true,
                max_replacements_per_tx: 10,
                cooldown_seconds: 60,
            },
        }
    }
}

impl<'de> Deserialize<'de> for RbfConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default = "default_rbf_mode")]
            mode: RbfMode,
            min_fee_rate_multiplier: Option<f64>,
            min_fee_bump_satoshis: Option<u64>,
            min_confirmations: Option<u32>,
            allow_package_replacements: Option<bool>,
            max_replacements_per_tx: Option<u32>,
            cooldown_seconds: Option<u64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let explicit = raw.min_fee_rate_multiplier.is_some()
            || raw.min_fee_bump_satoshis.is_some()
            || raw.min_confirmations.is_some()
            || raw.allow_package_replacements.is_some()
            || raw.max_replacements_per_tx.is_some()
            || raw.cooldown_seconds.is_some();
        if !explicit {
            return Ok(Self::with_mode(raw.mode));
        }
        let base = Self::default();
        Ok(Self {
            mode: raw.mode,
            min_fee_rate_multiplier: raw
                .min_fee_rate_multiplier
                .unwrap_or(base.min_fee_rate_multiplier),
            min_fee_bump_satoshis: raw
                .min_fee_bump_satoshis
                .unwrap_or(base.min_fee_bump_satoshis),
            min_confirmations: raw.min_confirmations.unwrap_or(base.min_confirmations),
            allow_package_replacements: raw
                .allow_package_replacements
                .unwrap_or(base.allow_package_replacements),
            max_replacements_per_tx: raw
                .max_replacements_per_tx
                .unwrap_or(base.max_replacements_per_tx),
            cooldown_seconds: raw.cooldown_seconds.unwrap_or(base.cooldown_seconds),
        })
    }
}

/// Transaction eviction strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvictionStrategy {
    LowestFeeRate,
    OldestFirst,
    LargestFirst,
    NoDescendantsFirst,
    Hybrid,
    SpamFirst,
}

/// Mempool policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MempoolPolicyConfig {
    #[serde(default = "default_max_mempool_mb")]
    pub max_mempool_mb: u64,

    #[serde(default = "default_max_mempool_txs")]
    pub max_mempool_txs: usize,

    /// Satoshis per 1,000 virtual bytes. A 67-virtual-byte transaction at the
    /// default of 100 pays 7 satoshis, rounded up.
    #[serde(default = "default_min_relay_fee_rate")]
    pub min_relay_fee_rate: u64,

    /// Weight units charged per sigop when sizing a transaction for relay and mining.
    /// Virtual size is `ceil(max(weight, sigop_cost * bytes_per_sigop) / 4)`.
    /// Default 20 matches Bitcoin Core `-bytespersigop` (5 vbytes per sigop).
    #[serde(default = "default_bytes_per_sigop")]
    pub bytes_per_sigop: u64,

    /// Count signature operations when sizing a transaction for relay and mining.
    /// Default true. `--disable-sigop` turns this off for the running node.
    #[serde(default = "default_count_sigops")]
    pub count_sigops: bool,

    #[serde(default = "default_min_tx_fee")]
    pub min_tx_fee: u64,

    /// Satoshis per kilovirtualbyte that a replacement must add, on top of the
    /// fees of every transaction it removes, to pay for its own relay.
    #[serde(default = "default_incremental_relay_fee")]
    pub incremental_relay_fee: u64,

    #[serde(default = "default_max_ancestor_count")]
    pub max_ancestor_count: u32,

    #[serde(default = "default_max_ancestor_size")]
    pub max_ancestor_size: u64,

    #[serde(default = "default_max_descendant_count")]
    pub max_descendant_count: u32,

    #[serde(default = "default_max_descendant_size")]
    pub max_descendant_size: u64,

    #[serde(default = "default_eviction_strategy")]
    pub eviction_strategy: EvictionStrategy,

    #[serde(default = "default_mempool_expiry_hours")]
    pub mempool_expiry_hours: u64,

    #[serde(default)]
    pub persist_mempool: bool,

    #[serde(default = "default_mempool_persistence_path")]
    pub mempool_persistence_path: String,

    #[serde(default = "default_tx_rate_limit_burst")]
    pub tx_rate_limit_burst: u32,

    #[serde(default = "default_tx_rate_limit_per_sec")]
    pub tx_rate_limit_per_sec: u32,

    #[serde(default = "default_tx_byte_rate_limit")]
    pub tx_byte_rate_limit: u64,

    #[serde(default = "default_tx_byte_rate_burst")]
    pub tx_byte_rate_burst: u64,

    /// Reject a transaction whose weight is above the standard limit, or whose
    /// output script is not a standard type. Default on.
    #[serde(default = "default_require_standard")]
    pub require_standard: bool,

    /// Reject transactions classified as spam before they enter the mempool (opt-in).
    /// When `spam_filter` is set, it overrides default spam filter parameters.
    #[serde(default = "default_reject_spam_in_mempool")]
    pub reject_spam_in_mempool: bool,

    /// Optional spam filter tuning when `reject_spam_in_mempool` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spam_filter: Option<SpamFilterConfigSerializable>,
}

fn default_max_mempool_mb() -> u64 {
    300
}

fn default_max_mempool_txs() -> usize {
    100_000
}

fn default_min_relay_fee_rate() -> u64 {
    100
}

fn default_bytes_per_sigop() -> u64 {
    20
}

fn default_min_tx_fee() -> u64 {
    0
}

fn default_incremental_relay_fee() -> u64 {
    100
}

fn default_max_ancestor_count() -> u32 {
    25
}

fn default_max_ancestor_size() -> u64 {
    101_000
}

fn default_max_descendant_count() -> u32 {
    25
}

fn default_max_descendant_size() -> u64 {
    101_000
}

fn default_eviction_strategy() -> EvictionStrategy {
    EvictionStrategy::LowestFeeRate
}

fn default_mempool_expiry_hours() -> u64 {
    336
}

fn default_mempool_persistence_path() -> String {
    "data/mempool.dat".to_string()
}

fn default_tx_rate_limit_burst() -> u32 {
    10
}

fn default_tx_rate_limit_per_sec() -> u32 {
    1
}

fn default_tx_byte_rate_limit() -> u64 {
    100_000
}

fn default_tx_byte_rate_burst() -> u64 {
    1_000_000
}

fn default_require_standard() -> bool {
    true
}

fn default_reject_spam_in_mempool() -> bool {
    false
}

fn default_count_sigops() -> bool {
    true
}

/// Copy a present `[mempool]` onto the consensus mempool, and a present `[rbf]`
/// fee bump onto `rbf_fee_increment`. Absent sections leave the env-loaded values.
pub fn overlay_mempool_onto_consensus(
    consensus: &mut blvm_protocol::consensus_config::MempoolConfig,
    mempool: Option<&MempoolPolicyConfig>,
    rbf: Option<&RbfConfig>,
) {
    if let Some(policy) = mempool {
        consensus.max_mempool_mb = policy.max_mempool_mb;
        consensus.max_mempool_txs = policy.max_mempool_txs;
        consensus.mempool_expiry_hours = policy.mempool_expiry_hours;
        consensus.min_relay_fee_rate = policy.min_relay_fee_rate;
        consensus.min_tx_fee = i64::try_from(policy.min_tx_fee).unwrap_or(i64::MAX);
    }
    if let Some(rbf) = rbf {
        consensus.rbf_fee_increment = i64::try_from(rbf.min_fee_bump_satoshis).unwrap_or(i64::MAX);
    }
}

impl Default for MempoolPolicyConfig {
    fn default() -> Self {
        Self {
            max_mempool_mb: 300,
            max_mempool_txs: 100_000,
            min_relay_fee_rate: 100,
            bytes_per_sigop: 20,
            count_sigops: true,
            min_tx_fee: 0,
            incremental_relay_fee: 100,
            max_ancestor_count: 25,
            max_ancestor_size: 101_000,
            max_descendant_count: 25,
            max_descendant_size: 101_000,
            eviction_strategy: EvictionStrategy::LowestFeeRate,
            mempool_expiry_hours: 336,
            persist_mempool: false,
            mempool_persistence_path: "data/mempool.dat".to_string(),
            tx_rate_limit_burst: 10,
            tx_rate_limit_per_sec: 1,
            tx_byte_rate_limit: 100_000,
            tx_byte_rate_burst: 1_000_000,
            require_standard: true,
            reject_spam_in_mempool: false,
            spam_filter: None,
        }
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::{MempoolPolicyConfig, RbfConfig, overlay_mempool_onto_consensus};
    use blvm_protocol::consensus_config::MempoolConfig;

    #[test]
    fn file_mempool_replaces_env_size() {
        let mut consensus = MempoolConfig::default();
        consensus.max_mempool_mb = 300;
        let mut policy = MempoolPolicyConfig::default();
        policy.max_mempool_mb = 50;
        overlay_mempool_onto_consensus(&mut consensus, Some(&policy), None);
        assert_eq!(consensus.max_mempool_mb, 50);
    }

    #[test]
    fn absent_mempool_keeps_env_size() {
        let mut consensus = MempoolConfig::default();
        consensus.max_mempool_mb = 300;
        overlay_mempool_onto_consensus(&mut consensus, None, None);
        assert_eq!(consensus.max_mempool_mb, 300);
    }

    #[test]
    fn mode_only_rbf_uses_the_preset() {
        let cfg: RbfConfig = toml::from_str("mode = \"conservative\"").unwrap();
        assert_eq!(cfg.min_fee_bump_satoshis, 5000);
        assert_eq!(cfg.cooldown_seconds, 300);
    }

    #[test]
    fn explicit_rbf_number_keeps_the_other_defaults() {
        let cfg: RbfConfig =
            toml::from_str("mode = \"conservative\"\nmin_fee_bump_satoshis = 1").unwrap();
        assert_eq!(cfg.min_fee_bump_satoshis, 1);
        assert_eq!(cfg.cooldown_seconds, 0);
        assert!(cfg.allow_package_replacements);
    }

    #[test]
    fn rbf_section_copies_fee_bump() {
        let mut consensus = MempoolConfig::default();
        consensus.rbf_fee_increment = 9;
        let mut rbf = RbfConfig::default();
        rbf.min_fee_bump_satoshis = 5_000;
        overlay_mempool_onto_consensus(&mut consensus, None, Some(&rbf));
        assert_eq!(consensus.rbf_fee_increment, 5_000);
    }
}

//! Mempool policy and RBF (Replace-By-Fee) configuration.

use serde::{Deserialize, Serialize};

/// RBF (Replace-By-Fee) mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RbfMode {
    /// RBF disabled - no replacements allowed
    Disabled,

    /// Conservative RBF - strict BIP125 rules with additional safety checks
    Conservative,

    /// Standard RBF - strict BIP125 compliance (default)
    Standard,

    /// Aggressive RBF - relaxed rules for miners
    Aggressive,
}

/// RBF (Replace-By-Fee) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    1.1
}

fn default_rbf_min_fee_bump() -> u64 {
    1000
}

fn default_rbf_min_confirmations() -> u32 {
    0
}

fn default_rbf_allow_packages() -> bool {
    false
}

fn default_rbf_max_replacements() -> u32 {
    10
}

fn default_rbf_cooldown_seconds() -> u64 {
    60
}

impl Default for RbfConfig {
    fn default() -> Self {
        Self {
            mode: RbfMode::Standard,
            min_fee_rate_multiplier: 1.1,
            min_fee_bump_satoshis: 1000,
            min_confirmations: 0,
            allow_package_replacements: false,
            max_replacements_per_tx: 10,
            cooldown_seconds: 60,
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

    #[serde(default = "default_min_relay_fee_rate")]
    pub min_relay_fee_rate: u64,

    #[serde(default = "default_min_tx_fee")]
    pub min_tx_fee: u64,

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
}

fn default_max_mempool_mb() -> u64 {
    300
}

fn default_max_mempool_txs() -> usize {
    100_000
}

fn default_min_relay_fee_rate() -> u64 {
    1
}

fn default_min_tx_fee() -> u64 {
    1000
}

fn default_incremental_relay_fee() -> u64 {
    1000
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

impl Default for MempoolPolicyConfig {
    fn default() -> Self {
        Self {
            max_mempool_mb: 300,
            max_mempool_txs: 100_000,
            min_relay_fee_rate: 1,
            min_tx_fee: 1000,
            incremental_relay_fee: 1000,
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
        }
    }
}

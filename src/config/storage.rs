//! Storage configuration: database backend, pruning, indexing, cache.

use serde::{Deserialize, Serialize};

use super::{default_false, default_true};

/// Pruning mode configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PruningMode {
    /// No pruning (keep all blocks)
    Disabled,

    /// Normal pruning (keep recent blocks for verification)
    Normal {
        #[serde(default = "default_zero")]
        keep_from_height: u64,
        #[serde(default = "default_min_recent_blocks")]
        min_recent_blocks: u64,
    },

    /// Aggressive pruning with UTXO commitments
    Aggressive {
        #[serde(default = "default_zero")]
        keep_from_height: u64,
        #[serde(default = "default_true")]
        keep_commitments: bool,
        #[serde(default = "default_false")]
        keep_filtered_blocks: bool,
        #[serde(default = "default_min_blocks")]
        min_blocks: u64,
    },

    /// Custom pruning configuration
    Custom {
        #[serde(default = "default_true")]
        keep_headers: bool,
        #[serde(default = "default_zero")]
        keep_bodies_from_height: u64,
        #[serde(default = "default_false")]
        keep_commitments: bool,
        #[serde(default = "default_false")]
        keep_filters: bool,
        #[serde(default = "default_false")]
        keep_filtered_blocks: bool,
        #[serde(default = "default_false")]
        keep_witnesses: bool,
        #[serde(default = "default_false")]
        keep_tx_index: bool,
    },
}

fn default_zero() -> u64 {
    0
}

fn default_min_recent_blocks() -> u64 {
    288
}

fn default_min_blocks() -> u64 {
    144
}

/// Pruning configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruningConfig {
    #[serde(default = "default_pruning_mode")]
    pub mode: PruningMode,

    #[serde(default = "default_false")]
    pub auto_prune: bool,

    #[serde(default = "default_auto_prune_interval")]
    pub auto_prune_interval: u64,

    #[serde(default = "default_min_blocks_to_keep")]
    pub min_blocks_to_keep: u64,

    #[serde(default = "default_false")]
    pub prune_on_startup: bool,

    #[serde(default = "default_false")]
    pub incremental_prune_during_ibd: bool,

    #[serde(default = "default_prune_window_size")]
    pub prune_window_size: u64,

    #[serde(default = "default_min_blocks_for_incremental_prune")]
    pub min_blocks_for_incremental_prune: u64,

    #[cfg(feature = "utxo-commitments")]
    pub utxo_commitments: Option<UtxoCommitmentsPruningConfig>,

    #[cfg(feature = "bip158")]
    pub bip158_filters: Option<Bip158PruningConfig>,
}

fn default_pruning_mode() -> PruningMode {
    PruningMode::Aggressive {
        keep_from_height: 0,
        keep_commitments: true,
        keep_filtered_blocks: false,
        min_blocks: 144,
    }
}

fn default_auto_prune_interval() -> u64 {
    144
}

fn default_min_blocks_to_keep() -> u64 {
    144
}

fn default_prune_window_size() -> u64 {
    144
}

fn default_min_blocks_for_incremental_prune() -> u64 {
    288
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            mode: PruningMode::Aggressive {
                keep_from_height: 0,
                keep_commitments: true,
                keep_filtered_blocks: false,
                min_blocks: 144,
            },
            auto_prune: true,
            auto_prune_interval: 144,
            min_blocks_to_keep: 144,
            prune_on_startup: false,
            incremental_prune_during_ibd: true,
            prune_window_size: 144,
            min_blocks_for_incremental_prune: 288,
            #[cfg(feature = "utxo-commitments")]
            utxo_commitments: None,
            #[cfg(feature = "bip158")]
            bip158_filters: None,
        }
    }
}

impl PruningConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let PruningMode::Aggressive { .. } = self.mode {
            #[cfg(not(feature = "utxo-commitments"))]
            {
                return Err(anyhow::anyhow!(
                    "Aggressive pruning mode requires the 'utxo-commitments' feature to be enabled. \
                    Please enable it in Cargo.toml or use Normal pruning mode instead."
                ));
            }
        }

        if self.min_blocks_to_keep == 0 {
            return Err(anyhow::anyhow!(
                "min_blocks_to_keep must be greater than 0 for safety"
            ));
        }

        if self.auto_prune && self.auto_prune_interval == 0 {
            return Err(anyhow::anyhow!(
                "auto_prune_interval must be greater than 0 when auto_prune is enabled"
            ));
        }

        match &self.mode {
            PruningMode::Normal {
                min_recent_blocks, ..
            } => {
                if *min_recent_blocks == 0 {
                    return Err(anyhow::anyhow!(
                        "min_recent_blocks must be greater than 0 in Normal pruning mode"
                    ));
                }
            }
            PruningMode::Aggressive { min_blocks, .. } => {
                if *min_blocks == 0 {
                    return Err(anyhow::anyhow!(
                        "min_blocks must be greater than 0 in Aggressive pruning mode"
                    ));
                }
            }
            PruningMode::Custom { keep_headers, .. } => {
                if !keep_headers {
                    return Err(anyhow::anyhow!(
                        "keep_headers must be true in Custom pruning mode (required for PoW verification)"
                    ));
                }
            }
            PruningMode::Disabled => {}
        }

        Ok(())
    }
}

#[cfg(feature = "utxo-commitments")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoCommitmentsPruningConfig {
    #[serde(default = "default_true")]
    pub keep_commitments: bool,
    #[serde(default = "default_false")]
    pub keep_filtered_blocks: bool,
    #[serde(default = "default_true")]
    pub generate_before_prune: bool,
    #[serde(default = "default_commitment_max_age")]
    pub max_commitment_age_days: u32,
}

#[cfg(feature = "utxo-commitments")]
fn default_commitment_max_age() -> u32 {
    0
}

#[cfg(feature = "utxo-commitments")]
impl Default for UtxoCommitmentsPruningConfig {
    fn default() -> Self {
        Self {
            keep_commitments: true,
            keep_filtered_blocks: false,
            generate_before_prune: true,
            max_commitment_age_days: 0,
        }
    }
}

#[cfg(feature = "bip158")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bip158PruningConfig {
    #[serde(default = "default_true")]
    pub keep_filters: bool,
    #[serde(default = "default_true")]
    pub keep_filter_headers: bool,
    #[serde(default = "default_filter_max_age")]
    pub max_filter_age_days: u32,
}

#[cfg(feature = "bip158")]
fn default_filter_max_age() -> u32 {
    0
}

#[cfg(feature = "bip158")]
impl Default for Bip158PruningConfig {
    fn default() -> Self {
        Self {
            keep_filters: true,
            keep_filter_headers: true,
            max_filter_age_days: 0,
        }
    }
}

/// Database backend configuration
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseBackendConfig {
    Sled,
    Redb,
    Rocksdb,
    Tidesdb,
    Auto,
}

fn default_database_backend() -> DatabaseBackendConfig {
    DatabaseBackendConfig::Auto
}

fn default_storage_path() -> String {
    "data".to_string()
}

fn default_dbcache_mb() -> usize {
    450
}

/// RocksDB tuning. Config file overrides ENV; ENV overrides defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RocksDBConfig {
    /// Parallelism (default: nproc or 4)
    #[serde(default)]
    pub parallelism: Option<i32>,

    /// Max background compactions (default: 2 on <=16GB RAM, 4 on 32GB+)
    #[serde(default)]
    pub max_background_compactions: Option<i32>,

    /// Max background flushes (default: 2 on <=16GB RAM, 4 on 32GB+)
    #[serde(default)]
    pub max_background_flushes: Option<i32>,

    /// Level-0 compaction trigger (default: 8)
    #[serde(default = "default_rocksdb_level0_trigger")]
    pub level0_compaction_trigger: i32,

    /// Write buffer size in MB (default: 128–256 by RAM)
    #[serde(default)]
    pub write_buffer_mb: Option<usize>,
}

fn default_rocksdb_level0_trigger() -> i32 {
    8
}

impl Default for RocksDBConfig {
    fn default() -> Self {
        Self {
            parallelism: None,
            max_background_compactions: None,
            max_background_flushes: None,
            level0_compaction_trigger: 8,
            write_buffer_mb: None,
        }
    }
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_database_backend")]
    pub database_backend: DatabaseBackendConfig,

    #[serde(default = "default_storage_path")]
    pub data_dir: String,

    #[serde(default = "default_dbcache_mb")]
    pub dbcache_mb: usize,

    /// RocksDB tuning (config > ENV > defaults)
    #[serde(default)]
    pub rocksdb: Option<RocksDBConfig>,

    pub tidesdb: Option<TidesDBConfig>,
    pub pruning: Option<PruningConfig>,
    pub cache: Option<StorageCacheConfig>,
    #[serde(default)]
    pub indexing: Option<IndexingConfig>,
    #[cfg(feature = "compression")]
    pub compression: Option<CompressionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TidesDBConfig {
    #[serde(default = "default_tidesdb_utxo_klog_threshold")]
    pub utxo_klog_threshold: usize,

    /// Flush threads. ENV BLVM_TIDESDB_FLUSH_THREADS overrides config.
    #[serde(default = "default_tidesdb_flush_threads")]
    pub flush_threads: i32,

    /// Compaction threads. ENV BLVM_TIDESDB_COMPACT_THREADS overrides config.
    #[serde(default = "default_tidesdb_compact_threads")]
    pub compact_threads: i32,
}

fn default_tidesdb_utxo_klog_threshold() -> usize {
    64 * 1024
}

fn default_tidesdb_flush_threads() -> i32 {
    4
}

fn default_tidesdb_compact_threads() -> i32 {
    4
}

impl Default for TidesDBConfig {
    fn default() -> Self {
        Self {
            utxo_klog_threshold: default_tidesdb_utxo_klog_threshold(),
            flush_threads: default_tidesdb_flush_threads(),
            compact_threads: default_tidesdb_compact_threads(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageCacheConfig {
    #[serde(default = "default_block_cache_mb")]
    pub block_cache_mb: usize,
    #[serde(default = "default_utxo_cache_mb")]
    pub utxo_cache_mb: usize,
    #[serde(default = "default_header_cache_mb")]
    pub header_cache_mb: usize,
}

fn default_block_cache_mb() -> usize {
    100
}

fn default_utxo_cache_mb() -> usize {
    50
}

fn default_header_cache_mb() -> usize {
    10
}

impl Default for StorageCacheConfig {
    fn default() -> Self {
        Self {
            block_cache_mb: 100,
            utxo_cache_mb: 50,
            header_cache_mb: 10,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_backend: DatabaseBackendConfig::Auto,
            data_dir: "data".to_string(),
            dbcache_mb: 450,
            rocksdb: None,
            tidesdb: None,
            pruning: None,
            cache: None,
            indexing: None,
            #[cfg(feature = "compression")]
            compression: None,
        }
    }
}

#[cfg(feature = "compression")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    #[serde(default = "default_true")]
    pub block_compression_enabled: bool,
    #[serde(default = "default_block_compression_level")]
    pub block_compression_level: u32,
    #[serde(default = "default_true")]
    pub utxo_compression_enabled: bool,
    #[serde(default = "default_utxo_compression_level")]
    pub utxo_compression_level: u32,
    #[serde(default = "default_true")]
    pub witness_compression_enabled: bool,
    #[serde(default = "default_witness_compression_level")]
    pub witness_compression_level: u32,
}

#[cfg(feature = "compression")]
fn default_block_compression_level() -> u32 {
    3
}

#[cfg(feature = "compression")]
fn default_utxo_compression_level() -> u32 {
    1
}

#[cfg(feature = "compression")]
fn default_witness_compression_level() -> u32 {
    2
}

#[cfg(feature = "compression")]
impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            block_compression_enabled: true,
            block_compression_level: 3,
            utxo_compression_enabled: true,
            utxo_compression_level: 1,
            witness_compression_enabled: true,
            witness_compression_level: 2,
        }
    }
}

/// Indexing strategy
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexingStrategy {
    Eager,
    Lazy,
}

fn default_indexing_strategy() -> IndexingStrategy {
    IndexingStrategy::Eager
}

fn default_max_indexed_addresses() -> usize {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingConfig {
    #[serde(default)]
    pub enable_address_index: bool,
    #[serde(default)]
    pub enable_value_index: bool,
    #[serde(default = "default_indexing_strategy")]
    pub strategy: IndexingStrategy,
    #[serde(default = "default_max_indexed_addresses")]
    pub max_indexed_addresses: usize,
    #[serde(default)]
    pub enable_compression: bool,
    #[serde(default)]
    pub background_indexing: bool,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            enable_address_index: false,
            enable_value_index: false,
            strategy: IndexingStrategy::Eager,
            max_indexed_addresses: 0,
            enable_compression: false,
            background_indexing: false,
        }
    }
}

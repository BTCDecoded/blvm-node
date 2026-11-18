//! Storage configuration: database backend, pruning, indexing, cache.

use serde::{Deserialize, Serialize};

use super::{default_false, default_true};

/// How many recent blocks a prune keeps. Blocks between automatic passes.
pub const PRUNE_KEEP_BLOCKS: u64 = 288;
pub const AUTO_PRUNE_INTERVAL: u64 = 144;

/// Which bodies a prune keeps. The window is [`PruningConfig::keep_blocks`],
/// a sibling key in `[storage.pruning]`.
///
/// `aggressive` also writes UTXO commitments for the bodies it drops and
/// drops their witnesses. Headers stay in every mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PruningMode {
    /// Keep every body. `keep_blocks` is ignored.
    Disabled,
    /// Keep the last `keep_blocks` bodies.
    Normal,
    /// Keep the last `keep_blocks` bodies, with UTXO commitments.
    Aggressive,
}

fn default_keep_blocks() -> u64 {
    PRUNE_KEEP_BLOCKS
}

/// Pruning configuration.
///
/// `Default` is the file policy: each field uses the same value as a key
/// omitted from TOML.
///
/// One window, `keep_blocks`, beside `mode` in `[storage.pruning]`. Automatic
/// passes run every [`AUTO_PRUNE_INTERVAL`] blocks when `auto_prune` is set.
/// Prune deletes a block body this store copied, and only when an undo exists.
/// With `reuse_core_block_files`, bodies stay in Core `blocks/` and the reader
/// keeps serving them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruningConfig {
    /// `disabled` keeps every body. `normal` keeps `keep_blocks` bodies.
    /// `aggressive` does that and writes UTXO commitments before the delete.
    /// It requires the `utxo-commitments` feature. Omitted in a file: `aggressive`.
    #[serde(default = "default_pruning_mode")]
    pub mode: PruningMode,

    /// Trailing bodies `normal` and `aggressive` keep. `disabled` ignores this.
    /// Omitted in a file: [`PRUNE_KEEP_BLOCKS`].
    #[serde(default = "default_keep_blocks")]
    pub keep_blocks: u64,

    /// When true, prune again every [`AUTO_PRUNE_INTERVAL`] blocks.
    /// Omitted in a file: false.
    #[serde(default = "default_false")]
    pub auto_prune: bool,

    /// Parallel IBD skips writing a body that is already outside `keep_blocks`
    /// when this flag is on and the mode is not `disabled`. Deleting those
    /// bodies during IBD still requires `aggressive` and UTXO commitments.
    /// The block-accepted prune does not read this flag. Omitted in a file: false.
    #[serde(default = "default_false")]
    pub incremental_prune_during_ibd: bool,
}

fn default_pruning_mode() -> PruningMode {
    PruningMode::Aggressive
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            mode: default_pruning_mode(),
            keep_blocks: default_keep_blocks(),
            auto_prune: default_false(),
            incremental_prune_during_ibd: default_false(),
        }
    }
}

impl PruningConfig {
    /// Recent bodies this mode keeps. `None` when pruning is off.
    pub fn keep_window(&self) -> Option<u64> {
        match self.mode {
            PruningMode::Disabled => None,
            PruningMode::Normal | PruningMode::Aggressive => Some(self.keep_blocks),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        #[cfg(not(feature = "utxo-commitments"))]
        if self.mode == PruningMode::Aggressive {
            return Err(anyhow::anyhow!(
                "Aggressive pruning mode requires the 'utxo-commitments' feature to be enabled. \
                Please enable it in Cargo.toml or use Normal pruning mode instead."
            ));
        }

        if self.mode != PruningMode::Disabled && self.keep_blocks == 0 {
            return Err(anyhow::anyhow!(
                "keep_blocks must be greater than 0 when pruning is enabled"
            ));
        }

        Ok(())
    }
}

/// Database backend configuration
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseBackendConfig {
    Sled,
    Redb,
    Rocksdb,
    Heed3,
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

/// RocksDB tuning. ENV overrides the file; the file overrides the RAM-tier default.
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

/// heed3 / LMDB tuning. ENV overrides the file; the file overrides auto-tune.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Heed3Config {
    /// LMDB map size in megabytes. When unset, auto-tuned from free disk (70%, clamped
    /// 48 GiB–4 TiB) plus `data.mdb + 128 GiB` headroom on resume. Override via this
    /// field or `BLVM_HEED3_MAP_SIZE_MB` only when operator policy requires a fixed cap.
    #[serde(default)]
    pub map_size_mb: Option<usize>,

    /// Maximum concurrent read transactions (default: 512).
    #[serde(default)]
    pub max_readers: Option<u32>,

    /// Maximum named sub-databases / trees (default: KNOWN_TREE_NAMES + 8).
    #[serde(default)]
    pub max_dbs: Option<u32>,
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

    /// RocksDB tuning (ENV > file > RAM-tier default)
    #[serde(default)]
    pub rocksdb: Option<RocksDBConfig>,

    /// heed3 / LMDB options when `database_backend = "heed3"`.
    #[serde(default)]
    pub heed3: Option<Heed3Config>,

    pub pruning: Option<PruningConfig>,
    pub cache: Option<StorageCacheConfig>,
    #[serde(default)]
    pub indexing: Option<IndexingConfig>,
    #[cfg(feature = "compression")]
    pub compression: Option<CompressionConfig>,

    /// Auto-migrate Bitcoin Core datadir into BLVM native store on startup (default: true).
    /// Disabled by `--no-auto-migrate` or `BLVM_NO_AUTO_MIGRATE_CORE=1`.
    #[serde(default = "default_auto_migrate_core")]
    pub auto_migrate_core: bool,

    /// Override BLVM store path when migrating from Core (default: `<datadir>/blvm/`).
    #[serde(default)]
    pub core_migrate_destination: Option<String>,

    /// Keep Core `blocks/` in place during migrate; BLVM reads blk*.dat via fallback reader (saves disk).
    /// Default **true**: migrating from a full Core datadir does not copy ~700 GB of block files.
    /// Set **`false`** or **`BLVM_REUSE_CORE_BLOCK_FILES=0`** to copy block bodies into the BLVM store.
    #[serde(default = "default_reuse_core_block_files")]
    pub reuse_core_block_files: bool,
}

fn default_auto_migrate_core() -> bool {
    true
}

fn default_reuse_core_block_files() -> bool {
    true
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
            heed3: None,
            pruning: None,
            cache: None,
            indexing: None,
            #[cfg(feature = "compression")]
            compression: None,
            auto_migrate_core: true,
            core_migrate_destination: None,
            reuse_core_block_files: true,
        }
    }
}

impl StorageConfig {
    /// Whether startup should auto-migrate a Core datadir (ENV overrides config).
    pub fn auto_migrate_core_effective(&self) -> bool {
        if crate::utils::env_bool("BLVM_NO_AUTO_MIGRATE_CORE") {
            return false;
        }
        if std::env::var("BLVM_AUTO_MIGRATE_CORE").is_ok() {
            return crate::utils::env_bool("BLVM_AUTO_MIGRATE_CORE");
        }
        self.auto_migrate_core
    }

    /// Resolved BLVM store directory when migrating from Core inside `data_dir`.
    pub fn resolve_core_migrate_destination(
        &self,
        data_dir: &std::path::Path,
    ) -> std::path::PathBuf {
        let dest = std::env::var("BLVM_CORE_MIGRATE_DESTINATION")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                self.core_migrate_destination
                    .as_ref()
                    .map(std::path::PathBuf::from)
            })
            .unwrap_or_else(|| data_dir.join("blvm"));
        if crate::config::path_is_strictly_within(data_dir, &dest) {
            return dest;
        }
        let fallback = data_dir.join("blvm");
        tracing::warn!(
            "core migrate destination {} is outside data_dir {} or equal to it; using {}",
            dest.display(),
            data_dir.display(),
            fallback.display()
        );
        fallback
    }

    /// Whether migrate should skip copying block bodies and read Core `blocks/` in place.
    pub fn reuse_core_block_files_effective(&self) -> bool {
        if std::env::var("BLVM_REUSE_CORE_BLOCK_FILES").is_ok() {
            return crate::utils::env_bool("BLVM_REUSE_CORE_BLOCK_FILES");
        }
        self.reuse_core_block_files
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

#[cfg(test)]
mod prune_keep_floor {
    use super::{PRUNE_KEEP_BLOCKS, PruningConfig, PruningMode};

    #[test]
    fn omitted_pruning_matches_default_and_keeps_288() {
        let config = PruningConfig::default();
        assert_eq!(config.mode, PruningMode::Aggressive);
        assert_eq!(config.keep_window(), Some(PRUNE_KEEP_BLOCKS));
        assert!(!config.auto_prune);
        assert!(!config.incremental_prune_during_ibd);

        let omitted: PruningConfig = toml::from_str("").unwrap();
        assert_eq!(omitted.mode, config.mode);
        assert_eq!(omitted.keep_blocks, config.keep_blocks);
        assert_eq!(omitted.auto_prune, config.auto_prune);
        assert_eq!(
            omitted.incremental_prune_during_ibd,
            config.incremental_prune_during_ibd
        );

        let normal: PruningConfig = toml::from_str("mode = \"normal\"\nkeep_blocks = 144").unwrap();
        assert_eq!(normal.mode, PruningMode::Normal);
        assert_eq!(normal.keep_blocks, 144);
        assert!(toml::from_str::<PruningConfig>("min_blocks_to_keep = 144").is_err());
        assert!(toml::from_str::<PruningConfig>("[mode]\ntype = \"normal\"\n").is_err());
        assert!(
            PruningConfig {
                mode: PruningMode::Normal,
                keep_blocks: 0,
                ..PruningConfig::default()
            }
            .validate()
            .is_err()
        );
    }
}

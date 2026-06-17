//! Bitcoin Core to BLVM migration logic.
//!
//! Used by `blvm migrate core` and the standalone migrate-bitcoin-core binary.

use super::assumeutxo::AssumeUtxoManager;
use super::bitcoin_core_blocks::{BitcoinCoreBlockReader, read_block_at_file_pos};
use super::bitcoin_core_format::{
    BLOCK_HAVE_DATA, core_utxo_key_to_outpoint_key, get_key_prefix, parse_best_block_value,
    parse_block_index_key, parse_coin, parse_core_coin, parse_disk_block_index, read_core_varint,
};
use super::bitcoin_core_obfuscation::CoreDbObfuscation;
use super::bitcoin_core_storage::BitcoinCoreStorage;
use super::bitcoin_detection::{BitcoinCoreDetection, CoreDataNetwork};
use super::blockstore::BlockStore;
use super::chainstate::{ChainInfo, ChainParams, ChainState};
use super::database::{DatabaseBackend, create_database, default_backend};
use super::txindex::TxIndex;
use super::utxostore::UtxoStore;
use anyhow::{Context, Result};
use blvm_muhash::{MuHash3072, serialize_coin_for_muhash};
use blvm_protocol::{Hash, UTXO};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::{info, warn};

/// On-disk marker written after a successful Core → BLVM migration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MigrationMarker {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub network: String,
    pub tip_hash: String,
    pub height: u64,
    /// Core-compatible UTXO set MuHash (hex), when computed at migration time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muhash: Option<String>,
    /// When true, block bodies were not copied; BLVM reads Core `blocks/` in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_core_blocks: Option<bool>,
    pub migrated_at: String,
}

/// Migration phase for resumable checkpoints (`blvm_meta/migration_checkpoint.json`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    Chainstate,
    BlockIndexes,
    Blocks,
    Complete,
}

/// In-progress migration state (removed when `migration.json` is written).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MigrationCheckpoint {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub network: String,
    pub phase: MigrationPhase,
    pub utxos_migrated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_utxo_key: Option<String>,
    pub block_indexes_migrated: u64,
    pub blocks_migrated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_block_height: Option<u64>,
    pub updated_at: String,
}

/// Path to the resumable migration checkpoint under a BLVM store directory.
pub fn migration_checkpoint_path(dest_dir: &Path) -> PathBuf {
    dest_dir.join("blvm_meta").join("migration_checkpoint.json")
}

pub fn read_migration_checkpoint(dest_dir: &Path) -> Result<Option<MigrationCheckpoint>> {
    let path = migration_checkpoint_path(dest_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&data)?))
}

pub fn clear_migration_checkpoint(dest_dir: &Path) -> Result<()> {
    let path = migration_checkpoint_path(dest_dir);
    if path.is_file() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

const CHECKPOINT_UTXO_INTERVAL: u64 = 10_000;
const CHECKPOINT_BLOCK_INTERVAL: u64 = 1_000;
const DEFAULT_BLOCK_MIGRATE_BATCH_SIZE: usize = 256;

/// One block body to read from Core `blocks/` during migrate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockImportJob {
    height: u64,
    hash: [u8; 32],
    file_loc: Option<(i32, u32)>,
}

fn default_core_migrate_block_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 8))
        .unwrap_or(4)
}

/// Worker count for parallel Core block reads (`BLVM_CORE_MIGRATE_BLOCK_WORKERS`, default ~cpus/2 capped at 8).
pub fn core_migrate_block_workers_effective() -> usize {
    let requested = std::env::var("BLVM_CORE_MIGRATE_BLOCK_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(default_core_migrate_block_workers);
    let cap = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    requested.min(cap.max(1))
}

fn core_migrate_block_batch_size() -> usize {
    std::env::var("BLVM_CORE_MIGRATE_BLOCK_BATCH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_BLOCK_MIGRATE_BATCH_SIZE)
}

fn prepare_block_import_jobs(
    mut jobs: Vec<BlockImportJob>,
    resume_after_height: Option<u64>,
) -> Vec<BlockImportJob> {
    jobs.sort_by_key(|j| j.height);
    if let Some(h) = resume_after_height {
        jobs.retain(|j| j.height > h);
    }
    jobs
}

fn read_block_for_job(
    blocks_dir: &Path,
    network: CoreDataNetwork,
    job: &BlockImportJob,
    fallback_reader: Option<&Arc<std::sync::Mutex<BitcoinCoreBlockReader>>>,
) -> Result<Option<blvm_protocol::Block>> {
    if let Some((n_file, n_data_pos)) = job.file_loc {
        read_block_at_file_pos(blocks_dir, n_file, n_data_pos, network)
    } else if let Some(reader) = fallback_reader {
        reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read_block(&job.hash)
    } else {
        Ok(None)
    }
}

/// Skip Core coin keys at or before the checkpoint key (LevelDB iteration order).
fn should_skip_utxo_key(key: &[u8], resume_after: Option<&[u8]>) -> bool {
    let Some(last) = resume_after else {
        return false;
    };
    !last.is_empty() && key <= last
}

fn checkpoint_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Optional early exit for tests (`BLVM_CORE_MIGRATE_STOP_AFTER=chainstate|block_indexes|blocks`).
fn env_stop_after_phase() -> Option<MigrationPhase> {
    let v = std::env::var("BLVM_CORE_MIGRATE_STOP_AFTER").ok()?;
    match v.as_str() {
        "chainstate" => Some(MigrationPhase::Chainstate),
        "block_indexes" => Some(MigrationPhase::BlockIndexes),
        "blocks" => Some(MigrationPhase::Blocks),
        _ => None,
    }
}

/// Path to the migration marker under a BLVM store directory.
pub fn migration_marker_path(dest_dir: &Path) -> PathBuf {
    dest_dir.join("blvm_meta").join("migration.json")
}

/// Returns true when a migration marker exists for `dest_dir`.
pub fn has_migration_marker(dest_dir: &Path) -> bool {
    migration_marker_path(dest_dir).is_file()
}

/// Read migration marker if present.
pub fn read_migration_marker(dest_dir: &Path) -> Result<Option<MigrationMarker>> {
    let path = migration_marker_path(dest_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&data)?))
}

/// Read Bitcoin Core chain tip hash from `core_dir/chainstate` (`'B'` key).
#[cfg(feature = "rocksdb")]
pub fn read_core_best_block_hash(core_dir: &Path) -> Result<Option<[u8; 32]>> {
    let chainstate = core_dir.join("chainstate");
    if BitcoinCoreDetection::detect_db_format(&chainstate).is_err() {
        return Ok(None);
    }
    let db = Arc::from(BitcoinCoreStorage::open_bitcoin_core_database(
        core_dir,
        CoreDataNetwork::Mainnet,
    )?);
    let obf = CoreDbObfuscation::load(&db)?;
    let tree = db.open_tree("default")?;
    for item in tree.iter() {
        let (key, mut value) = item?;
        if key.len() == 1 && key[0] == b'B' {
            obf.deobfuscate_value(&key, &mut value);
            return Ok(Some(parse_best_block_value(&value)?));
        }
    }
    Ok(None)
}

#[cfg(not(feature = "rocksdb"))]
pub fn read_core_best_block_hash(_core_dir: &Path) -> Result<Option<[u8; 32]>> {
    Ok(None)
}

/// Migration parameters.
#[derive(Debug, Clone)]
pub struct MigrateCoreArgs {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub network: CoreDataNetwork,
    pub verify: bool,
    pub verbose: bool,
    pub dest_backend: Option<DatabaseBackend>,
    /// Stop after this phase (checkpoint saved). Used by tests; env `BLVM_CORE_MIGRATE_STOP_AFTER` is a fallback.
    pub stop_after: Option<MigrationPhase>,
    /// Skip copying block bodies; keep reading Core `blocks/` via `BitcoinCoreBlockReader`.
    pub reuse_core_block_files: bool,
}

/// Run Bitcoin Core to BLVM migration (initializes tracing subscriber).
pub fn run_migrate_core(args: MigrateCoreArgs) -> Result<()> {
    if args.verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
    }

    migrate_core_data(args)
}

/// Run migration without touching the global tracing subscriber (for node startup).
pub fn migrate_core_data(args: MigrateCoreArgs) -> Result<()> {
    info!("[CORE_IMPORT] Bitcoin Core to BLVM migration starting");
    info!("[CORE_IMPORT] Source: {:?}", args.source);
    info!("[CORE_IMPORT] Destination: {:?}", args.destination);
    info!("[CORE_IMPORT] Network: {:?}", args.network);

    if !args.source.exists() {
        anyhow::bail!("Source directory does not exist: {:?}", args.source);
    }

    let chainstate = args.source.join("chainstate");
    if BitcoinCoreDetection::detect_db_format(&chainstate).is_err() {
        anyhow::bail!("Invalid Bitcoin Core database format in {:?}", chainstate);
    }

    BitcoinCoreStorage::ensure_not_locked(&args.source)?;

    if args.reuse_core_block_files {
        let cov = assess_core_block_coverage(&args.source, args.network)?;
        if !cov.tip_readable {
            anyhow::bail!(
                "Cannot read tip block at height {} from Core blk*.dat. \
                 reuse_core_block_files requires readable Core block files at the tip.",
                cov.tip_height
            );
        }
    } else {
        let cov = assess_core_block_coverage(&args.source, args.network)?;
        if let Some(msg) = cov.pruned_error_message() {
            anyhow::bail!("{msg}");
        }
    }

    std::fs::create_dir_all(&args.destination).with_context(|| {
        format!(
            "Failed to create destination directory {:?}",
            args.destination
        )
    })?;

    let backend = args.dest_backend.unwrap_or_else(default_backend);
    let migrator = Migrator::new(
        &args.source,
        &args.destination,
        args.network,
        backend,
        args.stop_after,
        args.reuse_core_block_files,
    )?;
    migrator.migrate(args.verify)?;

    if let Ok(Some(marker)) = read_migration_marker(&args.destination) {
        info!(
            "[CORE_IMPORT] layout=core action=complete dest={:?} height={} tip={}",
            args.destination, marker.height, marker.tip_hash
        );
    } else {
        info!(
            "[CORE_IMPORT] layout=core action=complete dest={:?}",
            args.destination
        );
    }

    Ok(())
}

struct Migrator {
    source_dir: PathBuf,
    dest_dir: PathBuf,
    network: CoreDataNetwork,
    stop_after: Option<MigrationPhase>,
    reuse_core_block_files: bool,
    chainstate_db: Arc<dyn crate::storage::database::Database>,
    chainstate_obf: CoreDbObfuscation,
    block_index_db: Option<Arc<dyn crate::storage::database::Database>>,
    block_index_obf: CoreDbObfuscation,
    dest_db: Arc<dyn crate::storage::database::Database>,
    progress: Arc<MigrationProgress>,
    tip_from_index: Arc<std::sync::Mutex<Option<(u64, Hash, blvm_protocol::BlockHeader)>>>,
}

struct MigrationProgress {
    coins_migrated: AtomicU64,
    blocks_migrated: AtomicU64,
    blocks_failed: AtomicU64,
    block_indexes_migrated: AtomicU64,
    start_time: Instant,
}

impl Migrator {
    fn new(
        source_dir: &Path,
        dest_dir: &Path,
        network: CoreDataNetwork,
        dest_backend: DatabaseBackend,
        stop_after: Option<MigrationPhase>,
        reuse_core_block_files: bool,
    ) -> Result<Self> {
        info!("[CORE_IMPORT] Opening chainstate...");
        let chainstate_db = Arc::from(BitcoinCoreStorage::open_bitcoin_core_database(
            source_dir, network,
        )?);
        let chainstate_obf = CoreDbObfuscation::load(&chainstate_db)?;

        let block_index_db = match BitcoinCoreStorage::open_bitcoin_core_block_index(source_dir) {
            Ok(db) => {
                info!("[CORE_IMPORT] Opened blocks/index LevelDB");
                Some(Arc::from(db))
            }
            Err(e) => {
                warn!("[CORE_IMPORT] blocks/index unavailable: {e}");
                None
            }
        };
        let block_index_obf = block_index_db
            .as_ref()
            .map(CoreDbObfuscation::load)
            .transpose()?
            .unwrap_or_default();

        info!("[CORE_IMPORT] Creating destination database ({dest_backend:?})...");
        let dest_db = Arc::from(create_database(dest_dir, dest_backend, None)?);

        Ok(Self {
            source_dir: source_dir.to_path_buf(),
            dest_dir: dest_dir.to_path_buf(),
            network,
            stop_after,
            reuse_core_block_files,
            chainstate_db,
            chainstate_obf,
            block_index_db,
            block_index_obf,
            dest_db,
            progress: Arc::new(MigrationProgress {
                coins_migrated: AtomicU64::new(0),
                blocks_migrated: AtomicU64::new(0),
                blocks_failed: AtomicU64::new(0),
                block_indexes_migrated: AtomicU64::new(0),
                start_time: Instant::now(),
            }),
            tip_from_index: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    fn migrate(&self, verify: bool) -> Result<()> {
        if has_migration_marker(&self.dest_dir) {
            info!("[CORE_IMPORT] Migration marker already present; skipping import");
            let _ = clear_migration_checkpoint(&self.dest_dir);
            return Ok(());
        }

        let mut cp = self.load_or_init_checkpoint()?;
        self.restore_progress_from_checkpoint(&cp);
        let stop_after = self.stop_after.or_else(env_stop_after_phase);

        if cp.phase == MigrationPhase::Chainstate {
            info!("[CORE_IMPORT] Phase: chainstate (UTXOs)");
            self.migrate_chainstate(&mut cp)?;
            cp.phase = MigrationPhase::BlockIndexes;
            cp.last_utxo_key = None;
            self.save_checkpoint(&cp)?;
            if stop_after == Some(MigrationPhase::Chainstate) {
                info!("[CORE_IMPORT] Stopped after chainstate (checkpoint saved)");
                return Ok(());
            }
        }

        if cp.phase == MigrationPhase::BlockIndexes {
            info!("[CORE_IMPORT] Phase: block indexes");
            self.migrate_block_indexes()?;
            cp.phase = MigrationPhase::Blocks;
            cp.block_indexes_migrated =
                self.progress.block_indexes_migrated.load(Ordering::Relaxed);
            cp.last_block_height = None;
            self.save_checkpoint(&cp)?;
            if stop_after == Some(MigrationPhase::BlockIndexes) {
                info!("[CORE_IMPORT] Stopped after block indexes (checkpoint saved)");
                return Ok(());
            }
        }

        if cp.phase == MigrationPhase::Blocks {
            if self.reuse_core_block_files {
                info!(
                    "[CORE_IMPORT] Phase: txindex from Core blocks/ (reuse_core_block_files — not copying block bodies)"
                );
            } else {
                info!("[CORE_IMPORT] Phase: block bodies");
            }
            self.migrate_blocks(&mut cp)?;
            cp.phase = MigrationPhase::Complete;
            cp.last_block_height = None;
            self.save_checkpoint(&cp)?;
            if stop_after == Some(MigrationPhase::Blocks) {
                info!("[CORE_IMPORT] Stopped after blocks (checkpoint saved)");
                return Ok(());
            }
        }

        self.write_chain_info()?;
        self.write_migration_marker()?;
        clear_migration_checkpoint(&self.dest_dir)?;

        if verify {
            info!("[CORE_IMPORT] Verifying migrated data...");
            self.verify()?;
        }

        self.print_summary();
        Ok(())
    }

    fn load_or_init_checkpoint(&self) -> Result<MigrationCheckpoint> {
        let network = self.network.to_string();
        if let Some(cp) = read_migration_checkpoint(&self.dest_dir)? {
            if cp.source == self.source_dir
                && cp.destination == self.dest_dir
                && cp.network == network
            {
                info!(
                    "[CORE_IMPORT] Resuming migration from checkpoint (phase={:?}, utxos={}, blocks={})",
                    cp.phase, cp.utxos_migrated, cp.blocks_migrated
                );
                return Ok(cp);
            }
            warn!(
                "[CORE_IMPORT] Ignoring stale migration checkpoint (source/dest/network mismatch)"
            );
        }
        Ok(MigrationCheckpoint {
            source: self.source_dir.clone(),
            destination: self.dest_dir.clone(),
            network,
            phase: MigrationPhase::Chainstate,
            utxos_migrated: 0,
            last_utxo_key: None,
            block_indexes_migrated: 0,
            blocks_migrated: 0,
            last_block_height: None,
            updated_at: checkpoint_timestamp(),
        })
    }

    fn save_checkpoint(&self, cp: &MigrationCheckpoint) -> Result<()> {
        let mut cp = cp.clone();
        cp.updated_at = checkpoint_timestamp();
        let path = migration_checkpoint_path(&self.dest_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&cp)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn restore_progress_from_checkpoint(&self, cp: &MigrationCheckpoint) {
        self.progress
            .coins_migrated
            .store(cp.utxos_migrated, Ordering::Relaxed);
        self.progress
            .block_indexes_migrated
            .store(cp.block_indexes_migrated, Ordering::Relaxed);
        self.progress
            .blocks_migrated
            .store(cp.blocks_migrated, Ordering::Relaxed);
    }

    fn migrate_chainstate(&self, cp: &mut MigrationCheckpoint) -> Result<()> {
        info!("[CORE_IMPORT] Migrating UTXOs from chainstate...");

        let source_tree = self.chainstate_db.open_tree("default")?;
        let dest_tree = self.dest_db.open_tree("utxos")?;

        let resume_after = cp.last_utxo_key.as_deref().map(hex::decode).transpose()?;
        let mut count = cp.utxos_migrated;
        let mut batch = Vec::new();
        const BATCH_SIZE: usize = 1000;
        let mut since_checkpoint = 0u64;

        for item in source_tree.iter() {
            let (key, mut value) = item?;
            if should_skip_utxo_key(&key, resume_after.as_deref()) {
                continue;
            }
            self.chainstate_obf.deobfuscate_value(&key, &mut value);
            let Some(prefix) = get_key_prefix(&key) else {
                continue;
            };

            let coin = match prefix {
                b'C' => parse_core_coin(&value)?,
                b'c' => parse_coin(&value)?,
                _ => continue,
            };

            let outpoint_key = core_utxo_key_to_outpoint_key(&key)?;
            let blvm_utxo = crate::storage::utxo_value_codec::encode_utxo_with_codec(
                crate::storage::utxo_value_codec::ValueCodec::for_database(self.dest_db.as_ref()),
                &UTXO {
                    value: coin.amount as i64,
                    script_pubkey: coin.script.into(),
                    height: coin.height as u64,
                    is_coinbase: coin.is_coinbase,
                },
            )?;

            batch.push((outpoint_key, blvm_utxo));

            if batch.len() >= BATCH_SIZE {
                self.flush_batch(dest_tree.as_ref(), &mut batch)?;
                count += BATCH_SIZE as u64;
                self.progress.coins_migrated.store(count, Ordering::Relaxed);
                cp.utxos_migrated = count;
                cp.last_utxo_key = Some(hex::encode(&key));
                since_checkpoint += BATCH_SIZE as u64;
                if since_checkpoint >= CHECKPOINT_UTXO_INTERVAL {
                    self.save_checkpoint(cp)?;
                    since_checkpoint = 0;
                }
                if count % 10_000 == 0 {
                    info!("[CORE_IMPORT] Migrated {count} UTXOs...");
                }
            }
        }

        if !batch.is_empty() {
            count += batch.len() as u64;
            self.flush_batch(dest_tree.as_ref(), &mut batch)?;
            cp.utxos_migrated = count;
        }

        self.progress.coins_migrated.store(count, Ordering::Relaxed);
        cp.utxos_migrated = count;
        info!("[CORE_IMPORT] Migrated {count} UTXOs");
        Ok(())
    }

    fn migrate_block_indexes(&self) -> Result<()> {
        let Some(ref block_index_db) = self.block_index_db else {
            warn!("[CORE_IMPORT] Skipping block index migration (no blocks/index DB)");
            return Ok(());
        };

        info!("[CORE_IMPORT] Migrating block indexes from blocks/index...");

        let source_tree = block_index_db.open_tree("default")?;
        let height_index = self.dest_db.open_tree("height_index")?;
        let hash_to_height = self.dest_db.open_tree("hash_to_height")?;
        let headers_tree = self.dest_db.open_tree("headers")?;

        let mut count = 0u64;
        let mut max_height = 0u64;
        let mut tip_hash = [0u8; 32];
        let mut tip_header = None;

        for item in source_tree.iter() {
            let (key, mut value) = item?;
            self.block_index_obf.deobfuscate_value(&key, &mut value);
            if get_key_prefix(&key) != Some(b'b') {
                continue;
            }

            let key_hash = parse_block_index_key(&key)?;
            let index = parse_disk_block_index(&value)?;
            let block_hash = if index.block_hash != key_hash {
                warn!(
                    "[CORE_IMPORT] Block index key/hash mismatch at height {}; using index key hash",
                    index.height
                );
                key_hash
            } else {
                index.block_hash
            };

            let height = index.height as u64;
            let height_key = height.to_be_bytes();
            height_index.insert(&height_key, block_hash.as_slice())?;
            hash_to_height.insert(block_hash.as_slice(), &height_key)?;

            let header_data = bincode::serialize(&index.header)?;
            headers_tree.insert(block_hash.as_slice(), &header_data)?;

            if height >= max_height {
                max_height = height;
                tip_hash = block_hash;
                tip_header = Some(index.header.clone());
            }

            count += 1;
            if count % 5000 == 0 {
                info!("[CORE_IMPORT] Migrated {count} block indexes...");
            }
        }

        if let Some(header) = tip_header {
            *self.tip_from_index.lock().unwrap() = Some((max_height, tip_hash, header));
        }

        self.progress
            .block_indexes_migrated
            .store(count, Ordering::Relaxed);
        info!("[CORE_IMPORT] Migrated {count} block indexes (tip height {max_height})");
        Ok(())
    }

    fn migrate_blocks(&self, cp: &mut MigrationCheckpoint) -> Result<()> {
        let blocks_dir = self.source_dir.join("blocks");
        if !blocks_dir.exists() {
            warn!("[CORE_IMPORT] No blocks/ directory — skipping block import");
            return Ok(());
        }

        let store_blocks = !self.reuse_core_block_files;
        if store_blocks {
            info!("[CORE_IMPORT] Migrating block bodies...");
        } else {
            info!("[CORE_IMPORT] Indexing transactions from Core blocks/...");
        }

        let blockstore = if store_blocks {
            Some(BlockStore::new(Arc::clone(&self.dest_db))?)
        } else {
            None
        };
        let txindex = TxIndex::new(Arc::clone(&self.dest_db))?;
        let height_index = self.dest_db.open_tree("height_index")?;
        let total = height_index.len()?;
        let mut count = cp.blocks_migrated;
        let mut failed = 0u64;
        let resume_after_height = cp.last_block_height;
        let mut since_checkpoint = 0u64;

        let index_by_height: HashMap<u64, (i32, u32, [u8; 32])> =
            if let Some(ref block_index_db) = self.block_index_db {
                self.load_block_locations(block_index_db)?
            } else {
                HashMap::new()
            };

        let jobs = self.collect_block_import_jobs(height_index.as_ref(), &index_by_height)?;
        let jobs = prepare_block_import_jobs(jobs, resume_after_height);
        if jobs.is_empty() {
            info!("[CORE_IMPORT] No block bodies pending import");
            return Ok(());
        }

        let mut workers = core_migrate_block_workers_effective();
        let batch_size = core_migrate_block_batch_size();
        #[cfg(not(feature = "rayon"))]
        if workers > 1 {
            warn!(
                "[CORE_IMPORT] Parallel block import requires the `rayon` feature; using 1 worker"
            );
            workers = 1;
        }
        if workers > 1 {
            info!(
                "[CORE_IMPORT] Parallel block import: {workers} workers, batch size {batch_size}"
            );
        }

        let needs_fallback = jobs.iter().any(|j| j.file_loc.is_none());
        let fallback_reader = if needs_fallback {
            Some(Arc::new(std::sync::Mutex::new(
                BitcoinCoreBlockReader::new_with_cache(
                    &blocks_dir,
                    self.network,
                    Some(self.dest_dir.as_path()),
                )?,
            )))
        } else {
            None
        };

        let action = if store_blocks { "Migrated" } else { "Indexed" };

        for batch in jobs.chunks(batch_size) {
            let read_results: Vec<Result<Option<blvm_protocol::Block>>> = if workers > 1 {
                #[cfg(feature = "rayon")]
                {
                    use rayon::prelude::*;
                    batch
                        .par_iter()
                        .map(|job| {
                            read_block_for_job(
                                &blocks_dir,
                                self.network,
                                job,
                                fallback_reader.as_ref(),
                            )
                        })
                        .collect()
                }
                #[cfg(not(feature = "rayon"))]
                {
                    batch
                        .iter()
                        .map(|job| {
                            read_block_for_job(
                                &blocks_dir,
                                self.network,
                                job,
                                fallback_reader.as_ref(),
                            )
                        })
                        .collect()
                }
            } else {
                batch
                    .iter()
                    .map(|job| {
                        read_block_for_job(&blocks_dir, self.network, job, fallback_reader.as_ref())
                    })
                    .collect()
            };

            for (job, block_result) in batch.iter().zip(read_results) {
                let height = job.height;
                let hash = job.hash;

                match block_result {
                    Ok(Some(block)) => {
                        let ok = if let Some(ref blockstore) = blockstore {
                            blockstore.store_block(&block).is_ok()
                        } else {
                            true
                        };
                        if ok {
                            if let Err(e) = txindex.index_block(&block, &hash, height) {
                                warn!(
                                    "[CORE_IMPORT] Failed to index transactions at height {height}: {e}"
                                );
                            }
                            count += 1;
                            cp.blocks_migrated = count;
                            cp.last_block_height = Some(height);
                            since_checkpoint += 1;
                            if since_checkpoint >= CHECKPOINT_BLOCK_INTERVAL {
                                self.save_checkpoint(cp)?;
                                since_checkpoint = 0;
                            }
                        } else {
                            failed += 1;
                        }
                    }
                    Ok(None) => {
                        failed += 1;
                        if failed <= 10 {
                            warn!(
                                "[CORE_IMPORT] Could not read block at height {height} ({})",
                                hex::encode(hash)
                            );
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        if failed <= 10 {
                            warn!("[CORE_IMPORT] Block read error at height {height}: {e}");
                        }
                    }
                }
            }

            if count > 0 && count % 1000 == 0 {
                info!("[CORE_IMPORT] {action} {count} / {total} blocks...");
            }
        }

        self.progress
            .blocks_migrated
            .store(count, Ordering::Relaxed);
        self.progress.blocks_failed.store(failed, Ordering::Relaxed);
        cp.blocks_migrated = count;
        if store_blocks {
            info!("[CORE_IMPORT] Migrated {count} blocks ({failed} missing/failed)");
        } else {
            info!(
                "[CORE_IMPORT] Indexed {count} blocks from Core blocks/ ({failed} missing/failed)"
            );
        }
        Ok(())
    }

    fn collect_block_import_jobs(
        &self,
        height_index: &dyn crate::storage::database::Tree,
        index_by_height: &HashMap<u64, (i32, u32, [u8; 32])>,
    ) -> Result<Vec<BlockImportJob>> {
        let mut jobs = Vec::new();
        for item in height_index.iter() {
            let (height_key, hash_bytes) = item?;
            if hash_bytes.len() != 32 || height_key.len() != 8 {
                continue;
            }
            let height = u64::from_be_bytes(
                height_key
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid height key"))?,
            );
            let mut hash = [0u8; 32];
            hash.copy_from_slice(hash_bytes.as_ref());
            let file_loc = index_by_height
                .get(&height)
                .map(|(n_file, n_data_pos, _)| (*n_file, *n_data_pos));
            jobs.push(BlockImportJob {
                height,
                hash,
                file_loc,
            });
        }
        Ok(jobs)
    }

    fn load_block_locations(
        &self,
        block_index_db: &Arc<dyn crate::storage::database::Database>,
    ) -> Result<HashMap<u64, (i32, u32, [u8; 32])>> {
        let source_tree = block_index_db.open_tree("default")?;
        let mut map = HashMap::new();
        for item in source_tree.iter() {
            let (key, mut value) = item?;
            self.block_index_obf.deobfuscate_value(&key, &mut value);
            if get_key_prefix(&key) != Some(b'b') {
                continue;
            }
            let index = parse_disk_block_index(&value)?;
            if index.status & BLOCK_HAVE_DATA == 0 {
                continue;
            }
            map.insert(
                index.height as u64,
                (index.n_file, index.n_data_pos, index.block_hash),
            );
        }
        Ok(map)
    }

    fn write_chain_info(&self) -> Result<()> {
        let chainstate = ChainState::new(Arc::clone(&self.dest_db))?;

        let tip_hash = self.read_best_block_hash()?;
        let tip_from_index = self.tip_from_index.lock().unwrap().clone();

        let (height, hash, header) = if let Some(best) = tip_hash {
            if let Some((h, idx_hash, idx_header)) = tip_from_index {
                if idx_hash == best {
                    (h, idx_hash, idx_header)
                } else {
                    warn!("[CORE_IMPORT] Best block hash != index tip; using chainstate 'B' entry");
                    let h = self.lookup_height_for_hash(&best)?.unwrap_or(0);
                    let header = self.lookup_header_for_hash(&best)?.unwrap_or_default();
                    (h, best, header)
                }
            } else {
                let h = self.lookup_height_for_hash(&best)?.unwrap_or(0);
                let header = self.lookup_header_for_hash(&best)?.unwrap_or_default();
                (h, best, header)
            }
        } else if let Some((h, hash, header)) = tip_from_index {
            (h, hash, header)
        } else {
            warn!("[CORE_IMPORT] No chain tip found — chain_info not written");
            return Ok(());
        };

        let network_name = match self.network {
            CoreDataNetwork::Mainnet => "mainnet",
            CoreDataNetwork::Testnet => "testnet",
            CoreDataNetwork::Regtest => "regtest",
            CoreDataNetwork::Signet => "signet",
        };

        let info = ChainInfo {
            tip_hash: hash,
            tip_header: header,
            height,
            total_work: 0,
            chain_params: ChainParams {
                network: network_name.to_string(),
                ..ChainParams::default()
            },
        };
        chainstate.store_chain_info(&info)?;
        info!(
            "[CORE_IMPORT] chain_info written: height={height} tip={}",
            hex::encode(hash)
        );
        Ok(())
    }

    fn write_migration_marker(&self) -> Result<()> {
        let tip_hash = self.read_best_block_hash()?.or_else(|| {
            self.tip_from_index
                .lock()
                .unwrap()
                .as_ref()
                .map(|(_, h, _)| *h)
        });

        let (height, hash_hex) = match tip_hash {
            Some(hash) => {
                let h = self
                    .lookup_height_for_hash(&hash)?
                    .or_else(|| {
                        self.tip_from_index
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|(height, _, _)| *height)
                    })
                    .unwrap_or(0);
                (h, super::hashing::hash_to_rpc_hex(&hash))
            }
            None => (0, String::new()),
        };

        let migrated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "0".to_string());

        let muhash = compute_dest_utxo_muhash(&self.dest_db)
            .ok()
            .map(hex::encode);

        let marker = MigrationMarker {
            source: self.source_dir.clone(),
            destination: self.dest_dir.clone(),
            network: self.network.to_string(),
            tip_hash: hash_hex,
            height,
            muhash,
            reuse_core_blocks: self.reuse_core_block_files.then_some(true),
            migrated_at,
        };

        let path = migration_marker_path(&self.dest_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&marker)?;
        std::fs::write(&path, json)?;
        info!("[CORE_IMPORT] Wrote migration marker at {:?}", path);
        Ok(())
    }

    fn read_best_block_hash(&self) -> Result<Option<[u8; 32]>> {
        let tree = self.chainstate_db.open_tree("default")?;
        for item in tree.iter() {
            let (key, mut value) = item?;
            if key.len() == 1 && key[0] == b'B' {
                self.chainstate_obf.deobfuscate_value(&key, &mut value);
                return Ok(Some(parse_best_block_value(&value)?));
            }
        }
        Ok(None)
    }

    fn lookup_height_for_hash(&self, hash: &[u8; 32]) -> Result<Option<u64>> {
        let h2h = self.dest_db.open_tree("hash_to_height")?;
        if let Some(hbytes) = h2h.get(hash.as_slice())? {
            if hbytes.len() == 8 {
                let arr: [u8; 8] = hbytes.as_slice().try_into().unwrap();
                return Ok(Some(u64::from_be_bytes(arr)));
            }
        }
        Ok(None)
    }

    fn lookup_header_for_hash(
        &self,
        hash: &[u8; 32],
    ) -> Result<Option<blvm_protocol::BlockHeader>> {
        let headers = self.dest_db.open_tree("headers")?;
        if let Some(data) = headers.get(hash.as_slice())? {
            let header: blvm_protocol::BlockHeader = bincode::deserialize(&data)?;
            return Ok(Some(header));
        }
        Ok(None)
    }

    fn flush_batch(
        &self,
        tree: &dyn crate::storage::database::Tree,
        batch: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        for (key, value) in batch.drain(..) {
            tree.insert(&key, &value)?;
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        let source_tree = self.chainstate_db.open_tree("default")?;
        let dest_tree = self.dest_db.open_tree("utxos")?;

        let mut source_count = 0u64;
        for item in source_tree.iter() {
            let (key, _) = item?;
            match get_key_prefix(&key) {
                Some(b'C') | Some(b'c') => source_count += 1,
                _ => {}
            }
        }

        let dest_count = dest_tree.len()? as u64;
        if source_count != dest_count {
            anyhow::bail!(
                "Verification failed: source has {source_count} UTXOs, destination has {dest_count}"
            );
        }

        if source_count == 0 {
            anyhow::bail!(
                "Verification failed: no UTXOs found in source chainstate ('C'/'c' keys)"
            );
        }

        let core_muhash = compute_core_chainstate_muhash(&self.chainstate_db, self.chainstate_obf)?;
        let dest_muhash = compute_dest_utxo_muhash(&self.dest_db)?;
        if core_muhash != dest_muhash {
            anyhow::bail!(
                "Verification failed: UTXO muhash mismatch (Core {}, BLVM {})",
                hex::encode(core_muhash),
                hex::encode(dest_muhash)
            );
        }
        info!(
            "[CORE_IMPORT] UTXO muhash verified: {}",
            hex::encode(dest_muhash)
        );

        let chain = ChainState::new(Arc::clone(&self.dest_db))?;
        let height = chain.get_height()?.unwrap_or(0);
        let indexes = self.progress.block_indexes_migrated.load(Ordering::Relaxed);
        if height == 0 && indexes > 1 {
            anyhow::bail!("Verification failed: chain height is 0 after index migration");
        }

        if let Some(best) = self.read_best_block_hash()? {
            let info = chain
                .load_chain_info()?
                .ok_or_else(|| anyhow::anyhow!("Verification failed: chain_info not written"))?;
            if info.tip_hash != best {
                anyhow::bail!(
                    "Verification failed: chain_info tip {} != Core best block {}",
                    hex::encode(info.tip_hash),
                    hex::encode(best)
                );
            }
            if info.height != height {
                anyhow::bail!(
                    "Verification failed: chain_info height {} != get_height() {}",
                    info.height,
                    height
                );
            }
            if let Some(index_height) = self.lookup_height_for_hash(&best)? {
                if index_height != height {
                    anyhow::bail!(
                        "Verification failed: height_index has {index_height} for tip, chain_info has {height}"
                    );
                }
            }
        } else if indexes > 0 {
            warn!("[CORE_IMPORT] Verify: no Core 'B' best-block entry; tip checked via index only");
        }

        let blockstore = self.blockstore_for_verify()?;
        if indexes > 0 {
            if blockstore.get_hash_by_height(0)?.is_none() {
                anyhow::bail!("Verification failed: genesis hash missing from height_index");
            }
            if height > 0 && blockstore.get_hash_by_height(height)?.is_none() {
                anyhow::bail!("Verification failed: tip height {height} missing from height_index");
            }
        }

        let blocks_migrated = self.progress.blocks_migrated.load(Ordering::Relaxed);
        let blocks_failed = self.progress.blocks_failed.load(Ordering::Relaxed);
        if blocks_failed > 0 {
            anyhow::bail!(
                "Verification failed: {blocks_failed} block(s) missing or failed to {} \
                 ({blocks_migrated} processed, {indexes} indexes)",
                if self.reuse_core_block_files {
                    "index"
                } else {
                    "import"
                }
            );
        }
        if indexes > 0 && blocks_migrated > 0 && blocks_migrated + blocks_failed < indexes {
            anyhow::bail!(
                "Verification failed: processed {blocks_migrated} blocks but {indexes} block indexes exist"
            );
        }

        if height > 0 {
            self.verify_sample_blocks(&blockstore, height)?;
        }

        info!(
            "[CORE_IMPORT] Verification passed: {dest_count} UTXOs, height={height}, indexes={indexes}, blocks={blocks_migrated}{}",
            if self.reuse_core_block_files {
                " (Core blocks/ reused in place)"
            } else {
                ""
            }
        );
        Ok(())
    }

    fn blockstore_for_verify(&self) -> Result<BlockStore> {
        if self.reuse_core_block_files {
            let blocks_dir = self.source_dir.join("blocks");
            let reader = Arc::new(BitcoinCoreBlockReader::new_with_cache(
                &blocks_dir,
                self.network,
                Some(self.dest_dir.as_path()),
            )?);
            BlockStore::new_with_bitcoin_core_reader(Arc::clone(&self.dest_db), Some(reader))
        } else {
            BlockStore::new(Arc::clone(&self.dest_db))
        }
    }

    fn verify_sample_blocks(&self, blockstore: &BlockStore, tip_height: u64) -> Result<()> {
        let height_index = self.dest_db.open_tree("height_index")?;
        let samples = sample_verify_heights(tip_height);
        let mut verified = 0u64;

        for h in samples {
            let hkey = h.to_be_bytes();
            let Some(hash_bytes) = height_index.get(&hkey)? else {
                if h == 0 || h == tip_height {
                    anyhow::bail!("Verification failed: height_index missing height {h}");
                }
                continue;
            };
            if hash_bytes.len() != 32 {
                anyhow::bail!("Verification failed: invalid height_index key at height {h}");
            }
            let mut expected = [0u8; 32];
            expected.copy_from_slice(&hash_bytes);

            let stored_hash = blockstore
                .get_hash_by_height(h)?
                .ok_or_else(|| anyhow::anyhow!("Verification failed: no hash at height {h}"))?;
            if stored_hash != expected {
                anyhow::bail!(
                    "Verification failed: height_index hash != hash_to_height at height {h}"
                );
            }

            let block = blockstore.get_block(&stored_hash)?.ok_or_else(|| {
                anyhow::anyhow!("Verification failed: block body missing at height {h}")
            })?;
            if blockstore.get_block_hash(&block) != expected {
                anyhow::bail!("Verification failed: block content hash mismatch at height {h}");
            }
            verified += 1;
        }

        if verified < 3 && tip_height >= 2 {
            anyhow::bail!(
                "Verification failed: only {verified} sample blocks checked (expected at least 3)"
            );
        }

        info!("[CORE_IMPORT] Sample block verify: {verified} heights OK");
        Ok(())
    }

    fn print_summary(&self) {
        let elapsed = self.progress.start_time.elapsed();
        let coins = self.progress.coins_migrated.load(Ordering::Relaxed);
        let blocks = self.progress.blocks_migrated.load(Ordering::Relaxed);
        let block_failures = self.progress.blocks_failed.load(Ordering::Relaxed);
        let indexes = self.progress.block_indexes_migrated.load(Ordering::Relaxed);

        info!("=== Migration Summary ===");
        info!("Time elapsed: {:?}", elapsed);
        info!("UTXOs migrated: {coins}");
        info!("Block indexes migrated: {indexes}");
        info!("Blocks migrated: {blocks}");
        if block_failures > 0 {
            info!("Blocks failed/missing: {block_failures}");
        }
        if elapsed.as_secs() > 0 {
            info!(
                "Rate: {:.2} UTXOs/second",
                coins as f64 / elapsed.as_secs() as f64
            );
        }
    }
}

/// Recompute Core-compatible MuHash3072 from Core chainstate `'C'`/`'c'` entries.
fn compute_core_chainstate_muhash(
    chainstate_db: &Arc<dyn crate::storage::database::Database>,
    obf: CoreDbObfuscation,
) -> Result<[u8; 32]> {
    let tree = chainstate_db.open_tree("default")?;
    let mut muhash = MuHash3072::new();

    for item in tree.iter() {
        let (key, mut value) = item?;
        obf.deobfuscate_value(&key, &mut value);
        let Some(prefix) = get_key_prefix(&key) else {
            continue;
        };
        let coin = match prefix {
            b'C' => parse_core_coin(&value)?,
            b'c' => parse_coin(&value)?,
            _ => continue,
        };
        if key.len() < 34 {
            continue;
        }
        let mut txid = [0u8; 32];
        txid.copy_from_slice(&key[1..33]);
        let (vout, _) = read_core_varint(&key, 33)?;
        let preimage = serialize_coin_for_muhash(
            &txid,
            vout as u32,
            coin.height,
            coin.is_coinbase,
            coin.amount as i64,
            &coin.script,
        );
        muhash = muhash.insert(&preimage);
    }

    Ok(muhash.finalize())
}

/// MuHash3072 of migrated BLVM UTXOs (matches Core `gettxoutsetinfo muhash`).
fn compute_dest_utxo_muhash(
    dest_db: &Arc<dyn crate::storage::database::Database>,
) -> Result<[u8; 32]> {
    let utxo_store = UtxoStore::new(Arc::clone(dest_db))?;
    let utxo_set = utxo_store.load_utxo_set()?;
    AssumeUtxoManager::calculate_utxo_hash(&utxo_set)
}

/// Heights to spot-check during `--verify` (genesis, early, tip, and spread).
fn sample_verify_heights(tip: u64) -> Vec<u64> {
    if tip == 0 {
        return vec![0];
    }
    let mut out = vec![0u64, 1, tip];
    const TARGET: usize = 10;
    if tip > 2 {
        let extra = TARGET.saturating_sub(out.len());
        for i in 1..=extra {
            let h = tip.saturating_mul(i as u64) / (extra as u64 + 1);
            if h > 1 && h < tip {
                out.push(h);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Summary of Core `blocks/index` vs on-disk `blk*.dat` availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreBlockCoverage {
    pub max_index_height: u64,
    pub block_indexes: u64,
    pub blocks_with_data: u64,
    pub tip_height: u64,
    pub tip_has_data: bool,
    pub tip_readable: bool,
    pub lowest_height_with_data: Option<u64>,
}

impl CoreBlockCoverage {
    /// True when the datadir appears pruned or block files do not cover the indexed chain.
    pub fn is_likely_pruned(&self) -> bool {
        if self.max_index_height == 0 {
            return false;
        }
        if !self.tip_has_data || !self.tip_readable {
            return true;
        }
        if self.lowest_height_with_data.unwrap_or(0) > 0 {
            return true;
        }
        self.blocks_with_data + 1 < self.max_index_height + 1
    }

    pub fn pruned_error_message(&self) -> Option<String> {
        if !self.is_likely_pruned() {
            return None;
        }
        Some(format!(
            "Bitcoin Core datadir appears pruned or block files are incomplete: \
             indexed through height {}, {} block(s) marked with data (lowest with data: {:?}), \
             tip readable={}. Use a non-pruned Core copy, enable storage.reuse_core_block_files \
             only when Core blocks/ remains available, or re-sync block files before migrating.",
            self.max_index_height,
            self.blocks_with_data,
            self.lowest_height_with_data,
            self.tip_readable
        ))
    }
}

/// Assess whether Core block files cover the indexed chain (detect pruned datadirs).
#[cfg(feature = "rocksdb")]
pub fn assess_core_block_coverage(
    source_dir: &Path,
    network: CoreDataNetwork,
) -> Result<CoreBlockCoverage> {
    use super::bitcoin_core_format::parse_disk_block_index;
    use super::bitcoin_core_obfuscation::CoreDbObfuscation;

    let blocks_dir = source_dir.join("blocks");
    let block_index_db = match BitcoinCoreStorage::open_bitcoin_core_block_index(source_dir) {
        Ok(db) => Arc::from(db),
        Err(_) => {
            return Ok(CoreBlockCoverage {
                max_index_height: 0,
                block_indexes: 0,
                blocks_with_data: 0,
                tip_height: 0,
                tip_has_data: false,
                tip_readable: false,
                lowest_height_with_data: None,
            });
        }
    };
    let block_index_obf = CoreDbObfuscation::load(&block_index_db)?;
    let source_tree = block_index_db.open_tree("default")?;

    let mut max_index_height = 0u64;
    let mut block_indexes = 0u64;
    let mut blocks_with_data = 0u64;
    let mut lowest_height_with_data: Option<u64> = None;
    let mut tip_height = 0u64;
    let mut tip_has_data = false;
    let mut tip_n_file = 0i32;
    let mut tip_n_data_pos = 0u32;

    for item in source_tree.iter() {
        let (key, mut value) = item?;
        block_index_obf.deobfuscate_value(&key, &mut value);
        if get_key_prefix(&key) != Some(b'b') {
            continue;
        }
        let index = parse_disk_block_index(&value)?;
        block_indexes += 1;
        let height = index.height as u64;
        if height > max_index_height {
            max_index_height = height;
        }
        if height > tip_height {
            tip_height = height;
            tip_has_data = index.status & BLOCK_HAVE_DATA != 0;
            tip_n_file = index.n_file;
            tip_n_data_pos = index.n_data_pos;
        }
        if index.status & BLOCK_HAVE_DATA != 0 {
            blocks_with_data += 1;
            lowest_height_with_data = Some(match lowest_height_with_data {
                Some(h) => h.min(height),
                None => height,
            });
        }
    }

    let tip_readable = if blocks_dir.is_dir() && tip_has_data {
        read_block_at_file_pos(&blocks_dir, tip_n_file, tip_n_data_pos, network)?.is_some()
    } else {
        false
    };

    Ok(CoreBlockCoverage {
        max_index_height,
        block_indexes,
        blocks_with_data,
        tip_height,
        tip_has_data,
        tip_readable,
        lowest_height_with_data,
    })
}

#[cfg(not(feature = "rocksdb"))]
pub fn assess_core_block_coverage(
    _source_dir: &Path,
    _network: CoreDataNetwork,
) -> Result<CoreBlockCoverage> {
    Ok(CoreBlockCoverage {
        max_index_height: 0,
        block_indexes: 0,
        blocks_with_data: 0,
        tip_height: 0,
        tip_has_data: false,
        tip_readable: false,
        lowest_height_with_data: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        BlockImportJob, CoreBlockCoverage, core_migrate_block_workers_effective,
        prepare_block_import_jobs, sample_verify_heights,
    };

    #[test]
    fn prepare_block_import_jobs_sorts_and_resumes() {
        let jobs = prepare_block_import_jobs(
            vec![
                BlockImportJob {
                    height: 10,
                    hash: [1u8; 32],
                    file_loc: Some((0, 100)),
                },
                BlockImportJob {
                    height: 2,
                    hash: [2u8; 32],
                    file_loc: Some((0, 200)),
                },
                BlockImportJob {
                    height: 5,
                    hash: [3u8; 32],
                    file_loc: None,
                },
            ],
            Some(4),
        );
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].height, 5);
        assert_eq!(jobs[1].height, 10);
    }

    #[test]
    fn core_migrate_block_workers_respects_env() {
        unsafe {
            std::env::set_var("BLVM_CORE_MIGRATE_BLOCK_WORKERS", "2");
        }
        assert_eq!(core_migrate_block_workers_effective(), 2);
        unsafe {
            std::env::remove_var("BLVM_CORE_MIGRATE_BLOCK_WORKERS");
        }
    }

    #[test]
    fn core_block_coverage_detects_pruned_gap() {
        let pruned = CoreBlockCoverage {
            max_index_height: 1000,
            block_indexes: 1001,
            blocks_with_data: 550,
            tip_height: 1000,
            tip_has_data: true,
            tip_readable: true,
            lowest_height_with_data: Some(451),
        };
        assert!(pruned.is_likely_pruned());

        let full = CoreBlockCoverage {
            max_index_height: 100,
            block_indexes: 101,
            blocks_with_data: 101,
            tip_height: 100,
            tip_has_data: true,
            tip_readable: true,
            lowest_height_with_data: Some(0),
        };
        assert!(!full.is_likely_pruned());
    }

    #[test]
    fn sample_verify_heights_includes_endpoints() {
        let s = sample_verify_heights(100);
        assert!(s.contains(&0));
        assert!(s.contains(&1));
        assert!(s.contains(&100));
        assert!(s.len() >= 3);
    }

    #[test]
    fn sample_verify_heights_genesis_only() {
        assert_eq!(sample_verify_heights(0), vec![0]);
    }
}

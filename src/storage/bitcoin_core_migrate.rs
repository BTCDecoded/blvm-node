//! Bitcoin Core to BLVM migration logic.
//!
//! Used by `blvm migrate core` and the standalone migrate-bitcoin-core binary.

use super::bitcoin_core_blocks::BitcoinCoreBlockReader;
use super::bitcoin_core_detection::{BitcoinCoreDetection, BitcoinCoreNetwork};
use super::bitcoin_core_format::{convert_key, get_key_prefix, parse_block_index, parse_coin};
use super::bitcoin_core_storage::BitcoinCoreStorage;
use super::blockstore::BlockStore;
use super::database::{create_database, DatabaseBackend};
use anyhow::{Context, Result};
use blvm_protocol::{Hash, UTXO};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

/// Migration parameters.
#[derive(Debug, Clone)]
pub struct MigrateCoreArgs {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub network: BitcoinCoreNetwork,
    pub verify: bool,
    pub verbose: bool,
}

/// Run Bitcoin Core to BLVM migration.
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

    info!("Bitcoin Core to BLVM Migration");
    info!("Source: {:?}", args.source);
    info!("Destination: {:?}", args.destination);
    info!("Network: {:?}", args.network);

    if !args.source.exists() {
        anyhow::bail!("Source directory does not exist: {:?}", args.source);
    }

    let chainstate = args.source.join("chainstate");
    if BitcoinCoreDetection::detect_db_format(&chainstate).is_err() {
        anyhow::bail!("Invalid Bitcoin Core database format in {:?}", chainstate);
    }

    std::fs::create_dir_all(&args.destination).with_context(|| {
        format!(
            "Failed to create destination directory {:?}",
            args.destination
        )
    })?;

    let migrator = Migrator::new(&args.source, &args.destination, args.network)?;
    migrator.migrate(args.verify)?;

    info!("Migration completed successfully!");
    Ok(())
}

struct Migrator {
    source_dir: PathBuf,
    _dest_dir: PathBuf,
    _network: BitcoinCoreNetwork,
    source_db: Arc<dyn crate::storage::database::Database>,
    dest_db: Arc<dyn crate::storage::database::Database>,
    progress: Arc<MigrationProgress>,
}

struct MigrationProgress {
    coins_migrated: AtomicU64,
    blocks_migrated: AtomicU64,
    block_indexes_migrated: AtomicU64,
    start_time: Instant,
}

impl Migrator {
    fn new(source_dir: &Path, dest_dir: &Path, network: BitcoinCoreNetwork) -> Result<Self> {
        info!("Opening source database...");
        let source_db = Arc::from(BitcoinCoreStorage::open_bitcoin_core_database(
            source_dir, network,
        )?);

        info!("Creating destination database...");
        let dest_db = Arc::from(create_database(dest_dir, DatabaseBackend::Redb, None)?);

        let progress = Arc::new(MigrationProgress {
            coins_migrated: AtomicU64::new(0),
            blocks_migrated: AtomicU64::new(0),
            block_indexes_migrated: AtomicU64::new(0),
            start_time: Instant::now(),
        });

        Ok(Self {
            source_dir: source_dir.to_path_buf(),
            _dest_dir: dest_dir.to_path_buf(),
            _network: network,
            source_db,
            dest_db,
            progress,
        })
    }

    fn migrate(&self, verify: bool) -> Result<()> {
        info!("Starting migration...");

        self.migrate_chainstate()?;
        self.migrate_block_indexes()?;

        if self.source_dir.join("blocks").exists() {
            self.migrate_blocks()?;
        }

        if verify {
            info!("Verifying migrated data...");
            self.verify()?;
        }

        self.print_summary();
        Ok(())
    }

    fn migrate_chainstate(&self) -> Result<()> {
        info!("Migrating chainstate (UTXOs)...");

        let source_tree = self.source_db.open_tree("default")?;
        let dest_tree = self.dest_db.open_tree("utxos")?;

        let mut count = 0;
        let mut batch = Vec::new();
        const BATCH_SIZE: usize = 1000;

        for item in source_tree.iter() {
            let (key, value) = item?;

            if let Some(b'c') = get_key_prefix(&key) {
                let coin_key = convert_key(&key)?;
                let coin = parse_coin(&value)?;

                let blvm_utxo = bincode::serialize(&UTXO {
                    value: coin.amount as i64,
                    script_pubkey: coin.script.into(),
                    height: coin.height as u64,
                    is_coinbase: coin.is_coinbase,
                })?;

                batch.push((coin_key, blvm_utxo));

                if batch.len() >= BATCH_SIZE {
                    self.flush_batch(dest_tree.as_ref(), &mut batch)?;
                    count += BATCH_SIZE;
                    self.progress
                        .coins_migrated
                        .store(count as u64, Ordering::Relaxed);

                    if count % 10000 == 0 {
                        info!("Migrated {} UTXOs...", count);
                    }
                }
            }
        }

        if !batch.is_empty() {
            self.flush_batch(dest_tree.as_ref(), &mut batch)?;
            count += batch.len();
        }

        self.progress
            .coins_migrated
            .store(count as u64, Ordering::Relaxed);
        info!("Migrated {} UTXOs", count);
        Ok(())
    }

    fn migrate_block_indexes(&self) -> Result<()> {
        info!("Migrating block indexes...");

        let source_tree = self.source_db.open_tree("default")?;
        let height_index = self.dest_db.open_tree("height_index")?;
        let hash_to_height = self.dest_db.open_tree("hash_to_height")?;

        let mut count = 0;

        for item in source_tree.iter() {
            let (key, value) = item?;

            if let Some(prefix) = get_key_prefix(&key) {
                if prefix == b'b' {
                    let block_index = parse_block_index(&value)?;

                    let block_hash: Hash = convert_key(&key)?
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid block hash length"))?;

                    let height_key = block_index.height.to_be_bytes();
                    let height_bytes = block_index.height.to_be_bytes();

                    height_index.insert(&height_key, block_hash.as_slice())?;
                    hash_to_height.insert(block_hash.as_slice(), &height_bytes)?;
                    count += 1;

                    if count % 1000 == 0 {
                        info!("Migrated {} block indexes...", count);
                    }
                }
            }
        }

        self.progress
            .block_indexes_migrated
            .store(count, Ordering::Relaxed);
        info!("Migrated {} block indexes", count);
        Ok(())
    }

    fn migrate_blocks(&self) -> Result<()> {
        info!("Migrating blocks from block files...");

        let blocks_dir = self.source_dir.join("blocks");
        let reader = BitcoinCoreBlockReader::new_with_cache(
            &blocks_dir,
            self._network,
            Some(self._dest_dir.as_path()),
        )?;

        let blockstore = BlockStore::new(Arc::clone(&self.dest_db))?;
        let height_index = self.dest_db.open_tree("height_index")?;

        let total = height_index.len()?;
        let mut count = 0u64;

        for item in height_index.iter() {
            let (height_key, hash_bytes) = item?;
            if hash_bytes.len() != 32 {
                continue;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(hash_bytes.as_ref());

            if let Ok(Some(block)) = reader.read_block(&hash) {
                if blockstore.store_block(&block).is_err() {
                    warn!(
                        "Failed to store block {}: already exists or error",
                        hex::encode(hash)
                    );
                } else {
                    count += 1;
                    self.progress
                        .blocks_migrated
                        .store(count, Ordering::Relaxed);

                    if count % 1000 == 0 {
                        info!("Migrated {} / {} blocks...", count, total);
                    }
                }
            }
        }

        info!("Migrated {} blocks from block files", count);
        Ok(())
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
        let source_tree = self.source_db.open_tree("default")?;
        let dest_tree = self.dest_db.open_tree("utxos")?;

        let mut source_count = 0;
        for item in source_tree.iter() {
            let (key, _) = item?;
            if let Some(prefix) = get_key_prefix(&key) {
                if prefix == b'c' {
                    source_count += 1;
                }
            }
        }

        let dest_count = dest_tree.len()?;

        if source_count != dest_count {
            anyhow::bail!(
                "Verification failed: source has {} UTXOs, destination has {}",
                source_count,
                dest_count
            );
        }

        info!(
            "Verification passed: {} UTXOs migrated correctly",
            dest_count
        );
        Ok(())
    }

    fn print_summary(&self) {
        let elapsed = self.progress.start_time.elapsed();
        let coins = self.progress.coins_migrated.load(Ordering::Relaxed);
        let blocks = self.progress.blocks_migrated.load(Ordering::Relaxed);
        let indexes = self.progress.block_indexes_migrated.load(Ordering::Relaxed);

        info!("=== Migration Summary ===");
        info!("Time elapsed: {:?}", elapsed);
        info!("UTXOs migrated: {}", coins);
        info!("Block indexes migrated: {}", indexes);
        info!("Blocks migrated: {}", blocks);
        if elapsed.as_secs() > 0 {
            info!(
                "Rate: {:.2} UTXOs/second",
                coins as f64 / elapsed.as_secs() as f64
            );
        }
    }
}

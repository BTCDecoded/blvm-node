//! Bitcoin Core to BLVM Migration Tool
//!
//! Migrates Bitcoin Core data (LevelDB/RocksDB) to BLVM's redb format.
//! Supports migrating chainstate, blocks, and UTXO data.

use anyhow::{Context, Result};
use blvm_node::storage::bitcoin_core_detection::{BitcoinCoreDetection, BitcoinCoreNetwork};
use blvm_node::storage::bitcoin_core_format::{parse_coin, parse_block_index, convert_key, get_key_prefix};
use blvm_node::storage::bitcoin_core_storage::BitcoinCoreStorage;
use blvm_node::storage::database::{create_database, DatabaseBackend};
use blvm_protocol::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[cfg(not(feature = "rocksdb"))]
fn main() {
    eprintln!("Error: Migration tool requires the 'rocksdb' feature");
    eprintln!("Build with: cargo build --bin migrate-bitcoin-core --features rocksdb");
    std::process::exit(1);
}

#[cfg(feature = "rocksdb")]
fn main() -> Result<()> {
    use clap::Parser;

    let args = Args::parse();

    // Initialize logging
    if args.verbose {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
    }

    info!("Bitcoin Core to BLVM Migration Tool");
    info!("Source: {:?}", args.source);
    info!("Destination: {:?}", args.destination);
    info!("Network: {:?}", args.network);

    // Detect Bitcoin Core data directory
    let core_dir = if let Some(dir) = args.source {
        PathBuf::from(dir)
    } else {
        BitcoinCoreDetection::detect_data_dir(args.network)?
            .ok_or_else(|| anyhow::anyhow!("Bitcoin Core data directory not found. Use --source to specify path."))?
    };

    if !core_dir.exists() {
        return Err(anyhow::anyhow!("Source directory does not exist: {:?}", core_dir));
    }

    // Verify Bitcoin Core database format
    let chainstate = core_dir.join("chainstate");
    if !BitcoinCoreDetection::detect_db_format(&chainstate).is_ok() {
        return Err(anyhow::anyhow!("Invalid Bitcoin Core database format in {:?}", chainstate));
    }

    // Create destination directory
    let dest_dir = PathBuf::from(args.destination);
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("Failed to create destination directory: {:?}", dest_dir))?;

    // Run migration
    let migrator = Migrator::new(&core_dir, &dest_dir, args.network)?;
    migrator.migrate(args.verify)?;

    info!("Migration completed successfully!");
    Ok(())
}

#[derive(clap::Parser, Debug)]
#[command(name = "migrate-bitcoin-core")]
#[command(about = "Migrate Bitcoin Core data to BLVM format")]
struct Args {
    /// Bitcoin Core data directory (default: auto-detect)
    #[arg(short, long)]
    source: Option<String>,

    /// Destination directory for BLVM database
    #[arg(short, long, required = true)]
    destination: String,

    /// Network type
    #[arg(short, long, default_value = "mainnet")]
    network: BitcoinCoreNetwork,

    /// Verify migrated data
    #[arg(short, long)]
    verify: bool,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

impl std::str::FromStr for BitcoinCoreNetwork {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mainnet" => Ok(BitcoinCoreNetwork::Mainnet),
            "testnet" => Ok(BitcoinCoreNetwork::Testnet),
            "regtest" => Ok(BitcoinCoreNetwork::Regtest),
            "signet" => Ok(BitcoinCoreNetwork::Signet),
            _ => Err(format!("Unknown network: {}", s)),
        }
    }
}

impl std::fmt::Display for BitcoinCoreNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BitcoinCoreNetwork::Mainnet => write!(f, "mainnet"),
            BitcoinCoreNetwork::Testnet => write!(f, "testnet"),
            BitcoinCoreNetwork::Regtest => write!(f, "regtest"),
            BitcoinCoreNetwork::Signet => write!(f, "signet"),
        }
    }
}

struct Migrator {
    source_dir: PathBuf,
    dest_dir: PathBuf,
    network: BitcoinCoreNetwork,
    source_db: Arc<dyn blvm_node::storage::database::Database>,
    dest_db: Arc<dyn blvm_node::storage::database::Database>,
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
        // Open source database (Bitcoin Core LevelDB via RocksDB)
        info!("Opening source database...");
        let source_db = Arc::from(
            BitcoinCoreStorage::open_bitcoin_core_database(source_dir, network)?
        );

        // Create destination database (redb)
        info!("Creating destination database...");
        let dest_db = Arc::from(create_database(dest_dir, DatabaseBackend::Redb)?);

        let progress = Arc::new(MigrationProgress {
            coins_migrated: AtomicU64::new(0),
            blocks_migrated: AtomicU64::new(0),
            block_indexes_migrated: AtomicU64::new(0),
            start_time: Instant::now(),
        });

        Ok(Self {
            source_dir: source_dir.to_path_buf(),
            dest_dir: dest_dir.to_path_buf(),
            network,
            source_db,
            dest_db,
            progress,
        })
    }

    fn migrate(&self, verify: bool) -> Result<()> {
        info!("Starting migration...");

        // Migrate chainstate (UTXOs)
        self.migrate_chainstate()?;

        // Migrate block indexes
        self.migrate_block_indexes()?;

        // Migrate blocks (if block files exist)
        if self.source_dir.join("blocks").exists() {
            self.migrate_blocks()?;
        }

        // Verify if requested
        if verify {
            info!("Verifying migrated data...");
            self.verify()?;
        }

        self.print_summary();
        Ok(())
    }

    fn migrate_chainstate(&self) -> Result<()> {
        info!("Migrating chainstate (UTXOs)...");

        // Open source chainstate tree (Bitcoin Core uses "default" column family)
        let source_tree = self.source_db.open_tree("default")?;
        
        // Open destination UTXO tree
        let dest_tree = self.dest_db.open_tree("utxos")?;

        let mut count = 0;
        let mut batch = Vec::new();
        const BATCH_SIZE: usize = 1000;

        // Iterate over all keys in source database
        for item in source_tree.iter() {
            let (key, value) = item?;

            // Check key prefix
            if let Some(prefix) = get_key_prefix(&key) {
                match prefix {
                    b'c' => {
                        // Coin (UTXO) entry
                        let coin_key = convert_key(&key)?;
                        
                        // Parse Bitcoin Core coin format
                        let coin = parse_coin(&value)?;
                        
                        // Convert to BLVM UTXO format
                        // Note: This is a simplified conversion - may need adjustment
                        let blvm_utxo = bincode::serialize(&UTXO {
                            value: coin.amount as i64,
                            script_pubkey: coin.script,
                            height: coin.height as u64,
                            is_coinbase: coin.is_coinbase,
                        })?;

                        batch.push((coin_key, blvm_utxo));

                        if batch.len() >= BATCH_SIZE {
                            self.flush_batch(&dest_tree, &mut batch)?;
                            count += BATCH_SIZE;
                            self.progress.coins_migrated.store(count, Ordering::Relaxed);
                            
                            if count % 10000 == 0 {
                                info!("Migrated {} UTXOs...", count);
                            }
                        }
                    }
                    _ => {
                        // Skip other key types for now
                    }
                }
            }
        }

        // Flush remaining batch
        if !batch.is_empty() {
            self.flush_batch(&dest_tree, &mut batch)?;
            count += batch.len();
        }

        self.progress.coins_migrated.store(count as u64, Ordering::Relaxed);
        info!("Migrated {} UTXOs", count);
        Ok(())
    }

    fn migrate_block_indexes(&self) -> Result<()> {
        info!("Migrating block indexes...");

        let source_tree = self.source_db.open_tree("default")?;
        let dest_tree = self.dest_db.open_tree("hash_to_height")?;

        let mut count = 0;

        for item in source_tree.iter() {
            let (key, value) = item?;

            if let Some(prefix) = get_key_prefix(&key) {
                if prefix == b'b' {
                    // Block index entry
                    let block_index = parse_block_index(&value)?;
                    
                    // Store height -> hash mapping
                    let height_key = block_index.height.to_be_bytes();
                    let block_hash: Hash = convert_key(&key)?.try_into()
                        .map_err(|_| anyhow::anyhow!("Invalid block hash length"))?;
                    
                    dest_tree.insert(&height_key, &block_hash)?;
                    count += 1;

                    if count % 1000 == 0 {
                        info!("Migrated {} block indexes...", count);
                    }
                }
            }
        }

        self.progress.block_indexes_migrated.store(count, Ordering::Relaxed);
        info!("Migrated {} block indexes", count);
        Ok(())
    }

    fn migrate_blocks(&self) -> Result<()> {
        info!("Migrating blocks from block files...");
        warn!("Block file migration is not yet implemented - blocks will be read on-demand");
        // TODO: Implement block file migration
        // This would involve reading blocks from blk*.dat files and storing them in the database
        Ok(())
    }

    fn flush_batch(
        &self,
        tree: &dyn blvm_node::storage::database::Tree,
        batch: &mut Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Result<()> {
        for (key, value) in batch.drain(..) {
            tree.insert(&key, &value)?;
        }
        Ok(())
    }

    fn verify(&self) -> Result<()> {
        // Verify UTXO count
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
            return Err(anyhow::anyhow!(
                "Verification failed: source has {} UTXOs, destination has {}",
                source_count,
                dest_count
            ));
        }

        info!("Verification passed: {} UTXOs migrated correctly", dest_count);
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
            info!("Rate: {:.2} UTXOs/second", coins as f64 / elapsed.as_secs() as f64);
        }
    }
}


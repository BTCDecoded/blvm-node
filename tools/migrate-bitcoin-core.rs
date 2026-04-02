//! Bitcoin Core to BLVM Migration Tool
//!
//! Standalone binary. Also available as `blvm migrate core`.

#![cfg(feature = "rocksdb")]

use anyhow::Result;
use blvm_node::storage::bitcoin_core_detection::BitcoinCoreNetwork;
use blvm_node::storage::bitcoin_core_migrate::{run_migrate_core, MigrateCoreArgs};
use clap::Parser;
use std::path::PathBuf;

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

fn main() -> Result<()> {
    let args = Args::parse();

    let source = if let Some(dir) = &args.source {
        PathBuf::from(dir)
    } else {
        blvm_node::storage::bitcoin_core_detection::BitcoinCoreDetection::detect_data_dir(
            args.network,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!("Bitcoin Core data directory not found. Use --source to specify path.")
        })?
    };

    run_migrate_core(MigrateCoreArgs {
        source,
        destination: PathBuf::from(args.destination),
        network: args.network,
        verify: args.verify,
        verbose: args.verbose,
    })
}

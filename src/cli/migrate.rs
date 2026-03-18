//! Migrate Bitcoin Core data to BLVM format.
//!
//! Used by `blvm migrate core`.

#![cfg(feature = "rocksdb")]

use anyhow::Result;
use std::path::PathBuf;

use crate::storage::bitcoin_core_detection::{BitcoinCoreDetection, BitcoinCoreNetwork};
use crate::storage::bitcoin_core_migrate::{run_migrate_core, MigrateCoreArgs};

/// Run migrate core: migrate Bitcoin Core data directory to BLVM format.
pub fn run_migrate_core_cli(
    source: Option<PathBuf>,
    destination: PathBuf,
    network: BitcoinCoreNetwork,
    verify: bool,
    verbose: bool,
) -> Result<()> {
    let core_dir = if let Some(dir) = source {
        dir
    } else {
        BitcoinCoreDetection::detect_data_dir(network)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Bitcoin Core data directory not found. Use --source to specify path."
                )
            })?
    };

    run_migrate_core(MigrateCoreArgs {
        source: core_dir,
        destination,
        network,
        verify,
        verbose,
    })
}

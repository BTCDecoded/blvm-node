//! Migrate Bitcoin Core data to BLVM format.
//!
//! Used by `blvm migrate core`.

use anyhow::Result;
use std::path::PathBuf;

use crate::storage::bitcoin_core_migrate::{MigrateCoreArgs, run_migrate_core};
use crate::storage::bitcoin_detection::{BitcoinCoreDetection, CoreDataNetwork};

/// Run migrate core: migrate Bitcoin Core data directory to BLVM format.
pub fn run_migrate_core_cli(
    source: Option<PathBuf>,
    destination: PathBuf,
    network: CoreDataNetwork,
    verify: bool,
    verbose: bool,
) -> Result<()> {
    let core_dir = if let Some(dir) = source {
        dir
    } else {
        BitcoinCoreDetection::detect_data_dir(network)?.ok_or_else(|| {
            anyhow::anyhow!("Bitcoin Core data directory not found. Use --source to specify path.")
        })?
    };

    run_migrate_core(MigrateCoreArgs {
        source: core_dir,
        destination,
        network,
        verify,
        verbose,
        dest_backend: None,
        stop_after: None,
        reuse_core_block_files: crate::config::StorageConfig::default()
            .reuse_core_block_files_effective(),
    })
}

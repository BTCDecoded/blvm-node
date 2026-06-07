//! Data directory detection
//!
//! Detects existing node installations and their database format.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Database format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseFormat {
    /// LevelDB format (standard chainstate)
    LevelDB,
}

/// Network selector for Bitcoin Core data-directory layout.
///
/// Not the same as `blvm_protocol::types::Network` — this enum also covers `Signet`,
/// which is not a BLVM protocol network but is a valid Bitcoin Core data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreDataNetwork {
    Mainnet,
    Testnet,
    Regtest,
    Signet,
}

impl CoreDataNetwork {
    fn directory_name(&self) -> &'static str {
        match self {
            CoreDataNetwork::Mainnet => "",
            CoreDataNetwork::Testnet => "testnet3",
            CoreDataNetwork::Regtest => "regtest",
            CoreDataNetwork::Signet => "signet",
        }
    }
}

impl std::str::FromStr for CoreDataNetwork {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mainnet" => Ok(CoreDataNetwork::Mainnet),
            "testnet" => Ok(CoreDataNetwork::Testnet),
            "regtest" => Ok(CoreDataNetwork::Regtest),
            "signet" => Ok(CoreDataNetwork::Signet),
            _ => Err(format!("Unknown network: {s}")),
        }
    }
}

impl std::fmt::Display for CoreDataNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreDataNetwork::Mainnet => write!(f, "mainnet"),
            CoreDataNetwork::Testnet => write!(f, "testnet"),
            CoreDataNetwork::Regtest => write!(f, "regtest"),
            CoreDataNetwork::Signet => write!(f, "signet"),
        }
    }
}

/// Bitcoin Core detection utilities
pub struct BitcoinCoreDetection;

impl BitcoinCoreDetection {
    /// Detect Bitcoin Core data directory
    ///
    /// Checks standard Bitcoin Core paths for the given network.
    /// Returns the path if found, None otherwise.
    pub fn detect_data_dir(network: CoreDataNetwork) -> Result<Option<PathBuf>> {
        let possible_dirs = Self::get_standard_paths(network);

        for dir in possible_dirs.into_iter().flatten() {
            if Self::is_core_layout_at(&dir) {
                return Ok(Some(dir));
            }
        }

        Ok(None)
    }

    /// Get standard Bitcoin Core data directory paths
    fn get_standard_paths(network: CoreDataNetwork) -> Vec<Option<PathBuf>> {
        let mut paths = Vec::new();

        // Standard home directory paths
        if let Some(home) = dirs::home_dir() {
            let base = home.join(".bitcoin");
            if network == CoreDataNetwork::Mainnet {
                paths.push(Some(base.clone()));
            } else {
                paths.push(Some(base.join(network.directory_name())));
            }
        }

        // System-wide paths
        paths.push(Some(PathBuf::from("/var/lib/bitcoind")));
        if network != CoreDataNetwork::Mainnet {
            paths.push(Some(
                PathBuf::from("/var/lib/bitcoind").join(network.directory_name()),
            ));
        }

        paths
    }

    /// Check if `dir` contains a Bitcoin Core datadir layout.
    pub fn is_core_layout_at(dir: &Path) -> bool {
        let chainstate = dir.join("chainstate");
        if !chainstate.exists() {
            return false;
        }
        let blocks = dir.join("blocks");
        if !blocks.exists() {
            return false;
        }
        if Self::detect_db_format(&chainstate).is_err() {
            return false;
        }
        true
    }

    /// Check if directory contains Bitcoin Core data (alias for [`is_core_layout_at`]).
    fn is_bitcoin_core_dir(dir: &Path, _network: CoreDataNetwork) -> bool {
        Self::is_core_layout_at(dir)
    }

    /// Detect database format (LevelDB)
    ///
    /// Checks if the given path contains a LevelDB database.
    /// LevelDB databases have a CURRENT file pointing to MANIFEST-XXXXXX.
    pub fn detect_db_format(data_dir: &Path) -> Result<DatabaseFormat> {
        let current_file = data_dir.join("CURRENT");

        if !current_file.exists() {
            return Err(anyhow::anyhow!(
                "CURRENT file not found - not a LevelDB database"
            ));
        }

        // Read CURRENT file - should contain "MANIFEST-XXXXXX"
        let contents = std::fs::read_to_string(&current_file)?;
        let trimmed = contents.trim();

        if trimmed.starts_with("MANIFEST-") {
            // Verify MANIFEST file exists
            let manifest_path = data_dir.join(trimmed);
            if manifest_path.exists() {
                return Ok(DatabaseFormat::LevelDB);
            }
        }

        Err(anyhow::anyhow!("Invalid LevelDB format"))
    }

    /// Detect network from data directory
    ///
    /// Attempts to detect the network by checking directory structure
    /// and configuration files.
    pub fn detect_network(data_dir: &Path) -> Option<CoreDataNetwork> {
        // Check directory name
        if let Some(dir_name) = data_dir.file_name().and_then(|n| n.to_str()) {
            match dir_name {
                "testnet3" => return Some(CoreDataNetwork::Testnet),
                "regtest" => return Some(CoreDataNetwork::Regtest),
                "signet" => return Some(CoreDataNetwork::Signet),
                _ => {}
            }
        }

        // Check parent directory
        if let Some(parent) = data_dir.parent() {
            if let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) {
                if parent_name == ".bitcoin" {
                    // Check if we're in a subdirectory
                    if let Some(dir_name) = data_dir.file_name().and_then(|n| n.to_str()) {
                        match dir_name {
                            "testnet3" => return Some(CoreDataNetwork::Testnet),
                            "regtest" => return Some(CoreDataNetwork::Regtest),
                            "signet" => return Some(CoreDataNetwork::Signet),
                            _ => return Some(CoreDataNetwork::Mainnet),
                        }
                    }
                }
            }
        }

        // Default to mainnet if we can't determine
        Some(CoreDataNetwork::Mainnet)
    }

    /// Verify database integrity
    ///
    /// Checks that the chainstate database is readable and valid.
    pub fn verify_database(data_dir: &Path) -> Result<()> {
        let chainstate = data_dir.join("chainstate");

        if !chainstate.exists() {
            return Err(anyhow::anyhow!("Chainstate directory not found"));
        }

        // Check for required LevelDB files
        let current = chainstate.join("CURRENT");
        if !current.exists() {
            return Err(anyhow::anyhow!("CURRENT file not found in chainstate"));
        }

        // Try to read CURRENT file
        let contents = std::fs::read_to_string(&current)?;
        let manifest_name = contents.trim();

        if !manifest_name.starts_with("MANIFEST-") {
            return Err(anyhow::anyhow!("Invalid CURRENT file format"));
        }

        let manifest = chainstate.join(manifest_name);
        if !manifest.exists() {
            return Err(anyhow::anyhow!(
                "MANIFEST file not found: {}",
                manifest_name
            ));
        }

        Ok(())
    }

    /// Fail when Core block files do not cover the indexed chain (pruned datadir).
    #[cfg(feature = "rocksdb")]
    pub fn ensure_blocks_available(source_dir: &Path, network: CoreDataNetwork) -> Result<()> {
        use crate::storage::bitcoin_core_migrate::assess_core_block_coverage;
        let coverage = assess_core_block_coverage(source_dir, network)?;
        if let Some(msg) = coverage.pruned_error_message() {
            anyhow::bail!("{msg}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_detect_network_from_path() {
        let temp = TempDir::new().unwrap();
        let testnet_path = temp.path().join("testnet3");
        std::fs::create_dir_all(&testnet_path).unwrap();

        assert_eq!(
            BitcoinCoreDetection::detect_network(&testnet_path),
            Some(CoreDataNetwork::Testnet)
        );
    }

    #[test]
    fn test_get_standard_paths() {
        let paths = BitcoinCoreDetection::get_standard_paths(CoreDataNetwork::Mainnet);
        assert!(!paths.is_empty());
    }
}

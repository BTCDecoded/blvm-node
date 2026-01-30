//! Bitcoin Core data directory detection
//!
//! Detects existing Bitcoin Core installations and their database format.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Bitcoin Core database format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseFormat {
    /// LevelDB format (standard for Bitcoin Core)
    LevelDB,
}

/// Bitcoin Core network type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitcoinCoreNetwork {
    Mainnet,
    Testnet,
    Regtest,
    Signet,
}

impl BitcoinCoreNetwork {
    fn directory_name(&self) -> &'static str {
        match self {
            BitcoinCoreNetwork::Mainnet => "",
            BitcoinCoreNetwork::Testnet => "testnet3",
            BitcoinCoreNetwork::Regtest => "regtest",
            BitcoinCoreNetwork::Signet => "signet",
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
    pub fn detect_data_dir(network: BitcoinCoreNetwork) -> Result<Option<PathBuf>> {
        let possible_dirs = Self::get_standard_paths(network);

        for dir in possible_dirs.into_iter().flatten() {
            if Self::is_bitcoin_core_dir(&dir, network) {
                return Ok(Some(dir));
            }
        }

        Ok(None)
    }

    /// Get standard Bitcoin Core data directory paths
    fn get_standard_paths(network: BitcoinCoreNetwork) -> Vec<Option<PathBuf>> {
        let mut paths = Vec::new();

        // Standard home directory paths
        if let Some(home) = dirs::home_dir() {
            let base = home.join(".bitcoin");
            if network == BitcoinCoreNetwork::Mainnet {
                paths.push(Some(base.clone()));
            } else {
                paths.push(Some(base.join(network.directory_name())));
            }
        }

        // System-wide paths
        paths.push(Some(PathBuf::from("/var/lib/bitcoind")));
        if network != BitcoinCoreNetwork::Mainnet {
            paths.push(Some(PathBuf::from("/var/lib/bitcoind").join(network.directory_name())));
        }

        paths
    }

    /// Check if directory contains Bitcoin Core data
    fn is_bitcoin_core_dir(dir: &Path, network: BitcoinCoreNetwork) -> bool {
        // Check for chainstate database (required)
        let chainstate = dir.join("chainstate");
        if !chainstate.exists() {
            return false;
        }

        // Check for blocks directory (required)
        let blocks = dir.join("blocks");
        if !blocks.exists() {
            return false;
        }

        // Verify chainstate is a valid database
        if Self::detect_db_format(&chainstate).is_err() {
            return false;
        }

        true
    }

    /// Detect database format (LevelDB)
    ///
    /// Checks if the given path contains a LevelDB database.
    /// LevelDB databases have a CURRENT file pointing to MANIFEST-XXXXXX.
    pub fn detect_db_format(data_dir: &Path) -> Result<DatabaseFormat> {
        let current_file = data_dir.join("CURRENT");

        if !current_file.exists() {
            return Err(anyhow::anyhow!("CURRENT file not found - not a LevelDB database"));
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
    pub fn detect_network(data_dir: &Path) -> Option<BitcoinCoreNetwork> {
        // Check directory name
        if let Some(dir_name) = data_dir.file_name().and_then(|n| n.to_str()) {
            match dir_name {
                "testnet3" => return Some(BitcoinCoreNetwork::Testnet),
                "regtest" => return Some(BitcoinCoreNetwork::Regtest),
                "signet" => return Some(BitcoinCoreNetwork::Signet),
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
                            "testnet3" => return Some(BitcoinCoreNetwork::Testnet),
                            "regtest" => return Some(BitcoinCoreNetwork::Regtest),
                            "signet" => return Some(BitcoinCoreNetwork::Signet),
                            _ => return Some(BitcoinCoreNetwork::Mainnet),
                        }
                    }
                }
            }
        }

        // Default to mainnet if we can't determine
        Some(BitcoinCoreNetwork::Mainnet)
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
            return Err(anyhow::anyhow!("MANIFEST file not found: {}", manifest_name));
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
            Some(BitcoinCoreNetwork::Testnet)
        );
    }

    #[test]
    fn test_get_standard_paths() {
        let paths = BitcoinCoreDetection::get_standard_paths(BitcoinCoreNetwork::Mainnet);
        assert!(!paths.is_empty());
    }
}


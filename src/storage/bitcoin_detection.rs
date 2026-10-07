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
    Testnet4,
}

impl CoreDataNetwork {
    fn directory_name(&self) -> &'static str {
        match self {
            CoreDataNetwork::Mainnet => "",
            CoreDataNetwork::Testnet => "testnet3",
            CoreDataNetwork::Regtest => "regtest",
            CoreDataNetwork::Signet => "signet",
            CoreDataNetwork::Testnet4 => "testnet4",
        }
    }
}

impl std::str::FromStr for CoreDataNetwork {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mainnet" => Ok(CoreDataNetwork::Mainnet),
            "testnet" | "testnet3" => Ok(CoreDataNetwork::Testnet),
            "testnet4" => Ok(CoreDataNetwork::Testnet4),
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
            CoreDataNetwork::Testnet4 => write!(f, "testnet4"),
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

    /// Check if `base_dir` contains Bitcoin Core data for the given network.
    ///
    /// Mainnet data lives at the root of `base_dir`. Other networks use a
    /// network-specific subdirectory: `testnet3/`, `testnet4/`, `signet/`, `regtest/`.
    pub fn is_bitcoin_core_dir(base_dir: &Path, network: CoreDataNetwork) -> bool {
        let data_dir = match network {
            CoreDataNetwork::Mainnet => base_dir.to_path_buf(),
            other => base_dir.join(other.directory_name()),
        };
        Self::is_core_layout_at(&data_dir)
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
    /// Attempts to detect the network by checking directory structure.
    /// Returns `None` if the network cannot be determined (caller must decide
    /// what to do—we do NOT silently fall back to mainnet).
    pub fn detect_network(data_dir: &Path) -> Option<CoreDataNetwork> {
        let dir_name = data_dir.file_name().and_then(|n| n.to_str())?;

        // Check well-known network subdirectory names
        match dir_name {
            "testnet3" => return Some(CoreDataNetwork::Testnet),
            "testnet4" => return Some(CoreDataNetwork::Testnet4),
            "regtest" => return Some(CoreDataNetwork::Regtest),
            "signet" => return Some(CoreDataNetwork::Signet),
            _ => {}
        }

        // Detect custom signet directories (signet_<challenge_hash>).
        // These are valid Bitcoin Core signet data directories but we cannot
        // auto-detect which signet network they belong to. Return None so the
        // caller can require explicit network selection.
        if dir_name.starts_with("signet_") {
            return None;
        }

        // If the directory is ".bitcoin" or "bitcoind" (common base names), it's mainnet
        if dir_name == ".bitcoin" || dir_name == "bitcoind" {
            return Some(CoreDataNetwork::Mainnet);
        }

        // Check parent directory for additional context
        if let Some(parent) = data_dir.parent() {
            if let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) {
                // If parent is .bitcoin, the given path is a network subdir
                if parent_name == ".bitcoin" {
                    // Already checked known names above; if we're here, it's unrecognized
                    return None;
                }
            }
        }

        // Cannot determine network from directory structure
        None
    }

    /// Detect network from data directory, returning an error for ambiguous cases.
    ///
    /// Unlike [`detect_network`], this method returns an explicit error when the
    /// directory appears to be a custom signet (`signet_<hash>`) rather than
    /// silently returning `None`. Use this when you need a clear error message.
    pub fn detect_network_strict(data_dir: &Path) -> Result<CoreDataNetwork> {
        let dir_name = data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid data directory path"))?;

        // Detect custom signet directories and reject with a clear error
        if dir_name.starts_with("signet_") {
            return Err(anyhow::anyhow!(
                "Custom signet directory detected: '{}'. \
                 Custom signets require explicit --network signet. \
                 Cannot auto-detect network from signet_<hash> directories.",
                dir_name
            ));
        }

        Self::detect_network(data_dir).ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine network from directory '{}'. \
                 Please specify the network explicitly with --network.",
                dir_name
            )
        })
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

    fn create_core_layout(dir: &Path) {
        let chainstate = dir.join("chainstate");
        std::fs::create_dir_all(&chainstate).unwrap();
        std::fs::write(chainstate.join("CURRENT"), "MANIFEST-000001\n").unwrap();
        std::fs::write(chainstate.join("MANIFEST-000001"), b"").unwrap();
        std::fs::create_dir_all(dir.join("blocks")).unwrap();
    }

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
    fn test_detect_testnet4_from_path() {
        let temp = TempDir::new().unwrap();
        let testnet4_path = temp.path().join("testnet4");
        std::fs::create_dir_all(&testnet4_path).unwrap();

        assert_eq!(
            BitcoinCoreDetection::detect_network(&testnet4_path),
            Some(CoreDataNetwork::Testnet4)
        );
    }

    #[test]
    fn test_detect_testnet4_under_bitcoin_parent() {
        let temp = TempDir::new().unwrap();
        let bitcoin_dir = temp.path().join(".bitcoin");
        let testnet4_path = bitcoin_dir.join("testnet4");
        std::fs::create_dir_all(&testnet4_path).unwrap();

        assert_eq!(
            BitcoinCoreDetection::detect_network(&testnet4_path),
            Some(CoreDataNetwork::Testnet4)
        );
    }

    #[test]
    fn test_detect_network_signet() {
        let temp = TempDir::new().unwrap();
        let signet_path = temp.path().join("signet");
        std::fs::create_dir_all(&signet_path).unwrap();

        assert_eq!(
            BitcoinCoreDetection::detect_network(&signet_path),
            Some(CoreDataNetwork::Signet)
        );
    }

    #[test]
    fn test_detect_network_custom_signet_returns_none() {
        let temp = TempDir::new().unwrap();
        let custom_signet = temp.path().join("signet_abc123def456");
        std::fs::create_dir_all(&custom_signet).unwrap();

        assert_eq!(BitcoinCoreDetection::detect_network(&custom_signet), None);
    }

    #[test]
    fn test_detect_network_strict_custom_signet_errors() {
        let temp = TempDir::new().unwrap();
        let custom_signet = temp.path().join("signet_abc123def456");
        std::fs::create_dir_all(&custom_signet).unwrap();

        let result = BitcoinCoreDetection::detect_network_strict(&custom_signet);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Custom signet directory detected"));
        assert!(err.contains("signet_abc123def456"));
    }

    #[test]
    fn test_detect_network_mainnet_base_dir() {
        let temp = TempDir::new().unwrap();
        let bitcoin_dir = temp.path().join(".bitcoin");
        std::fs::create_dir_all(&bitcoin_dir).unwrap();

        assert_eq!(
            BitcoinCoreDetection::detect_network(&bitcoin_dir),
            Some(CoreDataNetwork::Mainnet)
        );
    }

    #[test]
    fn test_detect_network_unknown_subdir_returns_none() {
        let temp = TempDir::new().unwrap();
        let bitcoin_dir = temp.path().join(".bitcoin");
        let unknown = bitcoin_dir.join("unknown_network");
        std::fs::create_dir_all(&unknown).unwrap();

        assert_eq!(BitcoinCoreDetection::detect_network(&unknown), None);
    }

    #[test]
    fn test_core_data_network_display_roundtrip() {
        for network in [
            CoreDataNetwork::Mainnet,
            CoreDataNetwork::Testnet,
            CoreDataNetwork::Testnet4,
            CoreDataNetwork::Regtest,
            CoreDataNetwork::Signet,
        ] {
            let s = network.to_string();
            let parsed: CoreDataNetwork = s.parse().unwrap();
            assert_eq!(parsed, network);
        }
    }

    #[test]
    fn test_from_str_testnet3_alias() {
        assert_eq!(
            "testnet3".parse::<CoreDataNetwork>().unwrap(),
            CoreDataNetwork::Testnet
        );
        assert_eq!(
            "testnet".parse::<CoreDataNetwork>().unwrap(),
            CoreDataNetwork::Testnet
        );
    }

    #[test]
    fn test_get_standard_paths() {
        let paths = BitcoinCoreDetection::get_standard_paths(CoreDataNetwork::Mainnet);
        assert!(!paths.is_empty());
    }

    #[test]
    fn test_is_bitcoin_core_dir_mainnet() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        create_core_layout(base);

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet
        ));
    }

    #[test]
    fn test_is_bitcoin_core_dir_testnet3() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        let testnet_dir = base.join("testnet3");
        create_core_layout(&testnet_dir);

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
    }

    #[test]
    fn test_is_bitcoin_core_dir_testnet4() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        let testnet4_dir = base.join("testnet4");
        create_core_layout(&testnet4_dir);

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet4
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet
        ));
    }

    #[test]
    fn test_is_bitcoin_core_dir_signet() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        let signet_dir = base.join("signet");
        create_core_layout(&signet_dir);

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Signet
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
    }

    #[test]
    fn test_is_bitcoin_core_dir_regtest() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        let regtest_dir = base.join("regtest");
        create_core_layout(&regtest_dir);

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Regtest
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
    }

    #[test]
    fn test_is_bitcoin_core_dir_multiple_networks() {
        let temp = TempDir::new().unwrap();
        let base = temp.path();
        create_core_layout(base);
        create_core_layout(&base.join("testnet3"));
        create_core_layout(&base.join("testnet4"));

        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Mainnet
        ));
        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet
        ));
        assert!(BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Testnet4
        ));
        assert!(!BitcoinCoreDetection::is_bitcoin_core_dir(
            base,
            CoreDataNetwork::Signet
        ));
    }

    #[test]
    fn test_core_data_network_directory_names() {
        assert_eq!(CoreDataNetwork::Mainnet.directory_name(), "");
        assert_eq!(CoreDataNetwork::Testnet.directory_name(), "testnet3");
        assert_eq!(CoreDataNetwork::Testnet4.directory_name(), "testnet4");
        assert_eq!(CoreDataNetwork::Signet.directory_name(), "signet");
        assert_eq!(CoreDataNetwork::Regtest.directory_name(), "regtest");
    }
}

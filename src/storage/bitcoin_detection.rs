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
        if Self::is_custom_signet_dir_name(dir_name) {
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
    /// Unlike [`Self::detect_network`], this method returns an explicit error when the
    /// directory appears to be a custom signet (`signet_<hash>`) rather than
    /// silently returning `None`. Use this when you need a clear error message.
    pub fn detect_network_strict(data_dir: &Path) -> Result<CoreDataNetwork> {
        let dir_name = data_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid data directory path"))?;

        // Detect custom signet directories and reject with a clear error
        if Self::is_custom_signet_dir_name(dir_name) {
            return Err(Self::custom_signet_error(dir_name));
        }

        Self::detect_network(data_dir).ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine network from directory '{}': it is not a standard \
                 Bitcoin Core network folder (.bitcoin, bitcoind, testnet3, testnet4, \
                 signet, regtest).",
                dir_name
            )
        })
    }

    /// True for Bitcoin Core custom signet folders (`signet_<challenge hash>`).
    fn is_custom_signet_dir_name(dir_name: &str) -> bool {
        dir_name.starts_with("signet_")
    }

    fn custom_signet_error(dir_name: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "Custom signet directory detected: '{}'. Its block files use a \
             challenge-specific network magic, so BLVM cannot read them as the standard \
             signet. Core block reuse does not support custom signets; set \
             storage.reuse_core_block_files = false to run without reusing Core's block files.",
            dir_name
        )
    }

    /// Decide which network's block files to read when reusing a Core datadir in place.
    ///
    /// Three sources can name the network, in this priority order:
    /// 1. `marker_network`: the `network` recorded in the BLVM migration marker
    ///    (`blvm_meta/migration.json`). An empty string counts as absent.
    /// 2. The Core datadir folder name, via [`Self::detect_network`].
    /// 3. `node_network`: the network the node itself is configured to run.
    ///
    /// Every source that is known must agree; the first known one is returned.
    /// Returns an error (caller must skip reuse) when:
    /// - `core_dir` is a custom signet folder (`signet_<hash>`), whatever the
    ///   other sources say: its block magic is not the standard signet magic;
    /// - the marker names a network BLVM does not know;
    /// - two known sources disagree;
    /// - no source is known.
    pub fn resolve_reuse_network(
        core_dir: &Path,
        marker_network: Option<&str>,
        node_network: Option<CoreDataNetwork>,
    ) -> Result<CoreDataNetwork> {
        if let Some(dir_name) = core_dir.file_name().and_then(|n| n.to_str()) {
            if Self::is_custom_signet_dir_name(dir_name) {
                return Err(Self::custom_signet_error(dir_name));
            }
        }

        let marker = match marker_network.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Some(s.parse::<CoreDataNetwork>().map_err(|e| {
                anyhow::anyhow!("Migration marker network '{s}' is not usable: {e}")
            })?),
            None => None,
        };
        let folder = Self::detect_network(core_dir);

        let sources = [
            ("migration marker", marker),
            ("Core datadir folder name", folder),
            ("node network", node_network),
        ];
        let mut chosen: Option<(&str, CoreDataNetwork)> = None;
        for (label, network) in sources {
            let Some(network) = network else { continue };
            match chosen {
                None => chosen = Some((label, network)),
                Some((first_label, first)) if first != network => {
                    return Err(anyhow::anyhow!(
                        "Network mismatch for Core datadir {:?}: {} says {} but {} says {}",
                        core_dir,
                        first_label,
                        first,
                        label,
                        network
                    ));
                }
                Some(_) => {}
            }
        }

        chosen.map(|(_, network)| network).ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine network for Core datadir {:?}: the migration marker \
                 records none, the folder name is not a standard Core network folder, \
                 and no node network was supplied.",
                core_dir
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

    // ---- resolve_reuse_network: marker, then folder name, then node network ----

    fn resolve(
        dir: &Path,
        marker: Option<&str>,
        node: Option<CoreDataNetwork>,
    ) -> Result<CoreDataNetwork> {
        BitcoinCoreDetection::resolve_reuse_network(dir, marker, node)
    }

    #[test]
    fn resolve_nonstandard_mainnet_names_use_node_network() {
        let temp = TempDir::new().unwrap();
        for name in ["bitcoin", "Bitcoin", "btc-data"] {
            let dir = temp.path().join(name);
            assert_eq!(
                resolve(&dir, None, Some(CoreDataNetwork::Mainnet)).unwrap(),
                CoreDataNetwork::Mainnet,
                "{name} with node network mainnet"
            );
        }
        let srv = temp.path().join("srv").join("btc-data");
        assert_eq!(
            resolve(&srv, None, Some(CoreDataNetwork::Mainnet)).unwrap(),
            CoreDataNetwork::Mainnet
        );
    }

    #[test]
    fn resolve_without_any_source_refuses_instead_of_defaulting_to_mainnet() {
        let temp = TempDir::new().unwrap();
        let err = resolve(&temp.path().join("bitcoin"), None, None).unwrap_err();
        assert!(
            err.to_string().contains("Cannot determine network"),
            "{err}"
        );
        // An empty marker network counts as absent, not as a source.
        assert!(resolve(&temp.path().join("bitcoin"), Some(""), None).is_err());
    }

    #[test]
    fn resolve_priority_marker_then_folder_then_node() {
        let temp = TempDir::new().unwrap();
        let odd = temp.path().join("chain-data");
        assert_eq!(
            resolve(&odd, Some("testnet4"), None).unwrap(),
            CoreDataNetwork::Testnet4
        );
        let t3 = temp.path().join(".bitcoin").join("testnet3");
        assert_eq!(resolve(&t3, None, None).unwrap(), CoreDataNetwork::Testnet);
        assert_eq!(
            resolve(&t3, Some("testnet"), Some(CoreDataNetwork::Testnet)).unwrap(),
            CoreDataNetwork::Testnet
        );
    }

    #[test]
    fn resolve_marker_folder_disagreement_refuses() {
        let temp = TempDir::new().unwrap();
        // S3: a mainnet marker on .bitcoin/signet must not open a mainnet reader.
        let signet = temp.path().join(".bitcoin").join("signet");
        let err = resolve(&signet, Some("mainnet"), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Network mismatch"), "{msg}");
        assert!(msg.contains("migration marker says mainnet"), "{msg}");
        assert!(msg.contains("folder name says signet"), "{msg}");
        let t4 = temp.path().join("testnet4");
        assert!(resolve(&t4, Some("testnet"), None).is_err());
    }

    #[test]
    fn resolve_node_disagreement_refuses() {
        let temp = TempDir::new().unwrap();
        let t3 = temp.path().join(".bitcoin").join("testnet3");
        assert!(resolve(&t3, None, Some(CoreDataNetwork::Mainnet)).is_err());
        let odd = temp.path().join("bitcoin");
        assert!(resolve(&odd, Some("regtest"), Some(CoreDataNetwork::Mainnet)).is_err());
        let dotbitcoin = temp.path().join(".bitcoin");
        assert!(resolve(&dotbitcoin, None, Some(CoreDataNetwork::Regtest)).is_err());
    }

    #[test]
    fn resolve_custom_signet_refuses_whatever_the_other_sources_say() {
        let temp = TempDir::new().unwrap();
        let custom = temp.path().join("signet_0f9188f13cb7b2c71f2a335e3a4fc328");
        for (marker, node) in [
            (None, None),
            (Some("signet"), None),
            (None, Some(CoreDataNetwork::Signet)),
            (Some("signet"), Some(CoreDataNetwork::Signet)),
        ] {
            let err = resolve(&custom, marker, node).unwrap_err();
            assert!(
                err.to_string().contains("Custom signet directory detected"),
                "marker={marker:?} node={node:?}: {err}"
            );
        }
    }

    #[test]
    fn resolve_unknown_marker_network_refuses() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("bitcoin");
        let err = resolve(&dir, Some("liquid"), Some(CoreDataNetwork::Mainnet)).unwrap_err();
        assert!(err.to_string().contains("liquid"), "{err}");
    }

    #[test]
    fn custom_signet_error_names_the_real_setting() {
        let temp = TempDir::new().unwrap();
        let custom = temp.path().join("signet_abc123");
        for err in [
            BitcoinCoreDetection::detect_network_strict(&custom).unwrap_err(),
            resolve(&custom, None, Some(CoreDataNetwork::Signet)).unwrap_err(),
        ] {
            let msg = err.to_string();
            assert!(msg.contains("storage.reuse_core_block_files"), "{msg}");
            assert!(!msg.contains("--network"), "{msg}");
        }
    }
}

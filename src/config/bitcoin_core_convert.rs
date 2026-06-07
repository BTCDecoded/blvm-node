//! Convert Bitcoin Core bitcoin.conf to blvm-node config.toml
//!
//! Used by `blvm config convert-core` and the standalone convert-bitcoin-core-config binary.

use std::fs;
use std::path::Path;

#[derive(Debug, Default)]
pub struct BitcoinCoreConfig {
    pub network: Option<String>,
    pub testnet: bool,
    pub regtest: bool,
    pub rpc_port: Option<u16>,
    pub rpc_bind: Option<String>,
    pub rpc_allowip: Vec<String>,
    pub rpc_user: Option<String>,
    pub rpc_password: Option<String>,
    pub rpc_auth: Vec<String>,
    pub max_connections: Option<usize>,
    pub listen: bool,
    pub bind: Option<String>,
    pub externalip: Option<String>,
    pub onlynet: Option<String>,
    pub proxy: Option<String>,
    pub seednode: Vec<String>,
    pub addnode: Vec<String>,
    pub connect: Vec<String>,
    pub discover: Option<bool>,
    pub server: bool,
    pub rpc_workqueue: Option<usize>,
    pub rpc_threads: Option<usize>,
    pub daemon: bool,
    pub printtoconsole: bool,
    pub datadir: Option<String>,
    pub logtimestamps: bool,
    pub logips: bool,
    pub logtimemicros: bool,
    pub showdebug: Option<String>,
    pub debug: Vec<String>,
    pub loglevel: Option<String>,
}

/// Parse a Bitcoin Core bitcoin.conf file.
pub fn parse_bitcoin_conf(path: &Path) -> std::io::Result<BitcoinCoreConfig> {
    let content = fs::read_to_string(path)?;
    let mut config = BitcoinCoreConfig::default();

    for line in content.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "testnet" => {
                    if value == "1" || value == "true" {
                        config.testnet = true;
                        config.network = Some("testnet3".to_string());
                    }
                }
                "regtest" => {
                    if value == "1" || value == "true" {
                        config.regtest = true;
                        config.network = Some("regtest".to_string());
                    }
                }
                "mainnet" => {
                    if value == "1" || value == "true" {
                        config.network = Some("bitcoin-v1".to_string());
                    }
                }
                "rpcport" => {
                    if let Ok(port) = value.parse() {
                        config.rpc_port = Some(port);
                    }
                }
                "rpcbind" => config.rpc_bind = Some(value.to_string()),
                "rpcallowip" => config.rpc_allowip.push(value.to_string()),
                "rpcuser" => config.rpc_user = Some(value.to_string()),
                "rpcpassword" => config.rpc_password = Some(value.to_string()),
                "rpcauth" => config.rpc_auth.push(value.to_string()),
                "maxconnections" => {
                    if let Ok(max) = value.parse() {
                        config.max_connections = Some(max);
                    }
                }
                "listen" => config.listen = value == "1" || value == "true",
                "bind" => config.bind = Some(value.to_string()),
                "externalip" => config.externalip = Some(value.to_string()),
                "onlynet" => config.onlynet = Some(value.to_string()),
                "proxy" => config.proxy = Some(value.to_string()),
                "seednode" => config.seednode.push(value.to_string()),
                "addnode" => config.addnode.push(value.to_string()),
                "connect" => config.connect.push(value.to_string()),
                "discover" => config.discover = Some(value == "1" || value == "true"),
                "server" => config.server = value == "1" || value == "true",
                "rpcworkqueue" => {
                    if let Ok(queue) = value.parse() {
                        config.rpc_workqueue = Some(queue);
                    }
                }
                "rpcthreads" => {
                    if let Ok(threads) = value.parse() {
                        config.rpc_threads = Some(threads);
                    }
                }
                "daemon" => config.daemon = value == "1" || value == "true",
                "datadir" => config.datadir = Some(value.to_string()),
                "printtoconsole" => config.printtoconsole = value == "1" || value == "true",
                "logtimestamps" => config.logtimestamps = value == "1" || value == "true",
                "logips" => config.logips = value == "1" || value == "true",
                "logtimemicros" => config.logtimemicros = value == "1" || value == "true",
                "showdebug" => config.showdebug = Some(value.to_string()),
                "debug" => config.debug.push(value.to_string()),
                "loglevel" => config.loglevel = Some(value.to_string()),
                _ => {}
            }
        }
    }

    Ok(config)
}

/// Expand a leading `~` in Core-style paths (e.g. `datadir=~/.bitcoin`).
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn default_core_datadir_base() -> Option<String> {
    dirs::home_dir().map(|h| h.join(".bitcoin").to_string_lossy().into_owned())
}

/// Resolve Core `datadir=` plus network flags to the directory containing `chainstate/`.
///
/// Core stores testnet at `<datadir>/testnet3`, regtest at `<datadir>/regtest`, mainnet at `<datadir>`.
pub fn effective_core_datadir(config: &BitcoinCoreConfig) -> Option<String> {
    let base = config
        .datadir
        .as_ref()
        .map(|s| expand_tilde(s))
        .or_else(default_core_datadir_base)?;
    let base = base.trim_end_matches('/').to_string();
    if config.regtest {
        Some(format!("{base}/regtest"))
    } else if config.testnet {
        Some(format!("{base}/testnet3"))
    } else {
        Some(base)
    }
}

/// Generate blvm-node TOML config from parsed Bitcoin Core config.
pub fn generate_toml_config(config: &BitcoinCoreConfig, input_path: &Path) -> String {
    let mut toml = String::new();

    toml.push_str("# blvm-node configuration\n");
    toml.push_str(&format!(
        "# Converted from Bitcoin Core: {}\n",
        input_path.display()
    ));
    toml.push_str(&format!(
        "# Generated: {}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    ));
    toml.push('\n');

    if let Some(effective) = effective_core_datadir(config) {
        toml.push_str("[storage]\n");
        toml.push_str(&format!("data_dir = \"{effective}\"\n"));
        if let Some(ref raw) = config.datadir {
            if config.testnet || config.regtest {
                let net = if config.regtest {
                    "regtest"
                } else {
                    "testnet3"
                };
                toml.push_str(&format!(
                    "# Core datadir=\"{raw}\" + {net} → effective path above\n"
                ));
            }
        } else if config.testnet || config.regtest {
            toml.push_str("# Default Core base ~/.bitcoin with network subdir applied\n");
        }
        toml.push_str("# When data_dir contains a synced Core layout (chainstate/ + blocks/),\n");
        toml.push_str("# blvm start auto-migrates to <data_dir>/blvm/ unless disabled.\n");
        toml.push_str("auto_migrate_core = true\n");
        toml.push('\n');
    } else {
        toml.push_str("# [storage]\n");
        toml.push_str("# data_dir = \"~/.bitcoin\"  # set from Core datadir= if present\n");
        toml.push('\n');
    }

    toml.push_str("[network]\n");
    if let Some(ref network) = config.network {
        toml.push_str(&format!("protocol_version = \"{network}\"\n"));
    }

    let port = if config.testnet {
        "18333"
    } else if config.regtest {
        "18444"
    } else {
        "8333"
    };

    if let Some(ref bind) = config.bind {
        toml.push_str(&format!("listen_addr = \"{bind}:{port}\"\n"));
    } else if config.listen {
        toml.push_str(&format!("listen_addr = \"0.0.0.0:{port}\"\n"));
    }

    if let Some(max) = config.max_connections {
        toml.push_str(&format!("max_peers = {max}\n"));
    }

    let mut persistent_peers = Vec::new();
    persistent_peers.extend_from_slice(&config.addnode);
    persistent_peers.extend_from_slice(&config.connect);

    if !persistent_peers.is_empty() {
        toml.push_str("\n# Persistent peers (from addnode/connect)\n");
        toml.push_str("persistent_peers = [\n");
        for peer in &persistent_peers {
            let peer_with_port = if !peer.contains(':') {
                format!("{peer}:{port}")
            } else {
                peer.clone()
            };
            toml.push_str(&format!("  \"{peer_with_port}\",\n"));
        }
        toml.push_str("]\n");
    }

    if config.rpc_port.is_some() || config.rpc_user.is_some() || !config.rpc_auth.is_empty() {
        toml.push_str("\n[rpc_auth]\n");
        if let Some(port) = config.rpc_port {
            toml.push_str(&format!("port = {port}\n"));
        }
        if let Some(ref bind) = config.rpc_bind {
            toml.push_str(&format!("bind = \"{bind}\"\n"));
        }
        if let (Some(ref user), Some(ref password)) = (&config.rpc_user, &config.rpc_password) {
            toml.push_str("# Basic auth (user/password)\n");
            toml.push_str(&format!("username = \"{user}\"\n"));
            toml.push_str(&format!("password = \"{password}\"\n"));
        } else if !config.rpc_auth.is_empty() {
            toml.push_str("# RPC auth (rpcauth format)\n");
            toml.push_str("# Note: rpcauth format needs manual conversion\n");
            for auth in &config.rpc_auth {
                toml.push_str(&format!("# Original: rpcauth={auth}\n"));
            }
        }
        if !config.rpc_allowip.is_empty() {
            toml.push_str("allowed_ips = [\n");
            for ip in &config.rpc_allowip {
                toml.push_str(&format!("  \"{ip}\",\n"));
            }
            toml.push_str("]\n");
        }
    }

    toml.push_str("\n[transport_preference]\n");
    toml.push_str("prefer_tcp = true\n");
    toml.push_str("prefer_quinn = false\n");
    toml.push_str("prefer_iroh = false\n");

    toml.push_str("\n[network_timing]\n");
    if let Some(max) = config.max_connections {
        toml.push_str(&format!("target_peer_count = {max}\n"));
    } else {
        toml.push_str("target_peer_count = 8\n");
    }

    toml.push_str("\n# Additional notes:\n");
    toml.push_str(
        "# - storage.data_dir targets the network-specific Core tree (see subdir comments)\n",
    );
    toml.push_str("# - Some Bitcoin Core options may not have direct equivalents\n");
    toml.push_str("# - Review and adjust settings as needed\n");

    toml
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn parse_and_emit_datadir_in_storage_section() {
        let dir = TempDir::new().unwrap();
        let conf = dir.path().join("bitcoin.conf");
        let mut f = std::fs::File::create(&conf).unwrap();
        writeln!(f, "datadir=/var/lib/bitcoind").unwrap();
        writeln!(f, "rpcuser=u").unwrap();
        drop(f);

        let parsed = parse_bitcoin_conf(&conf).unwrap();
        assert_eq!(parsed.datadir.as_deref(), Some("/var/lib/bitcoind"));
        assert_eq!(
            effective_core_datadir(&parsed).as_deref(),
            Some("/var/lib/bitcoind")
        );

        let toml = generate_toml_config(&parsed, &conf);
        assert!(toml.contains("[storage]"));
        assert!(toml.contains("data_dir = \"/var/lib/bitcoind\""));
        assert!(toml.contains("auto_migrate_core = true"));
    }

    #[test]
    fn effective_datadir_uses_network_subdir() {
        let mut cfg = BitcoinCoreConfig::default();
        cfg.datadir = Some("/data/btc".to_string());
        cfg.testnet = true;
        assert_eq!(
            effective_core_datadir(&cfg).as_deref(),
            Some("/data/btc/testnet3")
        );

        cfg.testnet = false;
        cfg.regtest = true;
        assert_eq!(
            effective_core_datadir(&cfg).as_deref(),
            Some("/data/btc/regtest")
        );
    }

    #[test]
    fn effective_datadir_expands_tilde() {
        let home = dirs::home_dir().expect("home");
        let mut cfg = BitcoinCoreConfig::default();
        cfg.datadir = Some("~/.bitcoin".to_string());
        assert_eq!(
            effective_core_datadir(&cfg).as_deref(),
            Some(home.join(".bitcoin").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn testnet_without_datadir_emits_default_storage_path() {
        let dir = TempDir::new().unwrap();
        let conf = dir.path().join("bitcoin.conf");
        let mut f = std::fs::File::create(&conf).unwrap();
        writeln!(f, "testnet=1").unwrap();
        drop(f);

        let parsed = parse_bitcoin_conf(&conf).unwrap();
        let toml = generate_toml_config(&parsed, &conf);
        assert!(toml.contains("[storage]"));
        assert!(toml.contains("testnet3"));
        assert!(toml.contains("auto_migrate_core = true"));
    }
}

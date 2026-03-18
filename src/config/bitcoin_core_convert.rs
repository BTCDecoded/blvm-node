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
    toml.push_str("\n# NOTE: Data directories are NOT converted - configure separately\n\n");

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
    toml.push_str("# - Data directories are NOT converted (configure separately)\n");
    toml.push_str("# - Some Bitcoin Core options may not have direct equivalents\n");
    toml.push_str("# - Review and adjust settings as needed\n");

    toml
}

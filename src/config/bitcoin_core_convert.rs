//! Convert Bitcoin Core bitcoin.conf to blvm-node config.toml
//!
//! Used by `blvm config convert-core` and the standalone convert-bitcoin-core-config binary.

use std::fs;
use std::path::Path;

#[derive(Debug, Default)]
pub struct BitcoinCoreConfig {
    pub network: Option<String>,
    pub testnet: bool,
    pub testnet4: bool,
    pub regtest: bool,
    pub dbcache_mb: Option<u64>,
    pub max_mempool_mb: Option<u64>,
    pub rpc_port: Option<u16>,
    pub rpc_bind: Option<String>,
    pub rpc_allowip: Vec<String>,
    pub rpc_user: Option<String>,
    pub rpc_password: Option<String>,
    pub rpc_auth: Vec<String>,
    pub max_connections: Option<usize>,
    pub listen: bool,
    pub listen_key_present: bool,
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
    /// Core keys that have no NodeConfig field, in file order.
    pub unmapped: Vec<(String, String)>,
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
                "chain" => {
                    if value.eq_ignore_ascii_case("testnet4") {
                        config.testnet4 = true;
                        config.testnet = false;
                        config.regtest = false;
                    }
                }
                "dbcache" => {
                    if let Ok(mb) = value.parse() {
                        config.dbcache_mb = Some(mb);
                    }
                }
                "maxmempool" => {
                    if let Ok(mb) = value.parse() {
                        config.max_mempool_mb = Some(mb);
                    }
                }
                "rpcport" => {
                    if let Ok(port) = value.parse() {
                        config.rpc_port = Some(port);
                    }
                }
                "rpcbind" => config.rpc_bind = Some(value.to_string()),
                "rpcallowip" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "rpcuser" => config.rpc_user = Some(value.to_string()),
                "rpcpassword" => config.rpc_password = Some(value.to_string()),
                "rpcauth" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "maxconnections" => {
                    if let Ok(max) = value.parse() {
                        config.max_connections = Some(max);
                    }
                }
                "listen" => {
                    config.listen_key_present = true;
                    config.listen = value == "1" || value.eq_ignore_ascii_case("true");
                }
                "bind" => config.bind = Some(value.to_string()),
                "externalip" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "onlynet" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "proxy" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "seednode" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "addnode" => config.addnode.push(value.to_string()),
                "connect" => config.connect.push(value.to_string()),
                "discover" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "server" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "rpcworkqueue" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "rpcthreads" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "daemon" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "datadir" => config.datadir = Some(value.to_string()),
                "printtoconsole" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "logtimestamps" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "logips" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "logtimemicros" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "showdebug" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "debug" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                "loglevel" => {
                    config.unmapped.push((key.clone(), value.to_string()));
                }
                _ => {}
            }
        }
    }

    Ok(config)
}

/// Expand a leading `~` in Core-style paths (e.g. `datadir=~/.bitcoin`).
/// A result that still contains `..` is left unexpanded.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            let joined = home.join(rest);
            if joined
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return path.to_string();
            }
            return joined.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn secret_conf_key(key: &str) -> bool {
    matches!(key, "rpcauth" | "rpcuser" | "rpcpassword")
}

pub fn format_unmapped_comment(key: &str, value: &str) -> String {
    if secret_conf_key(key) {
        format!("# ignored: {key}=(redacted)")
    } else {
        format!("# ignored: {key}={value}")
    }
}

/// TOML basic string, including the surrounding quotes.
fn toml_basic(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write generated config mode `0600`. An existing file is forced to `0600`.
pub fn write_private_config(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn default_core_datadir_base() -> Option<String> {
    dirs::home_dir().map(|h| h.join(".bitcoin").to_string_lossy().into_owned())
}

/// Resolve Core `datadir=` plus network flags to the directory containing `chainstate/`.
///
/// Core stores testnet at `<datadir>/testnet3`, testnet4 at `<datadir>/testnet4`,
/// regtest at `<datadir>/regtest`, mainnet at `<datadir>`.
pub fn effective_core_datadir(config: &BitcoinCoreConfig) -> Option<String> {
    let base = config
        .datadir
        .as_ref()
        .map(|s| expand_tilde(s))
        .or_else(default_core_datadir_base)?;
    let base = base.trim_end_matches('/').to_string();
    if config.regtest {
        Some(format!("{base}/regtest"))
    } else if config.testnet4 {
        Some(format!("{base}/testnet4"))
    } else if config.testnet {
        Some(format!("{base}/testnet3"))
    } else {
        Some(base)
    }
}

/// Parser stores `testnet3` and `bitcoin-v1`. The file uses the names NodeConfig loads.
fn canonical_protocol_version(config: &BitcoinCoreConfig) -> Option<&'static str> {
    if config.testnet4 {
        return Some("testnet4");
    }
    match config.network.as_deref() {
        Some("testnet3") | Some("testnet") => Some("testnet"),
        Some("bitcoin-v1") | Some("mainnet") => Some("mainnet"),
        Some("regtest") => Some("regtest"),
        _ => None,
    }
}

fn rpc_addr_value(config: &BitcoinCoreConfig) -> Option<String> {
    let port = config
        .rpc_port
        .map(|port| port.to_string())
        .unwrap_or_else(|| "8332".to_string());
    if let Some(bind) = config.rpc_bind.as_deref() {
        Some(addr_with_port(bind, &port))
    } else if config.rpc_port.is_some() {
        Some(format!("127.0.0.1:{port}"))
    } else {
        None
    }
}

fn network_port(config: &BitcoinCoreConfig) -> &'static str {
    if config.testnet4 {
        "48333"
    } else if config.testnet {
        "18333"
    } else if config.regtest {
        "18444"
    } else {
        "8333"
    }
}

/// Append `port` only when `addr` does not already name one.
///
/// Bracketed IPv6 (`[::1]`) contains colons and still needs a port. A trailing
/// `:port` or `]:port` is left unchanged.
fn addr_with_port(addr: &str, port: &str) -> String {
    if let Some(rest) = addr.strip_prefix('[') {
        if rest.contains("]:") {
            addr.to_string()
        } else {
            format!("{addr}:{port}")
        }
    } else if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{port}")
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

    if let Some(network) = canonical_protocol_version(config) {
        toml.push_str(&format!("protocol_version = \"{network}\"\n"));
    }

    let port = network_port(config);

    if let Some(ref bind) = config.bind {
        let listen_addr = addr_with_port(bind, port);
        toml.push_str(&format!("listen_addr = \"{listen_addr}\"\n"));
    } else if config.listen {
        toml.push_str(&format!("listen_addr = \"0.0.0.0:{port}\"\n"));
    }

    if config.listen_key_present && !config.listen {
        toml.push_str("listen = false\n");
    }

    if let Some(max) = config.max_connections {
        toml.push_str(&format!("max_peers = {max}\n"));
    }

    let mut persistent_peers = Vec::new();
    persistent_peers.extend_from_slice(&config.addnode);
    persistent_peers.extend_from_slice(&config.connect);

    if !persistent_peers.is_empty() {
        toml.push_str("\n# Persistent peers (from addnode/connect). DNS discovery stays on.\n");
        toml.push_str("persistent_peers = [\n");
        for peer in &persistent_peers {
            let peer_with_port = addr_with_port(peer, port);
            toml.push_str(&format!("  {},\n", toml_basic(&peer_with_port)));
        }
        toml.push_str("]\n");
    }

    if let Some(rpc_addr) = rpc_addr_value(config) {
        toml.push_str(&format!("rpc_addr = {}\n", toml_basic(&rpc_addr)));
    }

    toml.push_str("transport_preference = \"tcponly\"\n");
    toml.push('\n');

    if let Some(effective) = effective_core_datadir(config) {
        toml.push_str("[storage]\n");
        toml.push_str(&format!("data_dir = {}\n", toml_basic(&effective)));
        if let Some(ref raw) = config.datadir {
            if config.testnet || config.regtest {
                let net = if config.regtest {
                    "regtest"
                } else {
                    "testnet3"
                };
                toml.push_str(&format!(
                    "# Core datadir={} + {net} → effective path above\n",
                    toml_basic(raw)
                ));
            }
        } else if config.testnet || config.regtest {
            toml.push_str("# Default Core base ~/.bitcoin with network subdir applied\n");
        }
        toml.push_str("# When data_dir contains a synced Core layout (chainstate/ + blocks/),\n");
        toml.push_str("# blvm start auto-migrates to <data_dir>/blvm/ unless disabled.\n");
        toml.push_str("auto_migrate_core = true\n");
        if let Some(mb) = config.dbcache_mb {
            toml.push_str(&format!("dbcache_mb = {mb}\n"));
        }
        toml.push('\n');
    } else {
        toml.push_str("# [storage]\n");
        toml.push_str("# data_dir = \"~/.bitcoin\"  # set from Core datadir= if present\n");
        toml.push('\n');
    }

    if config.rpc_user.is_some() || config.rpc_password.is_some() {
        toml.push_str("\n[rpc_auth]\n");
        if let (Some(user), Some(password)) = (&config.rpc_user, &config.rpc_password) {
            toml.push_str("# Basic auth (user/password)\n");
            toml.push_str(&format!("username = {}\n", toml_basic(user)));
            toml.push_str(&format!("password = {}\n", toml_basic(password)));
        }
    }

    if let Some(mb) = config.max_mempool_mb {
        toml.push_str("\n[mempool]\n");
        toml.push_str(&format!("max_mempool_mb = {mb}\n"));
    }

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

    for (key, value) in &config.unmapped {
        toml.push_str(&format_unmapped_comment(key, value));
        toml.push('\n');
    }

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

    #[test]
    fn core_keys_keep_port_chain_cache_and_mempool() {
        let dir = TempDir::new().unwrap();
        let conf = dir.path().join("bitcoin.conf");
        let mut f = std::fs::File::create(&conf).unwrap();
        writeln!(f, "chain=testnet4").unwrap();
        writeln!(f, "datadir=/data").unwrap();
        writeln!(f, "bind=127.0.0.1:8333").unwrap();
        writeln!(f, "addnode=[::1]").unwrap();
        writeln!(f, "addnode=1.2.3.4:18444").unwrap();
        writeln!(f, "dbcache=4096").unwrap();
        writeln!(f, "maxmempool=50").unwrap();
        drop(f);

        let parsed = parse_bitcoin_conf(&conf).unwrap();
        assert_eq!(
            effective_core_datadir(&parsed).as_deref(),
            Some("/data/testnet4")
        );
        let toml = generate_toml_config(&parsed, &conf);
        assert!(toml.contains("data_dir = \"/data/testnet4\""));
        assert!(toml.contains("listen_addr = \"127.0.0.1:8333\""));
        assert!(!toml.contains("127.0.0.1:8333:8333"));
        assert!(toml.contains("\"[::1]:48333\""));
        assert!(toml.contains("\"1.2.3.4:18444\""));
        assert!(toml.contains("dbcache_mb = 4096"));
        assert!(toml.contains("max_mempool_mb = 50"));

        let mainnet = dir.path().join("mainnet.conf");
        let mut f = std::fs::File::create(&mainnet).unwrap();
        writeln!(f, "addnode=[::1]").unwrap();
        writeln!(f, "addnode=[::1]:8333").unwrap();
        drop(f);
        let parsed = parse_bitcoin_conf(&mainnet).unwrap();
        let toml = generate_toml_config(&parsed, &mainnet);
        assert!(toml.contains("\"[::1]:8333\""));
        assert!(!toml.contains("\"[::1]:8333:8333\""));
    }

    #[test]
    fn generated_toml_round_trips_probe_as_node_config() {
        let dir = TempDir::new().unwrap();
        let conf = dir.path().join("bitcoin.conf");
        let mut f = std::fs::File::create(&conf).unwrap();
        writeln!(f, "testnet=1").unwrap();
        writeln!(f, "listen=1").unwrap();
        writeln!(f, "maxconnections=40").unwrap();
        writeln!(f, "addnode=peer.example").unwrap();
        writeln!(f, "rpcport=18332").unwrap();
        writeln!(f, "rpcuser=u").unwrap();
        writeln!(f, "rpcpassword=p").unwrap();
        writeln!(f, "rpcallowip=10.0.0.1").unwrap();
        writeln!(f, "proxy=127.0.0.1:9050").unwrap();
        writeln!(f, "onlynet=onion").unwrap();
        drop(f);

        let parsed = parse_bitcoin_conf(&conf).unwrap();
        let body = generate_toml_config(&parsed, &conf);
        let loaded: crate::config::NodeConfig =
            toml::from_str(&body).expect("converted conf is NodeConfig");
        assert_eq!(loaded.protocol_version.as_deref(), Some("testnet"));
        assert_eq!(
            loaded.listen_addr,
            Some("0.0.0.0:18333".parse::<std::net::SocketAddr>().unwrap())
        );
        assert_eq!(loaded.max_outbound_peers, Some(40));
        assert_eq!(
            loaded.persistent_peers,
            vec!["peer.example:18333".to_string()]
        );
        assert_eq!(
            loaded.rpc_addr,
            Some("127.0.0.1:18332".parse::<std::net::SocketAddr>().unwrap())
        );
        assert_eq!(
            loaded
                .network_timing
                .unwrap()
                .effective_target_outbound_peers(),
            40
        );

        let t4 = dir.path().join("t4.conf");
        let mut f = std::fs::File::create(&t4).unwrap();
        writeln!(f, "chain=testnet4").unwrap();
        drop(f);
        let parsed = parse_bitcoin_conf(&t4).unwrap();
        let body = generate_toml_config(&parsed, &t4);
        let loaded: crate::config::NodeConfig = toml::from_str(&body).unwrap();
        assert_eq!(loaded.protocol_version.as_deref(), Some("testnet4"));

        let bind = dir.path().join("bind.conf");
        let mut f = std::fs::File::create(&bind).unwrap();
        writeln!(f, "rpcbind=127.0.0.1").unwrap();
        writeln!(f, "rpcport=18332").unwrap();
        drop(f);
        let parsed = parse_bitcoin_conf(&bind).unwrap();
        let body = generate_toml_config(&parsed, &bind);
        let loaded: crate::config::NodeConfig =
            toml::from_str(&body).expect("portless rpcbind is a SocketAddr");
        assert_eq!(
            loaded.rpc_addr,
            Some("127.0.0.1:18332".parse::<std::net::SocketAddr>().unwrap())
        );
    }

    #[tokio::test]
    async fn listen_defaults_true_and_core_listen_zero_is_explicit() {
        let omitted: crate::config::NodeConfig =
            toml::from_str("protocol_version = \"regtest\"\n").unwrap();
        assert!(omitted.listen);

        let off: crate::config::NodeConfig = toml::from_str("listen = false\n").unwrap();
        assert!(!off.listen);

        let listen: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let manager = crate::network::NetworkManager::with_config(
            listen,
            8,
            crate::network::TransportPreference::TCP_ONLY,
            Some(&off),
        );
        assert!(!manager.listens());

        let dir = TempDir::new().unwrap();
        let omitted_conf = dir.path().join("omit.conf");
        let mut f = std::fs::File::create(&omitted_conf).unwrap();
        writeln!(f, "testnet=1").unwrap();
        drop(f);
        let parsed = parse_bitcoin_conf(&omitted_conf).unwrap();
        let body = generate_toml_config(&parsed, &omitted_conf);
        assert!(!body.contains("listen = false"));

        let zero = dir.path().join("zero.conf");
        let mut f = std::fs::File::create(&zero).unwrap();
        writeln!(f, "listen=0").unwrap();
        drop(f);
        let parsed = parse_bitcoin_conf(&zero).unwrap();
        let body = generate_toml_config(&parsed, &zero);
        assert!(body.contains("listen = false"));
    }

    #[test]
    fn unmapped_core_keys_are_ignored_comments() {
        let dir = TempDir::new().unwrap();
        let conf = dir.path().join("bitcoin.conf");
        let mut f = std::fs::File::create(&conf).unwrap();
        writeln!(f, "proxy=127.0.0.1:9050").unwrap();
        writeln!(f, "onlynet=onion").unwrap();
        writeln!(f, "rpcallowip=10.0.0.1").unwrap();
        writeln!(f, "rpcauth=alice:salt$hash").unwrap();
        writeln!(f, "datadir=/tmp/a\"b").unwrap();
        drop(f);

        let parsed = parse_bitcoin_conf(&conf).unwrap();
        let body = generate_toml_config(&parsed, &conf);
        assert!(body.contains("# ignored: proxy=127.0.0.1:9050"));
        assert!(body.contains("# ignored: onlynet=onion"));
        assert!(body.contains("# ignored: rpcallowip=10.0.0.1"));
        assert!(body.contains("# ignored: rpcauth=(redacted)"));
        assert!(!body.contains("salt$hash"));
        assert!(body.contains("data_dir = \"/tmp/a\\\"b\""));
        assert!(!body.contains("allowed_ips"));

        let loaded: crate::config::NodeConfig =
            toml::from_str(&body).expect("ignored keys still load");
        let stripped = body
            .lines()
            .filter(|line| !line.starts_with("# ignored:"))
            .collect::<Vec<_>>()
            .join("\n");
        let without: crate::config::NodeConfig = toml::from_str(&stripped).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&without).unwrap()
        );
    }
}

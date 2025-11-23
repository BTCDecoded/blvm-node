//! Convert Bitcoin Core bitcoin.conf to bllvm-node config.toml
//!
//! This tool converts Bitcoin Core configuration files to bllvm-node format.
//! Data directories are NOT converted (as requested).

use std::fs;
use std::path::PathBuf;

struct Args {
    input: PathBuf,
    output: PathBuf,
    verbose: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let input = args.next().expect("Input file required").into();
        let output = args
            .next()
            .unwrap_or_else(|| "config.toml".to_string())
            .into();
        let verbose = args.any(|a| a == "-v" || a == "--verbose");
        Self {
            input,
            output,
            verbose,
        }
    }
}

#[derive(Debug, Default)]
struct BitcoinCoreConfig {
    // Network
    network: Option<String>,
    testnet: bool,
    regtest: bool,

    // RPC
    rpc_port: Option<u16>,
    rpc_bind: Option<String>,
    rpc_allowip: Vec<String>,
    rpc_user: Option<String>,
    rpc_password: Option<String>,
    rpc_auth: Vec<String>,

    // Network connections
    max_connections: Option<usize>,
    listen: bool,
    bind: Option<String>,
    externalip: Option<String>,
    onlynet: Option<String>,
    proxy: Option<String>,
    seednode: Vec<String>,
    addnode: Vec<String>,
    connect: Vec<String>,
    discover: Option<bool>,

    // Server
    server: bool,
    rpc_workqueue: Option<usize>,
    rpc_threads: Option<usize>,

    // Logging
    daemon: bool,
    printtoconsole: bool,
    logtimestamps: bool,
    logips: bool,
    logtimemicros: bool,
    showdebug: Option<String>,
    debug: Vec<String>,
    loglevel: Option<String>,
}

fn parse_bitcoin_conf(path: &PathBuf) -> std::io::Result<BitcoinCoreConfig> {
    let content = fs::read_to_string(path)?;
    let mut config = BitcoinCoreConfig::default();

    for line in content.lines() {
        // Remove comments
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        // Parse key=value
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                // Network
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

                // RPC
                "rpcport" => {
                    if let Ok(port) = value.parse() {
                        config.rpc_port = Some(port);
                    }
                }
                "rpcbind" => {
                    config.rpc_bind = Some(value.to_string());
                }
                "rpcallowip" => {
                    config.rpc_allowip.push(value.to_string());
                }
                "rpcuser" => {
                    config.rpc_user = Some(value.to_string());
                }
                "rpcpassword" => {
                    config.rpc_password = Some(value.to_string());
                }
                "rpcauth" => {
                    config.rpc_auth.push(value.to_string());
                }

                // Network connections
                "maxconnections" => {
                    if let Ok(max) = value.parse() {
                        config.max_connections = Some(max);
                    }
                }
                "listen" => {
                    config.listen = value == "1" || value == "true";
                }
                "bind" => {
                    config.bind = Some(value.to_string());
                }
                "externalip" => {
                    config.externalip = Some(value.to_string());
                }
                "onlynet" => {
                    config.onlynet = Some(value.to_string());
                }
                "proxy" => {
                    config.proxy = Some(value.to_string());
                }
                "seednode" => {
                    config.seednode.push(value.to_string());
                }
                "addnode" => {
                    config.addnode.push(value.to_string());
                }
                "connect" => {
                    config.connect.push(value.to_string());
                }
                "discover" => {
                    config.discover = Some(value == "1" || value == "true");
                }

                // Server
                "server" => {
                    config.server = value == "1" || value == "true";
                }
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

                // Logging
                "daemon" => {
                    config.daemon = value == "1" || value == "true";
                }
                "printtoconsole" => {
                    config.printtoconsole = value == "1" || value == "true";
                }
                "logtimestamps" => {
                    config.logtimestamps = value == "1" || value == "true";
                }
                "logips" => {
                    config.logips = value == "1" || value == "true";
                }
                "logtimemicros" => {
                    config.logtimemicros = value == "1" || value == "true";
                }
                "showdebug" => {
                    config.showdebug = Some(value.to_string());
                }
                "debug" => {
                    config.debug.push(value.to_string());
                }
                "loglevel" => {
                    config.loglevel = Some(value.to_string());
                }

                _ => {
                    // Unknown option - ignore
                }
            }
        }
    }

    Ok(config)
}

fn generate_toml_config(config: &BitcoinCoreConfig, input_path: &PathBuf) -> String {
    let mut toml = String::new();

    toml.push_str("# bllvm-node configuration\n");
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
    toml.push_str("# NOTE: Data directories are NOT converted - configure separately\n");
    toml.push('\n');

    // Network configuration
    toml.push_str("[network]\n");
    if let Some(ref network) = config.network {
        toml.push_str(&format!("protocol_version = \"{}\"\n", network));
    }

    // Listen address
    if let Some(ref bind) = config.bind {
        let port = if config.testnet {
            "18333"
        } else if config.regtest {
            "18444"
        } else {
            "8333"
        };
        toml.push_str(&format!("listen_addr = \"{}:{}\"\n", bind, port));
    } else if config.listen {
        let port = if config.testnet {
            "18333"
        } else if config.regtest {
            "18444"
        } else {
            "8333"
        };
        toml.push_str(&format!("listen_addr = \"0.0.0.0:{}\"\n", port));
    }

    // Max peers
    if let Some(max) = config.max_connections {
        toml.push_str(&format!("max_peers = {}\n", max));
    }

    // Persistent peers
    let mut persistent_peers = Vec::new();
    persistent_peers.extend_from_slice(&config.addnode);
    persistent_peers.extend_from_slice(&config.connect);

    if !persistent_peers.is_empty() {
        toml.push('\n');
        toml.push_str("# Persistent peers (from addnode/connect)\n");
        toml.push_str("persistent_peers = [\n");
        for peer in &persistent_peers {
            let peer_with_port = if !peer.contains(':') {
                let port = if config.testnet {
                    "18333"
                } else if config.regtest {
                    "18444"
                } else {
                    "8333"
                };
                format!("{}:{}", peer, port)
            } else {
                peer.clone()
            };
            toml.push_str(&format!("  \"{}\",\n", peer_with_port));
        }
        toml.push_str("]\n");
    }

    // RPC configuration
    if config.rpc_port.is_some() || config.rpc_user.is_some() || !config.rpc_auth.is_empty() {
        toml.push('\n');
        toml.push_str("[rpc_auth]\n");

        if let Some(port) = config.rpc_port {
            toml.push_str(&format!("port = {}\n", port));
        }

        if let Some(ref bind) = config.rpc_bind {
            toml.push_str(&format!("bind = \"{}\"\n", bind));
        }

        if let (Some(ref user), Some(ref password)) = (&config.rpc_user, &config.rpc_password) {
            toml.push_str("# Basic auth (user/password)\n");
            toml.push_str(&format!("username = \"{}\"\n", user));
            toml.push_str(&format!("password = \"{}\"\n", password));
        } else if !config.rpc_auth.is_empty() {
            toml.push_str("# RPC auth (rpcauth format)\n");
            toml.push_str("# Note: rpcauth format needs manual conversion\n");
            for auth in &config.rpc_auth {
                toml.push_str(&format!("# Original: rpcauth={}\n", auth));
            }
        }

        if !config.rpc_allowip.is_empty() {
            toml.push_str("allowed_ips = [\n");
            for ip in &config.rpc_allowip {
                toml.push_str(&format!("  \"{}\",\n", ip));
            }
            toml.push_str("]\n");
        }
    }

    // Transport preference
    toml.push('\n');
    toml.push_str("[transport_preference]\n");
    toml.push_str("prefer_tcp = true\n");
    toml.push_str("prefer_quinn = false\n");
    toml.push_str("prefer_iroh = false\n");

    // Network timing
    toml.push('\n');
    toml.push_str("[network_timing]\n");
    if let Some(max) = config.max_connections {
        toml.push_str(&format!("target_peer_count = {}\n", max));
    } else {
        toml.push_str("target_peer_count = 8\n");
    }

    toml.push('\n');
    toml.push_str("# Additional notes:\n");
    toml.push_str("# - Data directories are NOT converted (configure separately)\n");
    toml.push_str("# - Some Bitcoin Core options may not have direct equivalents\n");
    toml.push_str("# - Review and adjust settings as needed\n");

    toml
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if !args.input.exists() {
        eprintln!("Error: Input file '{}' not found", args.input.display());
        eprintln!();
        eprintln!("Usage: convert-bitcoin-core-config <bitcoin.conf> [output.toml]");
        eprintln!("Converts Bitcoin Core bitcoin.conf to bllvm-node config.toml");
        std::process::exit(1);
    }

    if args.verbose {
        eprintln!("Reading Bitcoin Core config from: {}", args.input.display());
    }

    let bitcoin_config = parse_bitcoin_conf(&args.input)?;

    if args.verbose {
        eprintln!("Generating bllvm-node config...");
    }

    let toml_config = generate_toml_config(&bitcoin_config, &args.input);

    fs::write(&args.output, toml_config)?;

    println!("✓ Configuration converted successfully!");
    println!("  Input:  {}", args.input.display());
    println!("  Output: {}", args.output.display());
    println!();
    println!("⚠️  IMPORTANT:");
    println!("  - Data directories are NOT converted");
    println!("  - Review the generated config and adjust as needed");
    println!("  - Some options may need manual configuration");

    Ok(())
}

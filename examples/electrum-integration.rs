//! Example: Connect Electrum wallet to bllvm-node
//!
//! This example demonstrates how to configure bllvm-node to work with Electrum wallet.
//! Electrum connects via JSON-RPC and requires specific RPC methods.
//!
//! Note: This is a configuration example. For a full running node, use the bllvm binary
//! with the generated config.toml file.

use bllvm_node::config::NodeConfig;

fn main() -> anyhow::Result<()> {
    println!("bllvm-node Electrum Integration Configuration");
    println!("=============================================");
    println!();

    // Create configuration optimized for Electrum
    let mut config = NodeConfig::default();

    // Network configuration
    config.protocol_version = Some("testnet3".to_string()); // Use "bitcoin-v1" for mainnet
    config.listen_addr = Some("127.0.0.1:18333".parse().unwrap()); // Testnet P2P port
    config.max_peers = Some(8);

    // RPC configuration
    // Note: RPC port is set via command line (--rpc-port) or defaults
    // RPC auth is optional - configure in generated config.toml if needed
    config.rpc_auth = Some(bllvm_node::config::RpcAuthConfig {
        required: false, // No auth required for localhost
        tokens: Vec::new(),
        certificates: Vec::new(),
        ..Default::default()
    });

    // Save configuration
    let config_path = "electrum-config.toml";
    config.to_toml_file(std::path::Path::new(config_path))?;

    println!("✓ Configuration created: {}", config_path);
    println!();
    println!("Configuration Summary:");
    println!("  RPC Port: 18332 (testnet)");
    println!("  RPC Bind: 127.0.0.1");
    println!("  Network: testnet3");
    println!("  P2P Port: 18333");
    println!();
    println!("To start the node with this config:");
    println!("  bllvm-node --config {} --network testnet", config_path);
    println!();
    println!("Electrum Configuration:");
    println!("  1. Open Electrum");
    println!("  2. Go to: Tools → Network → Server");
    println!("  3. Uncheck 'Select server automatically'");
    println!("  4. Enter: 127.0.0.1");
    println!("  5. Port: 18332 (testnet) or 8332 (mainnet)");
    println!("  6. Protocol: TCP");
    println!("  7. Click 'Close'");
    println!();
    println!("Electrum will now connect to your local bllvm-node!");

    Ok(())
}

//! Simple example module for blvm-node
//!
//! This module demonstrates:
//! - Module lifecycle (init, start, stop, shutdown)
//! - IPC communication with node
//! - Querying blockchain data
//! - Subscribing to node events
//! - Dynamic CLI: register "simple" command with "greet" subcommand
//!
//! Usage:
//!   simple-module --module-id <id> --socket-path <path> --data-dir <dir>
//!
//! When loaded, `blvm simple greet [name]` invokes the greet handler.

use blvm_node::module::integration::ModuleIntegration;
use blvm_node::module::ipc::protocol::{InvocationResultMessage, InvocationType};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    module_id: String,

    #[arg(long)]
    socket_path: PathBuf,

    #[arg(long)]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    blvm_node::utils::init_module_logging("simple_module", None);

    let args = Args::parse();

    info!("Simple Module starting");
    info!("Module ID: {}", args.module_id);
    info!("Socket path: {:?}", args.socket_path);
    info!("Data dir: {:?}", args.data_dir);

    let mut config = HashMap::new();
    for (key, value) in std::env::vars() {
        if key.starts_with("MODULE_CONFIG_") {
            let config_key = key.strip_prefix("MODULE_CONFIG_").unwrap().to_lowercase();
            config.insert(config_key, value);
        }
    }
    info!("Module config: {:?}", config);

    // CLI spec for "blvm simple greet [name]"
    let cli_spec = blvm_node::module::ipc::protocol::CliSpec {
        version: 1,
        name: "simple".to_string(),
        about: Some("Simple example module: greet subcommand".to_string()),
        subcommands: vec![blvm_node::module::ipc::protocol::CliSubcommandSpec {
            name: "greet".to_string(),
            about: Some("Print a greeting".to_string()),
            args: vec![blvm_node::module::ipc::protocol::CliArgSpec {
                name: "name".to_string(),
                long_name: None,
                short_name: None,
                required: Some(false),
                takes_value: Some(true),
                default: None,
            }],
        }],
    };

    // Connect to node IPC socket and perform handshake
    match ModuleIntegration::connect(
        args.socket_path.clone(),
        args.module_id.clone(),
        "simple-module".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
        Some(cli_spec),
    )
    .await
    {
        Ok(mut integration) => {
            info!("Module initialized and connected to node");

            let node_api = integration.node_api();
            let mut event_rx = integration.event_receiver();
            let invocation_rx = integration.invocation_receiver().unwrap();

            // Query chain tip as a sanity check
            if let Ok(tip) = node_api.get_chain_tip().await {
                info!("Chain tip: {}", hex::encode(tip));
            }

            // Main loop: process events and CLI invocations
            loop {
                tokio::select! {
                    event = event_rx.recv() => {
                        match event {
                            Ok(blvm_node::module::ipc::protocol::ModuleMessage::Event(e)) => {
                                info!("Received event: {:?}", e.event_type);
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    inv = invocation_rx.recv() => {
                        if let Some((invocation, result_tx)) = inv {
                            let result = handle_invocation(invocation).await;
                            let _ = result_tx.send(result);
                        } else {
                            break;
                        }
                    }
                    _ = sleep(Duration::from_secs(5)) => {
                        info!("Module running (heartbeat)");
                    }
                }
            }
        }
        Err(e) => {
            warn!("Could not connect to node IPC (node may not be running): {}", e);
            info!("Running in standalone mode (heartbeat only)");
            loop {
                info!("Module running (heartbeat)");
                sleep(Duration::from_secs(5)).await;
            }
        }
    }

    info!("Module shutting down");
    Ok(())
}

async fn handle_invocation(
    invocation: blvm_node::module::ipc::protocol::InvocationMessage,
) -> InvocationResultMessage {
    use blvm_node::module::ipc::protocol::InvocationResultPayload;
    let (success, payload, error) = match &invocation.invocation_type {
        InvocationType::Cli { subcommand, args } => {
            if subcommand == "greet" {
                let name = args.first().map(|s| s.as_str()).unwrap_or("world");
                let msg = format!("Hello, {}!\n", name);
                (
                    true,
                    Some(InvocationResultPayload::Cli {
                        stdout: msg,
                        stderr: String::new(),
                        exit_code: 0,
                    }),
                    None,
                )
            } else {
                (
                    false,
                    None,
                    Some(format!("Unknown subcommand: {}", subcommand)),
                )
            }
        }
        InvocationType::Rpc { .. } => (false, None, Some("RPC not implemented".to_string())),
    };
    InvocationResultMessage {
        correlation_id: invocation.correlation_id,
        success,
        payload,
        error,
    }
}

//! Simple example module for reference-node
//! 
//! This module demonstrates:
//! - Module lifecycle (init, start, stop, shutdown)
//! - IPC communication with node
//! - Querying blockchain data
//! - Subscribing to node events
//! 
//! Usage:
//!   simple-module --module-id <id> --socket-path <path> --data-dir <dir>

use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tracing::{info, warn, error};

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
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("simple_module=info,reference_node::module=debug")
        .init();
    
    let args = Args::parse();
    
    info!("Simple Module starting");
    info!("Module ID: {}", args.module_id);
    info!("Socket path: {:?}", args.socket_path);
    info!("Data dir: {:?}", args.data_dir);
    
    // Parse config from environment variables
    let mut config = HashMap::new();
    for (key, value) in std::env::vars() {
        if key.starts_with("MODULE_CONFIG_") {
            let config_key = key.strip_prefix("MODULE_CONFIG_").unwrap().to_lowercase();
            config.insert(config_key, value);
        }
    }
    
    info!("Module config: {:?}", config);
    
    // TODO: Connect to node IPC socket
    // let mut client = ModuleIpcClient::connect(&args.socket_path).await?;
    
    // TODO: Initialize module (connect to node, subscribe to events)
    info!("Module initialized");
    
    // Main module loop
    loop {
        // TODO: Process events from node
        // let event = client.receive_event().await?;
        
        // For now, just log periodic heartbeat
        info!("Module running (heartbeat)");
        sleep(Duration::from_secs(5)).await;
    }
    
    // This will never be reached, but shows structure
    // When shutdown is requested:
    // info!("Module shutting down");
    // Ok(())
}


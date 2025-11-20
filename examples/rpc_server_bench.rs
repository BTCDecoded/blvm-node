//! Simple RPC Server for Benchmarking
//!
//! This is a minimal RPC server that can be used for benchmarking.
//! It starts an HTTP/JSON-RPC server on the specified address.

use bllvm_node::rpc::RpcManager;
use std::net::SocketAddr;
use tokio::signal;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging using standard utility (respects RUST_LOG)
    bllvm_node::utils::init_logging(None);

    // Get server address from args or use default
    let server_addr: SocketAddr = match std::env::args().nth(1) {
        Some(addr) => addr.parse().unwrap_or_else(|_| {
            eprintln!("Invalid address: {}", addr);
            std::process::exit(1);
        }),
        None => "127.0.0.1:18332".parse().unwrap(),
    };

    info!("Starting Bitcoin Commons RPC server on {}", server_addr);

    // Create and start RPC manager
    let mut rpc_manager = RpcManager::new(server_addr);

    // Start server in background
    let server_handle = tokio::spawn(async move {
        if let Err(e) = rpc_manager.start().await {
            error!("RPC server error: {}", e);
            std::process::exit(1);
        }
    });

    // Wait for Ctrl+C or SIGTERM
    match signal::ctrl_c().await {
        Ok(()) => {
            info!("Shutting down RPC server...");
            server_handle.abort();
        }
        Err(err) => {
            error!("Unable to listen for shutdown signal: {}", err);
            std::process::exit(1);
        }
    }

    Ok(())
}

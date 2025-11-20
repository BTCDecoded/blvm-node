//! Signal handling utilities for graceful shutdown
//!
//! Provides signal handlers for SIGTERM, SIGINT, and other termination signals.

use tokio::signal;
use tracing::{info, warn};

/// Wait for shutdown signal (SIGTERM, SIGINT, or Ctrl+C)
///
/// Returns when a termination signal is received.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to register SIGTERM handler: {}", e);
                // Fall back to Ctrl+C only
                signal::ctrl_c().await.ok();
                return;
            }
        };

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to register SIGINT handler: {}", e);
                // Fall back to Ctrl+C only
                signal::ctrl_c().await.ok();
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down gracefully...");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT, shutting down gracefully...");
            }
            _ = signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down gracefully...");
            }
        }
    }

    #[cfg(not(unix))]
    {
        // On non-Unix systems, just use Ctrl+C
        match signal::ctrl_c().await {
            Ok(()) => {
                info!("Received Ctrl+C, shutting down gracefully...");
            }
            Err(e) => {
                warn!("Failed to listen for shutdown signal: {}", e);
            }
        }
    }
}

/// Create a shutdown signal future
///
/// Returns a future that completes when shutdown is requested.
pub async fn shutdown_signal() {
    wait_for_shutdown_signal().await;
}

/// Create a shutdown signal receiver
///
/// Returns a channel that receives a message when shutdown is requested.
/// Note: oneshot::Receiver doesn't have try_recv(), so use with tokio::select! or check in async context.
pub fn create_shutdown_receiver() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = tx.send(true);
    });

    rx
}


//! Signal handling utilities for graceful shutdown
//!
//! Provides signal handlers for SIGTERM, SIGINT, and other termination signals.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use tokio::signal;
use tokio::sync::watch;
use tracing::{info, warn};

struct ShutdownWatch {
    tx: watch::Sender<bool>,
    /// Keeps the watch channel open for the lifetime of the process.
    _rx: watch::Receiver<bool>,
}

static SHUTDOWN: OnceLock<ShutdownWatch> = OnceLock::new();
static GRACE_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn shutdown_watch() -> &'static ShutdownWatch {
    SHUTDOWN.get_or_init(|| {
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            begin_shutdown();
        });
        ShutdownWatch { tx, _rx: rx }
    })
}

/// Set the process shutdown watch and arm the 180-second backstop once.
///
/// SIGTERM and RPC `stop` both come through here. The main loop's 30-second
/// exit still runs first when a worker can poll. Unit tests do not arm the
/// process exit: a 180-second `exit(0)` would kill `cargo test`.
fn begin_shutdown() {
    #[cfg(feature = "production")]
    crate::node::parallel_ibd::IBD_SHUTDOWN_REQUESTED.store(true, Ordering::Release);
    let _ = shutdown_watch().tx.send(true);
    if GRACE_ARMED.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(not(test))]
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(180)).await;
        warn!("Shutdown grace period elapsed — forcing process exit");
        std::process::exit(0);
    });
}

/// Ask the process to shut down the same way SIGTERM does.
///
/// Safe to call more than once. The RPC `stop` callback uses this.
pub fn request_shutdown() {
    let _ = shutdown_watch();
    begin_shutdown();
}

/// Wait for shutdown signal (SIGTERM, SIGINT, or Ctrl+C)
///
/// Returns when a termination signal is received.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

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
        // Console Ctrl+C and the WinSW sample's service stop both land here.
        // WinSW sends Ctrl+C. There is no service-control handler, so
        // `sc.exe stop` does not reach this watch.
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

/// Subscribe to the process-wide shutdown watch channel.
///
/// The OS signal handler is registered once; all subscribers receive the same notification.
pub fn create_shutdown_receiver() -> watch::Receiver<bool> {
    shutdown_watch().tx.subscribe()
}

/// Returns true after a shutdown signal has been received.
pub fn shutdown_requested() -> bool {
    *shutdown_watch().tx.borrow()
}

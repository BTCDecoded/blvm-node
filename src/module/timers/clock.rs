//! Wall time and chain notifications for the timer actor.
//!
//! `WallClock` is the UTC millisecond source. Production reads the system clock.
//! Tests substitute a fake so a step can be injected without waiting.
//! `ChainEvent` is the connect and disconnect feed. It is not the module event bus.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

/// UTC unix milliseconds.
pub trait WallClock: Send + Sync {
    fn unix_ms(&self) -> u64;
}

pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Test clock. `set` is the step.
pub struct FakeWallClock {
    ms: AtomicU64,
}

impl FakeWallClock {
    pub fn new(ms: u64) -> Arc<Self> {
        Arc::new(Self {
            ms: AtomicU64::new(ms),
        })
    }

    pub fn set(&self, ms: u64) {
        self.ms.store(ms, Ordering::SeqCst);
    }
}

impl WallClock for FakeWallClock {
    fn unix_ms(&self) -> u64 {
        self.ms.load(Ordering::SeqCst)
    }
}

/// What the node tells the actor when the active chain changes.
#[derive(Debug, Clone, Copy)]
pub enum ChainEvent {
    Connected {
        height: u64,
        hash: [u8; 32],
        mtp: u64,
    },
    Disconnected {
        height: u64,
        hash: [u8; 32],
    },
}

static CHAIN_TX: OnceLock<mpsc::UnboundedSender<ChainEvent>> = OnceLock::new();

pub(crate) fn install_chain_feed(tx: mpsc::UnboundedSender<ChainEvent>) {
    let _ = CHAIN_TX.set(tx);
}

/// Called from a block connect path that does not go through the event publisher.
/// No-op until the node installs the feed.
pub fn dispatch_chain(event: ChainEvent) {
    if let Some(tx) = CHAIN_TX.get() {
        let _ = tx.send(event);
    }
}

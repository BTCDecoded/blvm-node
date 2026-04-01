//! Named duration constants for polling, cache refresh, and backoff.
//!
//! Centralizes tuning parameters so intent is clear and changes don't require
//! searching the codebase. For timeout limits, see `crate::utils::timeout`.

use std::time::Duration;

/// Cache TTL for chain tip hash/difficulty (getblockchaininfo, get_best_block_hash).
pub const CACHE_REFRESH_TIP: Duration = Duration::from_secs(1);

/// Cache TTL for uptime (getuptime).
pub const CACHE_REFRESH_UPTIME: Duration = Duration::from_millis(100);

/// Cache TTL for memory stats (getmemoryinfo).
pub const CACHE_REFRESH_MEMORY: Duration = Duration::from_secs(5);

/// Poll interval for waitfornewblock / waitforblock / waitforblockheight.
pub const POLL_INTERVAL_WAIT_FOR_BLOCK: Duration = Duration::from_millis(500);

/// Interval for auth rate limiter cleanup (stale entries).
pub const AUTH_RATE_LIMITER_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// Sleep between handshake message poll iterations (node startup).
pub const HANDSHAKE_POLL_SLEEP: Duration = Duration::from_millis(100);

/// Sleep in background message processor loop (avoids busy-loop when no messages).
pub const MESSAGE_PROCESSOR_POLL_SLEEP: Duration = Duration::from_millis(1);

/// Sleep between mempool loop iterations (prevents busy waiting).
pub const MEMPOOL_LOOP_SLEEP: Duration = Duration::from_millis(100);

/// Yield sleep in IBD when reorder buffer is full (avoids busy spin).
pub const IBD_YIELD_SLEEP: Duration = Duration::from_millis(1);

/// Backoff when a background task loop has work queued (avoids busy spin).
pub const BACKGROUND_TASK_BACKOFF_SLEEP: Duration = Duration::from_millis(500);

/// Brief wait after starting RPC server before connecting (allows bind/listen).
pub const RPC_SERVER_STARTUP_WAIT: Duration = Duration::from_millis(50);

/// Timeout for reading a single response in RPC server tests / health check.
pub const RPC_CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Delay after unloading a module before reload (allows cleanup to complete).
pub const MODULE_RELOAD_CLEANUP_DELAY: Duration = Duration::from_millis(500);

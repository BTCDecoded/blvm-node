//! Block chunk download for parallel IBD.
//!
//! Downloads blocks from a peer using pipelined, batched GetData requests.
//! Up to GETDATA_BATCH (64) hashes per GetData.
//! block hashes are sent per GetData message to reduce per-message overhead.
//! Max blocks_in_transit_per_peer across all workers is
//! configurable (default 128).

use super::local_block::{
    cached_feature_registry, empty_witness_unacceptable, has_real_witnesses,
    ibd_stall_aborts_inflight_gap_fetch, is_local_witness_hole, try_load_local_ibd_block,
    try_persist_gap_block_for_local_inject, try_persist_gap_block_for_local_inject_with_wire,
    try_repair_missing_witness,
};
use super::types::{SharedBlock, SharedWitnesses};
use crate::network::NetworkManager;
use crate::network::inventory::{MSG_BLOCK, MSG_WITNESS_BLOCK};
use crate::network::protocol::{GetDataMessage, InventoryVector, ProtocolMessage, ProtocolParser};
use crate::storage::blockstore::BlockStore;
use anyhow::{Context, Result};
use blvm_protocol::types::ARC_BLOCK_CREATED;
use blvm_protocol::{Block, Hash, ProtocolVersion, segwit::Witness};
use futures::stream::{FuturesUnordered, StreamExt};
use hex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};
use tracing::{debug, info, warn};

use super::ParallelIBDConfig;
use super::latch_env;

/// Synthetic peer id for zero-peer local replay (`BLVM_IBD_ALLOW_ZERO_PEERS`).
/// Workers use `try_load_local_ibd_block` only — never GetData / connect.
pub(crate) const LOCAL_DISK_PEER_ID: &str = "local-disk";

/// True for `local-disk` and `local-disk-N` (fixture multi-peer so `sequential=false`).
pub(crate) fn is_local_disk_peer(peer_id: &str) -> bool {
    peer_id == LOCAL_DISK_PEER_ID || peer_id.starts_with("local-disk-")
}

/// How many synthetic local-disk peers to inject on a zero-peer fixture.
/// Default 1 (legacy sequential). Set `BLVM_IBD_LOCAL_DISK_PEERS=2` so
/// `num_peers > 1` and coordinator `sequential` is false. Does not change
/// the sequential dispatch rule itself.
pub(crate) fn local_disk_peer_count() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_LOCAL_DISK_PEERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .clamp(1, 8)
    })
}

pub(crate) fn local_disk_peer_ids() -> Vec<String> {
    let n = local_disk_peer_count();
    (0..n)
        .map(|i| {
            if i == 0 {
                LOCAL_DISK_PEER_ID.to_string()
            } else {
                format!("{LOCAL_DISK_PEER_ID}-{i}")
            }
        })
        .collect()
}

/// Per-height HASH_FETCH lines (LOCAL_MISS / took / TIP_ENTER / batch / download)
/// were ~86 KiB/block and 11G by 93k. Count them; emit one line per 5s.
pub(crate) fn log_hf_hot(kind: &'static str, height: u64) {
    use std::sync::atomic::AtomicU64;
    static N_MISS: AtomicU64 = AtomicU64::new(0);
    static N_TOOK: AtomicU64 = AtomicU64::new(0);
    static N_ENTER: AtomicU64 = AtomicU64::new(0);
    static N_BATCH: AtomicU64 = AtomicU64::new(0);
    static N_DL: AtomicU64 = AtomicU64::new(0);
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static LAST_H: AtomicU64 = AtomicU64::new(0);
    match kind {
        "LOCAL_MISS" => {
            N_MISS.fetch_add(1, Ordering::Relaxed);
        }
        "took" => {
            N_TOOK.fetch_add(1, Ordering::Relaxed);
        }
        "TIP_ENTER" => {
            N_ENTER.fetch_add(1, Ordering::Relaxed);
        }
        "batch" => {
            N_BATCH.fetch_add(1, Ordering::Relaxed);
        }
        "download" => {
            N_DL.fetch_add(1, Ordering::Relaxed);
        }
        "idle" => {}
        _ => {}
    }
    LAST_H.store(height, Ordering::Relaxed);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) >= 5_000
        && LAST_MS
            .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        info!(
            "[IBD_HF_LOG] local_miss={} took={} tip_enter={} batch={} download={} last_h={} hf_missing={} hf_inflight={} hf_peers={} hf_tip_expire={} hf_tip_dup={} hf_tip_esc={} {} {} {}",
            N_MISS.load(Ordering::Relaxed),
            N_TOOK.load(Ordering::Relaxed),
            N_ENTER.load(Ordering::Relaxed),
            N_BATCH.load(Ordering::Relaxed),
            N_DL.load(Ordering::Relaxed),
            LAST_H.load(Ordering::Relaxed),
            crate::node::parallel_ibd::hash_fetch::missing_len(),
            crate::node::parallel_ibd::hash_fetch::inflight_len(),
            crate::node::parallel_ibd::hash_fetch::inflight_peer_count(),
            crate::node::parallel_ibd::hash_fetch::tip_expire_count(),
            crate::node::parallel_ibd::hash_fetch::tip_dup_count(),
            crate::node::parallel_ibd::hash_fetch::tip_esc_count(),
            crate::node::parallel_ibd::hash_fetch::gd_volume_suffix(),
            crate::node::parallel_ibd::hash_fetch::byte_share_suffix(),
            crate::node::parallel_ibd::hash_fetch::tip_state_suffix()
        );
    }
}

include!("download_parts/admit.rs");
include!("download_parts/gap.rs");
include!("download_parts/fetch.rs");

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;

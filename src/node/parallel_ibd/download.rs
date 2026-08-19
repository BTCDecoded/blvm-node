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
use std::sync::Arc;
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

pub(crate) fn is_local_disk_peer(peer_id: &str) -> bool {
    peer_id == LOCAL_DISK_PEER_ID
}

include!("download_parts/admit.rs");
include!("download_parts/gap.rs");
include!("download_parts/fetch.rs");

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
